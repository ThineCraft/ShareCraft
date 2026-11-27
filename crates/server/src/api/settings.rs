//! The organisation's settings: sharing (whether anyone links may be made and used, and how
//! long one may last), the regional defaults of sites (language and time zone), the file
//! types blocked everywhere, the malware scanner, the approved file shares and the approved
//! S3 endpoints with their default budgets.

use axum::Json;
use axum::extract::State;
use records::{
    ANYONE_LINK_MAX_DAYS, ANYONE_LINKS, DEFAULT_LANGUAGE, DEFAULT_TIME_ZONE, EndpointRefusal,
    IpRange, PathRefusal, RegionalDefaults, S3_BYTES_DOWN, S3_ENDPOINTS, S3_LIST_REQUESTS,
    S3_READ_REQUESTS, S3Endpoint, SCANNER_MAX_BYTES, SCANNER_TIMEOUT_SECONDS, SCANNER_TOO_LARGE,
    SCANNER_UNAVAILABLE, SCANNER_URL, SHARE_CACHE_MAX_BYTES, SHARE_LIST_SECONDS, SHARE_ROOTS,
    SOURCE_MAX_FILES, ScannerSettings, SharePath, SharingSettings, usable_here, valid_root,
    valid_scanner_setting, validate_language, validate_time_zone,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::{ApiError, JsonBody};
use crate::AppState;
use crate::auth::Signed;
use crate::icap::Client;
use crate::share::canonical_share_path;

/// The longest the test of new scanner settings waits for the scanner.
const TEST_TIMEOUT: Duration = Duration::from_secs(10);

/// The longest `anyone_link_max_days` may be, as the records layer allows.
const MAX_DAYS_LIMIT: i64 = 365;

pub(super) fn path_reason(reason: PathRefusal) -> &'static str {
    match reason {
        PathRefusal::NotAbsolute | PathRefusal::Empty => "relative",
        PathRefusal::Traversal => "traversal",
        PathRefusal::DevicePrefix => "device",
        PathRefusal::Stream => "stream",
        PathRefusal::TooLong => "too_long",
        PathRefusal::WholeDrive => "whole_drive",
        PathRefusal::AdministrativeShare => "administrative_share",
        PathRefusal::OtherSystem => "other_system",
        PathRefusal::EmptySegment
        | PathRefusal::BadCharacter
        | PathRefusal::DeviceName
        | PathRefusal::TrailingDotOrSpace => "characters",
    }
}

