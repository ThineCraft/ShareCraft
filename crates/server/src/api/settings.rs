//! The organisation's settings: sharing (whether anyone links may be made and used, and how
//! long one may last), the regional defaults of sites (language and time zone), the file
//! types blocked everywhere, and the malware scanner.

use axum::Json;
use axum::extract::State;
use records::{
    ANYONE_LINK_MAX_DAYS, ANYONE_LINKS, DEFAULT_LANGUAGE, DEFAULT_TIME_ZONE, RegionalDefaults,
    SCANNER_MAX_BYTES, SCANNER_TIMEOUT_SECONDS, SCANNER_TOO_LARGE, SCANNER_UNAVAILABLE,
    SCANNER_URL, ScannerSettings, SharingSettings, valid_scanner_setting, validate_language,
    validate_time_zone,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::{ApiError, JsonBody};
use crate::AppState;
use crate::auth::Signed;
use crate::icap::Client;

/// The longest the test of new scanner settings waits for the scanner.
const TEST_TIMEOUT: Duration = Duration::from_secs(10);

/// The longest `anyone_link_max_days` may be, as the records layer allows.
const MAX_DAYS_LIMIT: i64 = 365;

/// `GET /api/settings/sharing`.
#[derive(Serialize)]
pub(crate) struct SharingOut {
    anyone_links: bool,
    anyone_link_max_days: i64,
}

impl From<SharingSettings> for SharingOut {
    fn from(settings: SharingSettings) -> Self {
        SharingOut {
            anyone_links: settings.anyone_links,
            anyone_link_max_days: settings.anyone_link_max_days,
        }
    }
}

/// `GET /api/settings/sharing`: the sharing settings, for anyone who manages access somewhere
/// (so that they know what links they may make) and administrators; `403` for anyone else.
pub(crate) async fn show(
    State(state): State<AppState>,
    signed: Signed,
) -> Result<Json<SharingOut>, ApiError> {
    let db = state.db();
    let manages = signed.session.user.is_admin
        || db
            .manages_access_anywhere(signed.session.user.id)
            .await
            .map_err(|e| ApiError::unavailable(&e))?;
    if !manages {
        return Err(ApiError::Forbidden);
    }
    let settings = db
        .sharing_settings()
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
    Ok(Json(settings.into()))
}

/// The body of `PUT /api/settings/sharing`: the settings to change, at least one.
#[derive(Deserialize)]
pub(crate) struct SharingChange {
    /// A boolean.
    anyone_links: Option<Value>,
    /// An integer from 1 to 365.
    anyone_link_max_days: Option<Value>,
}

/// `PUT /api/settings/sharing {anyone_links?, anyone_link_max_days?}`: changes the settings
/// given, for administrators (`403` otherwise); `422 invalid_request` for a value of the wrong
/// type or out of range, or none given. Answers the settings as they then are. Switching
/// anyone links off stops every anyone link at once.
pub(crate) async fn change(
    State(state): State<AppState>,
    signed: Signed,
    JsonBody(change): JsonBody<SharingChange>,
) -> Result<Json<SharingOut>, ApiError> {
    // A cheap early refusal; the records layer decides again.
    if !signed.session.user.is_admin {
        return Err(ApiError::Forbidden);
    }
    let changes: Vec<(&str, Value)> = [
        (ANYONE_LINKS, change.anyone_links),
        (ANYONE_LINK_MAX_DAYS, change.anyone_link_max_days),
    ]
    .into_iter()
    .filter_map(|(key, value)| value.map(|value| (key, value)))
    .collect();
    // Checked here as well as by the records layer, so that a bad second value never leaves
    // the first one changed.
    let acceptable = |key: &str, value: &Value| match key {
        ANYONE_LINKS => value.is_boolean(),
        _ => value
            .as_i64()
            .is_some_and(|days| (1..=MAX_DAYS_LIMIT).contains(&days)),
    };
    if changes.is_empty() || !changes.iter().all(|(key, value)| acceptable(key, value)) {
        return Err(ApiError::InvalidRequest);
    }
    let db = state.db();
    let actor = signed.session.user.id;
    for (key, value) in changes {
        db.set_setting(actor, key, value).await?;
    }
    let settings = db
        .sharing_settings()
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
    Ok(Json(settings.into()))
}

/// `GET /api/settings/regional`.
#[derive(Serialize)]
pub(crate) struct RegionalOut {
    default_language: String,
    default_time_zone: String,
}

impl From<RegionalDefaults> for RegionalOut {
    fn from(defaults: RegionalDefaults) -> Self {
        RegionalOut {
            default_language: defaults.language,
            default_time_zone: defaults.time_zone,
        }
    }
}

/// `GET /api/settings/regional`: the organisation's default language and time zone, used by
/// pages outside a site and by new sites; for anyone signed in.
pub(crate) async fn regional(
    State(state): State<AppState>,
    _signed: Signed,
) -> Result<Json<RegionalOut>, ApiError> {
    let defaults = state
        .db()
        .regional_defaults()
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
    Ok(Json(defaults.into()))
}

/// The body of `PUT /api/settings/regional`: the defaults to change, at least one.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RegionalChange {
    default_language: Option<String>,
    default_time_zone: Option<String>,
}

