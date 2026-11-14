//! PostgreSQL's index of the revisions in Git: recorded after each checkpoint, reconciled when
//! the server starts, and rebuilt on request. Git stays the authority; whenever the index
//! disagrees with it, the document's list is replaced from Git. Recording a revision also
//! records the content it points at (a completed `blob_refs` row); those rows are rebuilt from
//! Git at a start when a flag asks for it ([`reconcile_blobs`]), and so is each site's usage,
//! from the sizes of its versions ([`reconcile_usage`]).

use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;
use std::time::SystemTime;

use collab_core::ids::DocId;
use records::{Db, NewEvent, RevisionRecord};
use revisions::{BlobRef, GitError, RevisionId, RevisionInfo};
use serde_json::Value;
use time::OffsetDateTime;
use tokio::sync::mpsc;

use crate::AppState;
use crate::blobs::flag_dirty;
use crate::checkpoint::Checkpointer;
use crate::events::{EventSink, error_text};

/// Content nothing names is erased at a start only once it is at least this old, so a save
/// that has stored its bytes but not yet claimed them never loses them.
const ERASE_AFTER: time::Duration = time::Duration::hours(24);

/// How many times a document's list is replaced from Git in a row when Git moves on while it
/// is being replaced.
const REPLACE_ATTEMPTS: u32 = 3;

/// What a reconcile or rebuild did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Documents found in Git.
    pub documents: usize,
    /// Documents whose revision list was replaced from Git.
    pub updated: usize,
    /// Documents that could not be read or recorded, and why.
    pub failed: Vec<(DocId, String)>,
}

impl fmt::Display for Report {
    /// One line with the counts.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} documents in Git, {} updated, {} failed",
            self.documents,
            self.updated,
            self.failed.len()
        )
    }
}

/// Brings PostgreSQL's revision lists up to date with Git: every document whose latest
/// recorded revision is not its head in Git gets its list replaced. One unreadable document
/// does not stop the others; it is listed in the report.
///
/// # Errors
/// The Git error if the documents in Git cannot be listed.
pub async fn reconcile(store: Arc<dyn Checkpointer>, db: &Db) -> Result<Report, GitError> {
    sync(&store, db, false).await
}

/// Replaces the revision list of every document in Git, whatever PostgreSQL holds.
///
/// # Errors
/// The Git error if the documents in Git cannot be listed.
pub async fn rebuild(store: Arc<dyn Checkpointer>, db: &Db) -> Result<Report, GitError> {
    sync(&store, db, true).await
}

async fn sync(store: &Arc<dyn Checkpointer>, db: &Db, all: bool) -> Result<Report, GitError> {
    let listing = Arc::clone(store);
    let docs = tokio::task::spawn_blocking(move || listing.documents())
        .await
        .map_err(GitError::new)??;
    let mut report = Report {
        documents: docs.len(),
        ..Report::default()
    };
    for doc in docs {
        match sync_one(store, db, doc, all).await {
            Ok(true) => report.updated = report.updated.saturating_add(1),
            Ok(false) => {}
            Err(error) => {
                let event = index_failed(doc, &error);
                if let Err(e) = db.append_event(&event).await {
                    eprintln!("sharecraft-server: could not record an index failure: {e}");
                }
                report.failed.push((doc, error));
            }
        }
    }
    Ok(report)
}

/// Replaces `doc`'s list from Git, unless `all` is false and its recorded head is its head in
/// Git. Returns whether it was replaced.
async fn sync_one(
    store: &Arc<dyn Checkpointer>,
    db: &Db,
    doc: DocId,
    all: bool,
) -> Result<bool, String> {
    if !all {
        let in_git = git_head(store, doc).await?.map(|r| r.to_string());
        let recorded = db
            .revisions(doc)
            .await
            .map_err(|e| e.to_string())?
            .pop()
            .map(|r| r.revision);
        if in_git == recorded {
            return Ok(false);
        }
    }
    replace_from_git(store, db, doc).await?;
    Ok(true)
}

