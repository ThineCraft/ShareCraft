//! Libraries' kinds and settings: one library with its settings, changing them, making a
//! library of a kind in a site, the asset libraries, and "New from template": the template
//! choices of a library and a new file copied from one.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use collab_core::ids::NodeId;
use records::{LibraryKind, LibrarySettings, Node};
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use uuid::Uuid;

use super::nodes::{Copied, NodeOut, node_id, rfc3339};
use super::{ApiError, JsonBody, to_completion};
use crate::AppState;
use crate::auth::Signed;
use crate::libraries::{self, LibraryEdit};
use crate::tree::{self, TreeOp};

/// A field that may be given as `null`: absent is `None`, `null` is `Some(None)`.
fn nullable<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

/// A library with its settings, as its routes answer it: `{id, name, site, kind, description,
/// folders, allowed_types, blocked_types, max_file_bytes, effective_allowed_types}`, where
/// `effective_allowed_types` is what it accepts in effect (for an image library with no list
/// of its own, the image types; `null` for every type).
fn library_out(node: &Node, settings: &LibrarySettings) -> Value {
    json!({
        "id": node.id.0.to_string(),
        "name": node.name,
        "site": node.site.0.to_string(),
        "kind": settings.kind.as_str(),
        "description": settings.description,
        "folders": settings.folders,
        "allowed_types": settings.allowed_types,
        "blocked_types": settings.blocked_types,
        "max_file_bytes": settings.max_file_bytes,
        "effective_allowed_types": settings.effective_allowed(),
    })
}

/// `GET /api/libraries/{id}`: the library with its settings (see [`library_out`]), for
/// someone who can view it; `404` otherwise.
pub(crate) async fn show(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let library = node_id(&id)?;
    let (node, settings) = libraries::show(&state, &signed.session.user, library).await?;
    Ok(Json(library_out(&node, &settings)))
}

/// The body of `PATCH /api/libraries/{id}`: each field given is set.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LibraryChangeBody {
    description: Option<String>,
    kind: Option<String>,
    folders: Option<bool>,
    /// `null` for every type.
    #[serde(default, deserialize_with = "nullable")]
    allowed_types: Option<Option<Vec<String>>>,
    blocked_types: Option<Vec<String>>,
    /// `null` for no limit of the library's own.
    #[serde(default, deserialize_with = "nullable")]
    max_file_bytes: Option<Option<u64>>,
}

/// `PATCH /api/libraries/{id} {description?, kind?, folders?, allowed_types?, blocked_types?,
/// max_file_bytes?}`: changes the library's kind and settings, with `manage_site` on it (the
/// kind `asset`, or a change away from it, needs an administrator too): one commit, recorded
/// as `library.changed`. Answers the library as it then is. `422 invalid_library_settings`
/// with the `reason` for settings that break a rule; `403 archived` on an archived site.
pub(crate) async fn change(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<LibraryChangeBody>,
) -> Result<Json<Value>, ApiError> {
    let library = node_id(&id)?;
    let actor = signed.session.user;
    let edit = LibraryEdit {
        description: body.description,
        kind: body.kind,
        folders: body.folders,
        allowed_types: body.allowed_types,
        blocked_types: body.blocked_types,
        max_file_bytes: body.max_file_bytes,
    };
    {
        let state = state.clone();
        let actor = actor.clone();
        to_completion(async move {
            libraries::change(&state, &actor, library, edit).await?;
            Ok(())
        })
        .await?;
    }
    let (node, settings) = libraries::show(&state, &actor, library).await?;
    Ok(Json(library_out(&node, &settings)))
}

/// The body of `POST /api/sites/{id}/libraries`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewLibrary {
    name: String,
    /// `document` when not given.
    kind: Option<String>,
}

