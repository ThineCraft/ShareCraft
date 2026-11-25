//! The types every write to an external library shares, the guard that says where a change
//! goes, and the names a written copy gets; staging, the checkpointer of bucket files and the
//! write worker.
//!
//! A change accepted for an external library is staged: its content is pinned in the
//! external-content cache and a pending row names it ([`stage`]). From then on it is the file's
//! current revision, served from the cache. The write worker ([`write_due`]) writes each
//! pending row to the store once its content has passed the malware scan. It holds the library's
//! source lock to check the row and again to record what was written, never while the content
//! travels (see [`attempt`]), so a long upload keeps no listing and no rename, move, delete or
//! copy of another item of the library waiting. A file changed in the store since it was read is
//! kept, and the change becomes a named copy beside it; a new file whose name was taken
//! meanwhile is written under a numbered name. A failed write is tried again with a growing
//! wait; a refused one blocks the library until a write probe passes again.
//!
//! The store-specific code is in `s3_write.rs`, the only module that changes a bucket.

use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use collab_core::ids::{DocId, NodeId, UserId};
use records::{
    Access, AccessError, BinStage, Db, Editor, ExternalEntry, NewEvent, NewPendingWrite, Node,
    NodeKind, PendingWrite, Perms, QuotaError, ScanState, SourceItem, SourceKind, SourceLock,
    SourceMode, Tri, User, WriteState, object_tag,
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
use crate::s3::{COPY_MAX, PutBody, S3Error, S3Target};
use crate::s3_source::nameable_segment;
use crate::s3_write::{
    LAST_NUMBER, Ledger, Moved, copy_item, delete_held, hold, holding_root, make_folder, move_item,
    name_free, object_key, probe_writes, remove_copies, split_path, sweep_holding, unhold,
    write_object_noted,
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
    /// The file is larger than a change to the library may be: 5 GiB, the server's largest
    /// file or half the external-content cache, whichever is smallest.
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
const COPY_FAILED: &str = "The file changed in the storage meanwhile, and the copy that keeps \
                           this change could not be made. Saving is tried again later.";
/// How long a change waits for its scan or a listing before it is looked at again.
const HELD_SECONDS: i64 = 5;
/// How long the write worker waits for the library's source lock to record what a transfer
/// did (a listing pass or a structure change holds it meanwhile). Past it, the row stays
/// pending and is looked at again after [`HELD_SECONDS`]: its content is found in the store and
/// recorded then, never sent twice. Ten seconds, as a request waits for the lock.
const FINISH_WAIT: Duration = Duration::from_secs(10);

/// What [`stage`] stages: the content `blob` (already in the content store, held by the caller)
/// as the new state of `node`, at the library-relative `path` of the S3 library `library`.
pub(crate) struct StageRequest<'a> {
    pub library: NodeId,
    pub node: NodeId,
    pub path: &'a str,
    pub blob: BlobRef,
    /// The tag the file was last seen with; `None` for a new file.
    pub base_tag: Option<&'a str>,
    /// The revision the change was made on, for an editor's save (see
    /// [`NewPendingWrite::seen_revision`]).
    pub seen_revision: Option<&'a str>,
    pub by: UserId,
    /// Write it at once rather than after the coalescing wait.
    pub flush: bool,
}

/// Why a change was not staged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StageError {
    /// The library's writes are blocked.
    Blocked,
    /// The content is larger than a change to the library may be (see [`write_limit`]).
    TooLarge,
    /// The library no longer takes changes, or the records failed.
    Failed,
}