/// Whether `path` and the server's private data folder overlap, lexically or after resolving
/// links. Canonicalization uses the file-system seam and never exposes its error text. Like
/// every other connect lookup it holds one of the server's lookup permits
/// ([`crate::share::SHARE_LOOKUPS`]) for its whole blocking call and is waited for no later
/// than `deadline`.
///
/// # Errors
/// `503 unavailable` when no lookup permit comes free by `deadline` (the server's lookups are
/// already tied up, most likely by a hung share server); `share_unreachable` (`offline`) when
/// the lookup does not finish by `deadline`. A lookup that fails answers no overlap.
pub(super) async fn overlaps_data_folder(
    state: &AppState,
    path: &SharePath,
    deadline: tokio::time::Instant,
) -> Result<bool, ApiError> {
    let data_text = state.config().data_dir.to_string_lossy();
    if let Ok(data) = SharePath::parse(&data_text)
        && (path.is_within(&data) || data.is_within(path))
    {
        return Ok(true);
    }
    let permit = tokio::time::timeout_at(deadline, state.shares().lookups().acquire_owned())
        .await
        .map_err(|_| ApiError::Unavailable)?
        .map_err(|_| ApiError::Unavailable)?;
    let fs = state.shares().fs();
    let share = path.to_path_buf();
    let data = state.config().data_dir.clone();
    let overlap = tokio::time::timeout_at(
        deadline,
        tokio::task::spawn_blocking(move || {
            let overlap = (|| {
                let share = fs.canonical(&share, &share).ok()?;
                let data = fs.canonical(&data, &data).ok()?;
                // Windows canonical paths carry a `\\?\` prefix, which only this conversion
                // removes.
                let share = canonical_share_path(&share).ok()?;
                let data = canonical_share_path(&data).ok()?;
                Some(share.is_within(&data) || data.is_within(&share))
            })();
            drop(permit);
            overlap
        }),
    )
    .await
    .map_err(|_| ApiError::ShareUnreachable("offline"))?;
    Ok(overlap.ok().flatten().unwrap_or(false))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ShareSettingsChange {
    roots: Option<Vec<String>>,
    cache_max_bytes: Option<Value>,
    list_seconds: Option<Value>,
    max_files: Option<Value>,
}

async fn shares_out(state: &AppState) -> Result<Value, ApiError> {
    let settings = state
        .db()
        .share_settings()
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
    let cache = state
        .db()
        .cached_bytes()
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
    let runs_as = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .ok();
    // Never the password: only who the stored login is, and when it was set.
    let logins: Vec<Value> = state
        .db()
        .share_logins()
        .await
        .map_err(|e| ApiError::unavailable(&e))?
        .into_iter()
        .map(|login| {
            json!({
                "server": login.server,
                "username": login.username,
                "set_at": super::nodes::rfc3339(Some(login.set_at)),
            })
        })
        .collect();
    Ok(json!({
        "logins": logins,
        "roots": settings.roots.iter().map(SharePath::display).collect::<Vec<_>>(),
        "cache_max_bytes": settings.cache_max_bytes,
        "list_seconds": settings.list_seconds,
        "max_files": settings.max_files,
        "cache": { "bytes": cache },
        "runs_as": runs_as,
    }))
}

/// `GET /api/settings/shares`: approved share roots and listing/cache limits, for administrators.
pub(crate) async fn shares(
    State(state): State<AppState>,
    signed: Signed,
) -> Result<Json<Value>, ApiError> {
    if !signed.session.user.is_admin {
        return Err(ApiError::Forbidden);
    }
    Ok(Json(shares_out(&state).await?))
}

/// `PUT /api/settings/shares`: changes the supplied file-share settings, for administrators.
pub(crate) async fn change_shares(
    State(state): State<AppState>,
    signed: Signed,
    JsonBody(change): JsonBody<ShareSettingsChange>,
) -> Result<Json<Value>, ApiError> {
    if !signed.session.user.is_admin {
        return Err(ApiError::Forbidden);
    }
    let roots = match change.roots {
        None => None,
        Some(values) => {
            // Every root's data-folder lookup together waits no longer than one listing may.
            let list_seconds = state
                .db()
                .share_settings()
                .await
                .map_err(|e| ApiError::unavailable(&e))?
                .list_seconds;
            let deadline = tokio::time::Instant::now() + Duration::from_secs(list_seconds);
            let mut parsed = Vec::with_capacity(values.len());
            for value in &values {
                let path = SharePath::parse(value)
                    .map_err(|reason| ApiError::InvalidSharePath(path_reason(reason)))?;
                valid_root(&path)
                    .and_then(|()| usable_here(&path))
                    .map_err(|reason| ApiError::InvalidSharePath(path_reason(reason)))?;
                if overlaps_data_folder(&state, &path, deadline).await? {
                    return Err(ApiError::ShareNotApproved);
                }
                parsed.push(path.display());
            }
            Some(json!(parsed))
        }
    };
    let changes = [
        (SHARE_ROOTS, roots),
        (SHARE_CACHE_MAX_BYTES, change.cache_max_bytes),
        (SHARE_LIST_SECONDS, change.list_seconds),
        (SOURCE_MAX_FILES, change.max_files),
    ];
    if changes.iter().all(|(_, value)| value.is_none()) {
        return Err(ApiError::InvalidRequest);
    }
    // Validate every value before writing the first one, so a bad later field changes nothing.
    for (key, value) in changes
        .iter()
        .filter_map(|(key, value)| value.as_ref().map(|v| (*key, v)))
    {
        let valid = match key {
            SHARE_ROOTS => true,
            SHARE_CACHE_MAX_BYTES => value.as_u64().is_some_and(|n| n >= 256 * 1024 * 1024),
            SHARE_LIST_SECONDS => value.as_u64().is_some_and(|n| (30..=3_600).contains(&n)),
            SOURCE_MAX_FILES => value.as_i64().is_some_and(|n| n >= 1),
            _ => false,
        };
        if !valid {
            return Err(ApiError::InvalidRequest);
        }
    }
    let changes: Vec<(&str, Value)> = changes
        .into_iter()
        .filter_map(|(key, value)| value.map(|v| (key, v)))
        .collect();
    let roots_changed = changes.iter().any(|(key, _)| *key == SHARE_ROOTS);
    let mut roots_turn = if roots_changed {
        Some(
            state
                .db()
                .lock_share_connections(SHARE_TURN_WAIT)
                .await
                .map_err(|e| ApiError::unavailable(&e))?
                .ok_or(ApiError::Unavailable)?,
        )
    } else {
        None
    };
    // A server left with no approved root loses its stored login in the same transaction.
    let orphaned = state
        .db()
        .set_settings_pruning_logins(signed.session.user.id, changes)
        .await?;
    drop(roots_turn.take());
    for secret in orphaned {
        state.secret_store().forget(state.db(), secret).await;
    }
    if roots_changed {
        // A share stopped because its folder was not approved is not retried on its own; the
        // roots changed, so each is made due and woken. Nothing about it fails this request.
        match state.db().requeue_stopped_shares().await {
            Ok(libraries) => {
                for library in libraries {
                    state.sources().forget_failures(library);
                    state.sources().wake(library);
                }
            }
            Err(e) => eprintln!("sharecraft-server: cannot wake stopped shares: {e}"),
        }
    }
    Ok(Json(shares_out(&state).await?))
}

/// The body of `PUT /api/settings/shares/logins/{server}`. Not `Debug`: it holds a password.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ShareLoginChange {
    username: Option<String>,
    password: Option<String>,
}

