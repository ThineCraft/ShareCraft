//! Resumable uploads: a file sent a chunk at a time, checked against its declared size and
//! SHA-256, then made into a new file or a new version of a file document.
//!
//! - `POST /api/uploads {parent | doc, name, size, sha256, base?}` starts a session: a new file
//!   named `name` in the library or folder `parent` (needs `add`), or a new version of the file
//!   document `doc` made from its revision `base` (needs `edit`; a plain-text document is
//!   refused as `text_document`, and while someone holds the editing seat as `being_edited`).
//!   The size is checked against the largest file allowed, and a person has at most
//!   [`records::UPLOADS_PER_PERSON`] sessions open (`too_many_uploads`).
//! - `PUT /api/uploads/{id}` with `Content-Range: bytes <start>-<end>/<size>` appends one
//!   chunk at the number of bytes received so far (otherwise `409 wrong_offset {expected}`),
//!   hashing it on the way. A chunk whose body ends early or breaks keeps what arrived (the
//!   answer, if the client is still there, is `400 incomplete_chunk {received}`).
//! - `GET /api/uploads/{id}` answers how many bytes were received.
//! - `POST /api/uploads/{id}/finish` checks the bytes against the declared size and SHA-256
//!   (`422 checksum_mismatch` ends the session and creates nothing), checks the permission
//!   again, moves the part file into the content store and claims it, then creates the file or
//!   saves the new version through the document's file session, exactly as an editor's save.
//!   A name now taken is `409 name_taken {suggested}`.
//! - `DELETE /api/uploads/{id}` abandons the session.
//!
//! A session is its owner's alone: anyone else is answered `404`. Its bytes go to its part file
//! `<data>/uploads/<id>.part`, on the same volume as the content store, so finishing renames
//! it into place. The running SHA-256 of what was received is kept in memory; after a restart it
//! is computed again from the part file, at the next chunk or at the finish. Every request on a
//! session takes its turn on a lock of the session's own, which the sweeper also takes (without
//! waiting: a busy session is skipped) before it removes a session that has received nothing
//! for a day, so a part file is never removed while a chunk is being written to it. A chunk
//! whose body sends nothing for `upload_idle` (a minute) is cut off, keeping what arrived.

use std::collections::HashMap;
use std::fs::File;
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::SystemTime;

use axum::Json;
use axum::extract::{Path as UrlPath, Request, State};
use axum::http::header::{CONTENT_LENGTH, CONTENT_RANGE};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use collab_core::ids::{DocId, NodeId};
use futures_util::StreamExt;
use records::{
    AccessError, Editor, NewEvent, NewUpload, UploadError, UploadSession, UploadTarget, User,
};
use revisions::{BlobRef, RevisionId, parse_hex};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use tokio::sync::mpsc;
use uuid::Uuid;

use super::content::{Created, create_uploaded, discard, save_answer};
use super::{ApiError, JsonBody, parse_id, to_completion};
use crate::auth::Signed;
use crate::blobs::ChannelReader;
use crate::file_room::{FileLink, SaveReply, check_upload, upload_version};
use crate::{AppState, tree};

/// A session that has received nothing for this long is swept.
const UPLOAD_LIFETIME: time::Duration = time::Duration::hours(24);
/// A part file with no session, untouched for this long, is removed by the sweeper.
const ORPHAN_AGE: std::time::Duration = std::time::Duration::from_secs(60 * 60);
/// How much of a part file is read at a time to hash it again.
const HASH_CHUNK: usize = 64 * 1024;
/// Body chunks queued between the request and the blocking task writing them.
const BODY_QUEUE: usize = 2;
/// How many times a file session that ends before answering is opened again.
const SESSION_ATTEMPTS: usize = 3;

/// The upload sessions' part files: `uploads` under `data_dir`.
#[must_use]
pub fn uploads_path(data_dir: &Path) -> PathBuf {
    data_dir.join("uploads")
}

/// The server's side of the upload sessions: where their part files are, and for each session
/// a lock that orders its requests and holds the running hash of what it received.
pub(crate) struct Uploads {
    dir: PathBuf,
    sessions: Mutex<HashMap<Uuid, Arc<tokio::sync::Mutex<Progress>>>>,
}