/// `POST /api/sites/{id}/libraries {name, kind?}`: makes a library of that kind (`document`
/// when not given) in the site, with `manage_site` on it (an `asset` library needs an
/// administrator too), with the defaults for its settings. `201` with the library and its
/// settings.
pub(crate) async fn create(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
    JsonBody(new): JsonBody<NewLibrary>,
) -> Result<Response, ApiError> {
    let site = node_id(&id)?;
    let kind = match new.kind.as_deref() {
        None => LibraryKind::Document,
        Some(word) => libraries::parse_kind(word)?,
    };
    let actor = signed.session.user;
    let node = {
        let state = state.clone();
        let actor = actor.clone();
        to_completion(async move {
            let id = NodeId(Uuid::new_v4());
            Ok(
                tree::create_library(&state, &actor, site, id, &new.name, kind)
                    .await?
                    .node,
            )
        })
        .await?
    };
    let (node, settings) = libraries::show(&state, &actor, node.id).await?;
    let mut out = library_out(&node, &settings);
    if let (Some(map), Ok(Value::Object(created))) = (
        out.as_object_mut(),
        serde_json::to_value(NodeOut::from(node)),
    ) {
        for (key, value) in created {
            map.entry(key).or_insert(value);
        }
    }
    Ok((StatusCode::CREATED, Json(out)).into_response())
}

/// `GET /api/assets`: the asset libraries the caller can view, by name, each with its settings
/// (see [`library_out`]).
pub(crate) async fn assets(
    State(state): State<AppState>,
    signed: Signed,
) -> Result<Json<Vec<Value>>, ApiError> {
    let found = libraries::assets(&state, &signed.session.user).await?;
    Ok(Json(
        found
            .iter()
            .map(|(node, settings)| library_out(node, settings))
            .collect(),
    ))
}

/// `GET /api/libraries/{id}/templates`: the "New from template" choices of the library, for
/// someone who can view it (`404` otherwise): the files in the template libraries of its site
/// that the caller can view and whose content is available, by name, each `{id, name, editor,
/// library, library_name, revised_at}` (`library` is the template library it is in).
pub(crate) async fn templates(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
) -> Result<Json<Vec<Value>>, ApiError> {
    let library = node_id(&id)?;
    let found = libraries::templates(&state, &signed.session.user, library).await?;
    Ok(Json(
        found
            .into_iter()
            .map(|file| {
                json!({
                    "id": file.id.0.to_string(),
                    "name": file.name,
                    "editor": file.editor,
                    "library": file.library.0.to_string(),
                    "library_name": file.library_name,
                    "revised_at": rfc3339(file.revised_at),
                })
            })
            .collect(),
    ))
}

/// The body of `POST /api/libraries/{id}/from-template`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FromTemplate {
    /// The template file to copy.
    template: String,
    /// The folder in the library the new file goes into; the library itself if absent.
    parent: Option<String>,
    /// The new file's name; the template's if absent.
    name: Option<String>,
}

/// `POST /api/libraries/{id}/from-template {template, parent?, name?}`: a new file in the
/// library, or in its folder `parent`, copied from a template file of the same site through
/// the copy path (see [`tree::apply`]): the template's latest version only, with no history.
/// `201 {node, renamed}` as a copy answers. `404` for a template that is not a file the caller
/// can view in a template library of the library's site; `400 wrong_place` for a `parent`
/// outside the library; `403` without `add` there (`archived` on an archived site); `422
/// blocked_file_type` / `413 file_too_large` for the library's policies; `507
/// quota_exceeded`; `423 held {scan}` for a template the malware scanner holds.
pub(crate) async fn from_template(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<FromTemplate>,
) -> Result<Response, ApiError> {
    let library = node_id(&id)?;
    let template = node_id(&body.template)?;
    let parent = body.parent.as_deref().map(node_id).transpose()?;
    let actor = signed.session.user;
    let copied = to_completion(async move {
        let op = TreeOp::FromTemplate {
            library,
            template,
            parent,
            name: body.name,
        };
        let applied = tree::apply(&state, &actor, op).await.map_err(|e| match e {
            // The name asked for has another extension than the template's.
            records::AccessError::WrongExtension => ApiError::InvalidName,
            e => e.into(),
        })?;
        Ok(Copied::from(applied))
    })
    .await?;
    Ok((StatusCode::CREATED, Json(copied)).into_response())
}