/// Whether `text` is a lower-case host name or address: dot-separated labels of one to 63
/// letters, digits, hyphens and underscores, none starting or ending with a hyphen.
fn valid_server(text: &str) -> bool {
    text.len() <= 255
        && text.split('.').all(|label| {
            (1..=63).contains(&label.len())
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
                && !label.starts_with('-')
                && !label.ends_with('-')
        })
}

/// User names are 1 to 256 characters with no control characters and no `/`.
fn valid_login_username(text: &str) -> bool {
    (1..=256).contains(&text.chars().count()) && !text.chars().any(|c| c.is_control() || c == '/')
}

/// Passwords are 1 to 256 bytes with no NUL.
fn valid_login_password(text: &str) -> bool {
    (1..=256).contains(&text.len()) && !text.contains('\0')
}

/// What the stored login of `server` must pass before it is stored. Task 4 signs in here.
#[allow(clippy::unused_async)]
async fn before_storing_login(
    _state: &AppState,
    _server: &str,
    _username: &str,
    _password: &str,
) -> Result<(), ApiError> {
    Ok(())
}

/// The answer for a secret that could not be stored.
fn secret_failure(error: &crate::secrets::SecretError) -> ApiError {
    // The error text is a plain sentence that never carries a value.
    eprintln!("sharecraft-server: a share login could not be stored: {error}");
    match error {
        crate::secrets::SecretError::Unavailable(_) => ApiError::SecretStoreUnavailable,
        _ => ApiError::Unavailable,
    }
}

