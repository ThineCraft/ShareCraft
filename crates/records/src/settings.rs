//! Organisation settings: a few known keys, each with a value of a known type, changed only by
//! administrators: sharing, the regional defaults of sites, and the file types blocked
//! everywhere. Every change records `setting.changed`.

use collab_core::ids::UserId;
use serde_json::{Value, json};
use sqlx::PgConnection;

use crate::events::{NewEvent, record};
use crate::policies::{BLOCKED_FILE_TYPES, valid_blocked_types};
use crate::s3_endpoints::{
    S3_BYTES_DOWN, S3_ENDPOINTS, S3_LIST_REQUESTS, S3_READ_REQUESTS, valid_bytes_setting,
    valid_endpoints_setting, valid_request_setting,
};
use crate::scans::{SCANNER_SETTINGS, SCANNER_URL, valid_scanner_setting};
use crate::share_paths::{
    SHARE_CACHE_MAX_BYTES, SHARE_LIST_SECONDS, SHARE_ROOTS, valid_cache_setting,
    valid_list_setting, valid_roots_setting,
};
use crate::sites::{DEFAULT_LANGUAGE, DEFAULT_TIME_ZONE, validate_language, validate_time_zone};
use crate::sources::{SOURCE_MAX_BYTES, SOURCE_MAX_FILES};
use crate::{AccessError, Db, DbError};

/// Whether anyone links may be created and used (a boolean; off by default).
pub const ANYONE_LINKS: &str = "anyone_links";

/// The longest an anyone link may last, in days (an integer from 1 to 365; 30 by default).
pub const ANYONE_LINK_MAX_DAYS: &str = "anyone_link_max_days";

const MAX_DAYS_LIMIT: i64 = 365;

/// The sharing settings, as read together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SharingSettings {
    /// Whether anyone links work.
    pub anyone_links: bool,
    /// The longest an anyone link may last, in days.
    pub anyone_link_max_days: i64,
}

/// `value` if it is acceptable for the setting `key`; `InvalidSetting` for an unknown key (the
/// global navigation is not set here) or a value of the wrong type or out of range,
/// `InvalidLanguage` or `InvalidTimeZone` for a default language or time zone that is not one.
fn validate(key: &str, value: &Value) -> Result<(), AccessError> {
    let text = || value.as_str().unwrap_or_default();
    let fits = match key {
        DEFAULT_LANGUAGE => validate_language(text()).is_ok(),
        DEFAULT_TIME_ZONE => validate_time_zone(text()).is_ok(),
        ANYONE_LINKS => value.is_boolean(),
        BLOCKED_FILE_TYPES => valid_blocked_types(value),
        key if SCANNER_SETTINGS.contains(&key) => valid_scanner_setting(key, value),
        SOURCE_MAX_FILES | SOURCE_MAX_BYTES => value.as_i64().is_some_and(|n| n >= 1),
        SHARE_ROOTS => valid_roots_setting(value),
        SHARE_CACHE_MAX_BYTES => valid_cache_setting(value),
        SHARE_LIST_SECONDS => valid_list_setting(value),
        S3_ENDPOINTS => valid_endpoints_setting(value),
        S3_LIST_REQUESTS | S3_READ_REQUESTS => valid_request_setting(value),
        S3_BYTES_DOWN => valid_bytes_setting(value),
        ANYONE_LINK_MAX_DAYS => value
            .as_i64()
            .is_some_and(|days| (1..=MAX_DAYS_LIMIT).contains(&days)),
        _ => false,
    };
    match (fits, key) {
        (true, _) => Ok(()),
        (false, DEFAULT_LANGUAGE) => Err(AccessError::InvalidLanguage),
        (false, DEFAULT_TIME_ZONE) => Err(AccessError::InvalidTimeZone),
        (false, SCANNER_URL) => Err(AccessError::InvalidScannerUrl),
        (false, _) => Err(AccessError::InvalidSetting),
    }
}

