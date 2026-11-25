//! The types every write to an external library shares, the guard that says where a change
//! goes, and the names a written copy gets; staging, the checkpointer of bucket files and the
//! write worker.
//!
//! A change accepted for an external library is staged: its content is pinned in the
//! external-content cache and a pending row names it ([`stage`]). From then on it is the file's
//! current revision, served from the cache. The write worker ([`write_due`]) writes each
//! pending row to the store once its content has passed the malware scan, holding the library's
//! source lock so no listing runs meanwhile. A file changed in the store since it was read is
//! kept, and the change becomes a named copy beside it; a new file whose name was taken
//! meanwhile is written under a numbered name. A failed write is tried again with a growing
//! wait; a refused one blocks the library until a write probe passes again.
//!
//! The store-specific code is in `s3_write.rs`, the only module that changes a bucket.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use collab_core::ids::{DocId, NodeId, UserId};
use records::{
    AccessError, Db, Editor, ExternalEntry, NewEvent, NewPendingWrite, Node, NodeKind,
    PendingWrite, Perms, ScanState, SourceItem, SourceKind, SourceLock, SourceMode, Tri, User,
    WriteState, object_tag,
};
use revisions::{
    BlobRef, Checkpoint, CheckpointError, Content, GitError, Meta, Outcome, RevisionId,
    RevisionInfo, Stored,
};
use serde_json::{Map, Value, json};
use time::{OffsetDateTime, UtcOffset};
use tokio::runtime::Handle;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::AppState;
use crate::api::ApiError;
use crate::api::content::{Created, discard};
use crate::blobs::Held;
use crate::checkpoint::Checkpointer;
use crate::s3::{PutBody, S3Error, S3Target};
use crate::s3_source::nameable_segment;
use crate::s3_write::{
    Moved, make_folder, move_item, object_key, probe_writes, write_beside, write_numbered,
    write_object,
};
use crate::tree::{self, TreeOp};

/// The store an external library's changes are written to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Store {
    S3(S3WriteTarget),
}

/// A read-and-write S3 library, as a write to it needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct S3WriteTarget {
    pub library: NodeId,
    /// Whether the store honours conditional writes, as the write probe last found.
    pub conditional: Tri,
}

/// Where a change to a node goes. Not `crate::room::Place`, the room registry's entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Place {
    /// Outside any connected library: the ordinary tree and content store.
    Ordinary,
    /// In a read-and-write library of an outside store: the change is staged and written there.
    External(Store),
}

/// Where a change to `node` by `user` goes, read afresh (never cached).
///
/// Outside a connected library: [`Place::Ordinary`], whoever asks (the handler judges access).
/// In one, someone who cannot see `node` is told nothing: `404`, as the other routes answer (a
/// recycle-bin entry, which nobody has access to as such, is judged by its library). Someone
/// who can: `409 source_read_only` for a GitHub, file-share or read-only S3 library; `409
/// source_write_blocked` for a read-and-write S3 library whose writes are blocked; otherwise
/// [`Place::External`] with what the probe learnt about conditional writes. Whether the
/// endpoint is still approved is not read here: a write to one no longer approved fails in the
/// worker and stays queued.
///
/// The guard belongs in the HTTP handlers, never in `tree::apply` or the upload functions,
/// which the sync engine calls directly.
pub(crate) async fn writable_place(db: &Db, user: UserId, node: NodeId) -> Result<Place, ApiError> {
    let unavailable = |e: records::DbError| ApiError::unavailable(&e);
    let Some(library) = db.source_library_of(node).await.map_err(unavailable)? else {
        return Ok(Place::Ordinary);
    };
    let visible = if db
        .effective_access(user, node)
        .await
        .map_err(unavailable)?
        .is_some()
    {
        true
    } else {
        let in_bin = db
            .node(node)
            .await
            .map_err(unavailable)?
            .is_some_and(|found| found.deleted);
        in_bin
            && db
                .effective_access(user, library)
                .await
                .map_err(unavailable)?
                .is_some()
    };
    if !visible {
        return Err(ApiError::NotFound);
    }
    let Some((config, _)) = db.source_of(library).await.map_err(unavailable)? else {
        // Disconnected a moment ago: judged as it now is.
        return Ok(Place::Ordinary);
    };
    if config.kind != SourceKind::S3 || config.mode != SourceMode::ReadWrite {
        return Err(ApiError::SourceReadOnly);
    }
    if let Some((WriteState::Blocked, _)) = db.write_state(library).await.map_err(unavailable)? {
        return Err(ApiError::SourceWriteBlocked);
    }
    let conditional = db
        .s3_place(library)
        .await
        .map_err(unavailable)?
        .map_or(Tri::Unknown, |place| place.conditional);
    Ok(Place::External(Store::S3(S3WriteTarget {
        library,
        conditional,
    })))
}

/// The longest name a conflict copy gets, in characters.
const NAME_MAX_CHARS: usize = 120;
/// The longest person name used in a conflict copy's name, in characters.
const PERSON_MAX_CHARS: usize = 40;
/// The longest extension kept apart from the stem, in characters (dot included).
const EXT_MAX_CHARS: usize = 20;

/// Why a write did not happen, in words the caller can turn into a plain sentence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteError {
    /// The store refuses this key the write.
    Denied,
    /// The store could not be reached now; trying again later may work.
    Offline,
    /// The name is taken (a file share's answer; S3 never gives it from `write_object`).
    NameTaken,
    /// The bucket or folder is gone.
    Gone,
    /// The store is full (never answered by S3 in slice 1).
    NoSpace,
    /// The file is larger than one write sends.
    TooLarge,
    /// Anything else.
    Failed,
    /// A folder step would touch more objects than one step may (`MOVE_MAX_OBJECTS`); the
    /// count is how many the folder holds.
    TooMany(u64),
    /// An object is larger than one copy can move.
    TooLargeToMove,
    /// What is in the bucket is not what ShareCraft recorded.
    Changed,
}

/// What a write expects to find: the tag the file was last seen with (`None`: it must not
/// exist) and the hash of the content that was seen.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Base {
    pub tag: Option<String>,
    pub sha256: Option<[u8; 32]>,
}

/// What the store holds after a write, as the next listing will see it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seen {
    pub tag: String,
    pub size: u64,
    pub modified: OffsetDateTime,
}

/// The outcome of a write that was not refused or lost.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Written {
    Done(Seen),
    /// The file is not what the write was based on. Nothing was overwritten.
    Conflict,
}

/// What the write probe found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Probe {
    ReadWrite {
        conditional: Tri,
    },
    ReadOnly,
    /// The probe could not ask, for this reason (`keys`, `bucket`, `region`, `clock`,
    /// `address` or `offline`).
    Unreachable(&'static str),
}

/// Whether a character may stand in a name written for a person.
fn name_char(c: char) -> bool {
    !c.is_control() && !matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
}

/// `name` split into stem and extension (the extension keeps its dot). A leading dot is part
/// of the stem, and an extension longer than 20 characters is not one.
fn split_name(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(at) if at > 0 => {
            let ext = name.get(at..).unwrap_or("");
            if ext.chars().count() > EXT_MAX_CHARS {
                (name, "")
            } else {
                (name.get(..at).unwrap_or(name), ext)
            }
        }
        _ => (name, ""),
    }
}

/// `stem` cut to `max` characters, never inside one.
fn cut(stem: &str, max: usize) -> String {
    stem.chars().take(max).collect()
}

/// The name of the copy that keeps an edit made while the file changed outside:
/// `<stem> (edited by <person> <YYYY-MM-DD HH.MM>)<.ext>`, the time in UTC.
///
/// The person's name keeps only characters a name may hold and is cut to 40 characters; the
/// stem is shortened so the whole name fits 120 characters.
#[must_use]
pub fn sibling_name(name: &str, person: &str, at: OffsetDateTime) -> String {
    let cleaned: String = person.chars().filter(|c| name_char(*c)).collect();
    let cleaned = cut(cleaned.trim(), PERSON_MAX_CHARS);
    let cleaned = cleaned.trim_end();
    let cleaned = if cleaned.is_empty() {
        "someone"
    } else {
        cleaned
    };
    // A reserved device name ("con") would be refused by Windows; a prefix avoids it.
    let cleaned = if records::windows_safe_name(cleaned) {
        cleaned.to_owned()
    } else {
        format!("_{cleaned}")
    };
    let at = at.to_offset(UtcOffset::UTC);
    let stamp = format!(
        "{:04}-{:02}-{:02} {:02}.{:02}",
        at.year(),
        u8::from(at.month()),
        at.day(),
        at.hour(),
        at.minute()
    );
    let (stem, ext) = split_name(name);
    let tail = format!(" (edited by {cleaned} {stamp}){ext}");
    let room = NAME_MAX_CHARS.saturating_sub(tail.chars().count()).max(1);
    let stem = cut(stem, room);
    format!("{stem}{tail}")
}