/// `PUT /api/settings/shares/logins/{server}`: stores the one login used for every approved
/// folder on `server`, for administrators.
pub(crate) async fn set_share_login(
    State(state): State<AppState>,
    signed: Signed,
    axum::extract::Path(server): axum::extract::Path<String>,
    JsonBody(change): JsonBody<ShareLoginChange>,
) -> Result<Json<Value>, ApiError> {
    if !signed.session.user.is_admin {
        return Err(ApiError::Forbidden);
    }
    let (Some(username), Some(password)) = (change.username, change.password) else {
        return Err(ApiError::InvalidRequest);
    };
    if !valid_server(&server)
        || !valid_login_username(&username)
        || !valid_login_password(&password)
    {
        return Err(ApiError::InvalidRequest);
    }
    // Under the share-connection turn, so a change of roots cannot slip in between the check
    // that the server has a root and the storing of its login.
    let turn = state
        .db()
        .lock_share_connections(SHARE_TURN_WAIT)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
        .ok_or(ApiError::Unavailable)?;
    let has_root = state
        .db()
        .share_settings()
        .await
        .map_err(|e| ApiError::unavailable(&e))?
        .roots
        .iter()
        .any(|root| root.server().as_deref() == Some(server.as_str()));
    if !has_root {
        return Err(ApiError::NoRootOnServer);
    }
    if !cfg!(windows) {
        return Err(ApiError::LoginsWindowsOnly);
    }
    before_storing_login(&state, &server, &username, &password).await?;
    let context = format!("share-login:{server}");
    let secret = state
        .secret_store()
        .put(state.db(), password.as_bytes(), context.as_bytes())
        .await
        .map_err(|e| secret_failure(&e))?;
    let replaced = match state
        .db()
        .set_share_login(signed.session.user.id, &server, &username, secret)
        .await
    {
        Ok(replaced) => replaced,
        Err(e) => {
            state.secret_store().forget(state.db(), secret).await;
            return Err(ApiError::unavailable(&e));
        }
    };
    drop(turn);
    if let Some(old) = replaced {
        state.secret_store().forget(state.db(), old).await;
    }
    Ok(Json(shares_out(&state).await?))
}

/// `DELETE /api/settings/shares/logins/{server}`: removes the stored login of `server`, for
/// administrators. A server without one is not an error.
pub(crate) async fn remove_share_login(
    State(state): State<AppState>,
    signed: Signed,
    axum::extract::Path(server): axum::extract::Path<String>,
) -> Result<Json<Value>, ApiError> {
    if !signed.session.user.is_admin {
        return Err(ApiError::Forbidden);
    }
    if !valid_server(&server) {
        return Err(ApiError::InvalidRequest);
    }
    let turn = state
        .db()
        .lock_share_connections(SHARE_TURN_WAIT)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
        .ok_or(ApiError::Unavailable)?;
    let removed = state
        .db()
        .remove_share_login_as(signed.session.user.id, &server)
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
    drop(turn);
    if let Some(secret) = removed {
        state.secret_store().forget(state.db(), secret).await;
    }
    Ok(Json(shares_out(&state).await?))
}

/// The most approved S3 endpoints, as the records layer allows.
const MAX_S3_ENDPOINTS: usize = 20;

/// The word an answer gives for an endpoint refusal.
fn refusal_word(refusal: EndpointRefusal) -> &'static str {
    match refusal {
        EndpointRefusal::Empty => "empty",
        EndpointRefusal::NotHttps => "not_https",
        EndpointRefusal::HasPath => "has_path",
        EndpointRefusal::HasQuery => "has_query",
        EndpointRefusal::HasCredentials => "has_credentials",
        EndpointRefusal::BadHost => "bad_host",
        EndpointRefusal::AddressLiteral => "address_literal",
        EndpointRefusal::BadPort => "bad_port",
        EndpointRefusal::BadRegion => "bad_region",
        EndpointRefusal::BadName => "bad_name",
        EndpointRefusal::NameTaken => "name_taken",
        EndpointRefusal::TooMany => "too_many",
        EndpointRefusal::BadRange => "bad_range",
        EndpointRefusal::Reserved => "reserved",
    }
}

/// Deserialises a field that may be `null`, telling `null` (`Some(Value::Null)`) from absent
/// (`None`, with `#[serde(default)]`).
fn present<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