/// What this process knows of a session's bytes.
#[derive(Default)]
struct Progress {
    /// The hash of the first `.0` bytes of the part file, if computed since the server started.
    hashed: Option<(u64, Sha256)>,
}

impl Progress {
    /// The running hash, if it covers exactly the first `received` bytes.
    fn take_at(&mut self, received: u64) -> Option<Sha256> {
        match self.hashed.take() {
            Some((at, hasher)) if at == received => Some(hasher),
            _ => None,
        }
    }
}

impl Uploads {
    /// The sessions whose part files are under `dir`, none of them locked yet.
    pub(crate) fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// The part file of session `id`.
    fn part(&self, id: Uuid) -> PathBuf {
        self.dir.join(format!("{}.part", id.simple()))
    }

    /// The lock of session `id`, made on first use.
    fn turn(&self, id: Uuid) -> Arc<tokio::sync::Mutex<Progress>> {
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(sessions.entry(id).or_default())
    }

    /// Forgets session `id`, once it has ended.
    fn forget(&self, id: Uuid) {
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        sessions.remove(&id);
    }
}

/// Body of `POST /api/uploads`.
#[derive(Deserialize)]
pub(crate) struct StartBody {
    /// The library or folder a new file goes into.
    parent: Option<String>,
    /// The file document a new version is for.
    doc: Option<String>,
    /// The new file's name; ignored for a new version, which keeps the document's.
    name: Option<String>,
    size: u64,
    /// 64 hexadecimal digits.
    sha256: String,
    /// The revision a new version was made from; none if the document has none.
    base: Option<String>,
}

/// `POST /api/uploads`: starts an upload session (see the module documentation). `201` with
/// `{id, name, size, received}`.
pub(crate) async fn start(
    State(state): State<AppState>,
    signed: Signed,
    JsonBody(body): JsonBody<StartBody>,
) -> Result<Response, ApiError> {
    let user = signed.session.user;
    if body.size == 0 {
        return Err(ApiError::InvalidRequest);
    }
    if body.size > state.config().max_file_bytes {
        return Err(ApiError::TooLarge);
    }
    let sha256 = parse_hex(&body.sha256.to_ascii_lowercase()).ok_or(ApiError::InvalidRequest)?;
    let db = state.db();
    let (target, name, base, site) = match (body.parent, body.doc) {
        (Some(parent), None) => {
            let parent = NodeId(parse_id(&parent)?);
            let name = body.name.ok_or(ApiError::BadRequest)?;
            if body.base.is_some() {
                return Err(ApiError::BadRequest);
            }
            let editor = Editor::for_file_name(name.trim()).ok_or(ApiError::BadRequest)?;
            match tree::check_create_document(&state, &user, parent, &name, editor).await {
                // A name taken now is settled when the upload finishes.
                Ok(()) | Err(AccessError::NameTaken) => {}
                // The name is only an extension.
                Err(AccessError::WrongExtension) => return Err(ApiError::BadRequest),
                Err(e) => return Err(e.into()),
            }
            let site = db
                .node(parent)
                .await
                .map_err(|e| ApiError::unavailable(&e))?
                .ok_or(ApiError::NotFound)?
                .site;
            (UploadTarget::NewFile { parent }, name, None, site)
        }
        (None, Some(doc)) => {
            let doc = DocId(parse_id(&doc)?);
            let base = match &body.base {
                Some(text) => Some(RevisionId::from_str(text).map_err(|_| ApiError::BadRequest)?),
                None => None,
            };
            let name = check_new_version(&state, &user, doc).await?;
            if let Some(refused) = ask_check(&state, doc, &name, base.clone()).await? {
                return Ok(save_answer(refused));
            }
            let site = db
                .node(doc.into())
                .await
                .map_err(|e| ApiError::unavailable(&e))?
                .ok_or(ApiError::NotFound)?
                .site;
            (UploadTarget::NewVersion { doc }, name, base, site)
        }
        _ => return Err(ApiError::BadRequest),
    };

    let id = Uuid::new_v4();
    let base_text = base.map(|b| b.to_string());
    let session = db
        .start_upload(NewUpload {
            id,
            owner: user.id,
            site,
            target,
            name: &name,
            size: body.size,
            sha256,
            base: base_text.as_deref(),
        })
        .await
        .map_err(|e| match e {
            UploadError::TooMany => ApiError::TooManyUploads,
            UploadError::Db(e) => ApiError::unavailable(&e),
        })?;
    let uploads = Arc::clone(state.uploads());
    let part = uploads.part(id);
    let made = tokio::task::spawn_blocking(move || {
        std::fs::create_dir_all(&uploads.dir)?;
        File::create(&part).map(drop)
    })
    .await;
    if !matches!(made, Ok(Ok(()))) {
        eprintln!("sharecraft-server: cannot create the part file of an upload");
        if let Err(e) = db.end_upload(id).await {
            eprintln!("sharecraft-server: database failure: {e}");
        }
        return Err(ApiError::Unavailable);
    }
    Ok((StatusCode::CREATED, Json(describe(&session))).into_response())
}

