//! The brand centre over HTTP: reading it (anyone signed in), its themes and images (brand
//! managers and administrators), naming the brand managers (administrators), and serving a
//! brand image to anyone signed in.

use std::io::Read;
use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::header::{
    CACHE_CONTROL, CONTENT_LENGTH, CONTENT_SECURITY_POLICY, CONTENT_TYPE, X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use collab_core::ids::{DocId, UserId};
use revisions::brand::{BUILT_IN_THEMES, BrandImage, ImageKind, OrgTheme, Theme, check_theme};
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use uuid::Uuid;

use super::{ApiError, JsonBody, parse_id};
use crate::AppState;
use crate::auth::{MaybeSigned, Signed};
use crate::brand::{self, BrandError, ThemeInput};

/// The policy a brand image is served with: nothing in it runs or loads, even opened alone.
const IMAGE_POLICY: &str = "sandbox; default-src 'none'; style-src 'unsafe-inline'";

impl IntoResponse for BrandError {
    fn into_response(self) -> Response {
        match self {
            BrandError::Api(e) => e.into_response(),
            BrandError::LowContrast(pairs) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({ "error": "low_contrast", "pairs": pairs })),
            )
                .into_response(),
            BrandError::InvalidTheme(reason) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({ "error": "invalid_theme", "reason": reason })),
            )
                .into_response(),
            BrandError::InvalidLook(reason) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({ "error": "invalid_look", "reason": reason })),
            )
                .into_response(),
            BrandError::ImagesGone(slots) => (
                StatusCode::CONFLICT,
                Json(json!({ "error": "image_gone", "slots": slots })),
            )
                .into_response(),
        }
    }
}

/// Runs `change` on a task of its own and waits for it, so a change whose caller goes away
/// still finishes (see [`super::to_completion`]).
pub(crate) async fn completed<T: Send + 'static>(
    change: impl Future<Output = Result<T, BrandError>> + Send + 'static,
) -> Result<T, BrandError> {
    match tokio::spawn(change).await {
        Ok(result) => result,
        Err(e) => {
            eprintln!("sharecraft-server: a change did not finish: {e}");
            Err(ApiError::Unavailable.into())
        }
    }
}

/// The address a brand image is served at.
pub(crate) fn image_url(sha256: &str) -> String {
    format!("/api/brand/images/{sha256}")
}

/// A theme as the brand centre lists it: its colours as given (`null` for a derived dark
/// colour) and as a page uses them (`resolved`).
pub(crate) fn theme_out(id: &str, from: &str, theme: &Theme, changed_at: Option<i64>) -> Value {
    let resolved = check_theme(theme).ok();
    json!({
        "id": id,
        "from": from,
        "name": theme.name,
        "primary": theme.primary,
        "secondary": theme.secondary,
        "primary_dark": theme.primary_dark,
        "secondary_dark": theme.secondary_dark,
        "resolved": resolved,
        "builtin": changed_at.is_none(),
        "changed_at": changed_at,
    })
}

/// An organisation theme as listed.
fn org_theme_out(theme: &OrgTheme) -> Value {
    let id = theme.id.to_string();
    theme_out(
        &id,
        &format!("org:{id}"),
        &theme.theme(),
        Some(theme.changed_at),
    )
}

/// A brand image as listed: `{id, kind, name, url}`.
fn image_out(image: &BrandImage) -> Value {
    json!({
        "id": image.id,
        "kind": image.kind,
        "name": image.image.name,
        "url": image_url(&image.image.sha256),
    })
}

/// `GET /api/brand`: the brand centre for anyone signed in, `{themes, images, can_manage}`:
/// the built-in themes first, then the organisation's; each image `{id, kind, name, url}`;
/// whether the caller may change themes and images. Administrators also get `managers`, each
/// `{id, name, email}`.
pub(crate) async fn show(
    State(state): State<AppState>,
    signed: Signed,
) -> Result<Json<Value>, ApiError> {
    let db = state.db();
    let user = &signed.session.user;
    let unavailable = |e: records::DbError| ApiError::unavailable(&e);
    let centre = db.org_brand().await.map_err(unavailable)?;
    let can_manage = db.can_manage_brand(user.id).await.map_err(unavailable)?;
    let mut themes: Vec<Value> = BUILT_IN_THEMES
        .iter()
        .map(|t| theme_out(t.id, t.id, &t.theme(), None))
        .collect();
    themes.extend(centre.themes.iter().map(org_theme_out));
    let mut out = json!({
        "themes": themes,
        "images": centre.images.iter().map(image_out).collect::<Vec<_>>(),
        "can_manage": can_manage,
    });
    if user.is_admin {
        let mut managers = Vec::new();
        for id in &centre.managers {
            if let Some(found) = db.user(UserId(*id)).await.map_err(unavailable)? {
                managers.push(json!({
                    "id": found.id.0,
                    "name": found.name,
                    "email": found.email,
                }));
            }
        }
        if let Some(object) = out.as_object_mut() {
            object.insert("managers".into(), Value::Array(managers));
        }
    }
    Ok(Json(out))
}

