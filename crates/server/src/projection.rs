//! PostgreSQL's index of the revisions in Git: recorded after each checkpoint, reconciled when
//! the server starts, and rebuilt on request. Git stays the authority; whenever the index
//! disagrees with it, the document's list is replaced from Git.

use std::fmt;
use std::sync::Arc;

use collab_core::ids::DocId;
use records::{Db, RevisionRecord};
use revisions::{GitError, RevisionId, RevisionInfo};
use serde_json::Value;
use time::OffsetDateTime;
use tokio::sync::mpsc;

use crate::checkpoint::Checkpointer;

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
            Err(error) => report.failed.push((doc, error)),
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
    /// Starts the task that records `doc`'s revisions, read from `store`, in `db`.
    pub(crate) fn start(doc: DocId, store: Arc<dyn Checkpointer>, db: Db) -> Recorder {
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
    let appended = db
        .append_revision(parent.as_deref(), &to_record(info)?)
        .await
        .map_err(|e| e.to_string())?;
    if !appended {
        replace_from_git(store, db, doc).await?;
    }
    Ok(())
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
    })
}