/// The session as the API describes it.
fn describe(session: &UploadSession) -> serde_json::Value {
    let (parent, doc) = match session.target {
        UploadTarget::NewFile { parent } => (Some(parent.0.to_string()), None),
        UploadTarget::NewVersion { doc } => (None, Some(doc.0.to_string())),
    };
    json!({
        "id": session.id.to_string(),
        "name": session.name,
        "size": session.size,
        "received": session.received,
        "parent": parent,
        "doc": doc,
        "base": session.base,
    })
}

/// The name of the file document `doc`, once `user` is found to be allowed to upload a new
/// version of it: `404` without a role on it, `400 text_document` for a plain-text document,
/// `403` without `edit`.
async fn check_new_version(state: &AppState, user: &User, doc: DocId) -> Result<String, ApiError> {
    let db = state.db();
    let access = db
        .doc_access(user.id, doc)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
        .ok_or(ApiError::NotFound)?;
    let document = db
        .document(doc)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
        .ok_or(ApiError::NotFound)?;
    if !document.editor.is_file() {
        return Err(ApiError::TextDocument);
    }
    if !access.can_edit() {
        return Err(ApiError::Forbidden);
    }
    Ok(document.name)
}

/// The file session of `doc` (named `name`), opened if needed.
fn session_of(state: &AppState, doc: DocId, name: &str) -> FileLink {
    state.open_file_room(doc, name.to_owned())
}

/// Asks `doc`'s file session whether it would take a new version made from `base` now (see
/// [`check_upload`]), opening it again if it ends before answering.
async fn ask_check(
    state: &AppState,
    doc: DocId,
    name: &str,
    base: Option<RevisionId>,
) -> Result<Option<SaveReply>, ApiError> {
    for _ in 0..SESSION_ATTEMPTS {
        // A session that ended meanwhile is opened again.
        if let Ok(answer) = check_upload(&session_of(state, doc, name), base.clone()).await {
            return Ok(answer);
        }
    }
    Err(ApiError::Unavailable)
}

/// Session `id` if it belongs to `user`; `404` otherwise, whoever it belongs to.
async fn owned(state: &AppState, id: Uuid, user: &User) -> Result<UploadSession, ApiError> {
    state
        .db()
        .upload_session(id)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
        .filter(|session| session.owner == user.id)
        .ok_or(ApiError::NotFound)
}

/// `GET /api/uploads/{id}`: the session, with how many bytes were received.
pub(crate) async fn show(
    State(state): State<AppState>,
    signed: Signed,
    UrlPath(id): UrlPath<String>,
) -> Result<Response, ApiError> {
    let session = owned(&state, parse_id(&id)?, &signed.session.user).await?;
    Ok(Json(describe(&session)).into_response())
}