/// The body of `PUT /api/settings/s3`: the settings to change, at least one.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct S3SettingsChange {
    /// `[{name, url, region, path_style, allow}]`: the whole list, replacing the old one.
    endpoints: Option<Value>,
    /// At least 1,000.
    list_requests: Option<Value>,
    /// At least 1,000.
    read_requests: Option<Value>,
    /// At least 1 GiB, or `null` for no limit.
    #[serde(default, deserialize_with = "present")]
    bytes_down: Option<Value>,
}

/// One approved endpoint as it is stored, checked as the records layer checks it (HTTPS only,
/// no path, query or credentials, no address literal outside the entry's own ranges). A
/// default `:443` is taken off the address, so the stored address is the one signed and sent.
fn stored_endpoint(index: usize, value: &Value) -> Result<(String, Value), ApiError> {
    let bad = |reason| ApiError::InvalidS3Endpoint(Some(index), reason);
    let object = value.as_object().ok_or(bad("bad_name"))?;
    if object.keys().any(|key| {
        !matches!(
            key.as_str(),
            "name" | "url" | "region" | "path_style" | "allow"
        )
    }) {
        return Err(bad("unknown_field"));
    }
    let text = |field: &str, refusal: &'static str| {
        object
            .get(field)
            .and_then(Value::as_str)
            .ok_or(bad(refusal))
    };
    let name = text("name", "bad_name")?;
    let url = text("url", "empty")?;
    let region = text("region", "bad_region")?;
    let path_style = match object.get("path_style") {
        None => false,
        Some(value) => value.as_bool().ok_or(bad("bad_path_style"))?,
    };
    let allow = match object.get("allow") {
        None => Vec::new(),
        Some(value) => value
            .as_array()
            .ok_or(bad("bad_range"))?
            .iter()
            .map(|range| range.as_str().map(str::to_owned).ok_or(bad("bad_range")))
            .collect::<Result<Vec<_>, _>>()?,
    };
    let endpoint = S3Endpoint::parse(name, url, region, path_style, &allow, false)
        .map_err(|refusal| bad(refusal_word(refusal)))?;
    let url = if endpoint.https() && endpoint.port() == Some(443) {
        endpoint
            .url
            .strip_suffix(":443")
            .unwrap_or(&endpoint.url)
            .to_owned()
    } else {
        endpoint.url.clone()
    };
    Ok((
        endpoint.name.clone(),
        json!({
            "name": endpoint.name,
            "url": url,
            "region": endpoint.region,
            "path_style": endpoint.path_style,
            "allow": endpoint.allow.iter().map(IpRange::display).collect::<Vec<_>>(),
        }),
    ))
}

/// The S3 settings as answered: the approved endpoints, the default monthly budgets and how
/// many libraries use each endpoint (by name).
async fn s3_out(state: &AppState) -> Result<Value, ApiError> {
    let settings = state
        .db()
        .s3_settings()
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
    let in_use = state
        .db()
        .s3_endpoints_in_use()
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
    Ok(json!({
        "endpoints": settings
            .endpoints
            .iter()
            .map(|endpoint| json!({
                "name": endpoint.name,
                "url": endpoint.url,
                "region": endpoint.region,
                "path_style": endpoint.path_style,
                "allow": endpoint.allow.iter().map(IpRange::display).collect::<Vec<_>>(),
            }))
            .collect::<Vec<_>>(),
        "list_requests": settings.list_requests,
        "read_requests": settings.read_requests,
        "bytes_down": settings.bytes_down,
        "in_use": in_use
            .into_iter()
            .map(|(name, libraries)| (name, json!(libraries)))
            .collect::<serde_json::Map<String, Value>>(),
    }))
}

/// `GET /api/settings/s3`: the approved S3 endpoints, the default monthly budgets and
/// `in_use` (`{name: libraries}`), for administrators (`403` otherwise).
pub(crate) async fn s3(
    State(state): State<AppState>,
    signed: Signed,
) -> Result<Json<Value>, ApiError> {
    if !signed.session.user.is_admin {
        return Err(ApiError::Forbidden);
    }
    Ok(Json(s3_out(&state).await?))
}