/// The largest file a change to an external library may be: the smallest of 5 GiB (the most
/// ShareCraft can move inside a bucket afterwards), the server's largest file and half the
/// external-content cache. Files over one `PUT` are written in parts.
pub(crate) async fn write_limit(state: &AppState) -> Result<u64, records::DbError> {
    let settings = state.db().share_settings().await?;
    let cache = state.shares().cache_max_bytes(settings.cache_max_bytes);
    Ok(COPY_MAX.min(state.config().max_file_bytes).min(cache / 2))
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
            seen_revision: s.seen_revision,
            by: s.by,
            flush: s.flush,
            now,
        })
        .await
        .map_err(|e| match e {
            // Switched to read-only (or disconnected) since the guard looked, or the file went
            // to the recycle bin meanwhile.
            AccessError::LibraryReadOnly | AccessError::NotFound => StageError::Failed,
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
        seen_revision: None,
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

    /// The revision as the records hold it.
    fn revision_text(&self) -> Option<&str> {
        match (&self.pending, &self.item) {
            (Some(pending), _) => Some(&pending.revision),
            (None, Some(item)) => Some(&item.revision),
            (None, None) => None,
        }
    }

    fn revision(&self) -> Result<Option<RevisionId>, CheckpointError> {
        let Some(text) = self.revision_text() else {
            return Ok(None);
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
            // The path and tag read here may be moved on before the stage (a rename or move
            // in the bucket): the stage takes them from the tracking row, and from its tag while
            // the file is still at this revision.
            let request = StageRequest {
                library,
                node,
                path: &path,
                blob,
                base_tag: base_tag.as_deref(),
                seen_revision: now.revision_text(),
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
        self.flush_last_save(doc);
    }

    /// The same flush as when the session ends, as soon as the last person has left, so the last
    /// save does not wait out the room's grace period.
    fn room_emptied(&self, doc: DocId) {
        self.flush_last_save(doc);
    }
}

impl ExternalCheckpointer {
    fn flush_last_save(&self, doc: DocId) {
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
    /// When the holding sweep last ran for each library (see [`sweep_holds`]).
    swept: Mutex<HashMap<NodeId, OffsetDateTime>>,
    paused: AtomicBool,
    /// [`is_bucket_file`] answers a failure, as a database outage would. For tests.
    bucket_lookup_fails: AtomicBool,
    /// How long a finish waits for the source lock, when not [`FINISH_WAIT`]. For tests.
    finish_wait: Mutex<Option<Duration>>,
}

impl SourceWrites {
    /// Makes the worker wait `wait` (rather than [`FINISH_WAIT`]) for the library's source lock
    /// to record what a transfer did. For tests.
    pub fn set_finish_wait_for_tests(&self, wait: Duration) {
        *self
            .finish_wait
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(wait);
    }

    fn finish_wait(&self) -> Duration {
        self.finish_wait
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .unwrap_or(FINISH_WAIT)
    }

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

    /// Whether the holding sweep of `library` is due at `now` (it never ran, or ran a day or
    /// more before); if so, it is noted as run at `now`.
    fn sweep_due(&self, library: NodeId, now: OffsetDateTime) -> bool {
        let mut swept = self.swept.lock().unwrap_or_else(PoisonError::into_inner);
        if swept
            .get(&library)
            .is_some_and(|last| now - *last < SWEEP_EVERY)
        {
            return false;
        }
        swept.insert(library, now);
        true
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
/// content is available after the malware scan and when no listing holds its library as it
/// starts (see [`attempt`]); then
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
    // A conflict hands the content on to one more write in the same turn: the copy beside a
    // file changed in the store, or a new file under its numbered name. A second conflict is
    // left for the next turn (its row stays due).
    let mut acted = false;
    let mut next = Some(due);
    for _ in 0..2 {
        let Some(row) = next.take() else {
            break;
        };
        match attempt(state, &row, now).await {
            Attempt::Untouched => {}
            Attempt::Acted => acted = true,
            Attempt::Then(follow) => {
                acted = true;
                next = Some(*follow);
            }
        }
    }
    acted
}

/// What one [`attempt`] came to.
enum Attempt {
    /// Nothing was done (held back, withdrawn or changed meanwhile, or the records failed).
    Untouched,
    /// Written, written as a copy, failed or withdrawn.
    Acted,
    /// A conflict was settled and this row (the copy, or the file under its new name) is to be
    /// written next.
    Then(Box<PendingWrite>),
}

/// One write of `due`, in three steps, so that no byte travels while the library's source lock
/// is held (a rename, move, delete, restore or copy of any other item of the library would wait
/// for it, and give up with `409 source_busy`):
///
/// 1. Under the lock, briefly: the row is read again and the checks of [`ready`] are made. A
///    listing or a structure change holding the lock holds the row back a few seconds.
/// 2. Without the lock: the content is sent ([`write_object_noted`], a `PUT` or a multipart
///    upload). The path cannot change meanwhile: a rename, move, delete or copy of the file, or
///    of a folder with it inside, is refused with `409 source_item_saving` while its row is
///    there, and a listing leaves a file with a row alone (a tracked one keeps its recorded
///    entry; a new one's path is dropped from the listing).
/// 3. Under the lock again, waiting for it up to [`FINISH_WAIT`]: what was sent is recorded
///    ([`record_finish`]) or the conflict settled ([`conflict`]). A listing reads the recorded
///    items and the pending rows inside one locked pass, so it never sees a row gone without its
///    recorded tag. When the lock cannot be had, the row stays pending: the next attempt finds
///    its content in the store and records it, with nothing sent twice.
///
/// One worker runs writes ([`start_writer`]), one row at a time, so a row is never sent twice
/// at once though the lock is not held while it travels.
async fn attempt(state: &AppState, due: &PendingWrite, now: OffsetDateTime) -> Attempt {
    let db = state.db();
    // A listing holds the lock for its whole pass, a structure change for its own steps: the
    // row is looked at again shortly.
    let lock = match db.try_lock_source(due.library).await {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            defer(state, due, now).await;
            return Attempt::Untouched;
        }
        Err(e) => {
            eprintln!("sharecraft-server: cannot take a library's source lock: {e}");
            return Attempt::Untouched;
        }
    };
    let row = match db.pending_write(due.node).await {
        Ok(Some(row)) if row.blob == due.blob => row,
        // Withdrawn, written, or changed meanwhile: the new row is looked at next time.
        _ => return Attempt::Untouched,
    };
    let checked = ready(state, &row, now).await;
    drop(lock);
    let Ready {
        target,
        conditional,
        held,
    } = match checked {
        Ok(ready) => ready,
        Err(outcome) => return outcome,
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
    let ledger = ledger_of(state, &row, now);
    let written = write_object_noted(
        state.s3(),
        &target,
        &row.path,
        &body,
        &base,
        conditional,
        Some(&ledger),
    )
    .await;
    let outcome = match written {
        Ok(Written::Done(seen)) => {
            record_finish(state, &row, &seen, conditional, now).await;
            Attempt::Acted
        }
        Ok(Written::Conflict) => conflict(state, &target, &row, &body, now).await,
        Err(error) => {
            // Only the row's own state and retry time change: no lock is needed.
            failed_write(state, &row, error, now).await;
            Attempt::Acted
        }
    };
    drop(held);
    outcome
}

/// What a transfer needs, once [`ready`] has judged the row writable.
struct Ready {
    target: S3Target,
    conditional: Tri,
    /// Keeps the staged content in the store until the transfer is over.
    held: Held,
}

/// The checks before a transfer, made under the library's source lock just before it is given
/// up for the transfer: the connection is still S3 in read and write mode, its writes are not
/// blocked, its endpoint still approved (the target), what the probe found about conditional
/// writes, and the staged content is here.
///
/// They stay under the lock, though none of them sends a byte, because the lock is what proves
/// that no structure change is half done at this moment: a rename or move that began before the
/// row was staged may be copying objects, and it moves the row along when it is done; the row
/// read again under the lock (in [`attempt`]) therefore has the path the file has now. From the
/// moment the lock is given up, the row itself keeps every structure change away from the file
/// (`409 source_item_saving`). The connection checks need no lock to stay true during the
/// transfer: the mode cannot become read-only and the library cannot be disconnected while the
/// row is there (both are refused with changes pending), and a block or a change of endpoint
/// that comes during the transfer makes the transfer fail or is seen at the next attempt.
///
/// `Err` carries what the attempt came to when the row is not written now.
async fn ready(
    state: &AppState,
    row: &PendingWrite,
    now: OffsetDateTime,
) -> Result<Ready, Attempt> {
    let db = state.db();
    match db.source_of(row.library).await {
        Ok(Some((config, _)))
            if config.kind == SourceKind::S3 && config.mode == SourceMode::ReadWrite => {}
        Ok(_) => {
            fail(state, row, now, READ_ONLY_NOW).await;
            return Err(Attempt::Acted);
        }
        Err(e) => {
            eprintln!("sharecraft-server: cannot read a library's connection: {e}");
            return Err(Attempt::Untouched);
        }
    }
    match db.write_state(row.library).await {
        Ok(Some((WriteState::Blocked, _))) => {
            state.writes().note_blocked(row.library, None);
            return Err(Attempt::Untouched);
        }
        Ok(_) => {}
        Err(e) => {
            eprintln!("sharecraft-server: cannot read a library's write state: {e}");
            return Err(Attempt::Untouched);
        }
    }
    let target = match state.s3().target(state, row.library).await {
        Ok(target) => target,
        Err(S3Error::AddressRefused) => {
            fail(state, row, now, NOT_APPROVED).await;
            return Err(Attempt::Acted);
        }
        Err(_) => {
            fail(state, row, now, &sentence(WriteError::Offline)).await;
            return Err(Attempt::Acted);
        }
    };
    let conditional = match db.s3_place(row.library).await {
        Ok(place) => place.map_or(Tri::Unknown, |place| place.conditional),
        Err(e) => {
            eprintln!("sharecraft-server: cannot read a library's place: {e}");
            return Err(Attempt::Untouched);
        }
    };
    let Some(held) = state.blobs().hold_stored(row.blob).await else {
        fail(state, row, now, CONTENT_MISSING).await;
        return Err(Attempt::Acted);
    };
    Ok(Ready {
        target,
        conditional,
        held,
    })
}

/// The library's source lock for recording what a transfer did, waiting up to
/// [`FINISH_WAIT`] (as set for tests) for a listing or a structure change to give it up; `None`
/// after that, or when the records fail.
async fn finish_lock(state: &AppState, library: NodeId) -> Option<SourceLock> {
    let deadline = tokio::time::Instant::now() + state.writes().finish_wait();
    loop {
        match state.db().try_lock_source(library).await {
            Ok(Some(lock)) => return Some(lock),
            Ok(None) => {}
            Err(e) => {
                eprintln!("sharecraft-server: cannot take a library's source lock: {e}");
                return None;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Records that the row's content is in the store, as `seen`, under the library's source lock
/// ([`finish_lock`]). The row is read again first: if it was withdrawn meanwhile (a discard
/// during the transfer, perhaps followed by a new save, which is a new row with a new first
/// staging time), nothing is recorded, and the next listing sees the object as a change made in
/// the store, which is taken, never overwritten. A newer save of the same row is recorded as
/// [`Db::finish_write`] does (the row stays, based on what was written).
///
/// Without the lock the row stays pending, held back a few seconds, and is noted as based on
/// what was written ([`Db::rebase_write`]): the next attempt's first `HEAD` finds its own content
/// and records the finish with nothing sent again, and a newer save of the row meanwhile is
/// based on this content rather than taken for a conflict.
async fn record_finish(
    state: &AppState,
    row: &PendingWrite,
    seen: &Seen,
    conditional: Tri,
    now: OffsetDateTime,
) {
    let db = state.db();
    let Some(lock) = finish_lock(state, row.library).await else {
        if let Err(e) = db
            .rebase_write(row.node, row.first_staged_at, &seen.tag)
            .await
        {
            eprintln!("sharecraft-server: cannot note a write whose finish waits: {e}");
        }
        defer(state, row, now).await;
        return;
    };
    match db.pending_write(row.node).await {
        Ok(Some(current)) if current.first_staged_at == row.first_staged_at => {
            finished(state, row, seen, conditional).await;
        }
        Ok(_) => eprintln!(
            "sharecraft-server: a change was withdrawn while it was being written; the next \
             listing finds what was written"
        ),
        // The next attempt's first look finds our content there and finishes it.
        Err(e) => eprintln!("sharecraft-server: cannot read a written change again: {e}"),
    }
    drop(lock);
}

/// Where a multipart upload of the row's content is recorded while it is open.
fn ledger_of<'a>(state: &'a AppState, row: &PendingWrite, now: OffsetDateTime) -> Ledger<'a> {
    Ledger {
        db: state.db(),
        library: row.library,
        node: row.node,
        now,
    }
}

/// Records that the row's content is at its path in the store, as `seen`; the source lock is
/// held.
async fn finished(state: &AppState, row: &PendingWrite, seen: &Seen, conditional: Tri) {
    let db = state.db();
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
        // The next attempt's first look finds our content there and finishes it (a newer save
        // of the row meanwhile is then taken for a conflict: the records were failing).
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
/// nothing is overwritten. The conflict is settled under the library's source lock
/// ([`finish_lock`]) and sends no content itself: a tracked file's change becomes a new file
/// beside it ([`beside`]), and a new file moves to a numbered name ([`numbered`]); either is
/// then written as an ordinary new file, so its bytes travel without the lock and a listing
/// meanwhile leaves its path alone. Without the lock the row is held back a few seconds and the
/// next attempt meets the same conflict.
async fn conflict(
    state: &AppState,
    target: &S3Target,
    row: &PendingWrite,
    body: &PutBody,
    now: OffsetDateTime,
) -> Attempt {
    let Some(lock) = finish_lock(state, row.library).await else {
        defer(state, row, now).await;
        return Attempt::Acted;
    };
    let outcome = conflict_locked(state, target, row, body, now).await;
    drop(lock);
    outcome
}

/// [`conflict`], once the source lock is held.
async fn conflict_locked(
    state: &AppState,
    target: &S3Target,
    row: &PendingWrite,
    body: &PutBody,
    now: OffsetDateTime,
) -> Attempt {
    let db = state.db();
    // Read again: a newer save is looked at in its own turn, and a withdrawn one is gone.
    match db.pending_write(row.node).await {
        Ok(Some(current))
            if current.blob == row.blob && current.first_staged_at == row.first_staged_at => {}
        Ok(_) => return Attempt::Acted,
        Err(e) => {
            eprintln!("sharecraft-server: cannot read a change in conflict again: {e}");
            return Attempt::Acted;
        }
    }
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
            return Attempt::Acted;
        }
    };
    if tracked {
        beside(state, target, row, body, author.as_ref(), now).await
    } else {
        numbered(state, target, row, body, author.as_ref(), now).await
    }
}

/// A new file whose name was taken in the store meanwhile takes the first numbered name
/// (`name (2).ext` to `name (20).ext`) that is free in the store ([`name_free`]: nothing there,
/// or our own earlier attempt) and in its folder, and its change moves there (the row's path),
/// to be written next. A name that the store takes between this look and the write is a
/// conflict again, settled the same way. When every name is taken the row fails.
async fn numbered(
    state: &AppState,
    target: &S3Target,
    row: &PendingWrite,
    body: &PutBody,
    author: Option<&User>,
    now: OffsetDateTime,
) -> Attempt {
    let db = state.db();
    let (folder, name) = split_path(&row.path);
    for n in 2..=LAST_NUMBER {
        let candidate = numbered_name(name, n);
        let path = format!("{folder}{candidate}");
        match name_free(state.s3(), target, &path, body).await {
            Ok(true) => {}
            Ok(false) => continue,
            Err(error) => {
                failed_write(state, row, error, now).await;
                return Attempt::Acted;
            }
        }
        if let Some(author) = author {
            let renamed = TreeOp::Rename {
                id: row.node,
                name: candidate.clone(),
            };
            match tree::apply(state, author, renamed).await {
                Ok(_) => {}
                Err(AccessError::NameTaken) => continue,
                // The listing moves it to the name it has in the store.
                Err(e) => eprintln!("sharecraft-server: a new file could not take its name: {e}"),
            }
        }
        match db.repath_write(row.node, &row.blob.sha256, &path).await {
            Ok(true) => {}
            Ok(false) => return Attempt::Acted,
            Err(e) => {
                eprintln!(
                    "sharecraft-server: cannot record the name a new file is written under: {e}"
                );
                return Attempt::Acted;
            }
        }
        return match db.pending_write(row.node).await {
            Ok(Some(next)) if next.blob == row.blob => Attempt::Then(Box::new(next)),
            _ => Attempt::Acted,
        };
    }
    fail(state, row, now, &sentence(WriteError::Failed)).await;
    Attempt::Acted
}

/// A tracked file changed in the store since the change was based on it: the store's state is
/// kept, and the change becomes a new file beside it, credited to its author, named for the
/// author and `now` ([`sibling_name`], numbered `(2)` to `(20)` if taken) with the first name
/// free in the store ([`name_free`]) and in the folder. The copy's node is made and its content
/// staged as a new file's (to be written next); then the file's own change is withdrawn and the
/// file takes the store's state. Recorded as `source.conflict_copy {node, copy}`. When no copy
/// can be made the row fails and keeps its content.
async fn beside(
    state: &AppState,
    target: &S3Target,
    row: &PendingWrite,
    body: &PutBody,
    author: Option<&User>,
    now: OffsetDateTime,
) -> Attempt {
    let db = state.db();
    let Some(author) = author else {
        fail(state, row, now, COPY_FAILED).await;
        return Attempt::Acted;
    };
    let parent = match db.node(row.node).await {
        Ok(Some(node)) => node.parent,
        Ok(None) => None,
        Err(e) => {
            eprintln!("sharecraft-server: cannot read the file a copy is made beside: {e}");
            return Attempt::Acted;
        }
    };
    let Some(parent) = parent else {
        fail(state, row, now, COPY_FAILED).await;
        return Attempt::Acted;
    };
    let (folder, name) = split_path(&row.path);
    let first = sibling_name(name, &author.name, now);
    let mut names = vec![first.clone()];
    names.extend((2..=LAST_NUMBER).map(|n| numbered_name(&first, n)));
    for candidate in names {
        let path = format!("{folder}{candidate}");
        match name_free(state.s3(), target, &path, body).await {
            Ok(true) => {}
            Ok(false) => continue,
            Err(error) => {
                failed_write(state, row, error, now).await;
                return Attempt::Acted;
            }
        }
        let id = DocId(Uuid::new_v4());
        let editor = Editor::for_file_name(&candidate);
        let size = Some(row.blob.size);
        match tree::create_document(state, author, parent, id, &candidate, editor, size).await {
            Ok(_) => {}
            Err(AccessError::NameTaken) => continue,
            Err(e) => {
                eprintln!("sharecraft-server: the node of a copy could not be made: {e}");
                fail(state, row, now, COPY_FAILED).await;
                return Attempt::Acted;
            }
        }
        let copy = NodeId(id.0);
        let request = StageRequest {
            library: row.library,
            node: copy,
            path: &path,
            blob: row.blob,
            base_tag: None,
            seen_revision: None,
            by: row.by,
            flush: true,
        };
        if let Err(e) = stage(state, &request).await {
            eprintln!("sharecraft-server: the content of a copy could not be staged: {e:?}");
            if let Err(removed) = tree::apply(state, author, TreeOp::Delete { id: copy }).await {
                eprintln!(
                    "sharecraft-server: a copy with no content could not be removed: {removed}"
                );
            }
            fail(state, row, now, COPY_FAILED).await;
            return Attempt::Acted;
        }
        // The copy's row holds the content now; the file's own change is withdrawn.
        if let Err(e) = db.drop_write(row.node, &row.blob.sha256).await {
            eprintln!("sharecraft-server: cannot withdraw a change made into a copy: {e}");
        }
        taken_from_store(state, target, row).await;
        state.sources().wake(row.library);
        emit(
            state,
            "source.conflict_copy",
            row.library,
            Some(row.node),
            json!({ "node": row.node.0, "copy": copy.0 }),
        );
        return match db.pending_write(copy).await {
            Ok(Some(next)) => Attempt::Then(Box::new(next)),
            _ => Attempt::Acted,
        };
    }
    fail(state, row, now, &sentence(WriteError::Failed)).await;
    Attempt::Acted
}

/// The file of `row` is what the store holds now: its tracking row takes the store's tag.
async fn taken_from_store(state: &AppState, target: &S3Target, row: &PendingWrite) {
    let db = state.db();
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
        WriteError::TooLarge => "Files over 5 GiB cannot be saved to this S3 storage.",
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
pub(crate) fn emit(
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

/// How long a request that must hold a library's source lock waits for a listing, another
/// structure change or a write's checks or finish.
const LOCK_WAIT: Duration = Duration::from_secs(10);

/// The library's source lock, waiting up to [`LOCK_WAIT`] for a listing, another structure
/// change or the write worker (which holds it only to check a row and to record a finish, never
/// while content travels) to give it up (`409 source_busy` after that). Held, no listing runs,
/// no write starts and no write's finish is recorded in the library.
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

/// The tracking row of `node` in `library` and the rows of it and everything inside it, for a
/// step that copies it in the bucket (a rename, move or delete): `409 source_item_saving` when
/// it is a new file whose first write is still to come, or it or anything inside it has a
/// change not yet written; `409 source_action_unavailable` when the bucket does not know it;
/// `409 source_unavailable {reason: archived}` when anything of it is in archive storage, which
/// no copy can move (refused as reading it is, not as a change a later try might get past).
async fn tracked_subtree(
    db: &Db,
    library: NodeId,
    node: NodeId,
) -> Result<(SourceItem, Vec<SourceItem>), ApiError> {
    let unavailable = |e: records::DbError| ApiError::unavailable(&e);
    let Some(tracked) = db.source_item_of(node).await.map_err(unavailable)? else {
        // Not in the bucket yet: a new file whose first write is still to come.
        if db.pending_write(node).await.map_err(unavailable)?.is_some() {
            return Err(ApiError::SourceItemSaving);
        }
        return Err(ApiError::SourceActionUnavailable);
    };
    let from = tracked.repository_path.as_str();
    if db.writes_below(library, from).await.map_err(unavailable)? > 0 {
        return Err(ApiError::SourceItemSaving);
    }
    let items = subtree_rows(db, library, from).await?;
    if items
        .iter()
        .any(|i| i.blob_sha.as_deref().is_some_and(|tag| tag.ends_with("!a")))
    {
        return Err(ApiError::SourceUnavailable("archived", SourceKind::S3));
    }
    Ok((tracked, items))
}

/// The tracking rows of `library` at the repository path `from` and, for a folder (`from`
/// ending with `/`), below it.
async fn subtree_rows(db: &Db, library: NodeId, from: &str) -> Result<Vec<SourceItem>, ApiError> {
    let folder = from.ends_with('/');
    Ok(db
        .source_items(library)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
        .into_iter()
        .filter(|i| i.repository_path == from || (folder && i.repository_path.starts_with(from)))
        .collect())
}

/// What the tree recorded for `items`, as a bucket step checks it: library-relative paths and
/// tags (none for a folder).
fn known_of(items: &[SourceItem]) -> Vec<(String, String)> {
    items
        .iter()
        .map(|i| {
            (
                i.repository_path.clone(),
                i.blob_sha.clone().unwrap_or_default(),
            )
        })
        .collect()
}

/// Where `node` is for a step made in a bucket (a rename, move, delete or copy), judged as every
/// such step judges it: the read-and-write S3 library it is in (`404` for someone who cannot
/// see it, `409 source_read_only` or `409 source_write_blocked` as [`writable_place`] says, `409
/// source_action_unavailable` outside a library), a library itself `409 source_read_only`, and a
/// file or folder; with the node and the person's access to it, for the caller to judge the
/// right it needs.
async fn bucket_item(
    state: &AppState,
    actor: &User,
    node: NodeId,
) -> Result<(NodeId, Node, Access), ApiError> {
    let db = state.db();
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
    Ok((library, item, access))
}

/// Judges `destination` as the place `node` of `library` goes into (a move) or is copied into:
/// in that same library (`409 source_action_unavailable` anywhere else, `404` for someone who
/// cannot see it), a library or folder, `add` there, and never inside `node` itself.
async fn bucket_destination(
    state: &AppState,
    actor: &User,
    library: NodeId,
    destination: NodeId,
    node: NodeId,
) -> Result<(), ApiError> {
    let db = state.db();
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
        .map_err(|e| ApiError::unavailable(&e))?
    {
        return Err(AccessError::IntoItself.into());
    }
    Ok(())
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
    let (library, item, access) = bucket_item(state, actor, node).await?;
    tree::require(&access, Perms::EDIT)?;
    let current = item.parent.ok_or(ApiError::SourceActionUnavailable)?;
    let destination = parent.unwrap_or(current);
    if destination != current {
        bucket_destination(state, actor, library, destination, node).await?;
    }
    let new_name = match name {
        Some(name) => checked_name(db, &item, name).await?,
        None => item.name.clone(),
    };
    if new_name == item.name && destination == current {
        return Ok(None);
    }
    let (tracked, items) = tracked_subtree(db, library, node).await?;
    let from = tracked.repository_path;
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
    let known = known_of(&plan.items);
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
///
/// Runs under the library's source lock, so no write of the library runs meanwhile. The
/// pending writes below the item move with its tracking rows, and a write based on a file's old
/// tag is based on its copy's: a save staged while the bucket step ran is written at the new
/// place, on the moved object (see [`Db::move_source_items`] and [`Db::retag_source_item`]).
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
    retag_copies(
        state,
        plan.library,
        &plan.items,
        |path| plan.rebased(path),
        moved,
    )
    .await;
}

/// Gives each tracked file of `items` the tag its copy in `moved` was answered with, found at
/// `at(its repository path)`, keeping its revision and content (and moving the base of a
/// pending write along). A row that cannot be retagged is left to the listing, which is woken.
async fn retag_copies(
    state: &AppState,
    library: NodeId,
    items: &[SourceItem],
    at: impl Fn(&str) -> String,
    moved: &Moved,
) {
    let db = state.db();
    let before: HashMap<String, &SourceItem> = items
        .iter()
        .map(|item| (at(&item.repository_path), item))
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
            Ok(false) => state.sources().wake(library),
            Err(e) => {
                eprintln!("sharecraft-server: the tag of a moved file could not be recorded: {e}");
                state.sources().wake(library);
            }
        }
    }
}

/// A copy of an item of a bucket, judged and ready to be made.
struct CopyPlan {
    /// The copy's name, always given to the tree so it is the one the paths below are made of.
    name: String,
    /// The bucket step: `from` the item, `to` its copy, and the tracking rows of the item.
    plan: Restructure,
    /// Library-relative paths the copier may not view (a folder's ends `/`), left out.
    hidden: Vec<String>,
}

/// Everything about copying `node` into `parent` (as `name` or as it is) inside its
/// read-and-write S3 library that is judged before the bucket is asked: where the node and the
/// parent are (see [`bucket_item`] and [`bucket_destination`]), `view` on the node and `add` on
/// the parent (as [`tree::apply`] judges a copy), the folders setting for a folder, a name the
/// listing shows and the bucket takes (`422 invalid_name`), `409 source_item_saving` when the
/// item or anything inside it has a change not yet written, an item in archive storage (`409
/// source_unavailable {reason: archived}`), and a name free at the parent ignoring case (`409
/// name_taken`: a copy is never numbered).
async fn copy_allowed(
    state: &AppState,
    actor: &User,
    node: NodeId,
    parent: NodeId,
    name: Option<&str>,
) -> Result<CopyPlan, ApiError> {
    let db = state.db();
    let unavailable = |e: records::DbError| ApiError::unavailable(&e);
    let (library, item, access) = bucket_item(state, actor, node).await?;
    tree::require(&access, Perms::VIEW)?;
    bucket_destination(state, actor, library, parent, node).await?;
    if item.kind == NodeKind::Folder && folders_off(db, parent).await? {
        return Err(AccessError::FoldersOff.into());
    }
    let asked = name.map_or(item.name.as_str(), str::trim);
    let new_name = match checked_name(db, &item, asked).await {
        Err(ApiError::WrongExtension) => return Err(ApiError::InvalidName),
        checked => checked?,
    };
    let (tracked, mut items) = tracked_subtree(db, library, node).await?;
    // What the person may not view is left out of the copy, as the ordinary copy leaves it out:
    // a hidden folder takes everything under it with it.
    let mut hidden: Vec<String> = Vec::new();
    for item in items.iter().filter(|i| i.node != node) {
        if crate::s3_write::skipped(&item.repository_path, &hidden) {
            continue;
        }
        let viewable = db
            .effective_access(actor.id, item.node)
            .await
            .map_err(unavailable)?
            .is_some_and(|a| a.perms.contains(Perms::VIEW));
        if !viewable {
            hidden.push(item.repository_path.clone());
        }
    }
    items.retain(|i| !crate::s3_write::skipped(&i.repository_path, &hidden));
    let (_, path) = external_path(state, parent, &new_name).await?;
    if !db
        .name_free(parent, &new_name, None)
        .await
        .map_err(unavailable)?
    {
        return Err(AccessError::NameTaken.into());
    }
    let folder = item.kind == NodeKind::Folder;
    let to = if folder { format!("{path}/") } else { path };
    let shown_to = shown_in(db, library, parent, &new_name).await?;
    Ok(CopyPlan {
        name: new_name.clone(),
        hidden,
        plan: Restructure {
            library,
            kind: item.kind,
            name: Some(new_name),
            old_name: item.name,
            parent: Some(parent),
            from: tracked.repository_path,
            to,
            shown_from: tracked.path,
            shown_to,
            items,
        },
    })
}

/// Copies `node` into `parent` inside its read-and-write S3 library, for `actor`, named `name`
/// or as it is: in the bucket first (every object copied by the store itself, on the tag
/// ShareCraft recorded, so no byte is downloaded), then in the tree (new nodes with ids of their
/// own), then in the tracking rows, with the tags of the copies as the store answered them, so
/// the next listing sees nothing new. Each copied file has a revision of its own, and its
/// content is read when it is first opened. Answers the new node.
///
/// Everything that can be refused is judged before the bucket is asked (see [`copy_allowed`]),
/// and judged again once the library's source lock is held; the lock is held over every bucket
/// call and until the rows are recorded (`409 source_busy` after ten seconds). If the tree
/// refuses after the bucket was copied, the copies are deleted (best effort; one that cannot be
/// deleted is a new file at the next listing, which is woken) and the tree's error is answered.
/// Records `source.moved {node, from, objects, left_behind: 0, kind: "copy"}`.
///
/// # Errors
/// As [`copy_allowed`]; `409 source_too_many_objects {count}`, `409 source_too_large_to_move`,
/// `409 source_changed` (a listing is woken), `409 name_taken`, `409 source_write_blocked` (the
/// store refused: the library is blocked) and `source_unavailable` from the bucket.
pub(crate) async fn copy_within(
    state: &AppState,
    actor: &User,
    node: NodeId,
    parent: NodeId,
    name: Option<&str>,
) -> Result<NodeId, ApiError> {
    let db = state.db();
    let first = copy_allowed(state, actor, node, parent, name).await?;
    let lock = lock_source(db, first.plan.library).await?;
    // Judged again once the lock is held: a listing or a write may have run meanwhile.
    let CopyPlan { name, plan, hidden } = copy_allowed(state, actor, node, parent, name).await?;
    let target = bucket(state, plan.library).await?;
    let known = known_of(&plan.items);
    let copied = match copy_item(
        state.s3(),
        &target,
        &plan.from,
        &plan.to,
        plan.folder(),
        &known,
        &hidden,
    )
    .await
    {
        Ok(copied) => copied,
        Err(error) => return Err(refusal(state, plan.library, error).await),
    };
    let applied = tree::apply(
        state,
        actor,
        TreeOp::Copy {
            id: node,
            parent,
            name: Some(name),
        },
    )
    .await;
    let root = match applied {
        Ok(applied) => applied.node,
        Err(e) => {
            if remove_copies(state.s3(), &target, &copied.seen).await > 0 {
                state.sources().wake(plan.library);
            }
            return Err(match e {
                AccessError::WrongExtension => ApiError::InvalidName,
                e => e.into(),
            });
        }
    };
    record_copied(state, &plan, node, root.id, &copied).await;
    drop(lock);
    state.events().emit(NewEvent {
        kind: "source.moved",
        actor: Some(actor.id),
        subject: None,
        document: (plan.kind == NodeKind::Document).then_some(DocId(root.id.0)),
        node: Some(plan.library),
        detail: json!({
            "node": root.id.0.to_string(),
            "from": node.0.to_string(),
            "objects": copied.objects,
            "left_behind": 0,
            "kind": "copy",
        }),
    });
    Ok(root.id)
}

/// Records a copy made in the bucket and the tree: each new node (the tree made them with ids
/// of their own, below `root`, with the names of the originals below `from`) is tracked at the
/// path of its copy, a folder as the listing will show it and a file with the tag its copy was
/// answered with (never one computed here: the next listing must find the same) and a revision
/// of its own. A row that cannot be recorded is left to the listing, which is woken.
async fn record_copied(
    state: &AppState,
    plan: &Restructure,
    from: NodeId,
    root: NodeId,
    copied: &Moved,
) {
    let db = state.db();
    // Which new node is the copy of which old one: the same names, level by level.
    let mut twins: HashMap<NodeId, NodeId> = HashMap::from([(from, root)]);
    let mut level = vec![(from, root)];
    while let Some((old, new)) = level.pop() {
        let (olds, news) = match (db.live_children(old).await, db.live_children(new).await) {
            (Ok(olds), Ok(news)) => (olds, news),
            (Err(e), _) | (_, Err(e)) => {
                eprintln!("sharecraft-server: the nodes of a copy could not be read: {e}");
                state.sources().wake(plan.library);
                return;
            }
        };
        for child in olds {
            if let Some(twin) = news.iter().find(|n| n.name == child.name) {
                twins.insert(child.id, twin.id);
                level.push((child.id, twin.id));
            }
        }
    }
    let seen: HashMap<&str, &Seen> = copied
        .seen
        .iter()
        .map(|(path, seen)| (path.as_str(), seen))
        .collect();
    let mut folders = Vec::new();
    let mut files = Vec::new();
    for item in &plan.items {
        let (Some(node), Some(path)) = (twins.get(&item.node), plan.reshown(&item.path)) else {
            // Not copied as a node (out of the person's sight): the listing adds it.
            state.sources().wake(plan.library);
            continue;
        };
        let repository_path = plan.rebased(&item.repository_path);
        if item.blob_sha.is_none() {
            folders.push(SourceItem {
                node: *node,
                path,
                repository_path,
                blob_sha: None,
                size: item.size,
            });
            continue;
        }
        let Some(seen) = seen.get(repository_path.as_str()) else {
            state.sources().wake(plan.library);
            continue;
        };
        files.push(ExternalEntry {
            node: *node,
            path,
            repository_path,
            size: seen.size,
            mtime_ns: i64::try_from(seen.modified.unix_timestamp_nanos()).unwrap_or(0),
            tag: Some(seen.tag.clone()),
        });
    }
    if let Err(e) = db.upsert_source_items(plan.library, &folders).await {
        eprintln!("sharecraft-server: the folders of a copy could not be tracked: {e}");
        state.sources().wake(plan.library);
    }
    for entry in &files {
        if let Err(e) = db.record_external_item(plan.library, entry).await {
            eprintln!("sharecraft-server: a copied file could not be tracked: {e}");
            state.sources().wake(plan.library);
        }
    }
}

/// Where the items of a copy between libraries are read from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AcrossFrom {
    /// The ordinary content store (an ordinary or GitHub library).
    Tree,
    /// The bucket of the S3 library, in either mode, read through the external-content cache.
    Bucket(NodeId),
}

/// Where the items of a copy between libraries are made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AcrossInto {
    /// Ordinary files and folders, counted against the quota of the site.
    Tree { site: NodeId },
    /// Folders and staged files of the read-and-write S3 library.
    Bucket(NodeId),
}

/// One item a copy between libraries makes, in the order it is made: every folder before
/// what is in it.
struct AcrossItem {
    /// The original.
    source: NodeId,
    /// The item of the plan the copy goes into; `None` for the top, which goes into the
    /// parent asked for.
    within: Option<usize>,
    name: String,
    /// `None` for a folder.
    file: Option<AcrossFile>,
}

/// A file of a copy between libraries.
struct AcrossFile {
    editor: Editor,
    /// Its size: the tracked size of a bucket file, the stored size of an ordinary one.
    size: u64,
    /// The content of an ordinary file; a bucket file's is read when it is copied.
    content: Option<Content>,
}

/// A copy between libraries, judged and ready to be made.
struct AcrossPlan {
    from: AcrossFrom,
    into: AcrossInto,
    items: Vec<AcrossItem>,
    /// The bytes of every file.
    bytes: u64,
}

/// Where `node` of a connected library is read from for a copy out of it: `409
/// source_copy_unavailable` for a file share, whose files are not copied yet.
async fn across_from(db: &Db, node: NodeId) -> Result<AcrossFrom, ApiError> {
    let unavailable = |e: records::DbError| ApiError::unavailable(&e);
    let Some(library) = db.source_library_of(node).await.map_err(unavailable)? else {
        return Ok(AcrossFrom::Tree);
    };
    match db.source_of(library).await.map_err(unavailable)? {
        Some((config, _)) if config.kind == SourceKind::S3 => Ok(AcrossFrom::Bucket(library)),
        Some((config, _)) if config.kind == SourceKind::Share => {
            Err(ApiError::SourceCopyUnavailable)
        }
        _ => Ok(AcrossFrom::Tree),
    }
}

/// The items below `node` (which is `top` in the plan) that a copy takes, as the ordinary copy
/// takes them: every live item the person may view, a hidden folder taking everything in it
/// with it, and a file only with its document row (and, in a bucket, its tracking row).
async fn across_walk(
    db: &Db,
    actor: &User,
    node: NodeId,
    tracked: &HashMap<NodeId, u64>,
    from: AcrossFrom,
    items: &mut Vec<AcrossItem>,
) -> Result<(), ApiError> {
    let unavailable = |e: records::DbError| ApiError::unavailable(&e);
    let mut level = vec![(node, 0usize)];
    while let Some((folder, at)) = level.pop() {
        let mut children = db.live_children(folder).await.map_err(unavailable)?;
        children.sort_by(|a, b| a.name.cmp(&b.name));
        for child in children {
            let viewable = db
                .effective_access(actor.id, child.id)
                .await
                .map_err(unavailable)?
                .is_some_and(|a| a.perms.contains(Perms::VIEW));
            if !viewable {
                continue;
            }
            let file = match child.kind {
                NodeKind::Folder => None,
                NodeKind::Document => {
                    let Some(document) =
                        db.document(DocId(child.id.0)).await.map_err(unavailable)?
                    else {
                        continue;
                    };
                    let size = match from {
                        AcrossFrom::Bucket(_) => match tracked.get(&child.id) {
                            Some(size) => *size,
                            None => continue,
                        },
                        AcrossFrom::Tree => 0,
                    };
                    Some(AcrossFile {
                        editor: document.editor,
                        size,
                        content: None,
                    })
                }
                _ => continue,
            };
            let folder_child = file.is_none();
            items.push(AcrossItem {
                source: child.id,
                within: Some(at),
                name: child.name,
                file,
            });
            if folder_child {
                level.push((child.id, items.len() - 1));
            }
        }
    }
    Ok(())
}

/// The library-relative paths every item of `plan` takes in the bucket, the top at `top`.
fn across_paths(plan: &AcrossPlan, top: &str) -> Vec<String> {
    let mut paths: Vec<String> = Vec::with_capacity(plan.items.len());
    for item in &plan.items {
        let path = match item.within.and_then(|at| paths.get(at)) {
            Some(folder) => format!("{folder}/{}", item.name),
            None => top.to_owned(),
        };
        paths.push(path);
    }
    paths
}

/// The name a copy of `item` into the ordinary place `parent` takes, asked as `asked`: as the
/// ordinary copy names it (see `tree::plan_copy`), cleaned (a file keeping its editor's
/// extension, `422 invalid_name` otherwise) and numbered when taken there ignoring case.
async fn ordinary_copy_name(
    db: &Db,
    item: &Node,
    parent: NodeId,
    asked: &str,
) -> Result<String, ApiError> {
    let unavailable = |e: records::DbError| ApiError::unavailable(&e);
    let editor = if item.kind == NodeKind::Document {
        db.document(DocId(item.id.0))
            .await
            .map_err(unavailable)?
            .map(|document| document.editor)
    } else {
        None
    };
    let cleaned = match editor {
        Some(editor) => {
            let cleaned = tree::clean_name(asked, editor);
            records::validate_document_name(&cleaned, editor).map_err(|e| match e {
                AccessError::WrongExtension => ApiError::InvalidName,
                e => e.into(),
            })?;
            cleaned
        }
        None => tree::clean_folder_name(asked),
    };
    let taken: HashSet<String> = db
        .live_child_names(parent)
        .await
        .map_err(unavailable)?
        .into_iter()
        .collect();
    Ok(tree::unique_node_name(&cleaned, editor, &taken))
}

/// Everything about copying `node` into `parent` of another library (as `name` or as it is)
/// that is judged before a byte is read or anything is made:
///
/// - where it comes from: `404` for someone who cannot see it, `view` on it, a file or folder,
///   `409 source_copy_unavailable` out of a file share; out of a bucket `409
///   source_item_saving` while it or anything inside has a change not yet written and `409
///   source_unavailable {reason: archived}` for anything in archive storage;
/// - where it goes: what [`writable_place`] says of `parent` (`409 source_read_only`, `409
///   source_write_blocked`), a library or folder, `add` there, the folders setting for a
///   folder; into a bucket a name free there ignoring case (`409 name_taken`: never numbered)
///   and a name the listing shows and the bucket takes for every item (`422 invalid_name`),
///   elsewhere the name the ordinary copy gives (see [`ordinary_copy_name`]);
/// - the limits: at most [`crate::s3_write::MOVE_MAX_OBJECTS`] files and folders (`409
///   source_too_many_objects {count}`), and every file within the destination's largest file
///   (into a bucket [`write_limit`], `413 source_file_too_large {limit}`; elsewhere the
///   server's, `413 too_large`) and its library's file policies.
///
/// What the person may not view is left out, as the ordinary copy leaves it out. A bucket
/// file's size is its tracked size, so nothing is read from the bucket here.
async fn across_allowed(
    state: &AppState,
    actor: &User,
    node: NodeId,
    parent: NodeId,
    name: Option<&str>,
) -> Result<AcrossPlan, ApiError> {
    let db = state.db();
    let unavailable = |e: records::DbError| ApiError::unavailable(&e);
    let (item, access) = tree::visible(db, actor, node).await?;
    tree::require(&access, Perms::VIEW)?;
    if !matches!(item.kind, NodeKind::Folder | NodeKind::Document) {
        return Err(AccessError::WrongPlace.into());
    }
    let from = across_from(db, node).await?;
    let bucket = match writable_place(db, actor.id, parent).await? {
        Place::External(Store::S3(target)) => Some(target.library),
        Place::Ordinary => None,
    };
    if let (AcrossFrom::Bucket(a), Some(b)) = (from, bucket)
        && a == b
    {
        // Inside one library: a copy in the bucket (see `copy_within`).
        return Err(ApiError::SourceActionUnavailable);
    }
    let (place, place_access) = tree::visible(db, actor, parent).await?;
    if !matches!(place.kind, NodeKind::Library | NodeKind::Folder) {
        return Err(AccessError::WrongPlace.into());
    }
    tree::require(&place_access, Perms::ADD)?;
    if db.is_at_or_below(parent, node).await.map_err(unavailable)? {
        return Err(AccessError::IntoItself.into());
    }
    let into = bucket.map_or(AcrossInto::Tree { site: place.site }, AcrossInto::Bucket);
    let asked = name.map_or(item.name.as_str(), str::trim);
    let new_name = match into {
        AcrossInto::Bucket(_) => match checked_name(db, &item, asked).await {
            Err(ApiError::WrongExtension) => return Err(ApiError::InvalidName),
            checked => checked?,
        },
        AcrossInto::Tree { .. } => ordinary_copy_name(db, &item, parent, asked).await?,
    };
    let tracked: HashMap<NodeId, u64> = match from {
        AcrossFrom::Bucket(library) => {
            let (_, rows) = tracked_subtree(db, library, node).await?;
            rows.into_iter()
                .map(|row| (row.node, u64::try_from(row.size).unwrap_or(0)))
                .collect()
        }
        AcrossFrom::Tree => HashMap::new(),
    };
    let top = match item.kind {
        NodeKind::Document => {
            let document = db
                .document(DocId(node.0))
                .await
                .map_err(unavailable)?
                .ok_or(ApiError::NotFound)?;
            let size = match from {
                AcrossFrom::Bucket(_) => *tracked
                    .get(&node)
                    .ok_or(ApiError::SourceActionUnavailable)?,
                AcrossFrom::Tree => 0,
            };
            Some(AcrossFile {
                editor: document.editor,
                size,
                content: None,
            })
        }
        _ => None,
    };
    let folder = top.is_none();
    let mut items = vec![AcrossItem {
        source: node,
        within: None,
        name: new_name.clone(),
        file: top,
    }];
    if folder {
        across_walk(db, actor, node, &tracked, from, &mut items).await?;
    }
    let count = u64::try_from(items.len()).unwrap_or(u64::MAX);
    if count > crate::s3_write::MOVE_MAX_OBJECTS {
        return Err(ApiError::SourceTooManyObjects(count));
    }
    // The content of an ordinary file: its latest version, which the scanner must not hold. A
    // file with no content yet is left out (it keeps its place, so the folders keep theirs).
    if from == AcrossFrom::Tree {
        for item in &mut items {
            let top = item.within.is_none();
            let Some(file) = item.file.as_mut() else {
                continue;
            };
            let doc = DocId(item.source.0);
            let Some(stored) =
                crate::api::content::blocking(state, move |store| store.load(doc)).await?
            else {
                if top {
                    return Err(ApiError::SourceActionUnavailable);
                }
                continue;
            };
            if let Content::Blob(blob) = &stored.content
                && let Some(scan) = db.availability(&blob.sha256).await.map_err(unavailable)?
            {
                return Err(ApiError::Held(scan));
            }
            file.size = match &stored.content {
                Content::Blob(blob) => blob.size,
                Content::Inline(bytes) => u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            };
            file.content = Some(stored.content);
        }
    }
    if folder && folders_off(db, parent).await? {
        return Err(AccessError::FoldersOff.into());
    }
    // Into a bucket a taken name is refused, never numbered; elsewhere it was numbered above.
    if matches!(into, AcrossInto::Bucket(_))
        && !db
            .name_free(parent, &new_name, None)
            .await
            .map_err(unavailable)?
    {
        return Err(AccessError::NameTaken.into());
    }
    let limit = match into {
        AcrossInto::Bucket(_) => write_limit(state).await.map_err(unavailable)?,
        AcrossInto::Tree { .. } => state.config().max_file_bytes,
    };
    let mut bytes = 0u64;
    for item in &items {
        let Some(file) = &item.file else { continue };
        if from == AcrossFrom::Tree && file.content.is_none() {
            continue;
        }
        if file.size > limit {
            return Err(match into {
                AcrossInto::Bucket(_) => ApiError::SourceFileTooLarge(limit),
                AcrossInto::Tree { .. } => ApiError::TooLarge,
            });
        }
        tree::check_policies(db, parent, &item.name, Some(file.size)).await?;
        bytes = bytes.saturating_add(file.size);
    }
    let plan = AcrossPlan {
        from,
        into,
        items,
        bytes,
    };
    if let AcrossInto::Bucket(library) = into {
        let (_, top) = external_path(state, parent, &new_name).await?;
        let target = state.s3().target(state, library).await.ok();
        for path in across_paths(&plan, &top).iter().skip(1) {
            let nameable = path
                .rsplit('/')
                .next()
                .is_some_and(|last| nameable_segment(last) && !records::reserved_name(last));
            let keyed = target
                .as_ref()
                .is_none_or(|target| object_key(target, path).is_some());
            if !nameable || !keyed {
                return Err(ApiError::InvalidName);
            }
        }
    }
    Ok(plan)
}

/// The content of the file `item` of `plan`, held: an ordinary file's stored content, or a
/// bucket file's bytes read through the external-content cache (passing its scan gate), held
/// to the destination's limit `limit` again, since the bucket may hold more than was tracked.
async fn across_content(
    state: &AppState,
    plan: &AcrossPlan,
    item: &AcrossItem,
    limit: u64,
) -> Result<Held, ApiError> {
    let held = match (
        plan.from,
        item.file.as_ref().and_then(|f| f.content.clone()),
    ) {
        (AcrossFrom::Bucket(_), _) => {
            match crate::external::materialise(state, item.source).await {
                Ok(Some(ready)) => ready.held,
                Ok(None) => return Err(ApiError::Unavailable),
                Err(e) => return Err(crate::api::content::external_error(e, SourceKind::S3)),
            }
        }
        (AcrossFrom::Tree, Some(Content::Blob(blob))) => state
            .blobs()
            .hold_stored(blob)
            .await
            .ok_or(ApiError::Unavailable)?,
        (AcrossFrom::Tree, Some(Content::Inline(bytes))) => state
            .blobs()
            .write_bytes(bytes)
            .await
            .map_err(|e| ApiError::unavailable(&e))?,
        (AcrossFrom::Tree, None) => return Err(ApiError::Unavailable),
    };
    if held.blob().size > limit {
        discard(state, held).await;
        return Err(match plan.into {
            AcrossInto::Bucket(_) => ApiError::SourceFileTooLarge(limit),
            AcrossInto::Tree { .. } => ApiError::TooLarge,
        });
    }
    Ok(held)
}

/// Makes the item `item` of `plan` in `parent`: a folder, or a file with its content (an
/// ordinary file through the upload route's [`crate::api::content::create_uploaded`], a bucket
/// file staged through [`create_staged`]). Answers the new node; on failure the error, and a
/// file made whose content could not be stored (it stays, empty, for the caller to remove).
async fn across_one(
    state: &AppState,
    actor: &User,
    plan: &AcrossPlan,
    item: &AcrossItem,
    parent: NodeId,
    limit: u64,
) -> Result<NodeId, (ApiError, Option<NodeId>)> {
    let made = across_make(state, actor, plan, item, parent, limit)
        .await
        .map_err(|e| (e, None))?;
    match made {
        Created::Done { doc, .. } => Ok(NodeId(doc.0)),
        Created::Failed(_) => {
            // The upload route's answer names the empty file; it is found by its name, free
            // (or numbered) where it was made.
            let stray = state
                .db()
                .live_children(parent)
                .await
                .ok()
                .and_then(|found| {
                    found
                        .into_iter()
                        .find(|n| n.kind == NodeKind::Document && n.name == item.name)
                        .map(|n| n.id)
                });
            Err((ApiError::Unavailable, stray))
        }
    }
}

/// [`across_one`]'s work: the folder made (as a finished creation) or the file's creation.
async fn across_make(
    state: &AppState,
    actor: &User,
    plan: &AcrossPlan,
    item: &AcrossItem,
    parent: NodeId,
    limit: u64,
) -> Result<Created, ApiError> {
    let folder_made = |id: NodeId| Created::Done {
        doc: DocId(id.0),
        body: Value::Null,
    };
    let Some(file) = &item.file else {
        return match plan.into {
            AcrossInto::Bucket(library) => Ok(folder_made(
                create_folder(state, actor, library, parent, &item.name)
                    .await?
                    .id,
            )),
            AcrossInto::Tree { .. } => Ok(folder_made(
                tree::apply(
                    state,
                    actor,
                    TreeOp::Create {
                        parent: Some(parent),
                        kind: NodeKind::Folder,
                        name: item.name.clone(),
                        id: NodeId(Uuid::new_v4()),
                    },
                )
                .await?
                .node
                .id,
            )),
        };
    };
    let held = across_content(state, plan, item, limit).await?;
    match plan.into {
        AcrossInto::Bucket(_) => {
            create_staged(state, actor, parent, &item.name, file.editor, held, false).await
        }
        AcrossInto::Tree { .. } => {
            crate::api::content::create_uploaded(
                state,
                actor,
                parent,
                &item.name,
                file.editor,
                held,
                false,
            )
            .await
        }
    }
}

/// Copies `node` into `parent` of another library, for `actor`, named `name` or as it is:
/// out of an S3 library into an ordinary place, from an ordinary place into a read-and-write
/// S3 library, or from one S3 library into another. Answers the new node.
///
/// Everything that can be refused is judged first, before a byte is read (see
/// [`across_allowed`]); a copy out is then reserved against the destination site's quota
/// (`507 quota_exceeded`), still before a byte is read. Then each item is made, every folder
/// before what is in it:
///
/// - out of a bucket, each file's bytes are read through the external-content cache
///   ([`crate::external::materialise`], which passes them through the scan gate) and made an
///   ordinary file through the upload route's [`crate::api::content::create_uploaded`], which
///   counts them in the site's usage;
/// - into a bucket, folders are made with [`create_folder`] (the marker written at once) and
///   files with [`create_staged`], on the original's content (already scanned) or the bytes
///   just read, so the write worker writes them after the scan gate; nothing else is written
///   in the request.
///
/// If a step fails part way, a copy out deletes and purges what it made (best effort, see
/// [`remove_partial`]) and answers the error; a copy into a bucket leaves what it made (its files are accepted saves) and answers
/// `SourceCopyIncomplete` with the error's status and code and how many items it made.
pub(crate) async fn copy_across(
    state: &AppState,
    actor: &User,
    node: NodeId,
    parent: NodeId,
    name: Option<&str>,
) -> Result<NodeId, ApiError> {
    let db = state.db();
    let plan = across_allowed(state, actor, node, parent, name).await?;
    let limit = match plan.into {
        AcrossInto::Bucket(_) => write_limit(state)
            .await
            .map_err(|e| ApiError::unavailable(&e))?,
        AcrossInto::Tree { .. } => state.config().max_file_bytes,
    };
    let reservation = match plan.into {
        AcrossInto::Tree { site } if plan.bytes > 0 => Some(
            db.reserve_bytes(site, plan.bytes)
                .await
                .map_err(|e| match e {
                    QuotaError::Exceeded => ApiError::QuotaExceeded,
                    QuotaError::Db(e) => ApiError::unavailable(&e),
                })?,
        ),
        _ => None,
    };
    // The node made of each item, in order; `None` for a file left out.
    let mut made: Vec<Option<NodeId>> = Vec::with_capacity(plan.items.len());
    let mut failed = None;
    // A file made whose content could not be stored.
    let mut stray = None;
    for item in &plan.items {
        if plan.from == AcrossFrom::Tree
            && item
                .file
                .as_ref()
                .is_some_and(|file| file.content.is_none())
        {
            made.push(None);
            continue;
        }
        let into = match item.within {
            Some(at) => match made.get(at).copied().flatten() {
                Some(folder) => folder,
                None => {
                    failed = Some(ApiError::Unavailable);
                    break;
                }
            },
            None => parent,
        };
        match across_one(state, actor, &plan, item, into, limit).await {
            Ok(new) => made.push(Some(new)),
            Err((e, left)) => {
                failed = Some(e);
                stray = left;
                break;
            }
        }
    }
    // The files made were counted with their versions.
    if let Some(reservation) = reservation {
        crate::blobs::release(db, reservation).await;
    }
    let top = made.first().copied().flatten();
    let count = made.iter().flatten().count();
    let Some(error) = failed else {
        return top.ok_or(ApiError::Unavailable);
    };
    match plan.into {
        AcrossInto::Tree { .. } => {
            // Everything it made is below the top; a top file whose content could not be
            // stored is the stray one.
            if let Some(made) = top.or(stray) {
                remove_partial(state, actor, made).await;
            }
            Err(error)
        }
        AcrossInto::Bucket(_) if count == 0 => Err(error),
        AcrossInto::Bucket(_) => {
            eprintln!(
                "sharecraft-server: a copy into S3 storage stopped after {count} items: {}",
                error.code()
            );
            let (status, code) = error.status_and_code();
            Err(ApiError::SourceCopyIncomplete {
                made: u64::try_from(count).unwrap_or(u64::MAX),
                status,
                code,
            })
        }
    }
}

/// Removes `made`, what a copy out made before it failed, with everything in it: deleted and
/// purged at once, so its bytes are freed and nothing the person did not delete shows in the
/// recycle bin. Best effort: a failure is logged.
async fn remove_partial(state: &AppState, actor: &User, made: NodeId) {
    if let Err(e) = tree::apply(state, actor, TreeOp::Delete { id: made }).await {
        eprintln!("sharecraft-server: a copy that failed part way was not removed: {e}");
        return;
    }
    if let Err(e) = tree::purge_now(state, made).await {
        eprintln!("sharecraft-server: a copy that failed part way was not purged: {e}");
    }
}

/// Everything about deleting `node` inside its read-and-write S3 library that is judged before
/// the bucket is asked, as [`tree::apply`] judges a delete and [`restructure_allowed`] a bucket
/// step: where the node is (`404`, `409 source_read_only`, `409 source_write_blocked`, a library
/// itself `409 source_read_only`), `delete` on it, then [`tracked_subtree`]. The plan's `to` and
/// `shown_to` are left for the hold to fill in.
async fn delete_allowed(
    state: &AppState,
    actor: &User,
    node: NodeId,
) -> Result<Restructure, ApiError> {
    let db = state.db();
    let (library, item, access) = bucket_item(state, actor, node).await?;
    tree::require(&access, Perms::DELETE)?;
    let (tracked, items) = tracked_subtree(db, library, node).await?;
    Ok(Restructure {
        library,
        kind: item.kind,
        old_name: item.name,
        name: None,
        parent: None,
        from: tracked.repository_path,
        to: String::new(),
        shown_from: tracked.path,
        shown_to: String::new(),
        items,
    })
}

/// Deletes `node` inside its read-and-write S3 library, for `actor`: its objects are moved into
/// the holding folder (`.sharecraft-bin/<today, UTC>/<node>/<its path>`, every object copied on
/// the tag ShareCraft recorded, then every original deleted), a hold row names the root, then
/// the node goes to the recycle bin as an entry of its own (id `node`). The tracking rows
/// follow the objects into the holding folder, where no listing looks, so Restore knows where
/// everything was and the bin shows the size.
///
/// Judged as [`delete_allowed`], again under the library's source lock (`409 source_busy` after
/// ten seconds), which is held until the rows are recorded. The hold row is written before the
/// tree is asked, so an entry in the bin always names its objects; if it cannot be written, or
/// the tree refuses, the objects are moved back (best effort) and the hold forgotten. Originals
/// that could not be deleted are counted in `left_behind` and come back as new files at the
/// listing, which is woken. Records `source.held {node, objects, left_behind}`.
///
/// A file with a save not yet written cannot be deleted (`409 source_item_saving`), and saves
/// take no lock, so one can be staged while the objects are being held. That save refuses the
/// delete as one staged before it would: it is looked for after the hold (the objects are moved
/// back) and again after the tree's delete, which from then on refuses every new save of the
/// deleted items; one staged in between takes the delete back (the entry is restored and the
/// objects moved back). Either way the save is written where the file still is.
///
/// # Errors
/// As [`delete_allowed`]; `409 source_too_many_objects {count}`, `409 source_too_large_to_move`,
/// `409 source_changed` (a listing is woken), `409 source_write_blocked` (the store refused: the
/// library is blocked), `409 source_item_saving` (a save arrived meanwhile) and
/// `source_unavailable` from the bucket.
pub(crate) async fn delete_to_holding(
    state: &AppState,
    actor: &User,
    node: NodeId,
) -> Result<Restructured, ApiError> {
    let db = state.db();
    let first = delete_allowed(state, actor, node).await?;
    let lock = lock_source(db, first.library).await?;
    // Judged again once the lock is held: a listing or a write may have run meanwhile.
    let mut plan = delete_allowed(state, actor, node).await?;
    let target = bucket(state, plan.library).await?;
    // One time for the root's date and the hold's: a delete at midnight is filed under the day
    // its hold row names.
    let now = OffsetDateTime::now_utc();
    let root = holding_root(now.date(), node);
    plan.to = format!("{root}{}", plan.from);
    plan.shown_to = plan.to.trim_end_matches('/').to_owned();
    let known = known_of(&plan.items);
    let held = hold(
        state.s3(),
        &target,
        &plan.from,
        plan.folder(),
        &root,
        &known,
    )
    .await;
    let moved = match held {
        Ok(moved) => moved,
        Err(error) => return Err(refusal(state, plan.library, error).await),
    };
    if let Err(e) = db
        .hold_record(node, plan.library, &root, moved.objects, now)
        .await
    {
        // Without its hold row a bin entry could not be restored: no delete then.
        eprintln!("sharecraft-server: the hold of a deleted item could not be recorded: {e}");
        put_back(state, &target, &root, &plan, &moved).await;
        return Err(ApiError::unavailable(&e));
    }
    let undo = Undo {
        entry: node,
        target: &target,
        root: &root,
        plan: &plan,
        moved: &moved,
    };
    match saving_below(db, &plan).await {
        Ok(false) => {}
        Ok(true) => return Err(undo.run(state, ApiError::SourceItemSaving).await),
        Err(e) => return Err(undo.run(state, e).await),
    }
    if let Err(e) = crate::api::nodes::delete_one(state, actor, node).await {
        return Err(undo.run(state, e).await);
    }
    // A save staged after the look above and before the tree's delete took its node.
    if !matches!(saving_below(db, &plan).await, Ok(false)) {
        match tree::apply(state, actor, TreeOp::Restore { id: node }).await {
            Ok(_) => return Err(undo.run(state, ApiError::SourceItemSaving).await),
            Err(e) => eprintln!(
                "sharecraft-server: a delete with a save staged meanwhile could not be taken \
                 back ({e}); the save stays with the deleted item"
            ),
        }
    }
    record_moved(state, &plan, &moved).await;
    drop(lock);
    if moved.left_behind > 0 {
        state.sources().wake(plan.library);
    }
    state.events().emit(NewEvent {
        kind: "source.held",
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

/// A delete given up after its objects were held and its hold recorded.
struct Undo<'a> {
    entry: NodeId,
    target: &'a S3Target,
    root: &'a str,
    plan: &'a Restructure,
    moved: &'a Moved,
}

impl Undo<'_> {
    /// Moves the held objects back and forgets the hold; answers `error`.
    async fn run(&self, state: &AppState, error: ApiError) -> ApiError {
        put_back(state, self.target, self.root, self.plan, self.moved).await;
        if let Err(e) = state.db().forget_held(self.entry).await {
            eprintln!(
                "sharecraft-server: the hold of an item not deleted could not be forgotten: {e}"
            );
        }
        error
    }
}

/// Whether a save of the item `plan` deletes, or of anything inside it, waits to be written.
async fn saving_below(db: &Db, plan: &Restructure) -> Result<bool, ApiError> {
    Ok(db
        .writes_below(plan.library, &plan.from)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
        > 0)
}

/// Moves the objects a delete held back where they were, best effort, for a delete that does
/// not go ahead; the tracking rows take the tags of the copies back (and a save staged
/// meanwhile its base), so the next listing finds nothing new. What cannot be moved back stays
/// held, and a listing is woken.
async fn put_back(
    state: &AppState,
    target: &S3Target,
    root: &str,
    plan: &Restructure,
    moved: &Moved,
) {
    let back = unhold(
        state.s3(),
        target,
        root,
        &plan.from,
        &plan.from,
        plan.folder(),
    )
    .await;
    match back {
        Ok(back) => {
            retag_copies(state, plan.library, &plan.items, str::to_owned, &back).await;
            if back.left_behind > 0 || moved.left_behind > 0 {
                state.sources().wake(plan.library);
            }
        }
        Err(error) => {
            eprintln!(
                "sharecraft-server: objects held for a refused delete could not be moved back: \
                 {error:?}"
            );
            state.sources().wake(plan.library);
        }
    }
}

/// What a restore from the holding folder did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Restored {
    pub node: NodeId,
    /// The name it had, when it came back under a numbered one.
    pub renamed: Option<String>,
}

/// A restore from the holding folder, judged and ready to be made.
struct Unhold {
    /// The hold's root, and the path held under it (where the item was when it was deleted).
    root: String,
    held: String,
    /// Whether the store honours conditional writes, for the folder marker.
    conditional: Tri,
    /// Where it goes back to: the folder it was deleted from.
    parent: NodeId,
    /// The move out of the holding folder; `to` and `shown_to` are filled in once a name is
    /// free.
    plan: Restructure,
}

/// Everything about restoring the bin entry `entry` from the holding folder that is judged
/// before the bucket is asked: where it is (`404`, `409 source_read_only`, `409
/// source_write_blocked`), the hold (`404` without one), the person's rights as the bin judges
/// them (`delete` on it as if it were live in the first stage, `manage_site` in the second),
/// and the folder it goes back to, which must be live (`409 parent_in_bin`).
async fn restore_allowed(
    state: &AppState,
    actor: &User,
    entry: NodeId,
) -> Result<Unhold, ApiError> {
    let db = state.db();
    let unavailable = |e: records::DbError| ApiError::unavailable(&e);
    let target = match writable_place(db, actor.id, entry).await? {
        Place::External(Store::S3(target)) => target,
        Place::Ordinary => return Err(ApiError::SourceActionUnavailable),
    };
    let held = db
        .held(entry)
        .await
        .map_err(unavailable)?
        .filter(|held| held.library == target.library)
        .ok_or(ApiError::NotFound)?;
    let bin = db
        .bin_state(entry)
        .await
        .map_err(unavailable)?
        .ok_or(ApiError::NotFound)?;
    match bin.stage {
        BinStage::First => {
            let access = db
                .access_as_if_live(actor.id, entry)
                .await
                .map_err(unavailable)?
                .ok_or(ApiError::NotFound)?;
            tree::require(&access, Perms::DELETE)?;
        }
        BinStage::Second => {
            let manages = db
                .effective_access(actor.id, bin.site)
                .await
                .map_err(unavailable)?
                .is_some_and(|access| access.perms.contains(Perms::MANAGE_SITE));
            if !manages {
                return Err(ApiError::NotFound);
            }
        }
    }
    let node = db
        .node(entry)
        .await
        .map_err(unavailable)?
        .ok_or(ApiError::NotFound)?;
    let parent = node.parent.ok_or(ApiError::SourceActionUnavailable)?;
    let parent_live = db
        .node(parent)
        .await
        .map_err(unavailable)?
        .is_some_and(|found| !found.deleted);
    if !parent_live {
        return Err(ApiError::ParentInBin);
    }
    let row = db
        .source_item_of(entry)
        .await
        .map_err(unavailable)?
        .ok_or(ApiError::SourceActionUnavailable)?;
    let held_path = row
        .repository_path
        .strip_prefix(held.root.as_str())
        .filter(|rest| !rest.is_empty())
        .ok_or(ApiError::SourceActionUnavailable)?
        .to_owned();
    let items = subtree_rows(db, target.library, &row.repository_path).await?;
    Ok(Unhold {
        root: held.root,
        held: held_path,
        conditional: target.conditional,
        parent,
        plan: Restructure {
            library: target.library,
            kind: node.kind,
            old_name: node.name,
            name: None,
            parent: None,
            from: row.repository_path,
            to: String::new(),
            shown_from: row.path,
            shown_to: String::new(),
            items,
        },
    })
}

/// The `n`th name tried for an item named `name` coming back: the name itself, then `name (2)`
/// (a file's before its extension) up to `(20)`.
fn restore_name(name: &str, folder: bool, n: u32) -> String {
    match n {
        0 | 1 => name.to_owned(),
        n if folder => format!("{name} ({n})"),
        n => numbered_name(name, n),
    }
}

/// Restores the bin entry `entry` of a read-and-write S3 library from the holding folder, for
/// `actor`: its objects go back where it was deleted from (copied, then the held ones deleted),
/// then the entry comes back in the tree with everything deleted with it, then the tracking rows
/// take the new paths and tags, so the next listing finds nothing new. The node keeps its id,
/// permissions and links.
///
/// The name is the one it had if no live item there has it, ignoring case, else `name (2)` and
/// on up to `(20)`; a name the bucket has but the tree does not yet (not listed) is passed over
/// for the next. A folder whose marker was not held gets one. Runs under the library's source
/// lock (`409 source_busy` after ten seconds), judged before and again under it. Records
/// `source.unheld {node, objects, left_behind}`.
///
/// # Errors
/// As [`restore_allowed`]; `409 name_taken` when all twenty names are taken; the bucket's
/// refusals (as a move's) and the tree's.
pub(crate) async fn restore_from_holding(
    state: &AppState,
    actor: &User,
    entry: NodeId,
) -> Result<Restored, ApiError> {
    let db = state.db();
    let unavailable = |e: records::DbError| ApiError::unavailable(&e);
    let first = restore_allowed(state, actor, entry).await?;
    let lock = lock_source(db, first.plan.library).await?;
    let Unhold {
        root,
        held,
        conditional,
        parent,
        mut plan,
    } = restore_allowed(state, actor, entry).await?;
    let target = bucket(state, plan.library).await?;
    let folder = plan.folder();
    let original = plan.old_name.clone();
    let taken: HashSet<String> = db
        .live_child_names(parent)
        .await
        .map_err(unavailable)?
        .into_iter()
        .collect();
    let mut found = None;
    for n in 1..=LAST_RESTORE_NUMBER {
        let name = restore_name(&original, folder, n);
        if taken.contains(&name.to_lowercase()) {
            continue;
        }
        let (_, path) = external_path(state, parent, &name).await?;
        let to = if folder { format!("{path}/") } else { path };
        match unhold(state.s3(), &target, &root, &held, &to, folder).await {
            Ok(moved) => {
                found = Some((name, to, moved));
                break;
            }
            // Taken in the bucket but not listed yet: the next number.
            Err(WriteError::NameTaken) => {}
            Err(error) => return Err(refusal(state, plan.library, error).await),
        }
    }
    let Some((name, to, moved)) = found else {
        return Err(AccessError::NameTaken.into());
    };
    let restored = match tree::apply(state, actor, TreeOp::Restore { id: entry }).await {
        Ok(applied) => applied,
        Err(e) => {
            // The objects stay where they now are, and come in as new files at the listing:
            // a duplicate at worst, never a loss.
            eprintln!(
                "sharecraft-server: a restore was refused after its objects left the holding \
                 folder: {e}"
            );
            state.sources().wake(plan.library);
            return Err(e.into());
        }
    };
    if restored.node.name != name {
        let renamed = TreeOp::Rename {
            id: entry,
            name: name.clone(),
        };
        if let Err(e) = tree::apply(state, actor, renamed).await {
            eprintln!("sharecraft-server: a restored item could not take its new name: {e}");
            state.sources().wake(plan.library);
        }
    }
    plan.to.clone_from(&to);
    match shown_in(db, plan.library, parent, &name).await {
        Ok(shown) => {
            plan.shown_to = shown;
            record_moved(state, &plan, &moved).await;
        }
        Err(_) => state.sources().wake(plan.library),
    }
    // A folder whose marker was not held (a folder only its files made) gets one, so it stays
    // when it is emptied.
    if folder && !moved.seen.iter().any(|(path, _)| *path == to) {
        let marker = to.trim_end_matches('/');
        if let Err(error) = make_folder(state.s3(), &target, marker, conditional).await {
            eprintln!(
                "sharecraft-server: the marker of a restored folder could not be written: \
                 {error:?}"
            );
        }
    }
    if let Err(e) = db.forget_held(entry).await {
        eprintln!("sharecraft-server: the hold of a restored item could not be forgotten: {e}");
    }
    drop(lock);
    if moved.left_behind > 0 {
        state.sources().wake(plan.library);
    }
    state.events().emit(NewEvent {
        kind: "source.unheld",
        actor: Some(actor.id),
        subject: None,
        document: (plan.kind == NodeKind::Document).then_some(DocId(entry.0)),
        node: Some(plan.library),
        detail: json!({
            "node": entry.0.to_string(),
            "objects": moved.objects,
            "left_behind": moved.left_behind,
        }),
    });
    Ok(Restored {
        node: entry,
        renamed: (name != original).then_some(original),
    })
}

/// How many names a restore tries: the plain one, then `(2)` to `(20)`.
const LAST_RESTORE_NUMBER: u32 = 20;

/// How long a purge whose held objects could not be deleted waits before trying again.
const HELD_RETRY: time::Duration = time::Duration::hours(1);

/// Lets go of what the bin entry `entry` holds in a bucket, before it is purged. Answers whether
/// the purge may go ahead:
///
/// - no hold: `true`;
/// - a read-and-write S3 library: the held objects are deleted, the hold forgotten,
///   `source.held_purged {node, objects}`, `true`;
/// - a library read-only, blocked or disconnected by now: nothing is written, the hold is forgotten,
///   `source.held_left {node, objects}` (the objects stay), `true`;
/// - the bucket offline or failing, or the library's source lock busy for ten seconds: the hold
///   waits an hour (`retry_at`), `false`.
///
/// The held objects are deleted under the library's source lock, as every holding step is.
///
/// # Errors
/// A database failure (`503`).
pub(crate) async fn release_held(
    state: &AppState,
    entry: NodeId,
    now: OffsetDateTime,
) -> Result<bool, ApiError> {
    let db = state.db();
    let unavailable = |e: records::DbError| ApiError::unavailable(&e);
    let Some(held) = db.held(entry).await.map_err(unavailable)? else {
        return Ok(true);
    };
    let writable = db
        .source_of(held.library)
        .await
        .map_err(unavailable)?
        .is_some_and(|(config, _)| {
            config.kind == SourceKind::S3 && config.mode == SourceMode::ReadWrite
        });
    // A library whose writes are blocked is not written either: the purge goes ahead as in a
    // read-only one.
    let blocked = matches!(
        db.write_state(held.library).await.map_err(unavailable)?,
        Some((WriteState::Blocked, _))
    );
    if !writable || blocked {
        db.forget_held(entry).await.map_err(unavailable)?;
        emit(
            state,
            "source.held_left",
            held.library,
            None,
            json!({ "node": entry.0.to_string(), "objects": held.objects }),
        );
        return Ok(true);
    }
    // Under the library's source lock, as every other step of the holding folder: a busy
    // library waits as an offline one does.
    let lock = match lock_source(db, held.library).await {
        Ok(lock) => lock,
        Err(ApiError::SourceBusy) => {
            db.defer_held(entry, now + HELD_RETRY)
                .await
                .map_err(unavailable)?;
            return Ok(false);
        }
        Err(e) => return Err(e),
    };
    // A restore may have taken the entry back while the lock was awaited.
    let Some(held) = db.held(entry).await.map_err(unavailable)? else {
        return Ok(true);
    };
    let deleted = match state.s3().target(state, held.library).await {
        Ok(target) => delete_held(state.s3(), &target, &held.root)
            .await
            .map_err(|error| format!("{error:?}")),
        Err(error) => Err(format!("{error:?}")),
    };
    drop(lock);
    match deleted {
        Ok(objects) => {
            db.forget_held(entry).await.map_err(unavailable)?;
            emit(
                state,
                "source.held_purged",
                held.library,
                None,
                json!({ "node": entry.0.to_string(), "objects": objects }),
            );
            Ok(true)
        }
        Err(error) => {
            eprintln!(
                "sharecraft-server: the held objects of bin entry {} could not be deleted \
                 ({error}); trying again in an hour",
                entry.0
            );
            db.defer_held(entry, now + HELD_RETRY)
                .await
                .map_err(unavailable)?;
            Ok(false)
        }
    }
}

/// How old a day folder of the holding folder must be before the sweep looks inside it.
const SWEEP_AFTER_DAYS: i64 = 94;
/// How often the holding sweep runs for one library.
const SWEEP_EVERY: time::Duration = time::Duration::days(1);

/// The daily holding sweep: once a day for each read-and-write S3 library whose writes are not
/// blocked (the last run kept in memory, so a restart runs it again), deletes the objects under
/// its holding folder whose day folder is more than 94 days before `now` (UTC) and whose entry no
/// hold names (see [`crate::s3_write::sweep_holding`]); records `source.holding_swept {count}`
/// when it deleted any. Then aborts the multipart uploads a crash left open for a day. Answers
/// how many objects it deleted. Failures are logged and left for the next day.
pub async fn sweep_holds(state: &AppState, now: OffsetDateTime) -> usize {
    let db = state.db();
    let libraries = match db.writable_s3_libraries().await {
        Ok(libraries) => libraries,
        Err(e) => {
            eprintln!("sharecraft-server: database failure: {e}");
            Vec::new()
        }
    };
    let older_than = now
        .to_offset(UtcOffset::UTC)
        .checked_sub(time::Duration::days(SWEEP_AFTER_DAYS))
        .map(OffsetDateTime::date);
    let mut swept = 0;
    for library in libraries {
        let Some(older_than) = older_than else {
            break;
        };
        if !state.writes().sweep_due(library, now) {
            continue;
        }
        let keep: HashSet<NodeId> = match db.held_entries_of(library).await {
            Ok(entries) => entries.into_iter().collect(),
            Err(e) => {
                eprintln!("sharecraft-server: database failure: {e}");
                continue;
            }
        };
        let Ok(target) = state.s3().target(state, library).await else {
            continue;
        };
        match sweep_holding(state.s3(), &target, older_than, &keep).await {
            Ok(0) => {}
            Ok(count) => {
                swept += usize::try_from(count).unwrap_or(usize::MAX);
                emit(
                    state,
                    "source.holding_swept",
                    library,
                    None,
                    json!({ "count": count }),
                );
            }
            Err(error) => eprintln!(
                "sharecraft-server: the holding folder of library {} could not be swept: \
                 {error:?}",
                library.0
            ),
        }
    }
    crate::s3_write::abort_stale(state, now).await;
    swept
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
/// taken first (`409 source_busy` after ten seconds), so a write whose finish is being recorded
/// finishes first: if it was recorded meanwhile there is nothing left to discard (`404`).
///
/// The worker does not hold the lock while the content travels, so a discard can come during
/// the transfer. The change is withdrawn all the same; if its bytes then land, the worker finds
/// the row gone and records nothing, and the next listing sees the object as a change made in
/// the store: a tracked file takes it as its content (it is the person's own content, and the
/// file's history keeps what it had), a new file appears again from the store. Nothing is
/// overwritten and nothing is left half recorded. Records `source.write_discarded {node}`.
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
