//! A file document's bytes over HTTP: download, save, and upload as a new document.
//!
//! ShareCraft never interprets these bytes. A download is always an attachment of type
//! `application/octet-stream`, behind headers that keep a browser from rendering it. A save
//! goes to the document's file session and nowhere else; an upload creates a document and its
//! first revision, which nobody can have open yet.

use std::fmt::Write as _;
use std::str::FromStr;

use axum::Json;
use axum::body::Bytes;
use axum::extract::rejection::QueryRejection;
use axum::extract::{FromRequest, Path, Query, Request, State};
use axum::http::header::{
    CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_SECURITY_POLICY, CONTENT_TYPE,
    X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use collab_core::ids::{DocId, NodeId};
use records::{AccessError, Editor};
use revisions::{Author, Checkpoint, CheckpointError, Outcome, RevisionId, Stored};
use serde::Deserialize;
use serde_json::{Map, json};
use uuid::Uuid;

use super::{ApiError, parse_id, to_completion};
use crate::checkpoint::Checkpointer;
use crate::file_room::SaveReply;
use crate::projection::record_first;
use crate::room::credited_name;
use crate::tree;
use crate::{AppState, auth::Signed};

/// The request header carrying the seat token. It is never logged, stored or echoed.
const SEAT_HEADER: &str = "x-sharecraft-seat";
/// The response header naming the revision a download holds.
const REVISION_HEADER: &str = "x-sharecraft-revision";

/// Query of `GET /api/docs/{id}/content`.
#[derive(Deserialize)]
pub(crate) struct DownloadQuery {
    revision: Option<String>,
}

/// Query of `PUT /api/docs/{id}/content`.
#[derive(Deserialize)]
pub(crate) struct SaveQuery {
    base: Option<String>,
}

/// Query of `POST /api/docs/upload`.
#[derive(Deserialize)]
pub(crate) struct UploadQuery {
    /// The library or folder the new document goes into.
    parent: Option<String>,
    name: Option<String>,
}

/// A query that does not parse is a `400`.
fn query_of<T>(query: Result<Query<T>, QueryRejection>) -> Result<T, ApiError> {
    query.map(|Query(q)| q).map_err(|_| ApiError::BadRequest)
}

/// Runs `read` on the store without blocking the async workers.
pub(crate) async fn blocking<T: Send + 'static>(
    state: &AppState,
    read: impl FnOnce(&dyn Checkpointer) -> Result<T, CheckpointError> + Send + 'static,
) -> Result<T, ApiError> {
    let store = std::sync::Arc::clone(state.checkpointer());
    match tokio::task::spawn_blocking(move || read(&*store)).await {
        Ok(result) => result.map_err(|e| ApiError::unavailable(&e)),
        Err(e) => Err(ApiError::unavailable(&e)),
    }
}

/// The bytes of a request body, refused with `413` when the limit set for the route is passed
/// (the limit is applied while reading) and `400` when the body cannot be read.
async fn body_bytes(request: Request, state: &AppState) -> Result<Bytes, ApiError> {
    Bytes::from_request(request, state)
        .await
        .map_err(|rejection| match rejection.status() {
            StatusCode::PAYLOAD_TOO_LARGE => ApiError::TooLarge,
            _ => ApiError::BadRequest,
        })
}

/// `GET /api/docs/{id}/content[?revision=<id>]`: the document's bytes, as its current revision
/// or the given one. `204` if the document has no revision yet; `404` if the caller has no role
/// on it or the revision is not one of its own.
pub(crate) async fn download(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
    query: Result<Query<DownloadQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    let doc = DocId(parse_id(&id)?);
    let db = state.db();
    db.doc_access(signed.session.user.id, doc)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
        .ok_or(ApiError::NotFound)?;
    let document = db
        .document(doc)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
        .ok_or(ApiError::NotFound)?;
    let query = query_of(query)?;

    let stored = match query.revision {
        Some(text) => {
            // Text that is not a revision id names nothing.
            let revision = RevisionId::from_str(&text).map_err(|_| ApiError::NotFound)?;
            blocking(&state, move |store| store.load_at(doc, &revision))
                .await?
                .ok_or(ApiError::NotFound)?
        }
        None => match blocking(&state, move |store| store.load(doc)).await? {
            Some(stored) => stored,
            None => return Ok(StatusCode::NO_CONTENT.into_response()),
        },
    };

    file_answer(&document.name, stored)
}

