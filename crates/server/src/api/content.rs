//! A file document's bytes over HTTP: download, save, and upload as a new document.
//!
//! ShareCraft never interprets these bytes. A download is always an attachment of type
//! `application/octet-stream`, behind headers that keep a browser from rendering it, and may
//! ask for one byte range. A save goes to the document's file session and nowhere else; an
//! upload creates a document and its first revision, which nobody can have open yet.
//!
//! No file is ever held in memory: a save or an upload streams its body into the content store
//! while hashing it, and hands on only a reference to the stored content; a download streams
//! from the content store.

use std::fmt::Write as _;
use std::io::{Seek, SeekFrom};
use std::str::FromStr;
use std::sync::Arc;

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, Request, State};
use axum::http::header::{
    ACCEPT_RANGES, CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE,
    CONTENT_SECURITY_POLICY, CONTENT_TYPE, RANGE, X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use collab_core::ids::{DocId, NodeId};
use futures_util::Stream;
use records::{AccessError, Editor};
use revisions::{
    Author, BlobRef, Checkpoint, CheckpointError, Content, Outcome, RevisionId, Stored,
};
use serde::Deserialize;
use serde_json::{Map, json};
use tokio::io::AsyncReadExt;
use uuid::Uuid;

use super::{ApiError, parse_id, to_completion};
use crate::blobs::{BodyError, write_claimed};
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
/// How much of a file a download reads and sends at a time.
const DOWNLOAD_CHUNK: usize = 64 * 1024;

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

/// Streams a request body into the content store and answers what it stored. Refused with `413`
/// past the largest file allowed (at once when the body declares a larger length, otherwise as
/// soon as more arrives; nothing is stored), and `400` when the body cannot be read.
async fn body_blob(state: &AppState, request: Request) -> Result<BlobRef, ApiError> {
    let limit = state.config().max_file_bytes;
    let declared = request
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    if declared.is_some_and(|length| length > limit) {
        return Err(ApiError::TooLarge);
    }
    match state.blobs().write_body(request.into_body(), limit).await {
        Ok(blob) => Ok(blob),
        Err(BodyError::TooLarge) => Err(ApiError::TooLarge),
        Err(BodyError::Broken) => Err(ApiError::BadRequest),
        Err(BodyError::Store(e)) => Err(ApiError::unavailable(&e)),
    }
}

/// Erases `blob`, the body of a request refused after it was stored, unless something names it
/// (the same content saved by someone else, or by an earlier version). Nothing is erased while
/// a flag says the content rows may be incomplete, as a version could then name the content
/// without a row; the start-up erasure deals with it after the rebuild. Failures are logged.
async fn discard(state: &AppState, blob: BlobRef) {
    match state.db().blob_refs_flagged().await {
        Ok(false) => {}
        Ok(true) => return,
        Err(e) => {
            eprintln!("sharecraft-server: database failure: {e}");
            return;
        }
    }
    if let Err(e) = state
        .blobs()
        .erase_if_unreferenced(state.db(), &blob.sha256)
        .await
    {
        eprintln!("sharecraft-server: cannot erase the content of a refused request: {e}");
    }
}

/// `GET /api/docs/{id}/content[?revision=<id>]`: the document's bytes, as its current revision
/// or the given one, or the one byte range asked for (see [`file_answer`]). `204` if the
/// document has no revision yet; `404` if the caller has no role on it or the revision is not
/// one of its own.
pub(crate) async fn download(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
    query: Result<Query<DownloadQuery>, QueryRejection>,
    headers: HeaderMap,
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

    file_answer(&state, &document.name, stored, headers.get(RANGE)).await
}

/// Which bytes of a file a download sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Span {
    /// All of them.
    Whole,
    /// From `start` to `end`, both included.
    Part { start: u64, end: u64 },
    /// The range asked for starts beyond the end.
    Unsatisfiable,
}

