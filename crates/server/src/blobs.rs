//! The server's side of the content store: streaming bytes into it, claiming them for a
//! checkpoint (see [`records::Db::claim_blob`]), settling the claim afterwards, and erasing
//! content nothing names.
//!
//! Claims and erasures also take turns on a lock held by this process: a claim holds it shared
//! while it writes its pending row and checks the file is there, an erasure holds it alone
//! while it releases the row and deletes the file. So a claim either finishes before an erasure
//! starts (which then counts it and erases nothing), or starts after the file is gone (and
//! fails, saving nothing). PostgreSQL's row locks order the rows; this lock orders the file.

use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::{Body, Bytes};
use collab_core::ids::DocId;
use futures_util::StreamExt;
use records::{BLOB_REFS_DIRTY, ClaimId, Db};
use revisions::{BlobError, BlobRef, BlobStore, CheckpointError, Content, Outcome, RevisionId};
use tokio::sync::{RwLock, mpsc};

use crate::AppState;
use crate::checkpoint::{Checkpointer, WriteError};

/// Body chunks queued between the request and the blocking task writing them.
const BODY_QUEUE: usize = 2;

/// The content store under `data_dir`.
#[must_use]
pub fn blobs_path(data_dir: &Path) -> PathBuf {
    data_dir.join("blobs")
}

/// The content store, shared by the whole server.
pub(crate) struct Blobs {
    store: BlobStore,
    /// Held shared by a claim, alone by an erasure; see the module documentation.
    erasing: RwLock<()>,
}

/// Why a request body could not be stored.
#[derive(Debug)]
pub(crate) enum BodyError {
    /// The body is larger than the limit; nothing was stored.
    TooLarge,
    /// The body could not be read: the client went away or sent something broken.
    Broken,
    /// The store failed.
    Store(BlobError),
}

impl Blobs {
    /// The store `store`, with nothing claimed or being erased.
    pub(crate) fn new(store: BlobStore) -> Self {
        Self {
            store,
            erasing: RwLock::new(()),
        }
    }

    /// The store itself.
    pub(crate) fn store(&self) -> &BlobStore {
        &self.store
    }

    /// Stores `bytes`, already in memory (a text document's text), without blocking the async
    /// workers.
    pub(crate) async fn write_bytes(self: &Arc<Self>, bytes: Vec<u8>) -> Result<BlobRef, String> {
        let blobs = Arc::clone(self);
        tokio::task::spawn_blocking(move || blobs.store.write_from(&bytes[..], u64::MAX))
            .await
            .map_err(|e| format!("content store task failed: {e}"))?
            .map_err(|e| e.to_string())
    }

    /// Streams `body` into the store, hashing it on the way, holding no more than a few chunks
    /// at a time; at most `limit` bytes.
    pub(crate) async fn write_body(
        self: &Arc<Self>,
        body: Body,
        limit: u64,
    ) -> Result<BlobRef, BodyError> {
        let (tx, rx) = mpsc::channel(BODY_QUEUE);
        let blobs = Arc::clone(self);
        let writer = tokio::task::spawn_blocking(move || {
            blobs.store.write_from(ChannelReader::new(rx), limit)
        });
        let mut stream = body.into_data_stream();
        while let Some(frame) = stream.next().await {
            let item = frame.map_err(|e| std::io::Error::new(ErrorKind::ConnectionAborted, e));
            let failed = item.is_err();
            // The writer stops early when the limit is passed or the disk fails.
            if tx.send(item).await.is_err() || failed {
                break;
            }
        }
        drop(tx);
        match writer.await {
            Ok(Ok(blob)) => Ok(blob),
            Ok(Err(BlobError::TooLarge { .. })) => Err(BodyError::TooLarge),
            Ok(Err(BlobError::Io(e))) if e.kind() == ErrorKind::ConnectionAborted => {
                Err(BodyError::Broken)
            }
            Ok(Err(e)) => Err(BodyError::Store(e)),
            Err(e) => Err(BodyError::Store(BlobError::Io(std::io::Error::other(e)))),
        }
    }

    /// The whole of `content` in memory, without blocking the async workers. For text.
    pub(crate) async fn read_all(self: &Arc<Self>, content: Content) -> Result<Vec<u8>, String> {
        let blobs = Arc::clone(self);
        tokio::task::spawn_blocking(move || blobs.store.read_all(&content))
            .await
            .map_err(|e| format!("content store task failed: {e}"))?
            .map_err(|e| e.to_string())
    }

    /// Claims `blob` for a version of `doc` about to be committed, and checks it is still in
    /// the store. A claim for content erased meanwhile is withdrawn and fails.
    pub(crate) async fn claim(
        &self,
        db: &Db,
        blob: BlobRef,
        doc: DocId,
    ) -> Result<ClaimId, String> {
        let _turn = self.erasing.read().await;
        let claim = db
            .claim_blob(blob, doc)
            .await
            .map_err(|e| format!("cannot claim content {}: {e}", blob.hex()))?;
        if !self.store.contains(&blob.sha256) {
            self.settle(db, claim, None).await;
            return Err(format!(
                "content {} was erased before it could be saved",
                blob.hex()
            ));
        }
        Ok(claim)
    }