/// A file's bytes as every download answers them: an attachment named `name`, of type
/// `application/octet-stream`, that a browser neither sniffs nor renders (`nosniff`, a sandbox
/// policy), never cached, naming its revision.
pub(crate) fn file_answer(name: &str, stored: Stored) -> Result<Response, ApiError> {
    let mut headers = HeaderMap::new();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    headers.insert(
        CONTENT_DISPOSITION,
        HeaderValue::from_str(&content_disposition(name)).map_err(|e| ApiError::unavailable(&e))?,
    );
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(CONTENT_SECURITY_POLICY, HeaderValue::from_static("sandbox"));
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        HeaderName::from_static(REVISION_HEADER),
        HeaderValue::from_str(&stored.revision.to_string())
            .map_err(|e| ApiError::unavailable(&e))?,
    );
    Ok((StatusCode::OK, headers, stored.bytes).into_response())
}

/// `PUT /api/docs/{id}/content?base=<id>`: the seat holder saves the file as the document's
/// next revision, made from revision `base` (none if the document has none yet). The request
/// carries the seat token in `X-ShareCraft-Seat`.
///
/// The role is checked before the body is read. The save is handed to the document's file
/// session and nothing else, and finishes even if the caller goes away.
pub(crate) async fn save(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
    query: Result<Query<SaveQuery>, QueryRejection>,
    headers: HeaderMap,
    request: Request,
) -> Result<Response, ApiError> {
    let doc = DocId(parse_id(&id)?);
    let user = signed.session.user.id;
    let db = state.db();
    let access = db
        .doc_access(user, doc)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
        .ok_or(ApiError::NotFound)?;
    if !access.can_edit() {
        return Err(ApiError::Forbidden);
    }
    let document = db
        .document(doc)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
        .ok_or(ApiError::NotFound)?;
    // A text document is edited in its room, never as a file.
    if !document.editor.is_file() {
        return Err(ApiError::BadRequest);
    }
    let base = match query_of(query)?.base {
        Some(text) => Some(RevisionId::from_str(&text).map_err(|_| ApiError::BadRequest)?),
        None => None,
    };
    let token = headers
        .get(SEAT_HEADER)
        .and_then(|value| value.to_str().ok())
        .ok_or(ApiError::Forbidden)?
        .to_owned();
    let bytes = body_bytes(request, &state).await?;

    let reply = {
        let state = state.clone();
        to_completion(async move {
            Ok(state
                .rooms()
                .save_file(doc, user, &token, base, bytes.to_vec())
                .await)
        })
        .await?
    };
    Ok(match reply {
        SaveReply::Saved(revision) => saved(&revision, true),
        SaveReply::Unchanged(revision) => saved(&revision, false),
        SaveReply::NotHolder => ApiError::Forbidden.into_response(),
        SaveReply::Conflict { current } => (
            StatusCode::CONFLICT,
            Json(json!({ "current": current.map(|r| r.to_string()) })),
        )
            .into_response(),
        SaveReply::Failed => ApiError::Unavailable.into_response(),
        // Distinct from a failed write: nothing was tried, and trying again later may work.
        SaveReply::Busy => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "busy" })),
        )
            .into_response(),
    })
}

/// The answer to a save that stored (or found equal) `revision`.
fn saved(revision: &RevisionId, changed: bool) -> Response {
    Json(json!({ "revision": revision.to_string(), "changed": changed })).into_response()
}