/// `DELETE /api/uploads/{id}`: abandons the session and removes its part file. `204`.
pub(crate) async fn abandon(
    State(state): State<AppState>,
    signed: Signed,
    UrlPath(id): UrlPath<String>,
) -> Result<Response, ApiError> {
    let id = parse_id(&id)?;
    let user = signed.session.user;
    owned(&state, id, &user).await?;
    to_completion(async move {
        let turn = state.uploads().turn(id);
        let _turn = turn.lock().await;
        // Gone meanwhile (finished or swept): as if never there.
        owned(&state, id, &user).await?;
        end_session(&state, id).await?;
        Ok(StatusCode::NO_CONTENT.into_response())
    })
    .await
}

/// Ends session `id`: its row, then its part file, then what this process knows of it. The
/// caller holds its turn.
async fn end_session(state: &AppState, id: Uuid) -> Result<(), ApiError> {
    state
        .db()
        .end_upload(id)
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
    remove_part(state, id).await;
    state.uploads().forget(id);
    Ok(())
}

/// Removes session `id`'s part file, if there is one. A failure is logged; the sweeper removes
/// part files left without a session.
async fn remove_part(state: &AppState, id: Uuid) {
    let part = state.uploads().part(id);
    let removed = tokio::task::spawn_blocking(move || match std::fs::remove_file(&part) {
        Err(e) if e.kind() != ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    })
    .await;
    match removed {
        Ok(Ok(())) => {}
        Ok(Err(e)) => eprintln!("sharecraft-server: cannot remove an upload's part file: {e}"),
        Err(e) => eprintln!("sharecraft-server: part file task failed: {e}"),
    }
}

/// A chunk's place in the whole file, from `Content-Range: bytes <start>-<end>/<size>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ChunkRange {
    start: u64,
    /// The last byte, included.
    end: u64,
    size: u64,
}

impl ChunkRange {
    fn len(self) -> u64 {
        self.end - self.start + 1
    }
}

/// The range a `Content-Range` header value names, if it is one well-formed range of bytes.
fn chunk_range(text: &str) -> Option<ChunkRange> {
    let number = |text: &str| -> Option<u64> {
        if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        text.parse().ok()
    };
    let spec = text.trim().strip_prefix("bytes ")?;
    let (span, size) = spec.split_once('/')?;
    let (start, end) = span.split_once('-')?;
    let range = ChunkRange {
        start: number(start.trim())?,
        end: number(end.trim())?,
        size: number(size.trim())?,
    };
    (range.start <= range.end && range.end < range.size).then_some(range)
}

