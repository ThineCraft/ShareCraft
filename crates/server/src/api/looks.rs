//! A site's look over HTTP: the effective look (anyone who can see the site), its settings,
//! history and rollback, changing it, and the site's own themes (people who manage the site).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use collab_core::ids::NodeId;
use serde::Deserialize;
use serde_json::Value;

use super::brand::{NewTheme, ThemeEdit, completed};
use super::{JsonBody, parse_id};
use crate::AppState;
use crate::auth::Signed;
use crate::brand::{BrandError, ThemeInput};
use crate::looks::{self, LookInput, Which};

/// The largest body of `PUT /api/sites/{id}/look`: a footer of four groups of eight links, each
/// address up to 2,000 characters, with room to spare.
pub(crate) const MAX_LOOK_BODY: usize = 256 * 1024;

/// A site id from a path, or `422 invalid_request`.
fn site_id(text: &str) -> Result<NodeId, BrandError> {
    Ok(NodeId(parse_id(text)?))
}

/// `GET /api/sites/{id}/look`: the site's effective look, with the hub rule applied (see
/// [`looks::effective`]). `404` unless the caller can see the site.
pub(crate) async fn show(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
) -> Result<Json<Value>, BrandError> {
    let site = site_id(&id)?;
    Ok(Json(
        looks::effective(&state, &signed.session.user, site).await?,
    ))
}

/// `GET /api/sites/{id}/look/settings`: `{look, themes, theme_source_changed, in_hub?}` for
/// people who manage the site (see [`looks::settings`]).
pub(crate) async fn settings(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
) -> Result<Json<Value>, BrandError> {
    let site = site_id(&id)?;
    Ok(Json(
        looks::settings(&state, &signed.session.user, site).await?,
    ))
}

/// The body of `PUT /api/sites/{id}/look`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LookBody {
    look: LookInput,
}

/// `PUT /api/sites/{id}/look {look}`: sets the site's look (see [`looks::set_look`]); answers
/// the look settings. `manage_site` (`403 archived` on an archived site); `422 invalid_theme`,
/// `low_contrast` or `invalid_look`; the image errors of the brand centre.
pub(crate) async fn set(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<LookBody>,
) -> Result<Json<Value>, BrandError> {
    let site = site_id(&id)?;
    let actor = signed.session.user;
    let answer =
        completed(async move { looks::set_look(&state, &actor, site, body.look).await }).await?;
    Ok(Json(answer))
}

/// `GET /api/sites/{id}/look/history`: the last 50 changes of the look, newest first (see
/// [`looks::history`]).
pub(crate) async fn history(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
) -> Result<Json<Value>, BrandError> {
    let site = site_id(&id)?;
    Ok(Json(
        looks::history(&state, &signed.session.user, site).await?,
    ))
}

/// The body of `POST /api/sites/{id}/look/rollback`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RollbackBody {
    event_id: i64,
    #[serde(default)]
    which: Which,
}

/// `POST /api/sites/{id}/look/rollback {event_id, which?}`: applies the look after (or with
/// `"which": "before"`, before) that change again, checked again (see [`looks::rollback`]);
/// answers the look settings. `404` for no such entry; `409 image_gone {slots}`.
pub(crate) async fn rollback(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<RollbackBody>,
) -> Result<Json<Value>, BrandError> {
    let site = site_id(&id)?;
    let actor = signed.session.user;
    let answer =
        completed(
            async move { looks::rollback(&state, &actor, site, body.event_id, body.which).await },
        )
        .await?;
    Ok(Json(answer))
}

/// `POST /api/sites/{id}/themes {name, primary, secondary, primary_dark?, secondary_dark?}`:
/// adds a theme only this site offers (see [`looks::create_theme`]); `201` with the theme as
/// listed.
pub(crate) async fn create_theme(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<NewTheme>,
) -> Result<Response, BrandError> {
    let site = site_id(&id)?;
    let actor = signed.session.user;
    let input = ThemeInput::from(body);
    let theme =
        completed(async move { looks::create_theme(&state, &actor, site, input).await }).await?;
    Ok((StatusCode::CREATED, Json(theme)).into_response())
}

/// `PATCH /api/sites/{id}/themes/{theme}`: changes one of the site's own themes; each field
/// given is set, a dark colour set to `null` derived again (see [`looks::change_theme`]).
pub(crate) async fn change_theme(
    State(state): State<AppState>,
    signed: Signed,
    Path((id, theme)): Path<(String, String)>,
    JsonBody(body): JsonBody<ThemeEdit>,
) -> Result<Json<Value>, BrandError> {
    let site = site_id(&id)?;
    let theme = parse_id(&theme)?;
    let actor = signed.session.user;
    let answer = completed(async move {
        looks::change_theme(&state, &actor, site, theme, |current| {
            body.applied_to(current)
        })
        .await
    })
    .await?;
    Ok(Json(answer))
}

/// `DELETE /api/sites/{id}/themes/{theme}`: deletes one of the site's own themes; `204`. A look
/// that copied it keeps its copy.
pub(crate) async fn delete_theme(
    State(state): State<AppState>,
    signed: Signed,
    Path((id, theme)): Path<(String, String)>,
) -> Result<StatusCode, BrandError> {
    let site = site_id(&id)?;
    let theme = parse_id(&theme)?;
    let actor = signed.session.user;
    completed(async move { looks::delete_theme(&state, &actor, site, theme).await }).await?;
    Ok(StatusCode::NO_CONTENT)
}