/// `name` with its number before the extension: `numbered_name("a.txt", 2)` is `a (2).txt`.
#[must_use]
pub fn numbered_name(name: &str, n: u32) -> String {
    let (stem, ext) = split_name(name);
    format!("{stem} ({n}){ext}")
}

/// The rows the write worker takes in one step.
pub const WRITES_AT_ONCE: usize = 4;
/// How often a blocked library with changes waiting is probed again, at most.
pub const REPROBE_EVERY: Duration = Duration::from_secs(300);
/// How long the worker sleeps when nothing wakes it.
const WRITER_POLL: Duration = Duration::from_secs(5);
/// The wait after a first failed write; it doubles with each attempt.
const FIRST_RETRY_SECONDS: i64 = 30;
/// The longest wait between attempts.
const LAST_RETRY_SECONDS: i64 = 600;

/// Why a library's writes are blocked after the store refused one.
const BLOCKED_REASON: &str = "The storage refused a change with this access key.";
const NOT_APPROVED: &str = "The storage is no longer approved by an administrator.";
const READ_ONLY_NOW: &str =
    "The library is read-only now, so this change was not written to its storage.";
const CONTENT_MISSING: &str = "The content of this change is missing from this server.";
const SCAN_TOO_LARGE: &str =
    "The file is too large for the malware scanner, so it is not written to the storage.";
/// How long a change waits for its scan or a listing before it is looked at again.
const HELD_SECONDS: i64 = 5;

/// What [`stage`] stages: the content `blob` (already in the content store, held by the caller)
/// as the new state of `node`, at the library-relative `path` of the S3 library `library`.
pub(crate) struct StageRequest<'a> {
    pub library: NodeId,
    pub node: NodeId,
    pub path: &'a str,
    pub blob: BlobRef,
    /// The tag the file was last seen with; `None` for a new file.
    pub base_tag: Option<&'a str>,
    pub by: UserId,
    /// Write it at once rather than after the coalescing wait.
    pub flush: bool,
}

/// Why a change was not staged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StageError {
    /// The library's writes are blocked.
    Blocked,
    /// The content is larger than one write sends or the cache keeps.
    TooLarge,
    /// The library no longer takes changes, or the records failed.
    Failed,
}

/// The largest file a change to an external library may be: the smaller of one `PUT`, the
/// server's largest file and half the external-content cache.
pub(crate) async fn write_limit(state: &AppState) -> Result<u64, records::DbError> {
    let settings = state.db().share_settings().await?;
    let cache = state.shares().cache_max_bytes(settings.cache_max_bytes);
    Ok(state
        .s3()
        .single_put_max()
        .min(state.config().max_file_bytes)
        .min(cache / 2))
}

/// Stages `s`: the content is pinned in the external-content cache and a pending row names it,
/// so from this moment it is the file's current revision. The scan worker is told to look and
/// the write worker is woken. Answers the revision.
pub(crate) async fn stage(state: &AppState, s: &StageRequest<'_>) -> Result<String, StageError> {
    let db = state.db();
    let failed = |e: &dyn std::fmt::Display| {
        eprintln!("sharecraft-server: a change could not be staged: {e}");
        StageError::Failed
    };
    if let Some((WriteState::Blocked, _)) =
        db.write_state(s.library).await.map_err(|e| failed(&e))?
    {
        return Err(StageError::Blocked);
    }
    if s.blob.size > write_limit(state).await.map_err(|e| failed(&e))? {
        return Err(StageError::TooLarge);
    }
    let now = OffsetDateTime::now_utc();
    db.pin_cached(&s.blob, now).await.map_err(|e| failed(&e))?;
    let revision = db
        .stage_write(&NewPendingWrite {
            node: s.node,
            library: s.library,
            path: s.path,
            blob: s.blob,
            base_tag: s.base_tag,
            by: s.by,
            flush: s.flush,
            now,
        })
        .await
        .map_err(|e| match e {
            // Switched to read-only (or disconnected) since the guard looked.
            AccessError::LibraryReadOnly => StageError::Failed,
            e => failed(&e),
        })?;
    state.blobs().claimed().notify_one();
    state.writes().wake();
    Ok(revision)
}

/// Creates the file `name`, opened in `editor`, in `parent` of a read-and-write S3 library for
/// `user`, and stages `held` as its content, written at once: the answer is an ordinary
/// upload's with `"saving": true`. With `keep_both` a name taken meanwhile is chosen again. A
/// file whose content cannot be staged is removed again, and its content discarded.
pub(crate) async fn create_staged(
    state: &AppState,
    user: &User,
    parent: NodeId,
    name: &str,
    editor: Editor,
    held: Held,
    keep_both: bool,
) -> Result<Created, ApiError> {
    const NAME_ATTEMPTS: usize = 5;
    let db = state.db();
    let blob = held.blob();
    let folder = match staged_folder(db, parent).await {
        Ok(Some(folder)) => folder,
        Ok(None) => {
            discard(state, held).await;
            return Err(ApiError::SourceActionUnavailable);
        }
        Err(e) => {
            discard(state, held).await;
            return Err(e);
        }
    };
    let (library, folder) = folder;
    let id = DocId(Uuid::new_v4());
    let mut name = name.to_owned();
    let mut created =
        tree::create_document(state, user, parent, id, &name, editor, Some(blob.size)).await;
    for _ in 0..NAME_ATTEMPTS {
        if !(keep_both && matches!(created, Err(AccessError::NameTaken))) {
            break;
        }
        name = match tree::suggest_name(db, parent, &name, editor).await {
            Ok(name) => name,
            Err(e) => {
                discard(state, held).await;
                return Err(ApiError::unavailable(&e));
            }
        };
        created =
            tree::create_document(state, user, parent, id, &name, editor, Some(blob.size)).await;
    }
    let document = match created {
        Ok(document) => document,
        Err(e) => {
            discard(state, held).await;
            return Err(match e {
                AccessError::WrongExtension => ApiError::BadRequest,
                e => e.into(),
            });
        }
    };
    let node = NodeId(id.0);
    let path = format!("{folder}{}", document.name);
    let request = StageRequest {
        library,
        node,
        path: &path,
        blob,
        base_tag: None,
        by: user.id,
        flush: true,
    };
    if let Err(e) = stage(state, &request).await {
        // Nothing names the content: the new file goes again, so no file is left without it.
        if let Err(removed) = tree::apply(state, user, TreeOp::Delete { id: node }).await {
            eprintln!(
                "sharecraft-server: a new file whose content was not staged could not be \
                 removed: {removed}"
            );
        }
        discard(state, held).await;
        return Err(match e {
            StageError::Blocked => ApiError::SourceWriteBlocked,
            StageError::TooLarge => ApiError::TooLarge,
            StageError::Failed => ApiError::Unavailable,
        });
    }
    // The pending row and the cache pin keep the content now.
    drop(held);
    let access = db
        .doc_access(user.id, id)
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
    let body = json!({
        "id": document.id.0.to_string(),
        "name": document.name,
        "editor": document.editor,
        "access": access,
        "saving": true,
    });
    Ok(Created::Done { doc: id, body })
}

/// The library of `parent` and the library-relative path of the folder `parent` is (empty, or
/// ending in `/`); `None` when it is a folder the store does not know yet.
async fn staged_folder(db: &Db, parent: NodeId) -> Result<Option<(NodeId, String)>, ApiError> {
    let unavailable = |e: records::DbError| ApiError::unavailable(&e);
    let Some(library) = db.source_library_of(parent).await.map_err(unavailable)? else {
        return Ok(None);
    };
    if library == parent {
        return Ok(Some((library, String::new())));
    }
    Ok(db
        .source_item_of(parent)
        .await
        .map_err(unavailable)?
        .filter(|item| item.blob_sha.is_none())
        // A folder is recorded with its closing `/`.
        .map(|item| {
            let folder = item.repository_path.trim_end_matches('/');
            (library, format!("{folder}/"))
        }))
}

/// The library and the library-relative path the file `name` in `parent` of a connected
/// library would have; `None` when `parent` is a folder the store does not know yet.
pub(crate) async fn staged_path(
    db: &Db,
    parent: NodeId,
    name: &str,
) -> Result<Option<(NodeId, String)>, ApiError> {
    Ok(staged_folder(db, parent)
        .await?
        .map(|(library, folder)| (library, format!("{folder}{name}"))))
}

/// Whether `doc` is a file of an S3 library, in either mode: its file session uses the
/// [`ExternalCheckpointer`].
///
/// # Errors
/// `503 unavailable` when the records cannot say. The caller opens no session then: a bucket
/// file's session must never fall back to Git.
pub(crate) async fn is_bucket_file(state: &AppState, doc: DocId) -> Result<bool, ApiError> {
    if state.writes().bucket_lookup_fails() {
        return Err(ApiError::Unavailable);
    }
    let db = state.db();
    let unavailable = |e: records::DbError| ApiError::unavailable(&e);
    let Some(library) = db
        .source_library_of(NodeId(doc.0))
        .await
        .map_err(unavailable)?
    else {
        return Ok(false);
    };
    Ok(matches!(
        db.source_of(library).await.map_err(unavailable)?,
        Some((config, _)) if config.kind == SourceKind::S3
    ))
}