/// `PUT /api/uploads/{id}`: appends the chunk the body holds, at the place its
/// `Content-Range` names, which must be the number of bytes received so far (otherwise
/// `409 wrong_offset {expected}`). The range must lie within the declared size and match the
/// body's `Content-Length`, if given (otherwise `400 invalid_range`). `200 {received}`.
///
/// The bytes stream into the part file while they are hashed, and are flushed before the
/// session records them. A body that ends before the range does, or breaks, keeps what
/// arrived: `400 incomplete_chunk {received}`; so does a body that sends nothing for the
/// configured `upload_idle`. A body longer than its range keeps nothing of
/// it: `400 invalid_range {received}`. The chunk is written to the end even if the caller goes
/// away.
pub(crate) async fn chunk(
    State(state): State<AppState>,
    signed: Signed,
    UrlPath(id): UrlPath<String>,
    headers: HeaderMap,
    request: Request,
) -> Result<Response, ApiError> {
    let id = parse_id(&id)?;
    let user = signed.session.user;
    let session = owned(&state, id, &user).await?;
    let range = headers
        .get(CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(chunk_range)
        .filter(|range| range.size == session.size)
        .ok_or(ApiError::InvalidRange)?;
    let declared = headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim().parse::<u64>());
    if declared.is_some_and(|length| length != Ok(range.len())) {
        return Err(ApiError::InvalidRange);
    }
    let body = request.into_body();

    to_completion(async move {
        let turn = state.uploads().turn(id);
        let mut progress = turn.lock().await;
        let session = owned(&state, id, &user).await?;
        if range.start != session.received {
            return Ok(wrong_offset(session.received));
        }
        let received = session.received;
        let hashed = progress.take_at(received);
        let part = state.uploads().part(id);

        let (tx, rx) = mpsc::channel(BODY_QUEUE);
        let writer = tokio::task::spawn_blocking(move || {
            append(&part, received, hashed, range.len(), ChannelReader::new(rx))
        });
        let mut stream = body.into_data_stream();
        let idle = state.config().upload_idle;
        // A body that sends nothing for `idle` is cut off like a broken one, so a stalled
        // connection never holds the session's turn for ever.
        while let Ok(Some(frame)) = tokio::time::timeout(idle, stream.next()).await {
            let item = frame.map_err(|e| std::io::Error::new(ErrorKind::ConnectionAborted, e));
            let failed = item.is_err();
            if tx.send(item).await.is_err() || failed {
                break;
            }
        }
        drop(tx);
        let appended = match writer.await {
            Ok(Ok(appended)) => appended,
            Ok(Err(e)) => return Err(ApiError::unavailable(&e)),
            Err(e) => return Err(ApiError::unavailable(&e)),
        };
        if appended.overflow {
            progress.hashed = Some((received, appended.hasher));
            return Ok(chunk_refused("invalid_range", received));
        }
        let now = received + appended.written;
        if appended.written > 0 {
            match state.db().record_upload_progress(id, now).await {
                Ok(true) => {}
                // Abandoned meanwhile: impossible while holding the turn, but harmless.
                Ok(false) => return Err(ApiError::NotFound),
                // The part holds bytes the session does not count; the next chunk cuts them
                // off, and the hash is computed again.
                Err(e) => return Err(ApiError::unavailable(&e)),
            }
        }
        progress.hashed = Some((now, appended.hasher));
        if appended.written < range.len() {
            return Ok(chunk_refused("incomplete_chunk", now));
        }
        Ok(Json(json!({ "received": now })).into_response())
    })
    .await
}

/// `409 wrong_offset {expected}`: chunks go at the number of bytes received.
fn wrong_offset(expected: u64) -> Response {
    (
        StatusCode::CONFLICT,
        Json(json!({ "error": "wrong_offset", "expected": expected })),
    )
        .into_response()
}

/// `400 {error, received}` for a chunk not taken whole.
fn chunk_refused(error: &str, received: u64) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": error, "received": received })),
    )
        .into_response()
}

/// What appending a chunk did.
struct Appended {
    /// The chunk's bytes now written and flushed: all of them, unless the body ended early or
    /// broke, or none if it overflowed.
    written: u64,
    /// The hash of the part file's first `received + written` bytes.
    hasher: Sha256,
    /// More bytes came than the range named; nothing of the chunk was kept.
    overflow: bool,
}

/// Appends at most `length` bytes read from `body` to the part file at `part` after its first
/// `received` bytes (anything beyond them, left by a chunk that was never recorded, is cut off
/// first), hashing them on the way, and flushes them. `hashed` is the hash of the first
/// `received` bytes, if known; otherwise it is computed from the file. A read error from
/// `body` ends the chunk where it is. Blocks.
fn append(
    part: &Path,
    received: u64,
    hashed: Option<Sha256>,
    length: u64,
    mut body: impl Read,
) -> std::io::Result<Appended> {
    let mut file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(part)?;
    let hasher = match hashed {
        Some(hasher) => hasher,
        None => hash_prefix(&mut file, received)?,
    };
    file.set_len(received)?;
    file.seek(SeekFrom::Start(received))?;
    let before = hasher.clone();
    let mut hasher = hasher;
    let mut written: u64 = 0;
    let mut overflow = false;
    let mut buf = vec![0u8; HASH_CHUNK];
    loop {
        let n = match body.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            // The body broke: what arrived is kept.
            Err(_) => break,
        };
        let room = usize::try_from(length - written).unwrap_or(usize::MAX);
        let take = n.min(room);
        if take < n {
            overflow = true;
        }
        let bytes = buf.get(..take).unwrap_or_default();
        file.write_all(bytes)?;
        hasher.update(bytes);
        written += take as u64;
        if overflow {
            break;
        }
    }
    if overflow {
        file.set_len(received)?;
        file.sync_data()?;
        return Ok(Appended {
            written: 0,
            hasher: before,
            overflow: true,
        });
    }
    file.sync_data()?;
    Ok(Appended {
        written,
        hasher,
        overflow: false,
    })
}