/// `GET /api/s3/endpoints`: the names of the approved S3 endpoints as `[{name}]`, and nothing
/// else (no address, region, addressing style or ranges), for anyone signed in (`401` otherwise).
pub(crate) async fn s3_names(
    State(state): State<AppState>,
    _signed: Signed,
) -> Result<Json<Value>, ApiError> {
    let settings = state
        .db()
        .s3_settings()
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
    Ok(Json(Value::Array(
        settings
            .endpoints
            .iter()
            .map(|endpoint| json!({ "name": endpoint.name }))
            .collect(),
    )))
}

/// `PUT /api/settings/s3 {endpoints?, list_requests?, read_requests?, bytes_down?}`: changes
/// the settings given, for administrators (`403` otherwise). Everything is checked before
/// anything is written, then all of it is written in one transaction or none of it; each
/// changed key is recorded as `setting.changed`, the endpoints without their addresses. `422
/// invalid_s3_endpoint {entry, reason}` names the first bad endpoint; `422 invalid_request`
/// for a budget it does not take, or nothing given. Removing an endpoint disconnects no
/// library: theirs stop reading and listing at once. Answers the settings as they then are.
pub(crate) async fn change_s3(
    State(state): State<AppState>,
    signed: Signed,
    JsonBody(change): JsonBody<S3SettingsChange>,
) -> Result<Json<Value>, ApiError> {
    if !signed.session.user.is_admin {
        return Err(ApiError::Forbidden);
    }
    let endpoints = match change.endpoints {
        None => None,
        Some(value) => {
            let entries = value
                .as_array()
                .ok_or(ApiError::InvalidS3Endpoint(None, "bad_name"))?;
            if entries.len() > MAX_S3_ENDPOINTS {
                return Err(ApiError::InvalidS3Endpoint(None, "too_many"));
            }
            let mut names: Vec<String> = Vec::with_capacity(entries.len());
            let mut stored = Vec::with_capacity(entries.len());
            for (index, entry) in entries.iter().enumerate() {
                let (name, value) = stored_endpoint(index, entry)?;
                if names.contains(&name) {
                    return Err(ApiError::InvalidS3Endpoint(Some(index), "name_taken"));
                }
                names.push(name);
                stored.push(value);
            }
            Some(Value::Array(stored))
        }
    };
    let changes = [
        (S3_ENDPOINTS, endpoints),
        (S3_LIST_REQUESTS, change.list_requests),
        (S3_READ_REQUESTS, change.read_requests),
        (S3_BYTES_DOWN, change.bytes_down),
    ];
    if changes.iter().all(|(_, value)| value.is_none()) {
        return Err(ApiError::InvalidRequest);
    }
    // Every budget is checked here as the records layer checks it, before anything is written.
    for (key, value) in changes
        .iter()
        .filter_map(|(key, value)| value.as_ref().map(|v| (*key, v)))
    {
        let valid = match key {
            S3_ENDPOINTS => true,
            S3_LIST_REQUESTS | S3_READ_REQUESTS => value
                .as_u64()
                .is_some_and(|n| n >= 1_000 && i64::try_from(n).is_ok()),
            S3_BYTES_DOWN => {
                value.is_null()
                    || value
                        .as_u64()
                        .is_some_and(|n| n >= 1024 * 1024 * 1024 && i64::try_from(n).is_ok())
            }
            _ => false,
        };
        if !valid {
            return Err(ApiError::InvalidRequest);
        }
    }
    let changes: Vec<(&str, Value)> = changes
        .into_iter()
        .filter_map(|(key, value)| value.map(|v| (key, v)))
        .collect();
    state
        .db()
        .set_settings(signed.session.user.id, changes)
        .await?;
    Ok(Json(s3_out(&state).await?))
}

/// The longest a request waits for the organisation-wide share-connection turn before it is
/// refused with a retryable `503 unavailable`. Holders never do file-system I/O under the turn.
pub(crate) const SHARE_TURN_WAIT: Duration = Duration::from_secs(10);

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
