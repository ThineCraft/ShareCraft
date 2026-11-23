//! The brand centre over HTTP: reading it (anyone signed in), its themes, images, fonts, font
//! packages and default icon (brand managers and administrators), naming the brand managers
//! and setting the limits on site owners (administrators), and serving a brand image or font
//! file to anyone signed in.

use std::io::Read;
use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::http::header::{
    CACHE_CONTROL, CONTENT_LENGTH, CONTENT_SECURITY_POLICY, CONTENT_TYPE, X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use collab_core::ids::{DocId, UserId};
use revisions::brand::{
    BUILT_IN_FONT_PACKAGES, BUILT_IN_THEMES, BrandFont, BrandImage, FontPackage, FontRef,
    FontRoles, ImageKind, OrgBrand, OrgTheme, Theme, check_theme, family_name,
};
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use uuid::Uuid;

use super::{ApiError, JsonBody, parse_id};
use crate::AppState;
use crate::auth::{MaybeSigned, Signed};
use crate::brand::{self, BrandError, FontChoice, PackageEdit, PolicyEdit, ThemeInput};

/// The policy a brand image is served with: nothing in it runs or loads, even opened alone.
const IMAGE_POLICY: &str = "sandbox; default-src 'none'; style-src 'unsafe-inline'";

/// The policy a font file is served with: nothing in it runs or loads, even opened alone.
const FONT_POLICY: &str = "sandbox; default-src 'none'";

impl BrandError {
    /// The status and body of the answer, for every refusal but [`BrandError::Api`], which
    /// answers as its [`ApiError`] does.
    fn status_and_body(&self) -> Option<(StatusCode, Value)> {
        Some(match self {
            BrandError::Api(_) => return None,
            BrandError::LowContrast(pairs) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                json!({ "error": "low_contrast", "pairs": pairs }),
            ),
            BrandError::InvalidTheme(reason) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                json!({ "error": "invalid_theme", "reason": reason }),
            ),
            BrandError::InvalidLook(reason) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                json!({ "error": "invalid_look", "reason": reason }),
            ),
            BrandError::ImagesGone(slots) => (
                StatusCode::CONFLICT,
                json!({ "error": "image_gone", "slots": slots }),
            ),
            BrandError::InvalidFontPackage(reason) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                json!({ "error": "invalid_font_package", "reason": reason }),
            ),
            BrandError::NotAllowed(problems) => (
                StatusCode::CONFLICT,
                json!({ "error": "not_allowed", "problems": problems }),
            ),
        })
    }

    /// The refusal as one result among many: `{"error": "<code>"}` with the details its own
    /// answer gives (`reason`, `pairs`, `slots` or `problems`).
    pub(crate) fn as_result(&self) -> Value {
        match self.status_and_body() {
            Some((_, body)) => body,
            None => match self {
                BrandError::Api(e) => json!({ "error": e.code() }),
                _ => json!({ "error": ApiError::Unavailable.code() }),
            },
        }
    }
}

