//! Site templates, for administrators: the list of templates, saving a site as one, a
//! template's package, importing a package, and deleting a saved template (see
//! [`crate::templates`]).

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::http::header::{CONTENT_DISPOSITION, CONTENT_TYPE};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use super::content::content_disposition;
use super::nodes::node_id;
use super::{ApiError, JsonBody, to_completion};
use crate::AppState;
use crate::auth::Signed;
use crate::templates::{self, Cleaned, TemplateSummary};
use crate::tree::clean_folder_name;
use revisions::TemplateRecord;

/// The largest body `POST /api/templates/import` reads: a package larger than
/// [`records::MAX_PACKAGE_BYTES`] up to this is answered with its reason, beyond it `413`.
pub(crate) const MAX_IMPORT_BODY: usize = 2 * 1024 * 1024;

/// `GET /api/templates`: the templates, for administrators (`403` for anyone else): the
/// built-in `team` and `communication` first, then the saved ones by name, each
/// `{id, name, description, built_in, kind}`.
pub(crate) async fn list(
    State(state): State<AppState>,
    signed: Signed,
) -> Result<Json<Vec<TemplateSummary>>, ApiError> {
    Ok(Json(templates::list(&state, &signed.session.user).await?))
}

/// The body of `POST /api/sites/{id}/template`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SaveBody {
    name: String,
    #[serde(default)]
    description: String,
}

/// `POST /api/sites/{id}/template {name, description?}`: saves the site's shape as a template
/// (see [`templates::save_from_site`]). Administrators only (`403`); `404` for anything but a
/// live site; `422 invalid_template` with a `reason` for a name or description that breaks the
/// rules or a site too large for a template. `201` with the template as listed and `cleaned`,
/// the names made under older rules that had to change (`[{from, to}]`).
pub(crate) async fn save(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<SaveBody>,
) -> Result<Response, ApiError> {
    let site = node_id(&id)?;
    let actor = signed.session.user;
    to_completion(async move {
        let (record, cleaned) =
            templates::save_from_site(&state, &actor, site, &body.name, &body.description).await?;
        Ok((StatusCode::CREATED, Json(answered(&record, &cleaned))).into_response())
    })
    .await
}

/// `GET /api/templates/{id}/package`: the template (a built-in's name or a saved template's
/// id) as a package to carry to another installation, downloaded as `<name>.json`, the name
/// cleaned as a file name. Administrators only (`403`); `404` for an unknown or deleted
/// template.
pub(crate) async fn package(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let package = templates::package(&state, &signed.session.user, &id).await?;
    let mut bytes = serde_json::to_vec_pretty(&package).map_err(|e| ApiError::unavailable(&e))?;
    bytes.push(b'\n');
    let file = format!("{}.json", clean_folder_name(&package.name));
    Ok((
        [
            (CONTENT_TYPE, "application/json".to_owned()),
            (CONTENT_DISPOSITION, content_disposition(&file)),
        ],
        bytes,
    )
        .into_response())
}

/// `POST /api/templates/import <package>`: imports a template package (see
/// [`templates::read_package`]) as a new saved template. Administrators only (`403`).
/// `422 invalid_template` with a `reason` for a package refused; `413` for a body over 2 MiB.
/// `201` with the template as listed and `cleaned`, the names that had to change
/// (`[{from, to}]`).
pub(crate) async fn import(
    State(state): State<AppState>,
    signed: Signed,
    body: Bytes,
) -> Result<Response, ApiError> {
    let actor = signed.session.user;
    if !actor.is_admin {
        return Err(ApiError::Forbidden);
    }
    // Reading a package is bounded work, but not trivial: it runs off the async workers.
    let (package, cleaned) = tokio::task::spawn_blocking(move || templates::read_package(&body))
        .await
        .map_err(|e| ApiError::unavailable(&e))??;
    to_completion(async move {
        let record = templates::import(&state, &actor, package, &cleaned).await?;
        Ok((StatusCode::CREATED, Json(answered(&record, &cleaned))).into_response())
    })
    .await
}

/// A saved template as listed, with `cleaned`: the names that had to change.
fn answered(record: &TemplateRecord, cleaned: &[Cleaned]) -> Value {
    let mut out: Value =
        serde_json::to_value(templates::summary_of_record(record)).unwrap_or_default();
    if let Some(fields) = out.as_object_mut() {
        fields.insert("cleaned".to_owned(), json!(cleaned));
    }
    out
}

/// `DELETE /api/templates/{id}`: deletes a saved template (its record is kept, marked
/// deleted). Administrators only; the built-ins cannot be deleted (`403`); `404` for an
/// unknown or deleted template. `204`.
pub(crate) async fn remove(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let actor = signed.session.user;
    to_completion(async move {
        templates::delete(&state, &actor, &id).await?;
        Ok(StatusCode::NO_CONTENT.into_response())
    })
    .await
}