/// The sharing settings, on `conn`; with `share`, their rows are locked `FOR SHARE` so that
/// they hold until the transaction ends.
pub(crate) async fn sharing_in(
    conn: &mut PgConnection,
    share: bool,
) -> Result<SharingSettings, sqlx::Error> {
    let statement = if share {
        "select key, value from settings where key = any($1) order by key for share"
    } else {
        "select key, value from settings where key = any($1) order by key"
    };
    let rows: Vec<(String, Value)> = sqlx::query_as(statement)
        .bind(vec![ANYONE_LINKS, ANYONE_LINK_MAX_DAYS])
        .fetch_all(conn)
        .await?;
    let mut settings = SharingSettings {
        anyone_links: false,
        anyone_link_max_days: 30,
    };
    for (key, value) in rows {
        match key.as_str() {
            ANYONE_LINKS => settings.anyone_links = value.as_bool().unwrap_or(false),
            ANYONE_LINK_MAX_DAYS => {
                settings.anyone_link_max_days = value.as_i64().unwrap_or(30);
            }
            _ => {}
        }
    }
    Ok(settings)
}

impl Db {
    /// The value of the setting `key`, if it is a known setting.
    pub async fn setting(&self, key: &str) -> Result<Option<Value>, DbError> {
        Ok(
            sqlx::query_scalar("select value from settings where key = $1")
                .bind(key)
                .fetch_optional(self.pool())
                .await?,
        )
    }

    /// The sharing settings.
    pub async fn sharing_settings(&self) -> Result<SharingSettings, DbError> {
        let mut conn = self.pool().acquire().await?;
        Ok(sharing_in(&mut conn, false).await?)
    }

    /// Sets `key` to `value`. `actor` must be an enabled administrator (`Forbidden`);
    /// `InvalidSetting` for an unknown key or a value it does not take (`anyone_links` a
    /// boolean, `anyone_link_max_days` an integer from 1 to 365, `default_language` a language
    /// tag and `default_time_zone` a time zone name: `InvalidLanguage` and `InvalidTimeZone`
    /// for those; `blocked_file_types` a clean list of extensions, see
    /// [`crate::clean_blocked_types`]; the scanner settings as [`crate::ScannerSettings`]
    /// describes them, `InvalidScannerUrl` for an address that is not one). Records `setting.changed` with the values before and after, unless the value is unchanged. Switching
    /// `anyone_links` off stops every anyone link at once without revoking any.
    pub async fn set_setting(
        &self,
        actor: UserId,
        key: &str,
        value: Value,
    ) -> Result<(), AccessError> {
        validate(key, &value)?;
        let mut tx = self.pool().begin().await?;
        let admin: Option<bool> = sqlx::query_scalar(
            "select is_admin and disabled_at is null from users where id = $1 for share",
        )
        .bind(actor.0)
        .fetch_optional(&mut *tx)
        .await?;
        if admin != Some(true) {
            return Err(AccessError::Forbidden);
        }
        let before: Option<Value> =
            sqlx::query_scalar("select value from settings where key = $1 for update")
                .bind(key)
                .fetch_optional(&mut *tx)
                .await?;
        if before.as_ref() == Some(&value) {
            return Ok(());
        }
        sqlx::query(
            "insert into settings (key, value, changed_by, changed_at) values ($1, $2, $3, now()) \
             on conflict (key) do update set value = excluded.value, \
                 changed_by = excluded.changed_by, changed_at = excluded.changed_at",
        )
        .bind(key)
        .bind(&value)
        .bind(actor.0)
        .execute(&mut *tx)
        .await?;
        let event_values = if key == S3_ENDPOINTS {
            (
                before.as_ref().map(|_| json!("[redacted]")),
                json!("[redacted]"),
            )
        } else {
            (before.clone(), value.clone())
        };
        record(
            &mut tx,
            NewEvent {
                kind: "setting.changed",
                actor: Some(actor),
                subject: None,
                document: None,
                node: None,
                detail: json!({ "key": key, "from": event_values.0, "to": event_values.1 }),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }
}