/// The event for a failure to bring `doc`'s index up to date. `error` is about ids, paths, Git
/// and SQL, never document text.
fn index_failed(doc: DocId, error: &str) -> NewEvent {
    NewEvent {
        kind: "revision.index_failed",
        actor: None,
        subject: None,
        document: Some(doc),
        node: Some(doc.into()),
        detail: serde_json::json!({ "error": error_text(error) }),
    }
}

/// Records revisions of one document as its room writes them, in the order written, on a task
/// of its own. Cloning shares that task; it ends once every clone is dropped and what was sent
/// is recorded.
#[derive(Clone)]
pub(crate) struct Recorder {
    written: mpsc::UnboundedSender<Written>,
}

/// A checkpoint that wrote `revision` on top of `parent`.
struct Written {
    parent: Option<RevisionId>,
    revision: RevisionId,
}

impl Recorder {
    /// Starts the task that records `doc`'s revisions, read from `store`, in `db`. A revision
    /// that cannot be recorded is reported to `events`.
    pub(crate) fn start(
        doc: DocId,
        store: Arc<dyn Checkpointer>,
        db: Db,
        events: EventSink,
    ) -> Recorder {
        let (written, mut queue) = mpsc::unbounded_channel::<Written>();
        tokio::spawn(async move {
            while let Some(w) = queue.recv().await {
                if let Err(error) = record(&store, &db, doc, w.parent.as_ref(), &w.revision).await {
                    // The content is safe in Git; the next start reconciles the index.
                    eprintln!(
                        "sharecraft-server: revision {} of document {} is saved but could not \
                         be recorded in the database: {error}",
                        w.revision, doc.0
                    );
                    events.emit(index_failed(doc, &error));
                }
            }
        });
        Recorder { written }
    }

    /// Queues the revision a checkpoint just wrote on top of `parent`. Never waits, so it can be
    /// called from the blocking task that wrote it.
    pub(crate) fn written(&self, parent: Option<&RevisionId>, revision: &RevisionId) {
        // An error means the recording task is gone; the next start reconciles.
        let _ = self.written.send(Written {
            parent: parent.cloned(),
            revision: revision.clone(),
        });
    }
}

/// Records `revision`, the first of `doc`, before returning. A failure is reported to `events`
/// and standard error, not to the caller: the content is safe in Git, and the next start
/// reconciles the index.
pub(crate) async fn record_first(
    store: &Arc<dyn Checkpointer>,
    db: &Db,
    events: &EventSink,
    doc: DocId,
    revision: &RevisionId,
) {
    if let Err(error) = record(store, db, doc, None, revision).await {
        eprintln!(
            "sharecraft-server: revision {revision} of document {} is saved but could not be \
             recorded in the database: {error}",
            doc.0
        );
        events.emit(index_failed(doc, &error));
    }
}

/// Appends `revision` after `parent`; if the recorded head is not `parent`, replaces the
/// document's list from Git instead.
async fn record(
    store: &Arc<dyn Checkpointer>,
    db: &Db,
    doc: DocId,
    parent: Option<&RevisionId>,
    revision: &RevisionId,
) -> Result<(), String> {
    let wanted = revision.clone();
    let info = blocking(store, move |s| {
        s.describe(doc, &wanted).map_err(|e| e.to_string())
    })
    .await?
    .ok_or_else(|| format!("{revision} is not a revision of the document"))?;
    let parent = parent.map(ToString::to_string);
    let blob = info.blob;
    let appended = db
        .append_revision(parent.as_deref(), &to_record(info)?)
        .await
        .map_err(|e| e.to_string())?;
    if let Some(blob) = blob {
        record_ref(db, doc, blob, revision).await;
    }
    if !appended {
        replace_from_git(store, db, doc).await?;
    }
    Ok(())
}

/// Records that `revision` of `doc` names `blob`. A failure is logged and flags the content
/// rows for a rebuild at the next start; it never stops the revision list from being recorded.
async fn record_ref(db: &Db, doc: DocId, blob: BlobRef, revision: &RevisionId) {
    if let Err(e) = db.record_blob_ref(doc, blob, &revision.to_string()).await {
        eprintln!(
            "sharecraft-server: the content of revision {revision} of document {} could not be \
             recorded: {e}",
            doc.0
        );
        flag_dirty(db).await;
    }
}