/// The checkpointer of a bucket file's session: a save is staged for the bucket, never
/// committed to Git, and makes no content claim and counts no site usage. The current revision
/// is the pending write's, else the tracked item's. Revision history is the Git checkpointer's
/// (a bucket file has none).
///
/// Its calls block, as every checkpointer's do: they run their async work with the runtime's
/// [`Handle::block_on`], on the blocking thread the session calls them from, never on an async
/// worker thread.
pub(crate) struct ExternalCheckpointer {
    git: Arc<dyn Checkpointer>,
    state: AppState,
    runtime: Option<Handle>,
}

impl ExternalCheckpointer {
    /// The checkpointer of bucket files for `state`, reading history from `git`. Made on the
    /// runtime, whose handle it keeps.
    pub(crate) fn new(state: AppState, git: Arc<dyn Checkpointer>) -> Self {
        Self {
            git,
            state,
            runtime: Handle::try_current().ok(),
        }
    }

    fn block_on<T>(
        &self,
        work: impl std::future::Future<Output = Result<T, CheckpointError>>,
    ) -> Result<T, CheckpointError> {
        match &self.runtime {
            Some(runtime) => runtime.block_on(work),
            None => Err(failure("no runtime to reach the records from")),
        }
    }
}

/// A failure the session reports as a failed save or open.
fn failure(cause: impl std::fmt::Display) -> CheckpointError {
    CheckpointError::Git(GitError::new(cause))
}

/// What a bucket file is now: its pending write and its tracked item.
struct Now {
    pending: Option<PendingWrite>,
    item: Option<records::ExternalItem>,
}

impl Now {
    async fn read(db: &Db, node: NodeId) -> Result<Now, CheckpointError> {
        Ok(Now {
            pending: db.pending_write(node).await.map_err(failure)?,
            item: db.external_item(node).await.map_err(failure)?,
        })
    }

    fn revision(&self) -> Result<Option<RevisionId>, CheckpointError> {
        let text = match (&self.pending, &self.item) {
            (Some(pending), _) => &pending.revision,
            (None, Some(item)) => &item.revision,
            (None, None) => return Ok(None),
        };
        RevisionId::from_str(text)
            .map(Some)
            .map_err(|_| failure("an external revision is not a revision id"))
    }

    fn content(&self) -> Option<BlobRef> {
        match (&self.pending, &self.item) {
            (Some(pending), _) => Some(pending.blob),
            (None, Some(item)) => item.sha256.map(|sha256| BlobRef {
                sha256,
                size: item.size,
            }),
            (None, None) => None,
        }
    }
}

impl Checkpointer for ExternalCheckpointer {
    fn write(&self, c: &Checkpoint<'_>) -> Result<Outcome, CheckpointError> {
        let node = NodeId(c.doc.0);
        let blob = c.blob;
        let base = c.base.cloned();
        let by = c
            .authors
            .first()
            .and_then(|author| Uuid::parse_str(&author.id).ok())
            .map(UserId);
        let state = self.state.clone();
        self.block_on(async move {
            let db = state.db();
            let now = Now::read(db, node).await?;
            let current = now.revision()?;
            if base != current {
                return Err(CheckpointError::Stale { current });
            }
            if let (Some(current), Some(content)) = (&current, now.content())
                && content.sha256 == blob.sha256
            {
                return Ok(Outcome::Unchanged(current.clone()));
            }
            let (library, path) = match (&now.pending, &now.item) {
                (Some(pending), _) => (pending.library, pending.path.clone()),
                (None, Some(item)) => (item.library, item.repository_path.clone()),
                (None, None) => return Err(failure("the file is not in a connected library")),
            };
            let by = by.ok_or(CheckpointError::InvalidAuthor)?;
            let writable = matches!(
                db.source_of(library).await.map_err(failure)?,
                Some((config, _)) if config.kind == SourceKind::S3
                    && config.mode == SourceMode::ReadWrite
            );
            if !writable {
                return Err(failure("the library does not take changes"));
            }
            let base_tag = now.item.as_ref().map(|item| item.tag.clone());
            let request = StageRequest {
                library,
                node,
                path: &path,
                blob,
                base_tag: base_tag.as_deref(),
                by,
                flush: false,
            };
            let revision = stage(&state, &request).await.map_err(|e| match e {
                StageError::Blocked => failure("the library's writes are blocked"),
                StageError::TooLarge => failure("the file is too large for the storage"),
                StageError::Failed => failure("the change could not be staged"),
            })?;
            RevisionId::from_str(&revision)
                .map(Outcome::Written)
                .map_err(|_| failure("an external revision is not a revision id"))
        })
    }

    fn load(&self, doc: DocId) -> Result<Option<Stored>, CheckpointError> {
        let state = self.state.clone();
        self.block_on(async move {
            let db = state.db();
            let now = Now::read(db, NodeId(doc.0)).await?;
            let Some(revision) = now.revision()? else {
                return Ok(None);
            };
            let file_name = db
                .document(doc)
                .await
                .map_err(failure)?
                .map(|document| document.name)
                .unwrap_or_default();
            Ok(Some(Stored {
                revision,
                content: now
                    .content()
                    .map_or_else(|| Content::Inline(Vec::new()), Content::Blob),
                meta: Meta {
                    schema: 1,
                    doc_id: doc,
                    file_name,
                    fields: Map::new(),
                    contributors: Vec::new(),
                },
            }))
        })
    }

    fn load_at(
        &self,
        doc: DocId,
        revision: &RevisionId,
    ) -> Result<Option<Stored>, CheckpointError> {
        self.git.load_at(doc, revision)
    }

    fn describe(
        &self,
        doc: DocId,
        revision: &RevisionId,
    ) -> Result<Option<RevisionInfo>, CheckpointError> {
        self.git.describe(doc, revision)
    }

    fn history(&self, doc: DocId) -> Result<Vec<RevisionInfo>, CheckpointError> {
        self.git.history(doc)
    }

    fn documents(&self) -> Result<Vec<DocId>, GitError> {
        self.git.documents()
    }

    /// The session's last save is not held back by the coalescing wait: its pending write is
    /// flushed and the writer woken. Spawned, so the ending session does not wait. If the flush
    /// cannot be recorded, the write still happens once the wait is over.
    fn session_ended(&self, doc: DocId) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        let state = self.state.clone();
        runtime.spawn(async move {
            match state
                .db()
                .flush_write(NodeId(doc.0), OffsetDateTime::now_utc())
                .await
            {
                Ok(true) => state.writes().wake(),
                Ok(false) => {}
                Err(e) => eprintln!(
                    "sharecraft-server: the last save of an ended session was not flushed; it is \
                     written after the usual wait: {e}"
                ),
            }
        });
    }
}

/// The write worker's wake, the blocked libraries it probes again and when it last did, and
/// its switch for tests.
#[derive(Default)]
pub struct SourceWrites {
    wake: Notify,
    /// Blocked libraries with changes waiting, and when each was last probed (or blocked).
    blocked: Mutex<HashMap<NodeId, Option<OffsetDateTime>>>,
    paused: AtomicBool,
    /// [`is_bucket_file`] answers a failure, as a database outage would. For tests.
    bucket_lookup_fails: AtomicBool,
}

impl SourceWrites {
    /// Makes [`is_bucket_file`] fail (or work again), as it does when the database cannot be
    /// read. For tests.
    pub fn set_bucket_lookup_fails_for_tests(&self, fails: bool) {
        self.bucket_lookup_fails.store(fails, Ordering::SeqCst);
    }

    fn bucket_lookup_fails(&self) -> bool {
        self.bucket_lookup_fails.load(Ordering::SeqCst)
    }

    /// Asks the worker to look for due writes at once.
    pub fn wake(&self) {
        self.wake.notify_one();
    }

    /// Stops (or starts again) the background worker, as a server stopped before its next
    /// write would; [`write_due`] called directly still writes. For tests.
    pub fn set_paused_for_tests(&self, paused: bool) {
        self.paused.store(paused, Ordering::SeqCst);
    }

    fn paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    fn blocked(&self) -> std::sync::MutexGuard<'_, HashMap<NodeId, Option<OffsetDateTime>>> {
        self.blocked.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `library` is blocked with changes waiting, last probed (or blocked) at `probed`; with
    /// `None`, a library not known yet is probed at the next chance.
    fn note_blocked(&self, library: NodeId, probed: Option<OffsetDateTime>) {
        let mut blocked = self.blocked();
        match probed {
            Some(at) => {
                blocked.insert(library, Some(at));
            }
            None => {
                blocked.entry(library).or_insert(None);
            }
        }
    }
}

