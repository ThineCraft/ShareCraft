//! Documents: create, open, and their events. Who may access a document is served by the node
//! routes (see [`super::nodes`]).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use collab_core::ids::{DocId, NodeId};
use records::{Access, AccessError, Editor, Unavailable};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use super::nodes::{require_not_connected_for, require_reading_manager};
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

/// Where `doc`'s live log stands against its stored revisions, or `None` when it has never
/// been live. Best effort: the mark only decorates a document answer or a download, so a
/// database failure is reported on the console and the answer goes out without it.
pub(crate) async fn live_behind_mark(db: &records::Db, doc: DocId) -> Option<records::LiveBehind> {
    match db.live_behind(doc).await {
        Ok(live) => live,
        Err(e) => {
            eprintln!(
                "sharecraft-server: the live mark of document {} was left out: {e}",
                doc.0
            );
            None
        }
    }
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
/// there. `201` with the document and the caller's access to it. Its name is checked against
/// the file policies there (`422 blocked_file_type {extension}`). An empty PDF, or an empty
/// file no editor opens (`other`), is `400 upload_required`. `409 source_read_only` in a
/// library connected to a source.
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
    if editor == Editor::Other {
        return Ok(refused(
            "upload_required",
            "a file no editor opens is not made empty: upload the file instead",
        ));
    }
    let id = DocId(Uuid::new_v4());
    let name = new.name.unwrap_or_else(|| DEFAULT_NAME.to_owned());
    let actor = signed.session.user;
    let user = actor.id;
    require_not_connected_for(state.db(), user, parent).await?;
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
    /// The session kind the document answer routes to.
    session: &'static str,
    /// Whether a live log has entries beyond the newest stored revision.
    live: records::LiveBehind,
    /// `held` while the malware scanner holds the current version, `quarantined` when it is
    /// infected; absent when it is available.
    #[serde(skip_serializing_if = "Option::is_none")]
    scan: Option<&'static str>,
    /// The threat's name, when quarantined.
    #[serde(skip_serializing_if = "Option::is_none")]
    threat: Option<String>,
    /// File-share metadata for an external document.
    #[serde(skip_serializing_if = "Option::is_none")]
    external: Option<ExternalDetail>,
    /// A change to this file accepted and not yet written to its bucket.
    #[serde(skip_serializing_if = "Option::is_none")]
    writes: Option<WriteMark>,
}

/// Where a change accepted for a bucket stands.
#[derive(Serialize)]
struct WriteMark {
    /// `saving`, or `failed` when writing it failed and a retry is waited for.
    state: &'static str,
    /// Why it failed: shown only to the change's author and the library's owners.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
struct ExternalDetail {
    bytes: u64,
    modified_at: Option<String>,
    read: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    unavailable: Option<&'static str>,
    /// `true` for an S3 object in an archive storage class that has not been restored.
    #[serde(skip_serializing_if = "Option::is_none")]
    storage_class_archived: Option<bool>,
}

pub(crate) fn unavailable_name(unavailable: Unavailable) -> &'static str {
    match unavailable {
        Unavailable::Denied => "denied",
        Unavailable::Gone => "gone",
        Unavailable::TooLarge => "too_large",
        Unavailable::Locked => "locked",
        Unavailable::Archived => "archived",
    }
}

/// `GET /api/docs/{id}`: the document, if the caller has access to it. For a file no editor
/// opens, `editor` is `other`: the page shows a preview or offers a download. A document whose
/// current version the malware scanner holds is marked `scan: held`, or `scan: quarantined`
/// with `threat` when it is infected; its bytes are not handed out until it is available.
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
    let mark = db
        .scan_mark(doc)
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
    // Best effort: a failure here leaves the mark out rather than failing the answer.
    let live = live_behind_mark(db, doc)
        .await
        .unwrap_or(records::LiveBehind {
            behind: false,
            head: collab_core::ids::Seq(0),
            stored_through: collab_core::ids::Seq(0),
        });
    let (scan, threat) = scan_fields(mark);
    // A bucket file's session is never live; if that cannot be told, nothing is answered.
    let bucket_file = crate::source_write::is_bucket_file(&state, doc).await?;
    let session = match crate::ws::room_kind_for(&state, document.editor, bucket_file) {
        crate::ws::RoomKind::Live => "live",
        crate::ws::RoomKind::File => "file",
        crate::ws::RoomKind::Text => "text",
    };
    let external = db
        .external_item(NodeId(doc.0))
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
    // A change waiting to be written is the file's current revision, also of a new file the
    // store does not have yet.
    let pending = db
        .pending_write(NodeId(doc.0))
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
    let revision = match (&pending, &external) {
        (Some(pending), _) => Some(pending.revision.clone()),
        (None, Some(item)) => Some(item.revision.clone()),
        (None, None) => document.revision.clone(),
    };
    let writes = match &pending {
        None => None,
        Some(pending) => {
            let failed = pending.state == records::PendingState::Failed;
            let may_see_error = failed
                && (pending.by == signed.session.user.id
                    || db
                        .effective_access(signed.session.user.id, pending.library)
                        .await
                        .map_err(|e| ApiError::unavailable(&e))?
                        .is_some_and(|held| held.can_manage_access()));
            Some(WriteMark {
                state: if failed { "failed" } else { "saving" },
                error: pending.error.clone().filter(|_| may_see_error),
            })
        }
    };
    let staged_bytes = pending.as_ref().map(|pending| pending.blob.size);
    let external = match (external, staged_bytes) {
        (None, Some(bytes)) => Some(ExternalDetail {
            bytes,
            modified_at: None,
            read: true,
            unavailable: None,
            storage_class_archived: None,
        }),
        (external, _) => external.map(|item| ExternalDetail {
            bytes: staged_bytes.unwrap_or(item.size),
            modified_at: OffsetDateTime::from_unix_timestamp_nanos(i128::from(item.mtime_ns))
                .ok()
                .and_then(|at| at.format(&Rfc3339).ok()),
            read: item.sha256.is_some(),
            unavailable: item.unavailable.map(unavailable_name),
            storage_class_archived: records::external_tag_parts(&item.tag, item.size)
                .is_some_and(|parts| parts.archived)
                .then_some(true),
        }),
    };
    Ok(Json(DocDetail {
        id: document.id.0.to_string(),
        name: document.name,
        editor: document.editor,
        access,
        revision,
        session,
        live,
        scan,
        threat,
        external,
        writes,
    }))
}

/// The `scan` and `threat` fields that mark a held (`held`) or infected (`quarantined`) file.
pub(crate) fn scan_fields(
    mark: Option<records::ScanMark>,
) -> (Option<&'static str>, Option<String>) {
    match mark {
        None => (None, None),
        Some(mark) if mark.quarantined() => (Some("quarantined"), mark.threat),
        Some(_) => (Some("held"), None),
    }
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
    require_reading_manager(db, caller, doc.into()).await?;
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
