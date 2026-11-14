//! Resumable uploads: a file sent a chunk at a time, checked against its declared size and
//! SHA-256, then made into a new file or a new version of a file document.
//!
//! - `POST /api/uploads {parent | doc, name, size, sha256, base?, on_conflict?}` starts a
//!   session: a new file named `name` in the library or folder `parent` (needs `add`), or a new
//!   version of the file document `doc` made from its revision `base` (needs `edit`; a
//!   plain-text document is refused as `text_document`, and while someone holds the editing
//!   seat as `being_edited`). A new file's name is trimmed and its refused characters become
//!   `_`; the answer then says so (`renamed {from, to}`). The size is checked against the
//!   largest file allowed and reserved against the site's quota while the session is open
//!   (`507 quota_exceeded` when it has no room), and a person has at most
//!   [`records::UPLOADS_PER_PERSON`] sessions open (`too_many_uploads`). `on_conflict`
//!   (`replace`, `keep_both` or `skip`) says now what to do if the name is taken at the finish.
//! - `PUT /api/uploads/{id}` with `Content-Range: bytes <start>-<end>/<size>` appends one
//!   chunk at the number of bytes received so far (otherwise `409 wrong_offset {expected}`),
//!   hashing it on the way. A chunk whose body ends early or breaks keeps what arrived (the
//!   answer, if the client is still there, is `400 incomplete_chunk {received}`).
//! - `GET /api/uploads/{id}` answers how many bytes were received.
//! - `POST /api/uploads/{id}/finish` checks the bytes against the declared size and SHA-256
//!   (`422 checksum_mismatch` ends the session and creates nothing), checks the permission
//!   again, moves the part file into the content store and claims it, then creates the file or
//!   saves the new version through the document's file session, exactly as an editor's save.
//!   A name now taken is `409 name_taken {suggested}` until a choice is made, at the start or
//!   as `?on_conflict=` here: `replace` saves a new version of the file there (needs `edit`;
//!   `409 being_edited` while someone holds its seat), `keep_both` takes the suggested name,
//!   `skip` ends the session and makes nothing (`200 {skipped: true}`). The site must still
//!   have room for the bytes (`507 quota_exceeded`, the session left open): the reservation
//!   becomes usage once they are committed and is given back if they are not.
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
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path as UrlPath, Query, Request, State};
use axum::http::header::{CONTENT_LENGTH, CONTENT_RANGE};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use collab_core::ids::{DocId, NodeId};
use futures_util::StreamExt;
use records::{
    AccessError, Editor, NameConflict, NewEvent, NewUpload, NodeKind, QuotaError, Reservation,
    UploadError, UploadSession, UploadTarget, User,
};
use revisions::{BlobError, BlobRef, RevisionId, parse_hex};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use tokio::sync::mpsc;
use uuid::Uuid;

use super::content::{Created, create_uploaded, discard, query_of, save_answer};
use super::{ApiError, JsonBody, parse_id, to_completion};
use crate::auth::Signed;
use crate::blobs::{ChannelReader, Held, release};
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
    /// For a new file, what to do if its name is taken at the finish.
    on_conflict: Option<String>,
}

/// Query of `POST /api/uploads/{id}/finish`.
#[derive(Deserialize)]
pub(crate) struct FinishQuery {
    /// What to do if the new file's name is taken: `replace`, `keep_both` or `skip`.
    on_conflict: Option<String>,
}

/// The choice named by `text`, if given: `422 invalid_request` for an unknown one.
pub(super) fn conflict_choice(text: Option<&str>) -> Result<Option<NameConflict>, ApiError> {
    text.map(|text| NameConflict::parse(text).ok_or(ApiError::InvalidRequest))
        .transpose()
}

/// A new file's name as asked for, trimmed, made acceptable: refused and control characters
/// become `_` (see [`tree::clean_name`]). With the editor its extension opens it in, and the
/// name asked for (trimmed) if it had to change. `400` for a name with no extension any editor
/// takes, or that is only an extension.
pub(super) fn clean_upload_name(asked: &str) -> Result<(String, Editor, Option<String>), ApiError> {
    let asked = asked.trim();
    let editor = Editor::for_file_name(asked).ok_or(ApiError::BadRequest)?;
    // A name that is only an extension names nothing.
    if editor
        .native_extension()
        .is_some_and(|ext| asked.len() <= ext.len())
    {
        return Err(ApiError::BadRequest);
    }
    let name = tree::clean_name(asked, editor);
    let editor = Editor::for_file_name(&name).ok_or(ApiError::BadRequest)?;
    let renamed = (name != asked).then(|| asked.to_owned());
    Ok((name, editor, renamed))
}