/// Replaces `doc`'s list with its history in Git. If Git has a newer revision once the list is
/// stored, it may have been recorded before this list replaced it, so the list is read again.
async fn replace_from_git(
    store: &Arc<dyn Checkpointer>,
    db: &Db,
    doc: DocId,
) -> Result<(), String> {
    for _ in 0..REPLACE_ATTEMPTS {
        let history = blocking(store, move |s| s.history(doc).map_err(|e| e.to_string())).await?;
        let newest = history.last().map(|r| r.revision.clone());
        for info in &history {
            if let Some(blob) = info.blob {
                record_ref(db, doc, blob, &info.revision).await;
            }
        }
        let records = history
            .into_iter()
            .map(to_record)
            .collect::<Result<Vec<_>, _>>()?;
        db.replace_revisions(doc, &records)
            .await
            .map_err(|e| e.to_string())?;
        if git_head(store, doc).await? == newest {
            return Ok(());
        }
    }
    Err("the document kept changing while its revisions were recorded".to_owned())
}

/// `doc`'s current revision in Git.
async fn git_head(store: &Arc<dyn Checkpointer>, doc: DocId) -> Result<Option<RevisionId>, String> {
    blocking(store, move |s| {
        s.load(doc)
            .map(|stored| stored.map(|s| s.revision))
            .map_err(|e| e.to_string())
    })
    .await
}

/// Runs `f` on `store` without blocking the async workers.
async fn blocking<T: Send + 'static>(
    store: &Arc<dyn Checkpointer>,
    f: impl FnOnce(&dyn Checkpointer) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let store = Arc::clone(store);
    tokio::task::spawn_blocking(move || f(&*store))
        .await
        .map_err(|e| format!("reading Git failed: {e}"))?
}

/// The database's form of a revision Git describes.
fn to_record(info: RevisionInfo) -> Result<RevisionRecord, String> {
    let committed_at = OffsetDateTime::from_unix_timestamp(info.committed_at)
        .map_err(|e| format!("revision {} has an impossible time: {e}", info.revision))?;
    Ok(RevisionRecord {
        doc: info.doc,
        revision: info.revision.to_string(),
        committed_at,
        authors: info.authors.into_iter().map(|a| (a.id, a.name)).collect(),
        name: info.file_name,
        fields: Value::Object(info.fields),
        size: info.size,
    })
}

/// What a reconcile of the content rows did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BlobReport {
    /// Whether a flag asked for a rebuild, so one was made.
    pub rebuilt: bool,
    /// Versions found in Git that point into the content store.
    pub versions: usize,
    /// Contents erased because nothing names them.
    pub erased: usize,
    /// Documents whose history could not be read, and why.
    pub failed: Vec<(DocId, String)>,
}

impl fmt::Display for BlobReport {
    /// One line with the counts.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.rebuilt {
            return f.write_str("nothing to rebuild");
        }
        write!(
            f,
            "{} stored versions, {} failed, {} unused contents erased",
            self.versions,
            self.failed.len(),
            self.erased
        )
    }
}

/// Rebuilds the content rows (`blob_refs`) from Git, but only when the upgrade marker or the
/// dirty flag asks for it: every version of every document that points into the content store
/// is recorded, except the versions of files purged from the recycle bin. A rebuild that read
/// every document replaces the completed rows and clears the
/// flags; one with a failure only adds, keeps the flags, and erases nothing. After a rebuild
/// with no failure, content nothing names is erased, if it is older than 24 hours as of `now`
/// (by its row, or by its file when it has none), and so are abandoned files in the store's
/// `tmp` folder of that age. Old versions with their bytes inline in Git are left as they are.
///
/// # Errors
/// The Git error if the documents in Git cannot be listed.
pub async fn reconcile_blobs(
    state: &AppState,
    now: OffsetDateTime,
) -> Result<BlobReport, GitError> {
    let db = state.db();
    let mut report = BlobReport::default();
    match db.blob_refs_flagged().await {
        Ok(true) => {}
        Ok(false) => return Ok(report),
        Err(e) => {
            eprintln!("sharecraft-server: cannot read the content flags: {e}");
            return Ok(report);
        }
    }
    report.rebuilt = true;
    let store = state.checkpointer();
    let listing = Arc::clone(store);
    let docs = tokio::task::spawn_blocking(move || listing.documents())
        .await
        .map_err(GitError::new)??;
    // A purged file's versions name nothing any more: its contents may be erased.
    let purged = purged_documents(store).await?;
    let mut refs = Vec::new();
    for doc in docs.into_iter().filter(|doc| !purged.contains(doc)) {
        match blocking(store, move |s| s.history(doc).map_err(|e| e.to_string())).await {
            Ok(history) => refs.extend(
                history
                    .into_iter()
                    .filter_map(|info| Some((doc, info.blob?, info.revision.to_string()))),
            ),
            Err(error) => report.failed.push((doc, error)),
        }
    }
    report.versions = refs.len();
    let complete = report.failed.is_empty();
    if let Err(e) = db.rebuild_blob_refs(&refs, complete).await {
        // The flags stay set, so the next start tries again; nothing is erased meanwhile.
        eprintln!("sharecraft-server: cannot rebuild the content rows: {e}");
        report.rebuilt = false;
        return Ok(report);
    }
    if complete {
        report.erased = erase_unnamed(state, now, true).await;
    }
    Ok(report)
}