/// The body of `POST /api/brand/themes`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewTheme {
    name: String,
    primary: String,
    secondary: String,
    #[serde(default)]
    primary_dark: Option<String>,
    #[serde(default)]
    secondary_dark: Option<String>,
}

impl From<NewTheme> for ThemeInput {
    fn from(body: NewTheme) -> ThemeInput {
        ThemeInput {
            name: body.name,
            primary: body.primary,
            secondary: body.secondary,
            primary_dark: body.primary_dark,
            secondary_dark: body.secondary_dark,
        }
    }
}

/// Reads a field that may be given as `null`: absent is `None`, `null` is `Some(None)`.
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(deserializer).map(Some)
}

/// The body of `PATCH /api/brand/themes/{id}`: each field given is set; a dark colour set to
/// `null` is derived again.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ThemeEdit {
    name: Option<String>,
    primary: Option<String>,
    secondary: Option<String>,
    #[serde(default, deserialize_with = "present")]
    primary_dark: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    secondary_dark: Option<Option<String>>,
}

impl ThemeEdit {
    /// The theme `current` with this edit's fields set, as typed.
    pub(crate) fn applied_to(self, current: &Theme) -> ThemeInput {
        let colour = |c: revisions::brand::Colour| c.to_string();
        ThemeInput {
            name: self.name.unwrap_or_else(|| current.name.clone()),
            primary: self.primary.unwrap_or_else(|| colour(current.primary)),
            secondary: self.secondary.unwrap_or_else(|| colour(current.secondary)),
            primary_dark: self
                .primary_dark
                .unwrap_or_else(|| current.primary_dark.map(colour)),
            secondary_dark: self
                .secondary_dark
                .unwrap_or_else(|| current.secondary_dark.map(colour)),
        }
    }
}

/// `POST /api/brand/themes {name, primary, secondary, primary_dark?, secondary_dark?}`: adds a
/// theme (see [`brand::create_theme`]); `201` with the theme as listed. `403` unless a brand
/// manager or administrator; `422 invalid_theme {reason}`; `422 low_contrast {pairs}`; `409
/// name_taken`; `409 too_many {limit}`.
pub(crate) async fn create_theme(
    State(state): State<AppState>,
    signed: Signed,
    JsonBody(body): JsonBody<NewTheme>,
) -> Result<Response, BrandError> {
    let input = ThemeInput::from(body);
    let actor = signed.session.user;
    let theme = completed(async move { brand::create_theme(&state, &actor, &input).await }).await?;
    Ok((StatusCode::CREATED, Json(org_theme_out(&theme))).into_response())
}

/// `PATCH /api/brand/themes/{id}`: changes a theme of the brand centre (see
/// [`brand::change_theme`]); the theme as listed. `404` for no such theme; otherwise as
/// [`create_theme`].
pub(crate) async fn change_theme(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<ThemeEdit>,
) -> Result<Json<Value>, BrandError> {
    let id = parse_id(&id)?;
    let actor = signed.session.user;
    let current = state
        .db()
        .org_brand()
        .await?
        .themes
        .into_iter()
        .find(|t| t.id == id)
        .ok_or(ApiError::NotFound)?;
    let input = body.applied_to(&current.theme());
    let theme =
        completed(async move { brand::change_theme(&state, &actor, id, &input).await }).await?;
    Ok(Json(org_theme_out(&theme)))
}