/// Adds `renamed {from, to}` to the answer `body` (an object), `null` unless the name `from`
/// had to change to `to`.
pub(super) fn add_renamed(body: &mut serde_json::Value, from: Option<&str>, to: &str) {
    let renamed = match from {
        Some(from) if from != to => json!({ "from": from, "to": to }),
        _ => serde_json::Value::Null,
    };
    if let Some(fields) = body.as_object_mut() {
        fields.insert("renamed".to_owned(), renamed);
    }
}

/// Where a new file goes once its name is settled (see [`settle_name`]).
pub(super) enum Named {
    /// A new file named `name`; `renamed` is the name it had before, if it had to change.
    New {
        name: String,
        editor: Editor,
        renamed: Option<String>,
    },
    /// A new version of the file `doc` there (named `name`), made from `base`.
    Replace {
        doc: DocId,
        name: String,
        base: Option<RevisionId>,
    },
    /// The name is taken and nothing was chosen: `409 name_taken {suggested}`.
    Taken { suggested: String },
    /// The name is taken and the person chose to skip it.
    Skip,
    /// The file there would not take a new version now: the answer to give.
    Refused(SaveReply),
}

impl Named {
    /// The answer for `Taken` or `Skip`.
    pub(super) fn answer(&self) -> Option<Response> {
        match self {
            Named::Taken { suggested } => Some(
                (
                    StatusCode::CONFLICT,
                    Json(json!({ "error": "name_taken", "suggested": suggested })),
                )
                    .into_response(),
            ),
            Named::Skip => Some(Json(json!({ "skipped": true })).into_response()),
            Named::Refused(reply) => Some(save_answer(reply.clone())),
            Named::New { .. } | Named::Replace { .. } => None,
        }
    }
}

/// Settles where a new file named `name` (already cleaned), opened in `editor`, goes in
/// `parent` for `user`: as named if the name is free there; if it is taken, as `choice` says
/// (`keep_both` takes the first free name `name (2)`, `name (3)` …; `replace` a new version of
/// the file there, once `user` is found allowed to save one and its file session would take
/// it now), or nothing chosen. `404`, `403`, `wrong_place` and the like as for creating it.
pub(super) async fn settle_name(
    state: &AppState,
    user: &User,
    parent: NodeId,
    name: &str,
    editor: Editor,
    choice: Option<NameConflict>,
) -> Result<Named, ApiError> {
    match tree::check_create_document(state, user, parent, name, editor).await {
        Ok(()) => {
            return Ok(Named::New {
                name: name.to_owned(),
                editor,
                renamed: None,
            });
        }
        Err(AccessError::NameTaken) => {}
        // The name is only an extension.
        Err(AccessError::WrongExtension) => return Err(ApiError::BadRequest),
        Err(e) => return Err(e.into()),
    }
    let suggested = || async {
        tree::suggest_name(state.db(), parent, name, editor)
            .await
            .map_err(|e| ApiError::unavailable(&e))
    };
    match choice {
        None => Ok(Named::Taken {
            suggested: suggested().await?,
        }),
        Some(NameConflict::Skip) => Ok(Named::Skip),
        Some(NameConflict::KeepBoth) => Ok(Named::New {
            name: suggested().await?,
            editor,
            renamed: Some(name.to_owned()),
        }),
        Some(NameConflict::Replace) => replace_target(state, user, parent, name, editor).await,
    }
}