/// The sweeper's erasure, as of `now`: content nothing names (no `blob_refs` row, pending ones
/// included) and no write holds, older than 24 hours by its row or, without one, by its file,
/// and abandoned files in the store's `tmp` folder of that age; how many contents it erased.
/// Content with a row is looked for on every sweep; the store's files (for content with no
/// row, and `tmp`) are walked on the first sweep after start-up and then hourly.
/// Nothing is erased while a flag asks for the content rows to be rebuilt, as a version could
/// then name content without a row. Failures are logged.
pub(crate) async fn sweep_unnamed(state: &AppState, now: OffsetDateTime) -> usize {
    match state.db().blob_refs_flagged().await {
        Ok(false) => erase_unnamed(state, now, state.blobs().walk_due(now)).await,
        Ok(true) => 0,
        Err(e) => {
            eprintln!("sharecraft-server: cannot read the content flags: {e}");
            0
        }
    }
}

/// The files Git records as purged from the recycle bin. A store without a tree has none.
async fn purged_documents(store: &Arc<dyn Checkpointer>) -> Result<HashSet<DocId>, GitError> {
    let tree = Arc::clone(store);
    match tokio::task::spawn_blocking(move || tree.read_tree())
        .await
        .map_err(GitError::new)?
    {
        Ok(records) => Ok(records
            .into_iter()
            .filter(|r| r.purged.is_some() && r.kind == "document")
            .map(|r| DocId(r.id))
            .collect()),
        Err(revisions::CheckpointError::Unsupported) => Ok(HashSet::new()),
        Err(e) => Err(GitError::new(e)),
    }
}

/// Erases the content nothing names that is older than [`ERASE_AFTER`] as of `now`; how many
/// contents it erased. Content with a row is found from the rows; with `walk`, the store's
/// files are listed too (their rows looked up in batches) for content with no row, and the
/// abandoned files in `tmp` of that age are removed. Failures are logged.
async fn erase_unnamed(state: &AppState, now: OffsetDateTime, walk: bool) -> usize {
    let db = state.db();
    let blobs = state.blobs();
    let cutoff = now - ERASE_AFTER;
    let cutoff_time = SystemTime::UNIX_EPOCH
        + std::time::Duration::from_secs(u64::try_from(cutoff.unix_timestamp()).unwrap_or(0));
    let mut candidates: Vec<[u8; 32]> = match db.unreferenced_blobs(cutoff).await {
        Ok(found) => found,
        Err(e) => {
            eprintln!("sharecraft-server: cannot list unused contents: {e}");
            return 0;
        }
    };
    let seen: HashSet<[u8; 32]> = candidates.iter().copied().collect();
    if walk {
        candidates.extend(walk_store(state, cutoff_time, &seen).await);
    }
    let mut erased = 0;
    for sha256 in candidates {
        match blobs.erase_if_unreferenced(db, &sha256).await {
            Ok(true) => erased += 1,
            Ok(false) => {}
            Err(e) => eprintln!("sharecraft-server: cannot erase unused content: {e}"),
        }
    }
    erased
}