impl IntoResponse for BrandError {
    fn into_response(self) -> Response {
        if let BrandError::Api(e) = self {
            return e.into_response();
        }
        match self.status_and_body() {
            Some((status, body)) => (status, Json(body)).into_response(),
            None => ApiError::Unavailable.into_response(),
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

/// The address of the organisation's default tab icon, if it has one.
pub(crate) fn default_icon_url(centre: &OrgBrand) -> Option<String> {
    let id = centre.default_icon?;
    centre
        .images
        .iter()
        .find(|i| i.id == id)
        .map(|i| image_url(&i.image.sha256))
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

/// The address a font file is served at.
pub(crate) fn font_url(sha256: &str) -> String {
    format!("/api/brand/fonts/{sha256}")
}

/// A font file as a package or the brand centre shows it: `{name, media_type, family, url}`,
/// the family generated from its hash (never from its name).
pub(crate) fn font_ref_out(font: &FontRef) -> Value {
    json!({
        "name": font.name,
        "media_type": font.media_type,
        "family": family_name(&font.sha256),
        "url": font_url(&font.sha256),
    })
}

/// A brand-centre font as listed: `{id, name, media_type, family, url, licence}`.
fn font_out(font: &BrandFont) -> Value {
    let mut out = font_ref_out(&font.font);
    if let Some(map) = out.as_object_mut() {
        map.insert("id".into(), json!(font.id));
        map.insert("licence".into(), json!(font.font.licence));
    }
    out
}

/// A font package as listed: `{id, from, name, builtin, display, content, roles}`, `from`
/// being what a look names it by (`org:<id>`, `site:<id>`; a built-in's id).
pub(crate) fn package_out(package: &FontPackage, from: &str, id: &str, builtin: bool) -> Value {
    json!({
        "id": id,
        "from": from,
        "name": package.name,
        "builtin": builtin,
        "display": package.display.as_ref().map(font_ref_out),
        "content": package.content.as_ref().map(font_ref_out),
        "roles": package.roles,
    })
}

/// An organisation font package as listed.
fn org_package_out(package: &FontPackage) -> Value {
    let id = package.id.to_string();
    package_out(package, &format!("org:{id}"), &id, false)
}

/// `GET /api/brand`: the brand centre for anyone signed in, `{themes, images, fonts,
/// font_packages, policy, default_icon, default_icon_url, can_manage}`: the built-in themes
/// and font packages first, then the organisation's; each image `{id, kind, name, url}`; each
/// font `{id, name, media_type, family, url, licence}`; the limits on site owners; the
/// organisation's default tab icon (`null` for none); whether the caller may change the centre.
/// Administrators also get `managers`, each `{id, name, email}`.
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
    let mut packages: Vec<Value> = BUILT_IN_FONT_PACKAGES
        .iter()
        .map(|p| package_out(&p.package(), p.id, p.id, true))
        .collect();
    packages.extend(centre.font_packages.iter().map(org_package_out));
    let mut out = json!({
        "themes": themes,
        "images": centre.images.iter().map(image_out).collect::<Vec<_>>(),
        "fonts": centre.fonts.iter().map(font_out).collect::<Vec<_>>(),
        "font_packages": packages,
        "policy": centre.policy,
        "default_icon": centre.default_icon,
        "default_icon_url": default_icon_url(&centre),
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

/// The body of `PATCH /api/brand/images/{id}`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ImageRename {
    name: String,
}

/// `PATCH /api/brand/images/{id} {name}`: renames a brand image (see [`brand::rename_image`]);
/// `{id, kind, name, url}`. `403`; `404` for no such image; `422 invalid_request` for a name
/// that is empty, too long or holds control or invisible characters.
pub(crate) async fn rename_image(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<ImageRename>,
) -> Result<Json<Value>, BrandError> {
    let id = parse_id(&id)?;
    let actor = signed.session.user;
    let image =
        completed(async move { brand::rename_image(&state, &actor, id, &body.name).await }).await?;
    Ok(Json(image_out(&image)))
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
///
/// With `?size=banner` a PNG, JPEG or WebP is answered as a site header's banner picture
/// instead: at most 2400 pixels wide and at most half as high, its middle kept, upright, made
/// the first time it is asked for and kept beside the content (see
/// [`crate::previews::ThumbSize::Banner`]). Any other image, or one no banner can be made of
/// (or none in time), is answered as it is. Any other `size` is `400 bad_request`.
pub(crate) async fn image(
    State(state): State<AppState>,
    MaybeSigned(signed): MaybeSigned,
    Path(sha256): Path<String>,
    query: Result<Query<ImageQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    let banner = match super::content::query_of(query)?.size.as_deref() {
        None => false,
        Some("banner") => true,
        Some(_) => return Err(ApiError::BadRequest),
    };
    serve(&state, signed.is_some(), &sha256, Served::Image { banner }).await
}

/// Query of `GET /api/brand/images/{sha256}`.
#[derive(Deserialize)]
pub(crate) struct ImageQuery {
    /// `banner` for the banner-sized picture.
    size: Option<String>,
}

/// `GET /api/brand/fonts/{sha256}`: a font file's bytes, for anyone signed in, as the type
/// recorded when it was chosen, with `nosniff`, `sandbox; default-src 'none'`, readable by
/// this origin only (`Cross-Origin-Resource-Policy: same-origin`) and cached privately for a
/// year. `404` for a signed-out visitor and for content that no brand font, font package or
/// look names; `423 held {scan}` once the content is not available.
pub(crate) async fn font(
    State(state): State<AppState>,
    MaybeSigned(signed): MaybeSigned,
    Path(sha256): Path<String>,
) -> Result<Response, ApiError> {
    serve(&state, signed.is_some(), &sha256, Served::Font).await
}

/// What a brand file route serves.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Served {
    /// An image; with `banner`, its banner-sized picture where one can be made.
    Image {
        banner: bool,
    },
    Font,
}

/// Serves the brand content `sha256` as `what` to a signed-in caller (see [`image`] and
/// [`font`]).
async fn serve(
    state: &AppState,
    signed_in: bool,
    sha256: &str,
    what: Served,
) -> Result<Response, ApiError> {
    if !signed_in {
        return Err(ApiError::NotFound);
    }
    let hash = match revisions::parse_hex(sha256) {
        Some(hash) if revisions::hex(&hash) == sha256 => hash,
        _ => return Err(ApiError::NotFound),
    };
    let db = state.db();
    let found = match what {
        Served::Image { .. } => db.brand_image_type(&hash).await,
        Served::Font => db.brand_font_type(&hash).await,
    };
    let media_type = found
        .map_err(|e| ApiError::unavailable(&e))?
        .ok_or(ApiError::NotFound)?;
    if let Some(scan) = db
        .availability(&hash)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
    {
        return Err(ApiError::Held(scan));
    }
    let made = match what {
        Served::Image { banner: true } => banner(state, &hash, &media_type).await,
        _ => None,
    };
    let (media_type, bytes) = match made {
        Some(made) => made,
        None => (media_type, original(state, hash, sha256).await?),
    };
    let header = |text: &str| HeaderValue::from_str(text).map_err(|e| ApiError::unavailable(&e));
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, header(&media_type)?);
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    let policy = match what {
        Served::Image { .. } => IMAGE_POLICY,
        Served::Font => FONT_POLICY,
    };
    headers.insert(CONTENT_SECURITY_POLICY, HeaderValue::from_static(policy));
    if matches!(what, Served::Font) {
        headers.insert(
            HeaderName::from_static("cross-origin-resource-policy"),
            HeaderValue::from_static("same-origin"),
        );
    }
    headers.insert(
        CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=31536000, immutable"),
    );
    headers.insert(CONTENT_LENGTH, header(&bytes.len().to_string())?);
    Ok((StatusCode::OK, headers, Body::from(bytes)).into_response())
}

/// The banner-sized picture of the brand image `hash` of type `media_type`, made if needed, as
/// `(type, bytes)`; `None` to answer the image as it is: not a PNG, JPEG or WebP, none can be
/// made of it, or no turn came free in time.
async fn banner(state: &AppState, hash: &[u8; 32], media_type: &str) -> Option<(String, Vec<u8>)> {
    if !matches!(media_type, "image/png" | "image/jpeg" | "image/webp") {
        return None;
    }
    let made = state
        .thumbnailer()
        .make(
            Arc::clone(state.blobs()),
            *hash,
            crate::previews::ThumbSize::Banner,
        )
        .await;
    let path = match made {
        Ok(path) => path,
        Err(e) => {
            if matches!(
                e,
                crate::previews::ThumbError::Io(_)
                    | crate::previews::ThumbError::Store(_)
                    | crate::previews::ThumbError::Task(_)
            ) {
                eprintln!(
                    "sharecraft-server: cannot make the banner of brand content {}: {e}",
                    revisions::hex(hash)
                );
            }
            return None;
        }
    };
    let kind = if path.extension().is_some_and(|e| e == "jpg") {
        "image/jpeg"
    } else {
        "image/png"
    };
    // Erased meanwhile with its content: the original answers (or is missing too).
    let bytes = tokio::fs::read(&path).await.ok()?;
    Some((kind.to_owned(), bytes))
}

/// The bytes of the brand content `hash` (named `sha256`) as kept.
async fn original(state: &AppState, hash: [u8; 32], sha256: &str) -> Result<Vec<u8>, ApiError> {
    let blobs = Arc::clone(state.blobs());
    tokio::task::spawn_blocking(move || {
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
        eprintln!("sharecraft-server: brand content {sha256} is missing from the content store");
        ApiError::Unavailable
    })
}

/// The body of `POST /api/brand/fonts`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewFont {
    doc: String,
    #[serde(default)]
    licence_confirmed: bool,
    #[serde(default)]
    name: Option<String>,
}

/// `POST /api/brand/fonts {doc, licence_confirmed, name?}`: adds the current version of a font
/// file in an asset library the caller can view to the brand centre, once they confirm the
/// organisation holds its licence (see [`brand::add_font`]); `201 {id, name, media_type,
/// family, url, licence}`. `403` unless a brand manager or administrator; `422
/// licence_not_confirmed`; `404`; `400 wrong_place`; `422 not_a_font`; `413 font_too_large`;
/// `423 held {scan}`; `409 too_many {limit}`.
pub(crate) async fn add_font(
    State(state): State<AppState>,
    signed: Signed,
    JsonBody(body): JsonBody<NewFont>,
) -> Result<Response, BrandError> {
    let doc = DocId(parse_id(&body.doc)?);
    let actor = signed.session.user;
    let font = completed(async move {
        brand::add_font(
            &state,
            &actor,
            doc,
            body.licence_confirmed,
            body.name.as_deref(),
        )
        .await
    })
    .await?;
    Ok((StatusCode::CREATED, Json(font_out(&font))).into_response())
}

/// `DELETE /api/brand/fonts/{id}`: removes a font from the brand centre (see
/// [`brand::remove_font`]); `204`. `403`; `404` for no such font.
pub(crate) async fn remove_font(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
) -> Result<StatusCode, BrandError> {
    let id = parse_id(&id)?;
    let actor = signed.session.user;
    completed(async move { brand::remove_font(&state, &actor, id).await }).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// A font of a package as sent: `{brand_font: id}`, or `{doc: id, licence_confirmed}` for a
/// site's own package.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FontChoiceBody {
    #[serde(default)]
    brand_font: Option<String>,
    #[serde(default)]
    doc: Option<String>,
    #[serde(default)]
    licence_confirmed: Option<bool>,
}

impl FontChoiceBody {
    /// The choice this body makes.
    ///
    /// # Errors
    /// `422 invalid_font_package` for neither or both ways; `422 invalid_request` for an id
    /// that is not one.
    fn choice(self) -> Result<FontChoice, BrandError> {
        match (self.brand_font, self.doc) {
            (Some(id), None) if self.licence_confirmed.is_none() => {
                Ok(FontChoice::BrandFont(parse_id(&id)?))
            }
            (None, Some(doc)) => Ok(FontChoice::Doc {
                doc: DocId(parse_id(&doc)?),
                licence_confirmed: self.licence_confirmed.unwrap_or(false),
            }),
            _ => Err(BrandError::InvalidFontPackage(
                "a font is given as {brand_font} or {doc, licence_confirmed}".into(),
            )),
        }
    }
}

/// The body of `POST /api/brand/font-packages` and `POST /api/sites/{id}/font-packages`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewPackage {
    name: String,
    #[serde(default)]
    display: Option<FontChoiceBody>,
    #[serde(default)]
    content: Option<FontChoiceBody>,
    roles: FontRoles,
}

impl NewPackage {
    /// The new package as a change of a blank one.
    ///
    /// # Errors
    /// As [`FontChoiceBody::choice`].
    pub(crate) fn edit(self) -> Result<PackageEdit, BrandError> {
        Ok(PackageEdit {
            name: Some(self.name),
            display: Some(self.display.map(FontChoiceBody::choice).transpose()?),
            content: Some(self.content.map(FontChoiceBody::choice).transpose()?),
            roles: Some(self.roles),
        })
    }
}

/// The body of `PATCH /api/brand/font-packages/{id}` and its site counterpart: each field given
/// is set; a font set to `null` is removed.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PackageEditBody {
    #[serde(default)]
    name: Option<String>,
    #[serde(default, deserialize_with = "present")]
    display: Option<Option<FontChoiceBody>>,
    #[serde(default, deserialize_with = "present")]
    content: Option<Option<FontChoiceBody>>,
    #[serde(default)]
    roles: Option<FontRoles>,
}

impl PackageEditBody {
    /// The change this body asks for.
    ///
    /// # Errors
    /// As [`FontChoiceBody::choice`].
    pub(crate) fn edit(self) -> Result<PackageEdit, BrandError> {
        let font = |given: Option<Option<FontChoiceBody>>| {
            given
                .map(|f| f.map(FontChoiceBody::choice).transpose())
                .transpose()
        };
        Ok(PackageEdit {
            name: self.name,
            display: font(self.display)?,
            content: font(self.content)?,
            roles: self.roles,
        })
    }
}

/// `POST /api/brand/font-packages {name, display?, content?, roles}`: adds a font package of
/// brand-centre fonts (see [`brand::create_package`]); `201` with the package as listed. `403`;
/// `422 invalid_font_package {reason}`; `409 name_taken`; `409 too_many {limit}`.
pub(crate) async fn create_package(
    State(state): State<AppState>,
    signed: Signed,
    JsonBody(body): JsonBody<NewPackage>,
) -> Result<Response, BrandError> {
    let edit = body.edit()?;
    let actor = signed.session.user;
    let package =
        completed(async move { brand::create_package(&state, &actor, edit).await }).await?;
    Ok((StatusCode::CREATED, Json(org_package_out(&package))).into_response())
}

/// `PATCH /api/brand/font-packages/{id}`: changes a font package of the brand centre (see
/// [`brand::change_package`]); the package as listed. `404` for no such package; otherwise as
/// [`create_package`].
pub(crate) async fn change_package(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<PackageEditBody>,
) -> Result<Json<Value>, BrandError> {
    let id = parse_id(&id)?;
    let edit = body.edit()?;
    let actor = signed.session.user;
    let package =
        completed(async move { brand::change_package(&state, &actor, id, edit).await }).await?;
    Ok(Json(org_package_out(&package)))
}

/// `DELETE /api/brand/font-packages/{id}`: deletes a font package of the brand centre; `204`.
/// Looks that copied it keep their copy. `403`; `404`.
pub(crate) async fn delete_package(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
) -> Result<StatusCode, BrandError> {
    let id = parse_id(&id)?;
    let actor = signed.session.user;
    completed(async move { brand::delete_package(&state, &actor, id).await }).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The body of `PUT /api/brand/policy`: each limit given is set.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PolicyBody {
    #[serde(default)]
    hide_builtin_themes: Option<bool>,
    #[serde(default)]
    custom_colours: Option<bool>,
    #[serde(default)]
    site_themes: Option<bool>,
    #[serde(default)]
    site_images: Option<bool>,
    #[serde(default)]
    site_fonts: Option<bool>,
}

/// `PUT /api/brand/policy {hide_builtin_themes?, custom_colours?, site_themes?, site_images?,
/// site_fonts?}`: sets the limits on site owners (see [`brand::set_policy`]); the policy.
/// Administrators only (`403`).
pub(crate) async fn set_policy(
    State(state): State<AppState>,
    signed: Signed,
    JsonBody(body): JsonBody<PolicyBody>,
) -> Result<Json<Value>, BrandError> {
    let edit = PolicyEdit {
        hide_builtin_themes: body.hide_builtin_themes,
        custom_colours: body.custom_colours,
        site_themes: body.site_themes,
        site_images: body.site_images,
        site_fonts: body.site_fonts,
    };
    let actor = signed.session.user;
    let policy = completed(async move { brand::set_policy(&state, &actor, edit).await }).await?;
    Ok(Json(json!(policy)))
}

/// The body of `PUT /api/brand/default-icon`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DefaultIcon {
    image: Option<String>,
}

/// `PUT /api/brand/default-icon {image: id | null}`: makes a brand image of kind `icon` the
/// organisation's default tab icon, or clears it (see [`brand::set_default_icon`]); `204`.
/// `403` unless a brand manager or administrator; `404` for no such image; `422 not_an_icon`.
pub(crate) async fn set_default_icon(
    State(state): State<AppState>,
    signed: Signed,
    JsonBody(body): JsonBody<DefaultIcon>,
) -> Result<StatusCode, BrandError> {
    let image = body.image.as_deref().map(parse_id).transpose()?;
    let actor = signed.session.user;
    completed(async move { brand::set_default_icon(&state, &actor, image).await }).await?;
    Ok(StatusCode::NO_CONTENT)
}