/// The hash of the first `length` bytes of `file`. Fails if it is shorter.
fn hash_prefix(file: &mut File, length: u64) -> std::io::Result<Sha256> {
    file.seek(SeekFrom::Start(0))?;
    let mut hasher = Sha256::new();
    let mut reader = (&mut *file).take(length);
    let mut buf = vec![0u8; HASH_CHUNK];
    let mut read: u64 = 0;
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        hasher.update(buf.get(..n).unwrap_or_default());
        read += n as u64;
    }
    if read != length {
        return Err(std::io::Error::new(
            ErrorKind::UnexpectedEof,
            "an upload's part file is shorter than what it received",
        ));
    }
    Ok(hasher)
}

/// `POST /api/uploads/{id}/finish`: finishes the session (see the module documentation).
///
/// Before anything is stored: `409 incomplete {received}` unless every declared byte was
/// received; `422 checksum_mismatch` (ending the session) if the bytes are not the declared
/// SHA-256; the permission is checked again (`add` on the folder, `edit` on the document), a
/// new file's name must be free (`409 name_taken {suggested}`), and a new version is asked of
/// the file session (`409 being_edited` while someone holds the seat, `409 {current}` if the
/// base is no longer current). Each of these leaves the session as it was. Then the part file
/// becomes stored content and the session ends; a new file is created with it as its first
/// revision (`201`, as `POST /api/docs/upload` answers), and a new version is saved through
/// the file session (`200 {revision, changed}`). Content refused or failed from there is erased
/// unless something else names it. Runs to the end even if the caller goes away.
pub(crate) async fn finish(
    State(state): State<AppState>,
    signed: Signed,
    UrlPath(id): UrlPath<String>,
) -> Result<Response, ApiError> {
    let id = parse_id(&id)?;
    let user = signed.session.user;
    owned(&state, id, &user).await?;
    to_completion(async move {
        let turn = state.uploads().turn(id);
        let mut progress = turn.lock().await;
        let session = owned(&state, id, &user).await?;
        if session.received < session.size {
            return Ok((
                StatusCode::CONFLICT,
                Json(json!({ "error": "incomplete", "received": session.received })),
            )
                .into_response());
        }
        let hasher = match progress.take_at(session.size) {
            Some(hasher) => hasher,
            None => {
                let part = state.uploads().part(id);
                let size = session.size;
                tokio::task::spawn_blocking(move || {
                    let mut file = File::open(&part)?;
                    hash_prefix(&mut file, size)
                })
                .await
                .map_err(|e| ApiError::unavailable(&e))?
                .map_err(|e| ApiError::unavailable(&e))?
            }
        };
        let blob = BlobRef {
            sha256: hasher.clone().finalize().into(),
            size: session.size,
        };
        if blob.sha256 != session.sha256 {
            end_session(&state, id).await?;
            return Err(ApiError::ChecksumMismatch);
        }
        // Kept until the session ends, in case a check below refuses.
        progress.hashed = Some((session.size, hasher));

        match session.target {
            UploadTarget::NewFile { parent } => {
                finish_new_file(&state, &user, &session, parent, blob).await
            }
            UploadTarget::NewVersion { doc } => {
                finish_new_version(&state, &user, &session, doc, blob).await
            }
        }
    })
    .await
}

/// Moves the session's part file into the content store as `blob` and ends the session. The
/// caller holds its turn and has checked the bytes.
async fn adopt(state: &AppState, session: &UploadSession, blob: BlobRef) -> Result<(), ApiError> {
    let part = state.uploads().part(session.id);
    let blobs = Arc::clone(state.blobs());
    tokio::task::spawn_blocking(move || blobs.store().adopt_part(&part, blob))
        .await
        .map_err(|e| ApiError::unavailable(&e))?
        .map_err(|e| ApiError::unavailable(&e))?;
    end_session(state, session.id).await
}

