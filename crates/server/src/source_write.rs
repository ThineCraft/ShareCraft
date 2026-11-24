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
    AccessError, Db, Editor, ExternalEntry, NewEvent, NewPendingWrite, PendingWrite, ScanState,
    SourceKind, SourceMode, Tri, User, WriteState, object_tag,
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
use crate::s3_write::{object_key, probe_writes, write_beside, write_numbered, write_object};
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
/// [`ExternalCheckpointer`]. A database failure answers `false`.
pub(crate) async fn is_bucket_file(db: &Db, doc: DocId) -> bool {
    let Ok(Some(library)) = db.source_library_of(NodeId(doc.0)).await else {
        return false;
    };
    matches!(
        db.source_of(library).await,
        Ok(Some((config, _))) if config.kind == SourceKind::S3
    )
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
}

/// The write worker's wake, the blocked libraries it probes again and when it last did, and
/// its switch for tests.
#[derive(Default)]
pub struct SourceWrites {
    wake: Notify,
    /// Blocked libraries with changes waiting, and when each was last probed (or blocked).
    blocked: Mutex<HashMap<NodeId, Option<OffsetDateTime>>>,
    paused: AtomicBool,
}

impl SourceWrites {
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
        // Still being scanned, or held: left for a later step.
        Ok(Some(_)) => return false,
        Err(e) => {
            eprintln!("sharecraft-server: cannot read the scan of a change: {e}");
            return false;
        }
    }
    // A listing holds the lock for its whole pass: the row waits for a later step.
    let Ok(Some(lock)) = db.try_lock_source(due.library).await else {
        return false;
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
            fail(state, row, now, sentence(WriteError::Offline)).await;
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
fn sentence(error: WriteError) -> &'static str {
    match error {
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
    fail(state, row, now, sentence(error)).await;
    if error != WriteError::Denied {
        return;
    }
    state.writes().note_blocked(row.library, Some(now));
    match state
        .db()
        .set_write_state(row.library, WriteState::Blocked, Some(BLOCKED_REASON))
        .await
    {
        Ok(true) => emit(state, "source.write_blocked", row.library, None, json!({})),
        Ok(false) => {}
        Err(e) => eprintln!("sharecraft-server: cannot block a library's writes: {e}"),
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

/// Probes again each blocked library with changes waiting whose time has come. A pass unblocks
/// it, records what it found about conditional writes and makes its changes due.
async fn reprobe(state: &AppState, now: OffsetDateTime) {
    let every = time::Duration::seconds(i64::try_from(REPROBE_EVERY.as_secs()).unwrap_or(300));
    let libraries: Vec<(NodeId, Option<OffsetDateTime>)> = state
        .writes()
        .blocked()
        .iter()
        .map(|(library, at)| (*library, *at))
        .collect();
    let db = state.db();
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
            Ok(true) => emit(state, "source.write_restored", library, None, json!({})),
            Ok(false) => {}
            Err(e) => {
                eprintln!("sharecraft-server: cannot unblock a library's writes: {e}");
                continue;
            }
        }
        for row in rows {
            if let Err(e) = db.retry_write(row.node, now).await {
                eprintln!("sharecraft-server: cannot make a change due again: {e}");
            }
        }
        state.writes().blocked().remove(&library);
        state.writes().wake();
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