/// The running write worker; dropping it stops it.
pub(crate) struct Writer {
    task: JoinHandle<()>,
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Starts the write worker: [`write_due`] at once, then whenever it is woken and every five
/// seconds. Pending writes left by an earlier process are picked up so.
pub(crate) fn start_writer(state: AppState) -> Writer {
    let task = tokio::spawn(async move {
        loop {
            if !state.writes().paused() {
                write_due(&state, OffsetDateTime::now_utc()).await;
            }
            tokio::select! {
                () = state.writes().wake.notified() => {}
                () = tokio::time::sleep(WRITER_POLL) => {}
            }
        }
    });
    Writer { task }
}

/// Writes up to [`WRITES_AT_ONCE`] pending writes due at `now` (one per file), each once its
/// content is available after the malware scan and while no listing holds its library; then
/// probes again the blocked libraries whose time has come. Answers how many rows were acted
/// on: written, written as a copy, failed or withdrawn.
pub async fn write_due(state: &AppState, now: OffsetDateTime) -> usize {
    let limit = i64::try_from(WRITES_AT_ONCE).unwrap_or(i64::MAX);
    let due = match state.db().due_writes(now, limit).await {
        Ok(due) => due,
        Err(e) => {
            eprintln!("sharecraft-server: cannot read the changes to write: {e}");
            return 0;
        }
    };
    let mut done = 0;
    for row in due {
        if write_one(state, row, now).await {
            done += 1;
        }
    }
    reprobe(state, now).await;
    done
}

/// Writes one pending row; whether it was acted on.
async fn write_one(state: &AppState, due: PendingWrite, now: OffsetDateTime) -> bool {
    let db = state.db();
    let sha = due.blob.sha256;
    match db.availability(&sha).await {
        Ok(None) => {}
        Ok(Some(ScanState::Infected)) => {
            // Never written; the quarantine keeps its own marks on the content.
            if let Err(e) = db.drop_write(due.node, &sha).await {
                eprintln!("sharecraft-server: cannot withdraw an infected change: {e}");
            }
            return true;
        }
        // Over the scanner's limit: it is never let through, so it is never written.
        Ok(Some(ScanState::TooLarge)) => {
            fail(state, &due, now, SCAN_TOO_LARGE).await;
            return true;
        }
        // Still being scanned, or the scan is retried: looked at again shortly, so it never
        // keeps other files' writes waiting.
        Ok(Some(_)) => {
            defer(state, &due, now).await;
            return false;
        }
        Err(e) => {
            eprintln!("sharecraft-server: cannot read the scan of a change: {e}");
            return false;
        }
    }
    // A listing holds the lock for its whole pass: the row is looked at again shortly.
    let lock = match db.try_lock_source(due.library).await {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            defer(state, &due, now).await;
            return false;
        }
        Err(e) => {
            eprintln!("sharecraft-server: cannot take a library's source lock: {e}");
            return false;
        }
    };
    let row = match db.pending_write(due.node).await {
        Ok(Some(row)) if row.blob == due.blob => row,
        // Withdrawn, written, or changed meanwhile: the new row is looked at next time.
        _ => return false,
    };
    let acted = write_locked(state, &row, now).await;
    drop(lock);
    acted
}

/// [`write_one`], once the library's source lock is held and the row read again.
async fn write_locked(state: &AppState, row: &PendingWrite, now: OffsetDateTime) -> bool {
    let db = state.db();
    match db.source_of(row.library).await {
        Ok(Some((config, _)))
            if config.kind == SourceKind::S3 && config.mode == SourceMode::ReadWrite => {}
        Ok(_) => {
            fail(state, row, now, READ_ONLY_NOW).await;
            return true;
        }
        Err(e) => {
            eprintln!("sharecraft-server: cannot read a library's connection: {e}");
            return false;
        }
    }
    match db.write_state(row.library).await {
        Ok(Some((WriteState::Blocked, _))) => {
            state.writes().note_blocked(row.library, None);
            return false;
        }
        Ok(_) => {}
        Err(e) => {
            eprintln!("sharecraft-server: cannot read a library's write state: {e}");
            return false;
        }
    }
    let target = match state.s3().target(state, row.library).await {
        Ok(target) => target,
        Err(S3Error::AddressRefused) => {
            fail(state, row, now, NOT_APPROVED).await;
            return true;
        }
        Err(_) => {
            fail(state, row, now, &sentence(WriteError::Offline)).await;
            return true;
        }
    };
    let conditional = match db.s3_place(row.library).await {
        Ok(place) => place.map_or(Tri::Unknown, |place| place.conditional),
        Err(e) => {
            eprintln!("sharecraft-server: cannot read a library's place: {e}");
            return false;
        }
    };
    let Some(held) = state.blobs().hold_stored(row.blob).await else {
        fail(state, row, now, CONTENT_MISSING).await;
        return true;
    };
    let body = PutBody {
        path: Some(state.blobs().store().path_of(&row.blob.sha256)),
        size: row.blob.size,
        sha256: row.blob.sha256,
    };
    let base = Base {
        tag: row.base_tag.clone(),
        sha256: None,
    };
    match write_object(state.s3(), &target, &row.path, &body, &base, conditional).await {
        Ok(Written::Done(seen)) => finished(state, row, &row.path, &seen, conditional).await,
        Ok(Written::Conflict) => conflict(state, &target, row, &body, conditional, now).await,
        Err(error) => failed_write(state, row, error, now).await,
    }
    drop(held);
    true
}

/// Records that the row's content is at `path` in the store, as `seen`.
async fn finished(state: &AppState, row: &PendingWrite, path: &str, seen: &Seen, conditional: Tri) {
    let db = state.db();
    if path != row.path
        && let Err(e) = db.repath_write(row.node, &row.blob.sha256, path).await
    {
        eprintln!("sharecraft-server: cannot record the name a new file was written under: {e}");
        return;
    }
    if let Err(e) = db
        .finish_write(
            row.node,
            &row.blob.sha256,
            &seen.tag,
            seen.size,
            seen.modified,
        )
        .await
    {
        // The next attempt's first look finds our content there and finishes it.
        eprintln!("sharecraft-server: cannot record a finished write: {e}");
        return;
    }
    if conditional == Tri::No {
        emit(
            state,
            "source.unconditional_write",
            row.library,
            Some(row.node),
            json!({ "count": 1 }),
        );
    }
}

/// The file changed in the store since it was read, or a new file's name was taken meanwhile:
/// nothing is overwritten. A tracked file's change becomes a named copy beside it and the file
/// takes the store's state; a new file is written under a numbered name.
async fn conflict(
    state: &AppState,
    target: &S3Target,
    row: &PendingWrite,
    body: &PutBody,
    conditional: Tri,
    now: OffsetDateTime,
) {
    let db = state.db();
    let author = match db.user(row.by).await {
        Ok(author) => author,
        Err(e) => {
            eprintln!("sharecraft-server: cannot read the author of a change: {e}");
            None
        }
    };
    let tracked = match db.source_item_of(row.node).await {
        Ok(item) => item.is_some(),
        Err(e) => {
            eprintln!("sharecraft-server: cannot read a file's tracking row: {e}");
            return;
        }
    };
    if !tracked {
        match write_numbered(state.s3(), target, &row.path, body, conditional).await {
            Ok((path, seen)) => {
                if let Some(author) = &author {
                    let name = last_segment(&path).to_owned();
                    let renamed = TreeOp::Rename { id: row.node, name };
                    if let Err(e) = tree::apply(state, author, renamed).await {
                        // The listing moves it to the name it has in the store.
                        eprintln!("sharecraft-server: a new file could not take its name: {e}");
                    }
                }
                finished(state, row, &path, &seen, conditional).await;
            }
            Err(error) => failed_write(state, row, error, now).await,
        }
        return;
    }
    let person = author.as_ref().map_or("", |author| author.name.as_str());
    let written = write_beside(
        state.s3(),
        target,
        &row.path,
        body,
        person,
        now,
        conditional,
    )
    .await;
    let (path, seen) = match written {
        Ok(written) => written,
        Err(error) => {
            failed_write(state, row, error, now).await;
            return;
        }
    };
    let copy = match &author {
        Some(author) => copy_node(state, author, row, &path, &seen, now).await,
        None => None,
    };
    if let Err(e) = db.drop_write(row.node, &row.blob.sha256).await {
        eprintln!("sharecraft-server: cannot withdraw a change written as a copy: {e}");
    }
    // The file is what the store holds now.
    if let Some(key) = object_key(target, &row.path)
        && let Ok(head) = state.s3().head(target, &key).await
        && let Ok(Some(item)) = db.external_item(row.node).await
    {
        let tag = object_tag(head.size, head.modified_s, &head.etag, false);
        if tag != item.tag
            && let Err(e) = db
                .set_external_seen(row.node, &item.tag, &tag, head.size)
                .await
        {
            eprintln!("sharecraft-server: cannot record a file changed in the store: {e}");
        }
    }
    state.sources().wake(row.library);
    emit(
        state,
        "source.conflict_copy",
        row.library,
        Some(row.node),
        json!({ "node": row.node.0, "copy": copy.map(|copy| copy.0) }),
    );
}