    /// Settles `claim` with the revision that names its content, or withdraws it. A failure is
    /// logged and flags the rows for a rebuild at the next start; the pending row it leaves
    /// keeps the content safe until the sweeper settles it from Git.
    pub(crate) async fn settle(&self, db: &Db, claim: ClaimId, revision: Option<&RevisionId>) {
        let revision = revision.map(ToString::to_string);
        if let Err(e) = db.settle_claim(claim, revision.as_deref()).await {
            eprintln!("sharecraft-server: cannot settle a content claim: {e}");
            flag_dirty(db).await;
        }
    }

    /// Erases the content `sha256` if nothing names it, pending claims included; whether it
    /// did.
    pub(crate) async fn erase_if_unreferenced(
        &self,
        db: &Db,
        sha256: &[u8; 32],
    ) -> Result<bool, String> {
        let _turn = self.erasing.write().await;
        if !db
            .release_blob_if_unreferenced(sha256)
            .await
            .map_err(|e| e.to_string())?
        {
            return Ok(false);
        }
        self.store.erase(sha256).map_err(|e| e.to_string())?;
        Ok(true)
    }
}

/// Flags the content rows for a rebuild at the next start, logging a failure.
pub(crate) async fn flag_dirty(db: &Db) {
    if let Err(e) = db.set_blob_refs_flag(BLOB_REFS_DIRTY).await {
        eprintln!("sharecraft-server: cannot flag the content rows for a rebuild: {e}");
    }
}

/// Commits a checkpoint naming `blob` with `write`, claiming the content first and settling the
/// claim after, when there is a database to claim it in (`db`); without one, the content is
/// only checked to be there.
pub(crate) async fn write_claimed(
    blobs: &Blobs,
    db: Option<&Db>,
    store: Arc<dyn Checkpointer>,
    doc: DocId,
    blob: BlobRef,
    write: impl FnOnce(&dyn Checkpointer) -> Result<Outcome, CheckpointError> + Send + 'static,
) -> Result<Outcome, WriteError> {
    let claim = match db {
        Some(db) => Some(
            blobs
                .claim(db, blob, doc)
                .await
                .map_err(WriteError::Failed)?,
        ),
        None if blobs.store.contains(&blob.sha256) => None,
        None => {
            return Err(WriteError::Failed(format!(
                "content {} is not in the content store",
                blob.hex()
            )));
        }
    };
    let result = match tokio::task::spawn_blocking(move || write(&*store)).await {
        Ok(result) => result.map_err(WriteError::from),
        Err(e) => Err(WriteError::Failed(format!("checkpoint task failed: {e}"))),
    };
    if let (Some(db), Some(claim)) = (db, claim) {
        let revision = match &result {
            Ok(Outcome::Written(revision)) => Some(revision),
            // Unchanged: the version already stored names this content.
            _ => None,
        };
        blobs.settle(db, claim, revision).await;
    }
    result
}

/// A pending claim older than this is settled from Git by the sweeper.
const CLAIM_LIFETIME: time::Duration = time::Duration::hours(1);

/// Settles every claim pending for more than an hour as of `now`, as Git has it: completed
/// with the newest revision of its document that names its content, or withdrawn if none does.
/// Failures are logged; the next sweep tries again.
pub(crate) async fn settle_stale_claims(state: &AppState, now: time::OffsetDateTime) {
    let db = state.db();
    let stale = match db.stale_claims(now - CLAIM_LIFETIME).await {
        Ok(stale) => stale,
        Err(e) => {
            eprintln!("sharecraft-server: database failure: {e}");
            return;
        }
    };
    for claim in stale {
        let store = Arc::clone(state.checkpointer());
        let doc = claim.doc;
        let history = tokio::task::spawn_blocking(move || store.history(doc)).await;
        let revision = match history {
            Ok(Ok(history)) => history
                .into_iter()
                .rev()
                .find(|info| info.blob == Some(claim.blob))
                .map(|info| info.revision),
            Ok(Err(e)) => {
                eprintln!(
                    "sharecraft-server: cannot read the history of document {}: {e}",
                    doc.0
                );
                continue;
            }
            Err(e) => {
                eprintln!("sharecraft-server: history task failed: {e}");
                continue;
            }
        };
        state
            .blobs()
            .settle(db, claim.claim, revision.as_ref())
            .await;
    }
}

/// Reads the chunks sent on a channel, in order, as one stream of bytes; blocks while waiting
/// for the next. Ends when the sender is dropped.
struct ChannelReader {
    rx: mpsc::Receiver<std::io::Result<Bytes>>,
    current: Bytes,
}

impl ChannelReader {
    fn new(rx: mpsc::Receiver<std::io::Result<Bytes>>) -> Self {
        Self {
            rx,
            current: Bytes::new(),
        }
    }
}

impl Read for ChannelReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        while self.current.is_empty() {
            match self.rx.blocking_recv() {
                Some(Ok(bytes)) => self.current = bytes,
                Some(Err(e)) => return Err(e),
                None => return Ok(0),
            }
        }
        let n = buf.len().min(self.current.len());
        let chunk = self.current.split_to(n);
        if let Some(target) = buf.get_mut(..n) {
            target.copy_from_slice(&chunk);
        }
        Ok(n)
    }
}

/// A content store under `dir`, for the server's own tests.
#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub(crate) fn test_blobs(dir: &Path) -> Arc<Blobs> {
    Arc::new(Blobs::new(BlobStore::open(&blobs_path(dir)).unwrap()))
}
