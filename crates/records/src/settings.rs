//! Organisation settings: a few known keys, each with a value of a known type, changed only by
//! administrators. Every change records `setting.changed`.

use collab_core::ids::UserId;
use serde_json::{Value, json};
use sqlx::PgConnection;

use crate::events::{NewEvent, record};
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

/// `value` if it is acceptable for the setting `key`; `InvalidSetting` for an unknown key or a
/// value of the wrong type or out of range.
fn validate(key: &str, value: &Value) -> Result<(), AccessError> {
    let fits = match key {
        ANYONE_LINKS => value.is_boolean(),
        ANYONE_LINK_MAX_DAYS => value
            .as_i64()
            .is_some_and(|days| (1..=MAX_DAYS_LIMIT).contains(&days)),
        _ => false,
    };
    if fits {
        Ok(())
    } else {
        Err(AccessError::InvalidSetting)
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
    /// boolean, `anyone_link_max_days` an integer from 1 to 365). Records `setting.changed`
    /// with the values before and after, unless the value is unchanged. Switching
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
        record(
            &mut tx,
            NewEvent {
                kind: "setting.changed",
                actor: Some(actor),
                subject: None,
                document: None,
                node: None,
                detail: json!({ "key": key, "from": before, "to": value }),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }
}