/// `POST /api/docs/upload?parent=<id>&name=<file name>`: creates a document from the bytes
/// sent in the library or folder `parent` (of which the caller must be an editor), with them as
/// its first revision. The document inherits its access from there. The editor is the one
/// whose extension ends the name. If the bytes cannot be stored after the document was made,
/// the document stays, empty, and the answer is `503` naming it.
pub(crate) async fn upload(
    State(state): State<AppState>,
    signed: Signed,
    query: Result<Query<UploadQuery>, QueryRejection>,
    request: Request,
) -> Result<Response, ApiError> {
    let query = query_of(query)?;
    let parent = NodeId(parse_id(&query.parent.ok_or(ApiError::BadRequest)?)?);
    let name = query.name.ok_or(ApiError::BadRequest)?;
    let editor = Editor::for_file_name(name.trim()).ok_or(ApiError::BadRequest)?;
    let bytes = body_bytes(request, &state).await?;
    if bytes.is_empty() {
        return Err(ApiError::BadRequest);
    }
    let user = signed.session.user;

    to_completion(async move {
        let id = DocId(Uuid::new_v4());
        let created =
            tree::create_document(&state, &user, parent, id, &name, editor, Some(bytes.len()))
                .await;
        let document = match created {
            Ok(document) => document,
            // The name is only an extension.
            Err(AccessError::WrongExtension) => return Err(ApiError::BadRequest),
            Err(e) => return Err(e.into()),
        };
        let access = state
            .db()
            .doc_access(user.id, id)
            .await
            .map_err(|e| ApiError::unavailable(&e))?;
        let author = Author {
            id: user.id.0.to_string(),
            name: credited_name(&user.name),
        };
        let file_name = document.name.clone();
        let written = blocking(&state, move |store| {
            store.write(&Checkpoint {
                doc: id,
                file_name: &file_name,
                bytes: &bytes,
                base: None,
                authors: std::slice::from_ref(&author),
                fields: &Map::new(),
            })
        })
        .await;
        let revision = match written {
            Ok(Outcome::Written(revision) | Outcome::Unchanged(revision)) => revision,
            Err(_) => {
                // Already reported to standard error; the document stays, empty.
                let body = Json(json!({ "error": "unavailable", "document": id.0.to_string() }));
                return Ok((StatusCode::SERVICE_UNAVAILABLE, body).into_response());
            }
        };
        record_first(
            state.checkpointer(),
            state.db(),
            state.events(),
            id,
            &revision,
        )
        .await;
        let body = Json(json!({
            "id": document.id.0.to_string(),
            "name": document.name,
            "editor": document.editor,
            "access": access,
        }));
        Ok((StatusCode::CREATED, body).into_response())
    })
    .await
}

/// The `Content-Disposition` of a download of the file `name`: an attachment, with an ASCII
/// fallback name for old clients and the real name in `filename*` (RFC 6266, RFC 8187).
fn content_disposition(name: &str) -> String {
    let fallback: String = name
        .chars()
        .map(|c| {
            // `%` too: some browsers percent-decode the plain `filename`.
            if c.is_ascii() && !c.is_ascii_control() && !matches!(c, '"' | '\\' | '%') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let mut encoded = String::new();
    for byte in name.bytes() {
        if is_attr_char(byte) {
            encoded.push(char::from(byte));
        } else {
            // Writing to a string cannot fail.
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    format!("attachment; filename=\"{fallback}\"; filename*=UTF-8''{encoded}")
}

/// Whether RFC 8187 allows `byte` unescaped in an extended parameter value.
fn is_attr_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#' | b'$' | b'&' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
        )
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    #[test]
    fn plain_name() {
        assert_eq!(
            content_disposition("Budget 2026.xlsx"),
            "attachment; filename=\"Budget 2026.xlsx\"; filename*=UTF-8''Budget%202026.xlsx"
        );
    }

    #[test]
    fn non_ascii_name_gets_a_fallback_and_an_encoding() {
        assert_eq!(
            content_disposition("B\u{fc}dget \u{65e5}\u{672c}.docx"),
            "attachment; filename=\"B_dget __.docx\"; \
             filename*=UTF-8''B%C3%BCdget%20%E6%97%A5%E6%9C%AC.docx"
        );
    }

    #[test]
    fn quotes_backslashes_and_control_characters_cannot_break_out() {
        let header = content_disposition("a\"b\\c\td\u{7f}.xlsx");
        assert_eq!(
            header,
            "attachment; filename=\"a_b_c_d_.xlsx\"; filename*=UTF-8''a%22b%5Cc%09d%7F.xlsx"
        );
        // Whatever the name, the header is one valid header value.
        assert!(HeaderValue::from_str(&header).is_ok());
    }

    #[test]
    fn extended_value_keeps_only_the_characters_the_rfc_allows() {
        let header = content_disposition("a;b,c=d'e(f)%.pdf");
        assert!(
            header.starts_with("attachment; filename=\"a;b,c=d'e(f)_.pdf\"; "),
            "{header}"
        );
        assert!(
            header.ends_with("filename*=UTF-8''a%3Bb%2Cc%3Dd%27e%28f%29%25.pdf"),
            "{header}"
        );
    }
}