/// The file named `name` in `parent` that a `replace` saves a new version of: it must be a
/// file document (`wrong_place` for a folder, `text_document` for plain text) on which `user`
/// has `edit`, and its file session must take a new version now (otherwise `Refused` with its
/// answer, such as `being_edited`). The version is made from the revision the file session
/// holds. A name freed meanwhile makes a new file after all.
async fn replace_target(
    state: &AppState,
    user: &User,
    parent: NodeId,
    name: &str,
    editor: Editor,
) -> Result<Named, ApiError> {
    let db = state.db();
    let Some(node) = db
        .live_child_named(parent, name)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
    else {
        return Ok(Named::New {
            name: name.to_owned(),
            editor,
            renamed: None,
        });
    };
    if node.kind != NodeKind::Document {
        return Err(ApiError::WrongPlace);
    }
    let doc = DocId(node.id.0);
    let name = check_new_version(state, user, doc).await?;
    let mut base = db
        .document(doc)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
        .and_then(|document| document.revision)
        .and_then(|text| RevisionId::from_str(&text).ok());
    // The recorded revision may lag the file session's; the session's own is the one to
    // replace.
    for _ in 0..SESSION_ATTEMPTS {
        match ask_check(state, doc, &name, base.clone()).await? {
            None => return Ok(Named::Replace { doc, name, base }),
            Some(SaveReply::Conflict { current }) => base = current,
            Some(refused) => return Ok(Named::Refused(refused)),
        }
    }
    Ok(Named::Refused(SaveReply::Busy))
}

/// Saves the content `held` as a new version of `doc` (named `name`) made from `base`, for
/// `user`, through the document's file session, opened again if it ends before answering.
/// Content not saved is erased unless something else names it.
pub(super) async fn store_version(
    state: &AppState,
    user: &User,
    doc: DocId,
    name: &str,
    base: Option<RevisionId>,
    held: Held,
) -> SaveReply {
    let blob = held.blob();
    // A session that ended before taking the upload wrote nothing for it; it is opened again.
    let mut reply = SaveReply::Failed;
    for _ in 0..SESSION_ATTEMPTS {
        let link = session_of(state, doc, name);
        if let Ok(answer) =
            upload_version(&link, user.id, user.name.clone(), base.clone(), blob).await
        {
            reply = answer;
            break;
        }
    }
    match &reply {
        // Saved, or a version already names the content.
        SaveReply::Saved(_) | SaveReply::Unchanged(_) => {}
        _ => discard(state, held).await,
    }
    reply
}

/// `POST /api/uploads`: starts an upload session (see the module documentation). `201` with
/// `{id, name, size, received, parent, doc, base, on_conflict, renamed}`.
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
    let on_conflict = conflict_choice(body.on_conflict.as_deref())?;
    let mut renamed = None;
    let db = state.db();
    let (target, name, base, site) = match (body.parent, body.doc) {
        (Some(parent), None) => {
            let parent = NodeId(parse_id(&parent)?);
            let asked = body.name.ok_or(ApiError::BadRequest)?;
            if body.base.is_some() {
                return Err(ApiError::BadRequest);
            }
            let (name, editor, from) = clean_upload_name(&asked)?;
            renamed = from;
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
            on_conflict: on_conflict.filter(|_| matches!(target, UploadTarget::NewFile { .. })),
        })
        .await
        .map_err(|e| match e {
            UploadError::TooMany => ApiError::TooManyUploads,
            UploadError::QuotaExceeded => ApiError::QuotaExceeded,
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
    let mut out = describe(&session);
    add_renamed(&mut out, renamed.as_deref(), &session.name);
    Ok((StatusCode::CREATED, Json(out)).into_response())
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
        "on_conflict": session.on_conflict.map(NameConflict::as_str),
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
/// base is no longer current). Each of these leaves the session as it was, except a stale base,
/// which can never finish and ends it. A part file gone missing ends the session (`503`). Then
/// the part file becomes stored content and the session ends; a new file is created with it as
/// its first revision (`201`, as `POST /api/docs/upload` answers), and a new version is saved
/// through the file session (`200 {revision, changed}`). Content refused or failed from there
/// is erased unless something else names it. Runs to the end even if the caller goes away.
pub(crate) async fn finish(
    State(state): State<AppState>,
    signed: Signed,
    UrlPath(id): UrlPath<String>,
    query: Result<Query<FinishQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    let id = parse_id(&id)?;
    let choice = conflict_choice(query_of(query)?.on_conflict.as_deref())?;
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
                let hashed = tokio::task::spawn_blocking(move || {
                    let mut file = File::open(&part)?;
                    hash_prefix(&mut file, size)
                })
                .await
                .map_err(|e| ApiError::unavailable(&e))?;
                match hashed {
                    Ok(hasher) => hasher,
                    Err(e) if e.kind() == ErrorKind::NotFound => {
                        // Its bytes are gone: it can never finish.
                        eprintln!("sharecraft-server: the part file of an upload is missing");
                        end_session(&state, id).await?;
                        return Err(ApiError::Unavailable);
                    }
                    Err(e) => return Err(ApiError::unavailable(&e)),
                }
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
                let choice = choice.or(session.on_conflict);
                finish_new_file(&state, &user, &session, parent, blob, choice).await
            }
            UploadTarget::NewVersion { doc } => {
                finish_new_version(&state, &user, &session, doc, blob).await
            }
        }
    })
    .await
}