/// The store's files last written before `cutoff_time` that are not in `seen` and have no row,
/// after removing the abandoned files in `tmp` written before it.
async fn walk_store(
    state: &AppState,
    cutoff_time: SystemTime,
    seen: &HashSet<[u8; 32]>,
) -> Vec<[u8; 32]> {
    let blobs = state.blobs();
    let listed = {
        let blobs = Arc::clone(blobs);
        tokio::task::spawn_blocking(move || {
            let files = blobs.store().list()?;
            blobs.store().clear_tmp(cutoff_time)?;
            Ok::<_, revisions::BlobError>(files)
        })
        .await
    };
    let old: Vec<[u8; 32]> = match listed {
        Ok(Ok(files)) => files
            .into_iter()
            .filter(|(sha256, modified)| *modified < cutoff_time && !seen.contains(sha256))
            .map(|(sha256, _)| sha256)
            .collect(),
        Ok(Err(e)) => {
            eprintln!("sharecraft-server: cannot list the content store: {e}");
            return Vec::new();
        }
        Err(e) => {
            eprintln!("sharecraft-server: cannot list the content store: {e}");
            return Vec::new();
        }
    };
    // A file with a row is judged by its row's age, from the rows.
    match state.db().blobs_with_rows(&old).await {
        Ok(with_rows) => old
            .into_iter()
            .filter(|sha256| !with_rows.contains(sha256))
            .collect(),
        Err(e) => {
            eprintln!("sharecraft-server: cannot read the content rows: {e}");
            Vec::new()
        }
    }
}

/// What a reconcile of the sites' usage did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UsageReport {
    /// Whether a flag asked for a rebuild, so one was made.
    pub rebuilt: bool,
    /// Documents whose versions' sizes were read from Git.
    pub filled: usize,
    /// Documents whose versions' sizes could not be read, and why.
    pub failed: Vec<(DocId, String)>,
}

impl fmt::Display for UsageReport {
    /// One line with the counts.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.rebuilt {
            return f.write_str("nothing to rebuild");
        }
        write!(
            f,
            "usage rebuilt, sizes of {} documents read from Git, {} failed",
            self.filled,
            self.failed.len()
        )
    }
}

/// Drops the reservations of saves, uploads and copies that a stop interrupted (see
/// [`Db::reset_reservations`]), then rebuilds every site's usage when a flag asks for it: the
/// sizes of versions recorded before sizes were kept are read from Git (inline old versions
/// included), then each site's usage is set to the sum of the sizes of every version of every
/// item in it that is not purged. A rebuild with a document that could not be read counts what
/// it knows and keeps the flags, so the next start tries again. Failures are logged.
pub async fn reconcile_usage(state: &AppState) -> UsageReport {
    let db = state.db();
    let mut report = UsageReport::default();
    if let Err(e) = db.reset_reservations().await {
        eprintln!("sharecraft-server: cannot reset the quota reservations: {e}");
    }
    match db.usage_rebuild_flagged().await {
        Ok(true) => {}
        Ok(false) => return report,
        Err(e) => {
            eprintln!("sharecraft-server: cannot read the usage flags: {e}");
            return report;
        }
    }
    let missing = match db.revision_sizes_missing().await {
        Ok(missing) => missing,
        Err(e) => {
            eprintln!("sharecraft-server: cannot list the versions without a size: {e}");
            return report;
        }
    };
    let store = state.checkpointer();
    for doc in missing {
        let history = blocking(store, move |s| s.history(doc).map_err(|e| e.to_string())).await;
        let sizes: Vec<(String, u64)> = match history {
            Ok(history) => history
                .into_iter()
                .map(|info| (info.revision.to_string(), info.size))
                .collect(),
            Err(error) => {
                report.failed.push((doc, error));
                continue;
            }
        };
        match db.set_revision_sizes(doc, &sizes).await {
            Ok(()) => report.filled += 1,
            Err(e) => report.failed.push((doc, e.to_string())),
        }
    }
    match db.rebuild_usage(report.failed.is_empty()).await {
        Ok(()) => report.rebuilt = true,
        Err(e) => eprintln!("sharecraft-server: cannot rebuild the sites' usage: {e}"),
    }
    report
}