/// `PUT /api/settings/regional {default_language?, default_time_zone?}`: changes the defaults
/// given, for administrators (`403` otherwise); `422 invalid_language` or `invalid_time_zone`
/// for a value that is not one, `invalid_request` for none given. Answers the defaults as they
/// then are. Sites with a record keep their own settings; sites with none follow the new
/// defaults at once.
pub(crate) async fn change_regional(
    State(state): State<AppState>,
    signed: Signed,
    JsonBody(change): JsonBody<RegionalChange>,
) -> Result<Json<RegionalOut>, ApiError> {
    if !signed.session.user.is_admin {
        return Err(ApiError::Forbidden);
    }
    // Both are checked before either is set, so a bad second value never leaves the first
    // one changed.
    if let Some(tag) = &change.default_language {
        validate_language(tag)?;
    }
    if let Some(zone) = &change.default_time_zone {
        validate_time_zone(zone)?;
    }
    let changes: Vec<(&str, String)> = [
        (DEFAULT_LANGUAGE, change.default_language),
        (DEFAULT_TIME_ZONE, change.default_time_zone),
    ]
    .into_iter()
    .filter_map(|(key, value)| value.map(|value| (key, value)))
    .collect();
    if changes.is_empty() {
        return Err(ApiError::InvalidRequest);
    }
    let db = state.db();
    let actor = signed.session.user.id;
    // Under the tree lock, so that no site record is written while the sites with none are
    // brought to the new defaults (they read as the organisation's current defaults).
    let _turn = state.tree_lock().lock().await;
    for (key, value) in changes {
        db.set_setting(actor, key, Value::String(value)).await?;
    }
    if !crate::sites::follow_defaults(&state).await {
        state.tree_out_of_step().store(true, Ordering::SeqCst);
    }
    let defaults = db
        .regional_defaults()
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
    Ok(Json(defaults.into()))
}

/// `GET /api/settings/files`.
#[derive(Serialize)]
pub(crate) struct FilesOut {
    blocked_file_types: Vec<String>,
}

/// `GET /api/settings/files`: the organisation's block list of file types, for administrators
/// (`403` otherwise).
pub(crate) async fn files(
    State(state): State<AppState>,
    signed: Signed,
) -> Result<Json<FilesOut>, ApiError> {
    if !signed.session.user.is_admin {
        return Err(ApiError::Forbidden);
    }
    let policy = state
        .db()
        .org_policy()
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
    Ok(Json(FilesOut {
        blocked_file_types: policy.blocked_file_types,
    }))
}

/// The body of `PUT /api/settings/files`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FilesChange {
    /// Extensions, such as `.exe`; trimmed and lower-cased.
    blocked_file_types: Value,
}

/// `PUT /api/settings/files {blocked_file_types}`: sets the organisation's block list, for
/// administrators (`403` otherwise): at most 100 extensions (each trimmed and lower-cased, then
/// 2 to 20 characters of letters, digits and dots with its leading dot), none twice;
/// `422 invalid_request` otherwise. Recorded as `setting.changed`. Answers the list as it then
/// is. It applies at once to every new name, upload, copy and move; files already stored stay,
/// and are marked in their listings. Set under the tree's turn, so a change of the tree never
/// sees the list half way.
pub(crate) async fn change_files(
    State(state): State<AppState>,
    signed: Signed,
    JsonBody(change): JsonBody<FilesChange>,
) -> Result<Json<FilesOut>, ApiError> {
    if !signed.session.user.is_admin {
        return Err(ApiError::Forbidden);
    }
    let _turn = state.tree_lock().lock().await;
    let blocked_file_types = state
        .db()
        .set_blocked_file_types(signed.session.user.id, &change.blocked_file_types)
        .await?;
    Ok(Json(FilesOut { blocked_file_types }))
}