/// Ends the session for its finish, keeping its declared size reserved (refused with
/// `507 quota_exceeded`, the session left open, when the site has no room for it now), then
/// moves its part file into the content store as `blob`, held. The caller holds its turn and
/// has checked the bytes, and commits or releases the reservation answered. A part file that
/// cannot be moved (gone missing, or a failure of the store) gives the reservation back; the
/// session has ended either way.
async fn adopt(
    state: &AppState,
    session: &UploadSession,
    blob: BlobRef,
) -> Result<(Held, Reservation), ApiError> {
    let reservation = match state.db().claim_upload(session.id).await {
        Ok(Some(reservation)) => reservation,
        Ok(None) => return Err(ApiError::NotFound),
        Err(QuotaError::Exceeded) => return Err(ApiError::QuotaExceeded),
        Err(QuotaError::Db(e)) => return Err(ApiError::unavailable(&e)),
    };
    let part = state.uploads().part(session.id);
    let adopted = state.blobs().adopt_part(part, blob).await;
    state.uploads().forget(session.id);
    match adopted {
        Ok(held) => Ok((held, reservation)),
        Err(e) => {
            release(state.db(), reservation).await;
            remove_part(state, session.id).await;
            match e {
                BlobError::Io(e) if e.kind() == ErrorKind::NotFound => {
                    eprintln!("sharecraft-server: the part file of an upload is missing");
                    Err(ApiError::Unavailable)
                }
                e => Err(ApiError::unavailable(&e)),
            }
        }
    }
}

/// Finishes a session making a new file in `parent` with the content `blob`, its name settled
/// as `choice` says when it is taken (see [`settle_name`]).
async fn finish_new_file(
    state: &AppState,
    user: &User,
    session: &UploadSession,
    parent: NodeId,
    blob: BlobRef,
    choice: Option<NameConflict>,
) -> Result<Response, ApiError> {
    let editor = Editor::for_file_name(&session.name).ok_or(ApiError::BadRequest)?;
    let named = settle_name(state, user, parent, &session.name, editor, choice).await?;
    if matches!(named, Named::Skip) {
        end_session(state, session.id).await?;
    }
    if let Some(answer) = named.answer() {
        return Ok(answer);
    }
    let (name, editor, renamed) = match named {
        Named::New {
            name,
            editor,
            renamed,
        } => (name, editor, renamed),
        Named::Replace { doc, name, base } => {
            return save_upload(state, user, session, doc, &name, base, blob).await;
        }
        Named::Taken { .. } | Named::Skip | Named::Refused(_) => {
            return Err(ApiError::Unavailable);
        }
    };
    let (held, reservation) = adopt(state, session, blob).await?;
    let created = create_uploaded(state, user, parent, &name, editor, held).await;
    // A version written was counted with its commit.
    release(state.db(), reservation).await;
    Ok(match created? {
        Created::Done { doc, mut body } => {
            completed(state, user, doc, session, false);
            add_renamed(&mut body, renamed.as_deref(), &name);
            (StatusCode::CREATED, Json(body)).into_response()
        }
        Created::Failed(response) => response,
    })
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
        // A stale base can never finish: the session ends.
        if matches!(refused, SaveReply::Conflict { .. }) {
            end_session(state, session.id).await?;
        }
        return Ok(save_answer(refused));
    }
    save_upload(state, user, session, doc, &name, base, blob).await
}

/// Saves the session's content `blob` as a new version of `doc` (named `name`) made from
/// `base`, through the document's file session, once its bytes are claimed against the quota.
async fn save_upload(
    state: &AppState,
    user: &User,
    session: &UploadSession,
    doc: DocId,
    name: &str,
    base: Option<RevisionId>,
    blob: BlobRef,
) -> Result<Response, ApiError> {
    let (held, reservation) = adopt(state, session, blob).await?;
    let reply = store_version(state, user, doc, name, base, held).await;
    // A version written was counted with its commit.
    release(state.db(), reservation).await;
    if matches!(reply, SaveReply::Saved(_)) {
        completed(state, user, doc, session, true);
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