/// The node of the copy written at `path` beside `row`'s file, credited to `author`, tracked as
/// what the store holds (`seen`) with its content recorded. `None` if it could not be made:
/// the next listing adds it.
async fn copy_node(
    state: &AppState,
    author: &User,
    row: &PendingWrite,
    path: &str,
    seen: &Seen,
    now: OffsetDateTime,
) -> Option<NodeId> {
    let db = state.db();
    let parent = match db.node(row.node).await {
        Ok(Some(node)) => node.parent?,
        Ok(None) => return None,
        Err(e) => {
            eprintln!("sharecraft-server: cannot read the file a copy was written beside: {e}");
            return None;
        }
    };
    let name = last_segment(path);
    let id = DocId(Uuid::new_v4());
    let editor = Editor::for_file_name(name);
    let created =
        tree::create_document(state, author, parent, id, name, editor, Some(seen.size)).await;
    if let Err(e) = created {
        eprintln!("sharecraft-server: the node of a copy could not be made: {e}");
        return None;
    }
    let node = NodeId(id.0);
    let entry = ExternalEntry {
        node,
        path: path.to_owned(),
        repository_path: path.to_owned(),
        size: seen.size,
        mtime_ns: i64::try_from(seen.modified.unix_timestamp_nanos()).unwrap_or(0),
        tag: Some(seen.tag.clone()),
    };
    if let Err(e) = db.record_external_item(row.library, &entry).await {
        eprintln!("sharecraft-server: the copy could not be tracked: {e}");
        return Some(node);
    }
    if let Err(e) = db
        .set_external_content(node, &seen.tag, &row.blob.sha256, now)
        .await
    {
        eprintln!("sharecraft-server: the content of a copy could not be recorded: {e}");
    }
    Some(node)
}

/// The last segment of a `/`-separated path.
fn last_segment(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// What a failed write says to the people who see it.
fn sentence(error: WriteError) -> String {
    match error {
        WriteError::TooMany(count) => too_many_sentence(count),
        other => plain_sentence(other).to_owned(),
    }
}

/// What a folder of `count` objects, too many for one step, says to the people who see it.
pub(crate) fn too_many_sentence(count: u64) -> String {
    format!(
        "This folder holds {} objects in the bucket. ShareCraft renames, moves, copies or \
         deletes at most 1,000 at a time.",
        thousands(count)
    )
}

/// `n` with a comma between each group of three digits: `1,234,567`.
#[must_use]
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// The sentence for an error that carries no number.
fn plain_sentence(error: WriteError) -> &'static str {
    match error {
        WriteError::TooMany(_) => "This folder holds too many objects in the bucket.",
        WriteError::TooLargeToMove => {
            "Files over 5 GiB in S3 storage cannot be renamed, moved, copied or deleted from \
             ShareCraft yet."
        }
        WriteError::Changed => {
            "This changed in the storage since ShareCraft last looked. ShareCraft is checking it \
             again; try again in a minute."
        }
        WriteError::Offline => "The storage could not be reached. Saving is tried again shortly.",
        WriteError::Gone => "The bucket could not be found. Saving is tried again later.",
        WriteError::TooLarge => "The file is too large to be written to the storage in one piece.",
        WriteError::Denied => "The storage refused this change with this access key.",
        WriteError::NameTaken | WriteError::NoSpace | WriteError::Failed => {
            "The storage did not take the change. Saving is tried again later."
        }
    }
}

/// Records a failed write; a refused one also blocks the library.
async fn failed_write(
    state: &AppState,
    row: &PendingWrite,
    error: WriteError,
    now: OffsetDateTime,
) {
    fail(state, row, now, &sentence(error)).await;
    if error == WriteError::Denied {
        block(state, row.library, Some(now)).await;
    }
}

/// The row failed with `why`: it is tried again after 30 seconds, doubled with each earlier
/// attempt, at most 10 minutes.
async fn fail(state: &AppState, row: &PendingWrite, now: OffsetDateTime, why: &str) {
    let factor = 1_i64.checked_shl(row.attempts.min(20)).unwrap_or(i64::MAX);
    let wait = FIRST_RETRY_SECONDS
        .checked_mul(factor)
        .unwrap_or(LAST_RETRY_SECONDS)
        .min(LAST_RETRY_SECONDS);
    let next = now + time::Duration::seconds(wait);
    if let Err(e) = state
        .db()
        .fail_write(row.node, &row.blob.sha256, why, next)
        .await
    {
        eprintln!("sharecraft-server: cannot record a failed write: {e}");
    }
}

/// The row is held (its scan has not finished, or a listing holds the library): it is looked at
/// again after a few seconds, with no attempt counted.
async fn defer(state: &AppState, row: &PendingWrite, now: OffsetDateTime) {
    let until = now + time::Duration::seconds(HELD_SECONDS);
    if let Err(e) = state
        .db()
        .defer_write(row.node, &row.blob.sha256, until)
        .await
    {
        eprintln!("sharecraft-server: cannot hold a change back: {e}");
    }
}

/// Probes again each blocked library with changes waiting whose time has come. A pass unblocks
/// it, records what it found about conditional writes and makes its changes due. Blocked
/// libraries not yet known here (after a restart, say) are probed at once.
async fn reprobe(state: &AppState, now: OffsetDateTime) {
    let every = time::Duration::seconds(i64::try_from(REPROBE_EVERY.as_secs()).unwrap_or(300));
    let db = state.db();
    match db.blocked_with_writes().await {
        Ok(found) => {
            let mut blocked = state.writes().blocked();
            for library in found {
                blocked.entry(library).or_insert(None);
            }
        }
        Err(e) => eprintln!("sharecraft-server: cannot list blocked libraries: {e}"),
    }
    let libraries: Vec<(NodeId, Option<OffsetDateTime>)> = state
        .writes()
        .blocked()
        .iter()
        .map(|(library, at)| (*library, *at))
        .collect();
    for (library, probed) in libraries {
        if probed.is_some_and(|at| now - at < every) {
            continue;
        }
        let blocked = matches!(
            db.write_state(library).await,
            Ok(Some((WriteState::Blocked, _)))
        );
        let rows = db.pending_writes_of(library).await.unwrap_or_default();
        if !blocked || rows.is_empty() {
            state.writes().blocked().remove(&library);
            continue;
        }
        state.writes().note_blocked(library, Some(now));
        let Ok(target) = state.s3().target(state, library).await else {
            continue;
        };
        let (probe, left) = probe_writes(state.s3(), &target).await;
        if left.is_some() {
            eprintln!(
                "sharecraft-server: a write probe object could not be removed from library {}",
                library.0
            );
        }
        let Probe::ReadWrite { conditional } = probe else {
            continue;
        };
        let versioning = match db.s3_place(library).await {
            Ok(Some(place)) => place.versioning,
            _ => continue,
        };
        if let Err(e) = db.set_s3_probe(library, versioning, conditional).await {
            eprintln!("sharecraft-server: cannot record a write probe: {e}");
        }
        match db.set_write_state(library, WriteState::Ok, None).await {
            Ok(changed) => restored(state, library, changed, now).await,
            Err(e) => eprintln!("sharecraft-server: cannot unblock a library's writes: {e}"),
        }
    }
}

/// The writes of `library` are no longer blocked (a probe passed, by the worker or by an
/// administrator's switch to read and write): its pending changes are due at once and the writer
/// is woken. When the state really `changed`, it is announced and the open files below are
/// re-checked, so editing seats narrowed by the block widen again.
pub(crate) async fn restored(
    state: &AppState,
    library: NodeId,
    changed: bool,
    now: OffsetDateTime,
) {
    if changed {
        emit(state, "source.write_restored", library, None, json!({}));
    }
    match state.db().pending_writes_of(library).await {
        Ok(rows) => {
            for row in rows {
                if let Err(e) = state.db().retry_write(row.node, now).await {
                    eprintln!("sharecraft-server: cannot make a change due again: {e}");
                }
            }
        }
        Err(e) => eprintln!("sharecraft-server: cannot read a library's changes: {e}"),
    }
    state.writes().blocked().remove(&library);
    state.writes().wake();
    if changed {
        crate::api::recheck_below(state, library).await;
    }
}

/// Records an event about `library`, and the file `node` when there is one.
fn emit(
    state: &AppState,
    kind: &'static str,
    library: NodeId,
    node: Option<NodeId>,
    detail: Value,
) {
    state.events().emit(NewEvent {
        kind,
        actor: None,
        subject: None,
        document: node.map(|node| DocId(node.0)),
        node: Some(library),
        detail,
    });
}

/// How long a request that must hold a library's source lock waits for a listing or a write.
const LOCK_WAIT: Duration = Duration::from_secs(10);

