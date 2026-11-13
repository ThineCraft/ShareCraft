//! Documents: create, open, and their events. Who may access a document is served by the node
//! routes (see [`super::nodes`]).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use collab_core::ids::{DocId, NodeId};
use records::{Access, AccessError, Editor};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use super::nodes::require_manager;
use super::{ApiError, JsonBody, parse_id, to_completion};
use crate::AppState;
use crate::auth::Signed;
use crate::tree;

/// The name a document gets when the request gives none.
const DEFAULT_NAME: &str = "Untitled";

/// An RFC 3339 timestamp, or `None` when there is no time (or it cannot be written).
fn rfc3339(time: Option<OffsetDateTime>) -> Option<String> {
    time.and_then(|time| time.format(&Rfc3339).ok())
}

/// The body of `POST /api/docs`.
#[derive(Deserialize)]
pub(crate) struct NewDoc {
    /// The library or folder the document goes into.
    parent: String,
    name: Option<String>,
    /// An editor id; `text` when absent.
    editor: Option<String>,
}

/// `400` with a code and a sentence saying what to do.
fn refused(code: &str, message: &str) -> Response {
    let body = Json(serde_json::json!({ "error": code, "message": message }));
    (StatusCode::BAD_REQUEST, body).into_response()
}

/// `POST /api/docs {parent, name?, editor?}`: creates a document in the library or folder
/// `parent`, of which the caller must be an editor. The document inherits its access from
/// there. `201` with the document and the caller's access to it.
pub(crate) async fn create(
    State(state): State<AppState>,
    signed: Signed,
    JsonBody(new): JsonBody<NewDoc>,
) -> Result<Response, ApiError> {
    let parent = NodeId(parse_id(&new.parent)?);
    let editor = match new.editor.as_deref() {
        None => Editor::Text,
        Some(text) => match Editor::from_id(text) {
            Some(editor) => editor,
            None => return Ok(refused("unknown_editor", "that editor does not exist")),
        },
    };
    if editor == Editor::PdfCraft {
        return Ok(refused(
            "upload_required",
            "an empty PDF is not a file: upload a PDF instead",
        ));
    }
    let id = DocId(Uuid::new_v4());
    let name = new.name.unwrap_or_else(|| DEFAULT_NAME.to_owned());
    let actor = signed.session.user;
    let user = actor.id;
    let created =
        to_completion({
            let state = state.clone();
            async move {
                Ok(tree::create_document(&state, &actor, parent, id, &name, editor, None).await)
            }
        })
        .await?;
    let document = match created {
        Ok(document) => document,
        Err(AccessError::WrongExtension) => {
            let message = format!(
                "the name of a {} document must end with {}",
                editor.id(),
                editor.native_extension().unwrap_or_default()
            );
            return Ok(refused("wrong_extension", &message));
        }
        Err(e) => return Err(e.into()),
    };
    let access = state
        .db()
        .doc_access(user, id)
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
    let body = Json(serde_json::json!({
        "id": document.id.0.to_string(),
        "name": document.name,
        "editor": document.editor,
        "access": access,
    }));
    Ok((StatusCode::CREATED, body).into_response())
}

/// `GET /api/docs/{id}`.
#[derive(Serialize)]
pub(crate) struct DocDetail {
    id: String,
    name: String,
    editor: Editor,
    /// The caller's access: `{perms, roles}`.
    access: Access,
    revision: Option<String>,
}

/// `GET /api/docs/{id}`: the document, if the caller has access to it.
pub(crate) async fn open(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
) -> Result<Json<DocDetail>, ApiError> {
    let doc = DocId(parse_id(&id)?);
    let db = state.db();
    let access = db
        .doc_access(signed.session.user.id, doc)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
        .ok_or(ApiError::NotFound)?;
    let document = db
        .document(doc)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(DocDetail {
        id: document.id.0.to_string(),
        name: document.name,
        editor: document.editor,
        access,
        revision: document.revision,
    }))
}

/// Most events `GET /api/docs/{id}/events` returns.
const MAX_EVENTS: i64 = 200;

/// One row of `GET /api/docs/{id}/events`.
#[derive(Serialize)]
pub(crate) struct EventRow {
    id: i64,
    at: Option<String>,
    kind: String,
    actor: Option<String>,
    subject: Option<String>,
    detail: Value,
}

/// `GET /api/docs/{id}/events`: the latest events of the document, oldest first. Only for
/// those who manage access to it. A caller's own `request.created` events are left out: a
/// request is recorded only for an item that exists, so its event must never reach the person
/// who asked.
pub(crate) async fn events(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
) -> Result<Json<Vec<EventRow>>, ApiError> {
    let doc = DocId(parse_id(&id)?);
    let db = state.db();
    let caller = signed.session.user.id;
    require_manager(db, caller, doc.into()).await?;
    let events = db
        .recent_events_for(doc, MAX_EVENTS)
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
    Ok(Json(
        events
            .into_iter()
            .filter(|event| !(event.kind == "request.created" && event.subject == Some(caller)))
            .map(|event| EventRow {
                id: event.id,
                at: rfc3339(Some(event.at)),
                kind: event.kind,
                actor: event.actor.map(|u| u.0.to_string()),
                subject: event.subject.map(|u| u.0.to_string()),
                detail: event.detail,
            })
            .collect(),
    ))
}