/// The bytes of a file of `size` bytes that the `Range` header `range` asks for. One range of
/// bytes is served (`a-b`, `a-` or the suffix `-n`); anything else, several ranges included, is
/// ignored and the whole file is sent.
fn span_of(range: Option<&HeaderValue>, size: u64) -> Span {
    let number = |text: &str| -> Option<u64> {
        if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        text.parse().ok()
    };
    let Some(spec) = range
        .and_then(|value| value.to_str().ok())
        .and_then(|text| text.trim().strip_prefix("bytes="))
    else {
        return Span::Whole;
    };
    let Some((first, last)) = spec.split_once('-') else {
        return Span::Whole;
    };
    if spec.contains(',') {
        return Span::Whole;
    }
    let (first, last) = (first.trim(), last.trim());
    if first.is_empty() {
        let Some(suffix) = number(last) else {
            return Span::Whole;
        };
        if suffix == 0 || size == 0 {
            return Span::Unsatisfiable;
        }
        let suffix = suffix.min(size);
        return Span::Part {
            start: size - suffix,
            end: size - 1,
        };
    }
    let Some(start) = number(first) else {
        return Span::Whole;
    };
    let end = if last.is_empty() {
        None
    } else {
        match number(last) {
            Some(end) if end >= start => Some(end),
            _ => return Span::Whole,
        }
    };
    if start >= size {
        return Span::Unsatisfiable;
    }
    let last_byte = size - 1;
    Span::Part {
        start,
        end: end.map_or(last_byte, |end| end.min(last_byte)),
    }
}

/// A file's bytes as every download answers them: an attachment named `name`, of type
/// `application/octet-stream`, that a browser neither sniffs nor renders (`nosniff`, a sandbox
/// policy), never cached, naming its revision, with `Accept-Ranges: bytes`. When `range` asks
/// for one byte range it is `206` with that range and its `Content-Range`, or `416` if it
/// starts beyond the end. The bytes stream from the content store (an old version's from Git);
/// a version whose content is missing from the store is logged and answers `503`.
pub(crate) async fn file_answer(
    state: &AppState,
    name: &str,
    stored: Stored,
    range: Option<&HeaderValue>,
) -> Result<Response, ApiError> {
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
    headers.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    let size = stored.content.size();
    let file = match &stored.content {
        Content::Blob(blob) => Some(open_content(state, &stored.revision, *blob).await?),
        Content::Inline(_) => None,
    };
    let (status, start, length) = match span_of(range, size) {
        Span::Whole => (StatusCode::OK, 0, size),
        Span::Part { start, end } => {
            headers.insert(
                CONTENT_RANGE,
                header_value(&format!("bytes {start}-{end}/{size}"))?,
            );
            (StatusCode::PARTIAL_CONTENT, start, end - start + 1)
        }
        Span::Unsatisfiable => {
            headers.insert(CONTENT_RANGE, header_value(&format!("bytes */{size}"))?);
            return Ok((StatusCode::RANGE_NOT_SATISFIABLE, headers).into_response());
        }
    };
    headers.insert(CONTENT_LENGTH, header_value(&length.to_string())?);
    let body = match (stored.content, file) {
        (_, Some(file)) => Body::from_stream(file_stream(file, start, length)?),
        (Content::Inline(bytes), None) => {
            let from = usize::try_from(start).map_err(|e| ApiError::unavailable(&e))?;
            let to = usize::try_from(start + length).map_err(|e| ApiError::unavailable(&e))?;
            Body::from(bytes.get(from..to).unwrap_or_default().to_vec())
        }
        (Content::Blob(_), None) => return Err(ApiError::Unavailable),
    };
    Ok((status, headers, body).into_response())
}

fn header_value(text: &str) -> Result<HeaderValue, ApiError> {
    HeaderValue::from_str(text).map_err(|e| ApiError::unavailable(&e))
}

/// The stored content `blob` of `revision`, opened for reading. Content missing from the store,
/// or not of its recorded size, is logged and answers `503`.
async fn open_content(
    state: &AppState,
    revision: &RevisionId,
    blob: BlobRef,
) -> Result<std::fs::File, ApiError> {
    let blobs = Arc::clone(state.blobs());
    let opened = tokio::task::spawn_blocking(move || {
        let file = blobs.store().open_blob(&blob.sha256)?;
        match file {
            Some(file) if file.metadata()?.len() == blob.size => Ok(Some(file)),
            Some(_) | None => Ok::<_, revisions::BlobError>(None),
        }
    })
    .await
    .map_err(|e| ApiError::unavailable(&e))?
    .map_err(|e| ApiError::unavailable(&e))?;
    opened.ok_or_else(|| {
        eprintln!(
            "sharecraft-server: content {} of revision {revision} is missing from the content \
             store",
            blob.hex()
        );
        ApiError::Unavailable
    })
}