/// The library's source lock, waiting up to [`LOCK_WAIT`] for a listing or a write to give it up
/// (`409 source_busy` after that). Held, no listing and no write of the library runs.
async fn lock_source(db: &Db, library: NodeId) -> Result<SourceLock, ApiError> {
    let deadline = tokio::time::Instant::now() + LOCK_WAIT;
    loop {
        if let Some(lock) = db
            .try_lock_source(library)
            .await
            .map_err(|e| ApiError::unavailable(&e))?
        {
            return Ok(lock);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(ApiError::SourceBusy);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// The library and the library-relative path (the key below the prefix) of a new item named
/// `name` in `parent` of a read-and-write S3 library, once the name is one the listing will
/// show and the bucket takes: the name is valid (`422 invalid_name`), it is one segment the
/// listing shows and not one ShareCraft keeps for itself, and a key can be made of it.
/// `409 source_action_unavailable` when `parent` is a folder the store does not know yet.
pub(crate) async fn external_path(
    state: &AppState,
    parent: NodeId,
    name: &str,
) -> Result<(NodeId, String), ApiError> {
    records::validate_node_name(name)?;
    let Some((library, path)) = staged_path(state.db(), parent, name).await? else {
        return Err(ApiError::SourceActionUnavailable);
    };
    let nameable = path
        .rsplit('/')
        .next()
        .is_some_and(|last| nameable_segment(last) && !records::reserved_name(last));
    let keyed = match state.s3().target(state, library).await {
        Ok(target) => object_key(&target, &path).is_some(),
        // Not approved, or unreadable: the worker reports it; the name is judged on its own.
        Err(_) => true,
    };
    if !nameable || !keyed {
        return Err(ApiError::InvalidName);
    }
    Ok((library, path))
}

/// Whether the library that `place` is in (or is) has its folders off.
async fn folders_off(db: &Db, place: NodeId) -> Result<bool, ApiError> {
    let unavailable = |e: records::DbError| ApiError::unavailable(&e);
    let Some(library) = db.library_of(place).await.map_err(unavailable)? else {
        return Ok(false);
    };
    Ok(db
        .library_settings(library)
        .await
        .map_err(unavailable)?
        .is_some_and(|settings| !settings.folders))
}

/// Everything about a new folder `name` in `parent` that is judged before the bucket is asked,
/// as [`tree::apply`] judges a create: the person sees `parent` and may add there, it is a
/// library or folder, folders are on, the name is valid, one the listing shows and free there
/// ignoring case. Answers the library and the library-relative path of the folder.
async fn folder_allowed(
    state: &AppState,
    actor: &User,
    parent: NodeId,
    name: &str,
) -> Result<(NodeId, String), ApiError> {
    let db = state.db();
    let (parent_node, access) = tree::visible(db, actor, parent).await?;
    if !matches!(parent_node.kind, NodeKind::Library | NodeKind::Folder) {
        return Err(AccessError::WrongPlace.into());
    }
    tree::require(&access, Perms::ADD)?;
    if folders_off(db, parent).await? {
        return Err(AccessError::FoldersOff.into());
    }
    let name = records::validate_node_name(name)?;
    let found = external_path(state, parent, name).await?;
    if !db
        .name_free(parent, name, None)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
    {
        return Err(AccessError::NameTaken.into());
    }
    Ok(found)
}

/// Creates the folder `name` in `parent` of the read-and-write S3 library `library` for `actor`:
/// a zero-byte marker object `<path>/` is written to the bucket (one already there is
/// accepted; nothing is ever deleted or overwritten) and the folder is tracked as the listing
/// will see it, so the next listing plans no change.
///
/// Everything that can be refused is judged before the bucket is asked: the person's right to
/// add there (as [`tree::apply`] judges it), the folders setting, the name, and whether the name
/// is free ignoring case. The library's source lock is held from before the marker is written
/// until the folder is tracked, so no listing sees one without the other (`409 source_busy`
/// after ten seconds). If the folder cannot be made in the tree after the marker was written,
/// the marker is left: the next listing shows an empty folder.
///
/// # Errors
/// `404`, `403` and `409 name_taken` as for any create; `422 invalid_name`; `409 folders_off`;
/// `409 source_busy`; `409 source_write_blocked` when the store refuses the write (the library
/// is blocked until a probe passes); `source_unavailable` when it cannot be reached.
pub(crate) async fn create_folder(
    state: &AppState,
    actor: &User,
    library: NodeId,
    parent: NodeId,
    name: &str,
) -> Result<Node, ApiError> {
    let db = state.db();
    let name = records::validate_node_name(name)?.to_owned();
    folder_allowed(state, actor, parent, &name).await?;
    let lock = lock_source(db, library).await?;
    // Judged again once the lock is held: a listing or a write may have run meanwhile.
    let (_, path) = folder_allowed(state, actor, parent, &name).await?;
    let target = bucket(state, library).await?;
    let conditional = db
        .s3_place(library)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
        .map_or(Tri::Unknown, |place| place.conditional);
    if let Err(error) = make_folder(state.s3(), &target, &path, conditional).await {
        return Err(refusal(state, library, error).await);
    }
    let created = tree::apply(
        state,
        actor,
        TreeOp::Create {
            parent: Some(parent),
            kind: NodeKind::Folder,
            name: name.clone(),
            id: NodeId(Uuid::new_v4()),
        },
    )
    .await;
    let node = match created {
        Ok(applied) => applied.node,
        Err(e) => {
            eprintln!(
                "sharecraft-server: a folder marker was written but its folder could not be \
                 made: {e}"
            );
            return Err(e.into());
        }
    };
    // The listing's own name for it: below the parent's shown path.
    let shown = if parent == library {
        name.clone()
    } else {
        match db.source_item_of(parent).await {
            Ok(Some(item)) => format!("{}/{name}", item.path),
            _ => path.clone(),
        }
    };
    let item = SourceItem {
        node: node.id,
        path: shown,
        repository_path: format!("{path}/"),
        blob_sha: None,
        size: 0,
    };
    if let Err(e) = db.upsert_source_items(library, &[item]).await {
        // The folder exists without its tracking: the next listing tracks it by its path.
        eprintln!("sharecraft-server: a new folder could not be tracked: {e}");
    }
    drop(lock);
    Ok(node)
}

/// The bucket of the read-and-write S3 library `library`, for a change made in the request.
async fn bucket(state: &AppState, library: NodeId) -> Result<S3Target, ApiError> {
    match state.s3().target(state, library).await {
        Ok(target) => Ok(target),
        Err(S3Error::AddressRefused) => {
            Err(ApiError::SourceUnavailable("not_approved", SourceKind::S3))
        }
        Err(_) => Err(ApiError::Unavailable),
    }
}

/// The answer to a change in a bucket that the store refused or could not make, with what it
/// sets going: a refused write blocks the library, and a change found outside wakes a listing.
async fn refusal(state: &AppState, library: NodeId, error: WriteError) -> ApiError {
    match error {
        WriteError::TooMany(count) => ApiError::SourceTooManyObjects(count),
        WriteError::TooLargeToMove => ApiError::SourceTooLargeToMove,
        WriteError::Changed => {
            state.sources().wake(library);
            ApiError::SourceChanged
        }
        WriteError::NameTaken => ApiError::NameTaken,
        WriteError::Denied => {
            block(state, library, Some(OffsetDateTime::now_utc())).await;
            ApiError::SourceWriteBlocked
        }
        WriteError::Offline => ApiError::SourceUnavailable("offline", SourceKind::S3),
        WriteError::Gone => ApiError::SourceUnavailable("gone", SourceKind::S3),
        WriteError::NoSpace | WriteError::TooLarge | WriteError::Failed => ApiError::Unavailable,
    }
}

/// What a rename or move inside a bucket did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Restructured {
    pub node: NodeId,
    /// Originals that could not be deleted once every copy was made: they come back as new
    /// files at the next listing.
    pub left_behind: u64,
}

/// A rename or move of an item of a bucket, judged and ready to be made.
struct Restructure {
    library: NodeId,
    kind: NodeKind,
    /// The item's name now.
    old_name: String,
    /// The new name, when it changes.
    name: Option<String>,
    /// The new parent, when it changes.
    parent: Option<NodeId>,
    /// The library-relative paths before and after; a folder's end with `/`.
    from: String,
    to: String,
    /// The paths the listing shows before and after (a folder's without the `/`).
    shown_from: String,
    shown_to: String,
    /// The tracking rows of the item and of everything inside it.
    items: Vec<SourceItem>,
}

impl Restructure {
    fn folder(&self) -> bool {
        self.kind == NodeKind::Folder
    }

    /// Where the library-relative `path` of something moved is after the move.
    fn rebased(&self, path: &str) -> String {
        path.strip_prefix(self.from.as_str())
            .map_or_else(|| path.to_owned(), |rest| format!("{}{rest}", self.to))
    }

    /// Where the shown `path` of something moved is after the move; `None` when it is not
    /// below the item's shown path (a name the listing numbered keeps its own).
    fn reshown(&self, path: &str) -> Option<String> {
        if path == self.shown_from {
            return Some(self.shown_to.clone());
        }
        path.strip_prefix(self.shown_from.as_str())
            .and_then(|rest| rest.strip_prefix('/'))
            .map(|rest| format!("{}/{rest}", self.shown_to))
    }
}

/// The name `name` for `node` once checked as [`tree::apply`] checks a rename: a file's keeps
/// its editor's extension (`422 wrong_extension`), anything else is a valid node name.
async fn checked_name(db: &Db, node: &Node, name: &str) -> Result<String, ApiError> {
    let document = if node.kind == NodeKind::Document {
        db.document(DocId(node.id.0))
            .await
            .map_err(|e| ApiError::unavailable(&e))?
    } else {
        None
    };
    let checked = match document {
        Some(document) => records::validate_document_name(name, document.editor),
        None => records::validate_node_name(name),
    };
    checked.map(str::to_owned).map_err(|e| match e {
        AccessError::WrongExtension => ApiError::WrongExtension,
        e => e.into(),
    })
}

/// The path the listing shows for `name` in `parent` of `library` (no `/` at either end).
async fn shown_in(
    db: &Db,
    library: NodeId,
    parent: NodeId,
    name: &str,
) -> Result<String, ApiError> {
    if parent == library {
        return Ok(name.to_owned());
    }
    let item = db
        .source_item_of(parent)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
        .ok_or(ApiError::SourceActionUnavailable)?;
    Ok(format!("{}/{name}", item.path))
}

/// Everything about renaming `node` to `name` and/or moving it into `parent` inside its
/// read-and-write S3 library that is judged before the bucket is asked. `None` when nothing
/// would change.
///
/// In order: where the node is (`404`, `409 source_read_only`, `409 source_write_blocked`, a
/// library itself `409 source_read_only`, a parent elsewhere `409 source_action_unavailable`);
/// the person's rights, as [`tree::apply`] judges a rename and a move (`edit` on the node, `add`
/// where it goes, never into itself); `409 source_item_saving` when the item or anything
/// inside it has a change not yet written; the name (`422 invalid_name`, `422
/// wrong_extension`, the file policies, `409 name_taken` ignoring case); and an item in
/// archive storage (`409 source_unavailable {reason: archived}`), which no copy can move.
async fn restructure_allowed(
    state: &AppState,
    actor: &User,
    node: NodeId,
    name: Option<&str>,
    parent: Option<NodeId>,
) -> Result<Option<Restructure>, ApiError> {
    let db = state.db();
    let unavailable = |e: records::DbError| ApiError::unavailable(&e);
    let library = match writable_place(db, actor.id, node).await? {
        Place::External(Store::S3(target)) => target.library,
        Place::Ordinary => return Err(ApiError::SourceActionUnavailable),
    };
    if library == node {
        return Err(ApiError::SourceReadOnly);
    }
    let (item, access) = tree::visible(db, actor, node).await?;
    if !matches!(item.kind, NodeKind::Folder | NodeKind::Document) {
        return Err(AccessError::WrongPlace.into());
    }
    tree::require(&access, Perms::EDIT)?;
    let current = item.parent.ok_or(ApiError::SourceActionUnavailable)?;
    let destination = parent.unwrap_or(current);
    if destination != current {
        match writable_place(db, actor.id, destination).await {
            Ok(Place::External(Store::S3(target))) if target.library == library => {}
            Err(ApiError::NotFound) => return Err(ApiError::NotFound),
            _ => return Err(ApiError::SourceActionUnavailable),
        }
        let (place, place_access) = tree::visible(db, actor, destination).await?;
        if !matches!(place.kind, NodeKind::Library | NodeKind::Folder) {
            return Err(AccessError::WrongPlace.into());
        }
        tree::require(&place_access, Perms::ADD)?;
        if db
            .is_at_or_below(destination, node)
            .await
            .map_err(unavailable)?
        {
            return Err(AccessError::IntoItself.into());
        }
    }
    let new_name = match name {
        Some(name) => checked_name(db, &item, name).await?,
        None => item.name.clone(),
    };
    if new_name == item.name && destination == current {
        return Ok(None);
    }
    let Some(tracked) = db.source_item_of(node).await.map_err(unavailable)? else {
        // Not in the bucket yet: a new file whose first write is still to come.
        if db.pending_write(node).await.map_err(unavailable)?.is_some() {
            return Err(ApiError::SourceItemSaving);
        }
        return Err(ApiError::SourceActionUnavailable);
    };
    let from = tracked.repository_path;
    if db.writes_below(library, &from).await.map_err(unavailable)? > 0 {
        return Err(ApiError::SourceItemSaving);
    }
    if new_name != item.name && item.kind == NodeKind::Document {
        tree::check_policies(db, node, &new_name, None).await?;
    }
    let (_, path) = external_path(state, destination, &new_name).await?;
    if !db
        .name_free(destination, &new_name, Some(node))
        .await
        .map_err(unavailable)?
    {
        return Err(AccessError::NameTaken.into());
    }
    let folder = item.kind == NodeKind::Folder;
    let to = if folder { format!("{path}/") } else { path };
    let items: Vec<SourceItem> = db
        .source_items(library)
        .await
        .map_err(unavailable)?
        .into_iter()
        .filter(|i| i.repository_path == from || (folder && i.repository_path.starts_with(&from)))
        .collect();
    // An object in archive storage cannot be copied: refused as reading it is, not as a change
    // that a later try might get past.
    if items
        .iter()
        .any(|i| i.blob_sha.as_deref().is_some_and(|tag| tag.ends_with("!a")))
    {
        return Err(ApiError::SourceUnavailable("archived", SourceKind::S3));
    }
    let shown_to = shown_in(db, library, destination, &new_name).await?;
    Ok(Some(Restructure {
        library,
        kind: item.kind,
        name: (new_name != item.name).then_some(new_name),
        old_name: item.name,
        parent: (destination != current).then_some(destination),
        from,
        to,
        shown_from: tracked.path,
        shown_to,
        items,
    }))
}

/// Renames `node` to `name` and/or moves it into `parent`, inside its read-and-write S3
/// library, for `actor`: in the bucket first (every object copied on the tag ShareCraft
/// recorded, then every original deleted), then in the tree, then in the tracking rows, with
/// the tags of the copies as the store answered them, so the next listing sees nothing new.
/// The node keeps its id, permissions and links.
///
/// Everything that can be refused is judged before the bucket is asked (see
/// [`restructure_allowed`]), and judged again once the library's source lock is held; the lock
/// is held over every bucket call and until the rows are recorded (`409 source_busy` after ten
/// seconds). If the tree refuses after the bucket moved, the objects are moved back (best
/// effort) and the tree's error is answered. Originals that could not be deleted are counted in
/// `left_behind`, and a listing is woken to show them. Records `source.moved {node, objects,
/// left_behind}`.
///
/// # Errors
/// As [`restructure_allowed`]; `409 source_too_many_objects {count}`, `409
/// source_too_large_to_move`, `409 source_changed` (a listing is woken), `409 name_taken`, `409
/// source_write_blocked` (the store refused: the library is blocked) and `source_unavailable`
/// from the bucket.
pub(crate) async fn rename_or_move(
    state: &AppState,
    actor: &User,
    node: NodeId,
    name: Option<&str>,
    parent: Option<NodeId>,
) -> Result<Restructured, ApiError> {
    let db = state.db();
    let unchanged = Restructured {
        node,
        left_behind: 0,
    };
    let Some(first) = restructure_allowed(state, actor, node, name, parent).await? else {
        return Ok(unchanged);
    };
    let lock = lock_source(db, first.library).await?;
    // Judged again once the lock is held: a listing or a write may have run meanwhile.
    let Some(plan) = restructure_allowed(state, actor, node, name, parent).await? else {
        return Ok(unchanged);
    };
    let target = bucket(state, plan.library).await?;
    let known: Vec<(String, String)> = plan
        .items
        .iter()
        .map(|i| {
            (
                i.repository_path.clone(),
                i.blob_sha.clone().unwrap_or_default(),
            )
        })
        .collect();
    let moved = match move_item(
        state.s3(),
        &target,
        &plan.from,
        &plan.to,
        plan.folder(),
        &known,
    )
    .await
    {
        Ok(moved) => moved,
        Err(error) => return Err(refusal(state, plan.library, error).await),
    };
    if let Err(e) = restructure_tree(state, actor, node, &plan).await {
        move_back(state, &target, &plan, &moved).await;
        return Err(e);
    }
    record_moved(state, &plan, &moved).await;
    drop(lock);
    if plan.kind == NodeKind::Document && plan.name.is_some() {
        crate::api::nodes::after_rename(state, DocId(node.0)).await;
    }
    if moved.left_behind > 0 {
        state.sources().wake(plan.library);
    }
    state.events().emit(NewEvent {
        kind: "source.moved",
        actor: Some(actor.id),
        subject: None,
        document: (plan.kind == NodeKind::Document).then_some(DocId(node.0)),
        node: Some(plan.library),
        detail: json!({
            "node": node.0.to_string(),
            "objects": moved.objects,
            "left_behind": moved.left_behind,
        }),
    });
    Ok(Restructured {
        node,
        left_behind: moved.left_behind,
    })
}

/// The tree's half of [`rename_or_move`]: the rename, then the move. A move refused after the
/// rename takes the rename back, so the tree is as it was. The open files below a moved node
/// are re-checked whatever the answer.
async fn restructure_tree(
    state: &AppState,
    actor: &User,
    node: NodeId,
    plan: &Restructure,
) -> Result<(), ApiError> {
    let wrong_extension = |e: AccessError| match e {
        AccessError::WrongExtension => ApiError::WrongExtension,
        e => e.into(),
    };
    if let Some(name) = &plan.name {
        let renamed = TreeOp::Rename {
            id: node,
            name: name.clone(),
        };
        tree::apply(state, actor, renamed)
            .await
            .map_err(wrong_extension)?;
    }
    if let Some(parent) = plan.parent {
        let moved = tree::apply(state, actor, TreeOp::Move { id: node, parent }).await;
        crate::api::recheck_below(state, node).await;
        if let Err(e) = moved {
            if plan.name.is_some() {
                let back = TreeOp::Rename {
                    id: node,
                    name: plan.old_name.clone(),
                };
                if let Err(undo) = tree::apply(state, actor, back).await {
                    eprintln!("sharecraft-server: a rename could not be taken back: {undo}");
                }
            }
            return Err(e.into());
        }
    }
    Ok(())
}

/// Moves the objects of a step the tree then refused back where they were, best effort. What
/// cannot be moved back stays at the new place, and a listing is woken to show it.
async fn move_back(state: &AppState, target: &S3Target, plan: &Restructure, moved: &Moved) {
    let known: Vec<(String, String)> = moved
        .seen
        .iter()
        .map(|(path, seen)| (path.clone(), seen.tag.clone()))
        .collect();
    let back = move_item(
        state.s3(),
        target,
        &plan.to,
        &plan.from,
        plan.folder(),
        &known,
    )
    .await;
    match back {
        Ok(back) if back.left_behind == 0 && moved.left_behind == 0 => {}
        Ok(_) => state.sources().wake(plan.library),
        Err(error) => {
            eprintln!(
                "sharecraft-server: objects moved for a refused change could not be moved back: \
                 {error:?}"
            );
            state.sources().wake(plan.library);
        }
    }
}

/// Records a move made in the bucket and the tree: the tracking rows take the new paths, and
/// each moved file the tag its copy was answered with (never one computed here: the next
/// listing must find the same), keeping its revision and the content already read. A row that
/// cannot be recorded is left to the listing, which is woken.
async fn record_moved(state: &AppState, plan: &Restructure, moved: &Moved) {
    let db = state.db();
    if let Err(e) = db
        .move_source_items(plan.library, &plan.from, &plan.to)
        .await
    {
        eprintln!("sharecraft-server: the tracking of a moved item could not be moved: {e}");
        state.sources().wake(plan.library);
        return;
    }
    // The shown paths, as the listing will show them: the item's own (a folder's has no `/`
    // for the rows below to share) and everything below it.
    let reshown: Vec<SourceItem> = plan
        .items
        .iter()
        .filter_map(|item| {
            Some(SourceItem {
                node: item.node,
                path: plan.reshown(&item.path)?,
                repository_path: plan.rebased(&item.repository_path),
                blob_sha: item.blob_sha.clone(),
                size: item.size,
            })
        })
        .collect();
    if let Err(e) = db.upsert_source_items(plan.library, &reshown).await {
        eprintln!("sharecraft-server: the shown paths of a moved item could not be recorded: {e}");
        state.sources().wake(plan.library);
    }
    let before: HashMap<String, &SourceItem> = plan
        .items
        .iter()
        .map(|item| (plan.rebased(&item.repository_path), item))
        .collect();
    for (path, seen) in &moved.seen {
        let Some(item) = before.get(path) else {
            continue;
        };
        let Some(old) = &item.blob_sha else {
            // A folder marker: a folder has no tag.
            continue;
        };
        // The same content under a new tag: no new revision, so an open editor's base holds.
        match db
            .retag_source_item(item.node, old, &seen.tag, seen.size, seen.modified)
            .await
        {
            Ok(true) => {}
            Ok(false) => state.sources().wake(plan.library),
            Err(e) => {
                eprintln!("sharecraft-server: the tag of a moved file could not be recorded: {e}");
                state.sources().wake(plan.library);
            }
        }
    }
}

/// Blocks the writes of `library` after the store refused one (a write by the worker, or a new
/// folder). A real change is announced and the open files below are re-checked, so editing
/// seats narrow to read-only at once.
async fn block(state: &AppState, library: NodeId, probed: Option<OffsetDateTime>) {
    state.writes().note_blocked(library, probed);
    match state
        .db()
        .set_write_state(library, WriteState::Blocked, Some(BLOCKED_REASON))
        .await
    {
        Ok(true) => {
            emit(state, "source.write_blocked", library, None, json!({}));
            crate::api::recheck_below(state, library).await;
        }
        Ok(false) => {}
        Err(e) => eprintln!("sharecraft-server: cannot block a library's writes: {e}"),
    }
}

/// The pending write of `node` in `library` that `actor` may retry or discard: `404` when the
/// person cannot see the node or it has no pending change in that library; `403` unless the
/// person is the change's author or may manage access to the library (an owner).
async fn own_write(
    state: &AppState,
    actor: &User,
    library: NodeId,
    node: NodeId,
) -> Result<PendingWrite, ApiError> {
    let db = state.db();
    let unavailable = |e: records::DbError| ApiError::unavailable(&e);
    db.effective_access(actor.id, node)
        .await
        .map_err(unavailable)?
        .ok_or(ApiError::NotFound)?;
    let row = db
        .pending_write(node)
        .await
        .map_err(unavailable)?
        .filter(|row| row.library == library)
        .ok_or(ApiError::NotFound)?;
    if row.by == actor.id {
        return Ok(row);
    }
    let owner = db
        .effective_access(actor.id, library)
        .await
        .map_err(unavailable)?
        .is_some_and(|held| held.can_manage_access());
    if !owner {
        return Err(ApiError::Forbidden);
    }
    Ok(row)
}

/// Retry: makes the pending write of `node` due now and wakes the writer, for its author or an
/// owner of `library`. `404` without a pending change (or for a person who cannot see it),
/// `403` for anyone else.
pub(crate) async fn retry(
    state: &AppState,
    actor: &User,
    library: NodeId,
    node: NodeId,
) -> Result<(), ApiError> {
    own_write(state, actor, library, node).await?;
    let found = state
        .db()
        .retry_write(node, OffsetDateTime::now_utc())
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
    if !found {
        return Err(ApiError::NotFound);
    }
    state.writes().wake();
    Ok(())
}

/// Discard: withdraws the pending write of `node`, for its author or an owner of `library`.
/// A file the bucket has shows the bucket's content again, with a new revision; a new file
/// never written is deleted (to the recycle bin, as the person, and in slice 1 it cannot be
/// restored into this library). Nothing in the bucket is changed. The library's source lock is
/// taken first (`409 source_busy` after ten seconds), so a write in progress finishes first: if
/// it wrote the change meanwhile there is nothing left to discard (`404`). Records
/// `source.write_discarded {node}`.
pub(crate) async fn discard_write(
    state: &AppState,
    actor: &User,
    library: NodeId,
    node: NodeId,
) -> Result<(), ApiError> {
    own_write(state, actor, library, node).await?;
    let db = state.db();
    let unavailable = |e: records::DbError| ApiError::unavailable(&e);
    let lock = lock_source(db, library).await?;
    // Read again under the lock: the worker may have written it, or a newer change may have
    // replaced the one the person saw.
    let row = db
        .pending_write(node)
        .await
        .map_err(unavailable)?
        .ok_or(ApiError::NotFound)?;
    let tracked = db
        .source_item_of(node)
        .await
        .map_err(unavailable)?
        .is_some();
    if !tracked {
        // A new file goes with its change: the person must be allowed to delete it, which is
        // judged before anything is withdrawn, so a refusal leaves the change untouched.
        let access = db
            .effective_access(actor.id, node)
            .await
            .map_err(unavailable)?
            .ok_or(ApiError::NotFound)?;
        tree::require(&access, Perms::DELETE)?;
    }
    if !db
        .drop_write(node, &row.blob.sha256)
        .await
        .map_err(unavailable)?
    {
        return Err(ApiError::NotFound);
    }
    if !tracked {
        // A new file the bucket never got: it goes, with nothing of it left to write. The
        // lock is held until it has gone, so no newer change is staged for it meanwhile.
        crate::api::nodes::delete_one(state, actor, node).await?;
    }
    drop(lock);
    state.events().emit(NewEvent {
        kind: "source.write_discarded",
        actor: Some(actor.id),
        subject: None,
        document: Some(DocId(node.0)),
        node: Some(library),
        detail: json!({ "node": node.0.to_string() }),
    });
    Ok(())
}