/// Finishes a session making a new file in `parent` with the content `blob`.
async fn finish_new_file(
    state: &AppState,
    user: &User,
    session: &UploadSession,
    parent: NodeId,
    blob: BlobRef,
) -> Result<Response, ApiError> {
    let name = &session.name;
    let editor = Editor::for_file_name(name.trim()).ok_or(ApiError::BadRequest)?;
    match tree::check_create_document(state, user, parent, name, editor).await {
        Ok(()) => {}
        Err(AccessError::NameTaken) => {
            let suggested = tree::suggest_name(state.db(), parent, name.trim(), editor)
                .await
                .map_err(|e| ApiError::unavailable(&e))?;
            return Ok((
                StatusCode::CONFLICT,
                Json(json!({ "error": "name_taken", "suggested": suggested })),
            )
                .into_response());
        }
        Err(AccessError::WrongExtension) => return Err(ApiError::BadRequest),
        Err(e) => return Err(e.into()),
    }
    adopt(state, session, blob).await?;
    Ok(
        match create_uploaded(state, user, parent, name, editor, blob).await? {
            Created::Done { doc, body } => {
                completed(state, user, doc, session, false);
                (StatusCode::CREATED, Json(body)).into_response()
            }
            Created::Failed(response) => response,
        },
    )
}

/// Finishes a session making a new version of `doc` with the content `blob`, through the
/// document's file session.
async fn finish_new_version(
    state: &AppState,
    user: &User,
    session: &UploadSession,
    doc: DocId,
    blob: BlobRef,
) -> Result<Response, ApiError> {
    let name = check_new_version(state, user, doc).await?;
    let base = match &session.base {
        Some(text) => Some(RevisionId::from_str(text).map_err(|e| ApiError::unavailable(&e))?),
        None => None,
    };
    if let Some(refused) = ask_check(state, doc, &name, base.clone()).await? {
        return Ok(save_answer(refused));
    }
    adopt(state, session, blob).await?;
    // A session that ended before taking the upload wrote nothing for it; it is opened again.
    let mut reply = SaveReply::Failed;
    for _ in 0..SESSION_ATTEMPTS {
        let link = session_of(state, doc, &name);
        if let Ok(answer) =
            upload_version(&link, user.id, user.name.clone(), base.clone(), blob).await
        {
            reply = answer;
            break;
        }
    }
    match &reply {
        SaveReply::Saved(_) => completed(state, user, doc, session, true),
        // A version already names the content.
        SaveReply::Unchanged(_) => {}
        _ => discard(state, blob).await,
    }
    Ok(save_answer(reply))
}

/// Records `upload.completed` for the session that made `doc`, or a new version of it.
fn completed(state: &AppState, user: &User, doc: DocId, session: &UploadSession, version: bool) {
    state.events().emit(NewEvent {
        kind: "upload.completed",
        actor: Some(user.id),
        subject: None,
        document: Some(doc),
        node: Some(doc.into()),
        detail: json!({
            "upload": session.id.to_string(),
            "bytes": session.size,
            "new_version": version,
        }),
    });
}

/// Ends, as of `now`, every upload session that has received nothing for a day, and removes
/// part files left without a session for an hour. Each session is ended under its turn, so
/// never while a chunk is being written; a session whose turn is taken (a request under way)
/// is skipped, so the sweep never waits on a session. Failures are logged; the next sweep
/// tries again.
pub(crate) async fn sweep_uploads(state: &AppState, now: OffsetDateTime) {
    let db = state.db();
    let idle = match db.idle_uploads(now - UPLOAD_LIFETIME).await {
        Ok(idle) => idle,
        Err(e) => {
            eprintln!("sharecraft-server: database failure: {e}");
            return;
        }
    };
    let mut swept = 0usize;
    for id in idle {
        let turn = state.uploads().turn(id);
        // A session whose turn is taken has a request under way: it is not idle, and the
        // sweep never waits for it.
        let Ok(_turn) = turn.try_lock() else {
            continue;
        };
        match db.upload_session(id).await {
            Ok(Some(session)) if session.touched_at <= now - UPLOAD_LIFETIME => {
                if end_session(state, id).await.is_ok() {
                    swept += 1;
                }
            }
            Ok(Some(_)) => {}
            Ok(None) => state.uploads().forget(id),
            Err(e) => eprintln!("sharecraft-server: database failure: {e}"),
        }
    }
    if swept > 0 {
        println!("sharecraft-server: removed {swept} idle upload(s)");
    }
    remove_orphan_parts(state, now).await;
}