/// `DELETE /api/brand/themes/{id}`: deletes a theme of the brand centre (see
/// [`brand::delete_theme`]); `204`. `403`; `404` for no such theme.
pub(crate) async fn delete_theme(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
) -> Result<StatusCode, BrandError> {
    let id = parse_id(&id)?;
    let actor = signed.session.user;
    completed(async move { brand::delete_theme(&state, &actor, id).await }).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The body of `POST /api/brand/images`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewImage {
    doc: String,
    kind: ImageKind,
    #[serde(default)]
    name: Option<String>,
}

/// `POST /api/brand/images {doc, kind, name?}`: adds the current version of a document in an
/// asset library the caller can view as a brand image of `kind` (`logo`, `icon` or
/// `background`; see [`brand::add_image`]); `201 {id, kind, name, url}`. `403` unless a brand
/// manager or administrator; `404` for a document the caller cannot view; `400 wrong_place`
/// outside an asset library; `422 not_an_image`; `413 image_too_large`; `423 held {scan}`;
/// `409 too_many {limit}`; `409 image_gone`.
pub(crate) async fn add_image(
    State(state): State<AppState>,
    signed: Signed,
    JsonBody(body): JsonBody<NewImage>,
) -> Result<Response, BrandError> {
    let doc = DocId(parse_id(&body.doc)?);
    let actor = signed.session.user;
    let image = completed(async move {
        brand::add_image(&state, &actor, doc, body.kind, body.name.as_deref()).await
    })
    .await?;
    Ok((StatusCode::CREATED, Json(image_out(&image))).into_response())
}

/// `DELETE /api/brand/images/{id}`: removes a brand image (see [`brand::remove_image`]);
/// `204`. `403`; `404` for no such image.
pub(crate) async fn remove_image(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
) -> Result<StatusCode, BrandError> {
    let id = parse_id(&id)?;
    let actor = signed.session.user;
    completed(async move { brand::remove_image(&state, &actor, id).await }).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The body of `PUT /api/brand/managers`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Managers {
    users: Vec<String>,
}

/// `PUT /api/brand/managers {users}`: names the brand managers (see [`brand::set_managers`]);
/// `204`. Administrators only (`403`); `422 invalid_request` for an id that is not one; `404
/// no_such_user` for an unknown or disabled account; `409 too_many {limit}`.
pub(crate) async fn set_managers(
    State(state): State<AppState>,
    signed: Signed,
    JsonBody(body): JsonBody<Managers>,
) -> Result<StatusCode, BrandError> {
    let users = body
        .users
        .iter()
        .map(|id| parse_id(id))
        .collect::<Result<Vec<Uuid>, _>>()?;
    let actor = signed.session.user;
    completed(async move { brand::set_managers(&state, &actor, &users).await }).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/brand/images/{sha256}`: a brand image's bytes, for anyone signed in, as the type
/// recorded when it was chosen, with `nosniff`, a policy under which nothing in it runs or
/// loads, no `Content-Disposition`, and cached privately for a year (the address names the
/// content). `404` for a signed-out visitor and for content that neither the brand centre nor
/// a site's look names; `423 held {scan}` once the content is not available (a rescan found it
/// infected, say).
pub(crate) async fn image(
    State(state): State<AppState>,
    MaybeSigned(signed): MaybeSigned,
    Path(sha256): Path<String>,
) -> Result<Response, ApiError> {
    if signed.is_none() {
        return Err(ApiError::NotFound);
    }
    let hash = match revisions::parse_hex(&sha256) {
        Some(hash) if revisions::hex(&hash) == sha256 => hash,
        _ => return Err(ApiError::NotFound),
    };
    let db = state.db();
    let media_type = db
        .brand_image_type(&hash)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
        .ok_or(ApiError::NotFound)?;
    if let Some(scan) = db
        .availability(&hash)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
    {
        return Err(ApiError::Held(scan));
    }
    let blobs = Arc::clone(state.blobs());
    let bytes = tokio::task::spawn_blocking(move || {
        let Some(mut file) = blobs.store().open_blob(&hash)? else {
            return Ok::<_, revisions::BlobError>(None);
        };
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(Some(bytes))
    })
    .await
    .map_err(|e| ApiError::unavailable(&e))?
    .map_err(|e| ApiError::unavailable(&e))?
    .ok_or_else(|| {
        eprintln!("sharecraft-server: brand image {sha256} is missing from the content store");
        ApiError::Unavailable
    })?;
    let header = |text: &str| HeaderValue::from_str(text).map_err(|e| ApiError::unavailable(&e));
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, header(&media_type)?);
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(
        CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(IMAGE_POLICY),
    );
    headers.insert(
        CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=31536000, immutable"),
    );
    headers.insert(CONTENT_LENGTH, header(&bytes.len().to_string())?);
    Ok((StatusCode::OK, headers, Body::from(bytes)).into_response())
}