/// `GET /api/settings/scanner`.
#[derive(Serialize)]
pub(crate) struct ScannerOut {
    scanner_url: String,
    scanner_timeout_seconds: u64,
    scanner_max_bytes: u64,
    scanner_unavailable: &'static str,
    scanner_too_large: &'static str,
}

impl From<ScannerSettings> for ScannerOut {
    fn from(settings: ScannerSettings) -> Self {
        let choice = |allow: bool| if allow { "allow" } else { "hold" };
        ScannerOut {
            scanner_url: settings.url_text,
            scanner_timeout_seconds: settings.timeout_seconds,
            scanner_max_bytes: settings.max_bytes,
            scanner_unavailable: choice(settings.unavailable_allow),
            scanner_too_large: choice(settings.too_large_allow),
        }
    }
}

/// `GET /api/settings/scanner`: the malware scanner settings, for administrators (`403`
/// otherwise).
pub(crate) async fn scanner(
    State(state): State<AppState>,
    signed: Signed,
) -> Result<Json<ScannerOut>, ApiError> {
    if !signed.session.user.is_admin {
        return Err(ApiError::Forbidden);
    }
    let settings = state
        .db()
        .scanner_settings()
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
    Ok(Json(settings.into()))
}

/// The body of `PUT /api/settings/scanner`: the settings to change, at least one.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScannerChange {
    /// `icap://host[:port]/service`, or empty for no scanner.
    scanner_url: Option<Value>,
    /// 5 to 3600.
    scanner_timeout_seconds: Option<Value>,
    /// At least 1.
    scanner_max_bytes: Option<Value>,
    /// `hold` or `allow`.
    scanner_unavailable: Option<Value>,
    /// `hold` or `allow`.
    scanner_too_large: Option<Value>,
}

/// `PUT /api/settings/scanner {scanner_url?, scanner_timeout_seconds?, scanner_max_bytes?,
/// scanner_unavailable?, scanner_too_large?}`: changes the settings given, for administrators
/// (`403` otherwise), each recorded as `setting.changed`; `422 invalid_scanner_url` for an
/// address that is not `icap://host[:port]/service` (no user name or password), `422
/// invalid_request` for any other value it does not take, or none given; nothing is changed
/// then. The scanner is then asked for its `OPTIONS`: the answer is `{ok: true, server}` when
/// it answers (`server: null` with no scanner), `{ok: false, error}` when it does not. The
/// settings are kept either way, so new files wait until the scanner can be reached. Both carry
/// `settings`, as they then are.
pub(crate) async fn change_scanner(
    State(state): State<AppState>,
    signed: Signed,
    JsonBody(change): JsonBody<ScannerChange>,
) -> Result<Json<Value>, ApiError> {
    if !signed.session.user.is_admin {
        return Err(ApiError::Forbidden);
    }
    let changes: Vec<(&str, Value)> = [
        (SCANNER_URL, change.scanner_url),
        (SCANNER_TIMEOUT_SECONDS, change.scanner_timeout_seconds),
        (SCANNER_MAX_BYTES, change.scanner_max_bytes),
        (SCANNER_UNAVAILABLE, change.scanner_unavailable),
        (SCANNER_TOO_LARGE, change.scanner_too_large),
    ]
    .into_iter()
    .filter_map(|(key, value)| value.map(|value| (key, value)))
    .collect();
    if changes.is_empty() {
        return Err(ApiError::InvalidRequest);
    }
    // All checked first, so that a bad value never leaves another one changed.
    for (key, value) in &changes {
        if !valid_scanner_setting(key, value) {
            return Err(if *key == SCANNER_URL {
                ApiError::InvalidScannerUrl
            } else {
                ApiError::InvalidRequest
            });
        }
    }
    let db = state.db();
    let actor = signed.session.user.id;
    for (key, value) in changes {
        db.set_setting(actor, key, value).await?;
    }
    state.scanner().wake();
    let settings = db
        .scanner_settings()
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
    let mut answer = match &settings.url {
        None => json!({ "ok": true, "server": null }),
        Some(url) => {
            let timeout = Duration::from_secs(settings.timeout_seconds).min(TEST_TIMEOUT);
            match Client::new(url.clone(), timeout).options().await {
                Ok(options) => json!({ "ok": true, "server": options.server.or(options.istag) }),
                Err(e) => json!({ "ok": false, "error": e.to_string() }),
            }
        }
    };
    if let Some(object) = answer.as_object_mut() {
        object.insert(
            "settings".to_owned(),
            serde_json::to_value(ScannerOut::from(settings)).unwrap_or(Value::Null),
        );
    }
    Ok(Json(answer))
}
