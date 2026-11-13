//! The organisation's sharing settings: whether anyone links may be made and used, and how
//! long one may last.

use axum::Json;
use axum::extract::State;
use records::{ANYONE_LINK_MAX_DAYS, ANYONE_LINKS, SharingSettings};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{ApiError, JsonBody};
use crate::AppState;
use crate::auth::Signed;

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