/// Removes the part files with no session that were last written an hour or more before
/// `now`, each under its session's turn.
async fn remove_orphan_parts(state: &AppState, now: OffsetDateTime) {
    let dir = state.uploads().dir.clone();
    let before = SystemTime::from(now) - ORPHAN_AGE;
    let listed = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<Uuid>> {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut old = Vec::new();
        for entry in entries {
            let path = entry?.path();
            let Some(id) = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_suffix(".part"))
                .and_then(|stem| Uuid::parse_str(stem).ok())
            else {
                continue;
            };
            if std::fs::metadata(&path)?.modified()? <= before {
                old.push(id);
            }
        }
        Ok(old)
    })
    .await;
    let old = match listed {
        Ok(Ok(old)) => old,
        Ok(Err(e)) => {
            eprintln!("sharecraft-server: cannot list the upload part files: {e}");
            return;
        }
        Err(e) => {
            eprintln!("sharecraft-server: part file task failed: {e}");
            return;
        }
    };
    for id in old {
        let turn = state.uploads().turn(id);
        // Busy: a request is under way, so it is no orphan for now.
        let Ok(_turn) = turn.try_lock() else {
            continue;
        };
        match state.db().upload_session(id).await {
            Ok(None) => {
                remove_part(state, id).await;
                state.uploads().forget(id);
            }
            Ok(Some(_)) => {}
            Err(e) => eprintln!("sharecraft-server: database failure: {e}"),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn content_ranges() {
        assert_eq!(
            chunk_range("bytes 0-9/20"),
            Some(ChunkRange {
                start: 0,
                end: 9,
                size: 20
            })
        );
        assert_eq!(chunk_range("bytes 10-19/20").unwrap().len(), 10);
        // Past the end, backwards, unknown size, or not bytes.
        assert_eq!(chunk_range("bytes 10-20/20"), None);
        assert_eq!(chunk_range("bytes 9-8/20"), None);
        assert_eq!(chunk_range("bytes 0-9/*"), None);
        assert_eq!(chunk_range("items 0-9/20"), None);
        assert_eq!(chunk_range("bytes -9/20"), None);
        assert_eq!(chunk_range("bytes +0-9/20"), None);
    }

    #[test]
    fn a_short_chunk_keeps_what_arrived_and_an_overflow_keeps_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let part = dir.path().join("p.part");
        let first = append(&part, 0, None, 4, &b"abcd"[..]).unwrap();
        assert_eq!((first.written, first.overflow), (4, false));
        // Too many bytes: nothing kept, the hash as before.
        let long = append(&part, 4, Some(first.hasher), 2, &b"efg"[..]).unwrap();
        assert!(long.overflow);
        assert_eq!(std::fs::read(&part).unwrap(), b"abcd");
        // Too few: what came is kept; the hash is computed again from the file.
        let short = append(&part, 4, None, 4, &b"ef"[..]).unwrap();
        assert_eq!(short.written, 2);
        assert_eq!(std::fs::read(&part).unwrap(), b"abcdef");
        let digest: [u8; 32] = short.hasher.finalize().into();
        assert_eq!(digest, BlobRef::of(b"abcdef").sha256);
        // Bytes beyond what was recorded are cut off before the next chunk.
        let again = append(&part, 4, None, 2, &b"xy"[..]).unwrap();
        assert_eq!(again.written, 2);
        assert_eq!(std::fs::read(&part).unwrap(), b"abcdxy");
    }
}