/// `length` bytes of `file` from `start`, read a chunk at a time as the client takes them.
fn file_stream(
    mut file: std::fs::File,
    start: u64,
    length: u64,
) -> Result<impl Stream<Item = std::io::Result<Bytes>>, ApiError> {
    file.seek(SeekFrom::Start(start))
        .map_err(|e| ApiError::unavailable(&e))?;
    let reader = tokio::fs::File::from_std(file).take(length);
    Ok(futures_util::stream::unfold(
        Some(reader),
        |reader| async move {
            let mut reader = reader?;
            let mut buf = vec![0u8; DOWNLOAD_CHUNK];
            match reader.read(&mut buf).await {
                Ok(0) => None,
                Ok(n) => {
                    buf.truncate(n);
                    Some((Ok(Bytes::from(buf)), Some(reader)))
                }
                // The response ends after the error.
                Err(e) => Some((Err(e), None)),
            }
        },
    ))
}

/// `PUT /api/docs/{id}/content?base=<id>`: the seat holder saves the file as the document's
/// next revision, made from revision `base` (none if the document has none yet). The request
/// carries the seat token in `X-ShareCraft-Seat`.
///
/// The role, and with the file session the seat, the token and the base revision, are checked
/// before the body is read, so a save that would be refused stores nothing. The body streams
/// into the content store, and a reference to it is handed to the document's file session and
/// nothing else, which checks the save again; the save finishes even if the caller goes away.
/// A save refused after its body was stored has that content erased, unless something else
/// names it.
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
    if let Some(refused) = state
        .rooms()
        .check_save(doc, user, &token, base.clone())
        .await
    {
        return Ok(save_answer(refused));
    }
    let blob = body_blob(&state, request).await?;

    let reply = {
        let state = state.clone();
        to_completion(async move {
            let reply = state.rooms().save_file(doc, user, &token, base, blob).await;
            // Saved or unchanged, a version names the content.
            if !matches!(reply, SaveReply::Saved(_) | SaveReply::Unchanged(_)) {
                discard(&state, blob).await;
            }
            Ok(reply)
        })
        .await?
    };
    Ok(save_answer(reply))
}

/// The answer to a save the file session answered with `reply`.
fn save_answer(reply: SaveReply) -> Response {
    match reply {
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
    }
}

/// The answer to a save that stored (or found equal) `revision`.
fn saved(revision: &RevisionId, changed: bool) -> Response {
    Json(json!({ "revision": revision.to_string(), "changed": changed })).into_response()
}

/// `POST /api/docs/upload?parent=<id>&name=<file name>`: creates a document from the bytes
/// sent in the library or folder `parent` (of which the caller must be an editor), with them as
/// its first revision. The document inherits its access from there. The editor is the one
/// whose extension ends the name. The caller's right to add there and the name are checked
/// before the body is read (and again when the document is made); the bytes then stream into
/// the content store, and are claimed for the revision before it is committed. Content stored
/// for an upload that is then refused, or fails, is erased unless something else names it. If they cannot be stored after the
/// document was made, the document stays, empty, and the answer is `503` naming it.
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
    let user = signed.session.user;
    match tree::check_create_document(&state, &user, parent, &name, editor).await {
        Ok(()) => {}
        // The name is only an extension.
        Err(AccessError::WrongExtension) => return Err(ApiError::BadRequest),
        Err(e) => return Err(e.into()),
    }
    let blob = body_blob(&state, request).await?;
    if blob.size == 0 {
        discard(&state, blob).await;
        return Err(ApiError::BadRequest);
    }

    to_completion(async move {
        let id = DocId(Uuid::new_v4());
        let created =
            tree::create_document(&state, &user, parent, id, &name, editor, Some(blob.size)).await;
        let document = match created {
            Ok(document) => document,
            Err(e) => {
                discard(&state, blob).await;
                return Err(match e {
                    // The name is only an extension.
                    AccessError::WrongExtension => ApiError::BadRequest,
                    e => e.into(),
                });
            }
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
        let written = write_claimed(
            state.blobs(),
            Some(state.db()),
            Arc::clone(state.checkpointer()),
            id,
            blob,
            move |store| {
                store.write(&Checkpoint {
                    doc: id,
                    file_name: &file_name,
                    blob,
                    base: None,
                    authors: std::slice::from_ref(&author),
                    fields: &Map::new(),
                })
            },
        )
        .await;
        let revision = match written {
            Ok(Outcome::Written(revision) | Outcome::Unchanged(revision)) => revision,
            Err(e) => {
                eprintln!(
                    "sharecraft-server: upload of document {} failed: {e:?}",
                    id.0
                );
                discard(&state, blob).await;
                // The document stays, empty.
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
