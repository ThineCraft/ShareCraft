//! Persistence boundary for the live room.

use std::future::Future;
use std::str::FromStr;
use std::sync::Arc;

use collab_core::ids::{ClientId, DocId, Seq, UserId};
use records::{
    Db, DbError, GoneReason, LiveLoaded, LiveLocal, LiveReturnTombstoneRow, NewLiveCancel,
    NewLiveChange, NewLiveCheckpoint, Origin,
};
use revisions::{Author, BlobRef, Checkpoint, Outcome, RevisionId, RevisionInfo};
use serde_json::{Map, Value};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::blobs::{Blobs, write_claimed};
use crate::checkpoint::{Checkpointer, WriteError};

/// A participant-written checkpoint to commit and record at one live position.
pub(crate) struct LiveWrite<'a> {
    /// The revision it was written on top of.
    pub base: Option<&'a str>,
    /// The file bytes, already in the content store.
    pub blob: BlobRef,
    /// Everyone credited, the first as the commit author.
    pub authors: &'a [Author],
    pub at: Seq,
    pub session: Uuid,
    pub session_data: &'a [u8],
    pub created_at: OffsetDateTime,
}

/// Why a participant-written checkpoint was not stored.
#[derive(Debug)]
pub(crate) enum LiveWriteError {
    /// The document has a revision the checkpoint was not based on.
    Stale { current: Option<String> },
    /// Nothing was committed.
    Failed(String),
    /// Git committed `revision`, but its checkpoint record is not known to be stored.
    Unrecorded {
        revision: String,
        fields: Map<String, Value>,
        described: Option<Box<RevisionInfo>>,
        error: String,
    },
}

/// What a verifier's disagreeing (forensic) upload left in the history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ForensicWritten {
    /// The revision holding the verifier's bytes.
    pub forensic: String,
    /// The document's current revision afterwards: normally a new revision restoring the
    /// agreed bytes, or the forensic revision itself when that restore could not be written.
    pub current: String,
}

/// The fields a live checkpoint revision stores: the base revision's fields, without an
/// earlier checkpoint's live facts, plus its own.
pub(crate) fn live_fields(base: &Map<String, Value>, at: Seq) -> Map<String, Value> {
    let mut fields = base.clone();
    for key in [
        "verified",
        "live_through",
        "major",
        "major_reason",
        "major_person",
    ] {
        fields.remove(key);
    }
    fields.insert("verified".to_owned(), Value::Bool(false));
    fields.insert("live_through".to_owned(), Value::from(at.0));
    fields
}

pub(crate) trait LiveStore: Clone + Send + Sync + 'static {
    fn load_live(&self, doc: DocId) -> impl Future<Output = Result<LiveLoaded, DbError>> + Send;

    /// The document's current revision as recorded, for a log with no live checkpoint yet.
    fn current_revision(
        &self,
        doc: DocId,
    ) -> impl Future<Output = Result<Option<String>, DbError>> + Send;

    /// Commits `write` on top of its base and records it, unverified, at its position: the
    /// revision index row and the checkpoint metadata together.
    fn write_live_checkpoint(
        &self,
        doc: DocId,
        write: &LiveWrite<'_>,
    ) -> impl Future<Output = Result<String, LiveWriteError>> + Send;

    /// Keeps a verifier's disagreeing bytes for `write.at` as a history revision on top of
    /// `write.base`, then restores the agreed checkpoint `agreed` as the current revision. No
    /// live checkpoint is recorded: the forensic bytes never become a catch-up base.
    fn write_live_forensic(
        &self,
        doc: DocId,
        write: &LiveWrite<'_>,
        agreed: &str,
    ) -> impl Future<Output = Result<ForensicWritten, LiveWriteError>> + Send;

    #[allow(dead_code)] // Records an existing revision; participant writes use the write above.
    fn record_live_checkpoint(
        &self,
        doc: DocId,
        checkpoint: &NewLiveCheckpoint<'_>,
    ) -> impl Future<Output = Result<(), DbError>> + Send;

    /// Records the checkpoint metadata of a revision Git committed (`described`, written on top
    /// of `parent`) after an unknown database outcome. The revision index row is recorded
    /// first when it is missing, in the same transaction, so reconciliation can always finish.
    fn reconcile_live_checkpoint(
        &self,
        doc: DocId,
        parent: Option<&str>,
        checkpoint: &NewLiveCheckpoint<'_>,
        described: &RevisionInfo,
        expected_fields: &Map<String, Value>,
    ) -> impl Future<Output = Result<(), DbError>> + Send;

    /// Marks the checkpoint `revision` at exactly `through` verified and advances the log's
    /// verified position, in one transaction.
    fn verify_live_checkpoint(
        &self,
        doc: DocId,
        revision: &str,
        through: Seq,
        at: OffsetDateTime,
    ) -> impl Future<Output = Result<(), DbError>> + Send;

    #[allow(dead_code)]
    fn live_return_tombstone(
        &self,
        doc: DocId,
        session: Uuid,
        participant: u32,
    ) -> impl Future<Output = Result<Option<LiveReturnTombstoneRow>, DbError>> + Send;

    fn start_live_session(
        &self,
        doc: DocId,
        session: Uuid,
        format: &str,
        at: OffsetDateTime,
    ) -> impl Future<Output = Result<(), DbError>> + Send;

    fn add_live_participant(
        &self,
        doc: DocId,
        session: Uuid,
        client: ClientId,
        user: UserId,
        at: OffsetDateTime,
    ) -> impl Future<Output = Result<u32, DbError>> + Send;

    fn end_live_place(
        &self,
        session: Uuid,
        number: u32,
        reason: GoneReason,
        at: OffsetDateTime,
    ) -> impl Future<Output = Result<(), DbError>> + Send;

    fn append_live_change(
        &self,
        doc: DocId,
        change: &NewLiveChange<'_>,
    ) -> impl Future<Output = Result<(), DbError>> + Send;

    fn cancel_live_entry(
        &self,
        doc: DocId,
        cancel: &NewLiveCancel,
    ) -> impl Future<Output = Result<(), DbError>> + Send;

    fn consume_live_local(
        &self,
        doc: DocId,
        client: ClientId,
        user: UserId,
        local: u64,
        reason: &str,
        at: OffsetDateTime,
    ) -> impl Future<Output = Result<(), DbError>> + Send;

    fn live_local(
        &self,
        doc: DocId,
        client: ClientId,
        local: u64,
    ) -> impl Future<Output = Result<LiveLocal, DbError>> + Send;

    fn end_live_session(
        &self,
        session: Uuid,
        at: OffsetDateTime,
    ) -> impl Future<Output = Result<(), DbError>> + Send;
}

fn validate_checkpoint_description(
    doc: DocId,
    checkpoint: &NewLiveCheckpoint<'_>,
    described: &RevisionInfo,
    expected_fields: &Map<String, Value>,
) -> Result<(), DbError> {
    let metadata_matches = described.doc == doc
        && described.revision.to_string() == checkpoint.revision
        && described.fields == *expected_fields
        && described
            .blob
            .as_ref()
            .is_some_and(|blob| blob.sha256 == checkpoint.sha256)
        && expected_fields.get("verified") == Some(&Value::Bool(false))
        && expected_fields.get("live_through").and_then(Value::as_u64)
            == Some(checkpoint.through.0);
    if !metadata_matches {
        return Err(DbError::new(
            "committed checkpoint does not match the expected document, base fields, position, or content hash",
        ));
    }
    Ok(())
}

impl LiveStore for Db {
    async fn load_live(&self, doc: DocId) -> Result<LiveLoaded, DbError> {
        Db::load_live(self, doc).await
    }

    async fn current_revision(&self, doc: DocId) -> Result<Option<String>, DbError> {
        Ok(self
            .document(doc)
            .await?
            .and_then(|document| document.revision))
    }

    async fn write_live_checkpoint(
        &self,
        _doc: DocId,
        _write: &LiveWrite<'_>,
    ) -> Result<String, LiveWriteError> {
        Err(LiveWriteError::Failed(
            "the database alone cannot commit checkpoint bytes".to_owned(),
        ))
    }

    async fn write_live_forensic(
        &self,
        _doc: DocId,
        _write: &LiveWrite<'_>,
        _agreed: &str,
    ) -> Result<ForensicWritten, LiveWriteError> {
        Err(LiveWriteError::Failed(
            "the database alone cannot commit checkpoint bytes".to_owned(),
        ))
    }

    async fn record_live_checkpoint(
        &self,
        doc: DocId,
        checkpoint: &NewLiveCheckpoint<'_>,
    ) -> Result<(), DbError> {
        Db::record_live_checkpoint(self, doc, checkpoint).await
    }

    async fn reconcile_live_checkpoint(
        &self,
        doc: DocId,
        parent: Option<&str>,
        checkpoint: &NewLiveCheckpoint<'_>,
        described: &RevisionInfo,
        expected_fields: &Map<String, Value>,
    ) -> Result<(), DbError> {
        validate_checkpoint_description(doc, checkpoint, described, expected_fields)?;
        let record = crate::projection::to_record(described.clone()).map_err(DbError::new)?;
        Db::record_live_checkpoint_revision(self, parent, &record, checkpoint).await?;
        if let Some(blob) = described.blob {
            // The content-store reference the projection would have recorded with the row.
            self.record_blob_ref(doc, blob, checkpoint.revision).await?;
        }
        Ok(())
    }

    async fn verify_live_checkpoint(
        &self,
        doc: DocId,
        revision: &str,
        through: Seq,
        at: OffsetDateTime,
    ) -> Result<(), DbError> {
        Db::verify_live_checkpoint(self, doc, revision, through, at).await
    }

    async fn live_return_tombstone(
        &self,
        doc: DocId,
        session: Uuid,
        participant: u32,
    ) -> Result<Option<LiveReturnTombstoneRow>, DbError> {
        Db::live_return_tombstone(self, doc, session, participant).await
    }

    async fn start_live_session(
        &self,
        doc: DocId,
        session: Uuid,
        format: &str,
        at: OffsetDateTime,
    ) -> Result<(), DbError> {
        Db::start_live_session(self, doc, session, format, at).await
    }

    async fn add_live_participant(
        &self,
        doc: DocId,
        session: Uuid,
        client: ClientId,
        user: UserId,
        at: OffsetDateTime,
    ) -> Result<u32, DbError> {
        Db::add_live_participant(self, doc, session, client, user, at).await
    }

    async fn end_live_place(
        &self,
        session: Uuid,
        number: u32,
        reason: GoneReason,
        at: OffsetDateTime,
    ) -> Result<(), DbError> {
        Db::end_live_place(self, session, number, reason, at).await
    }

    async fn append_live_change(
        &self,
        doc: DocId,
        change: &NewLiveChange<'_>,
    ) -> Result<(), DbError> {
        Db::append_live_change(self, doc, change).await
    }

    async fn cancel_live_entry(&self, doc: DocId, cancel: &NewLiveCancel) -> Result<(), DbError> {
        Db::cancel_live_entry(self, doc, cancel).await
    }

    async fn consume_live_local(
        &self,
        doc: DocId,
        client: ClientId,
        user: UserId,
        local: u64,
        reason: &str,
        at: OffsetDateTime,
    ) -> Result<(), DbError> {
        Db::consume_live_local(self, doc, client, user, local, reason, at).await
    }

    async fn live_local(
        &self,
        doc: DocId,
        client: ClientId,
        local: u64,
    ) -> Result<LiveLocal, DbError> {
        Db::live_local(self, doc, client, local).await
    }

    async fn end_live_session(&self, session: Uuid, at: OffsetDateTime) -> Result<(), DbError> {
        Db::end_live_session(self, session, at).await
    }
}

/// The production live store: the database, plus the library and content store that a
/// participant-written checkpoint is committed to.
#[derive(Clone)]
pub(crate) struct LiveDb {
    pub db: Db,
    pub store: Arc<dyn Checkpointer>,
    pub blobs: Arc<Blobs>,
}

impl LiveDb {
    /// Records `revision`, written on top of `parent`, in the revision index; a failure is
    /// logged, since Git holds it and the next start reconciles the index.
    async fn record_index(&self, doc: DocId, parent: Option<&RevisionId>, revision: &RevisionId) {
        if let Err(error) =
            crate::projection::record(&self.store, &self.db, doc, parent, revision).await
        {
            eprintln!(
                "sharecraft-server: revision {revision} of document {} is saved but could not \
                 be recorded in the database: {error}",
                doc.0
            );
        }
    }

    /// Runs `read` on the library without blocking the async workers.
    async fn blocking<T: Send + 'static>(
        &self,
        read: impl FnOnce(&dyn Checkpointer) -> Result<T, String> + Send + 'static,
    ) -> Result<T, String> {
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || read(&*store))
            .await
            .map_err(|error| format!("reading the library failed: {error}"))?
    }
}

impl LiveStore for LiveDb {
    async fn load_live(&self, doc: DocId) -> Result<LiveLoaded, DbError> {
        self.db.load_live(doc).await
    }

    async fn current_revision(&self, doc: DocId) -> Result<Option<String>, DbError> {
        LiveStore::current_revision(&self.db, doc).await
    }

    async fn write_live_checkpoint(
        &self,
        doc: DocId,
        write: &LiveWrite<'_>,
    ) -> Result<String, LiveWriteError> {
        let failed = |error: String| LiveWriteError::Failed(error);
        let name = self
            .db
            .document(doc)
            .await
            .map_err(|error| failed(error.to_string()))?
            .ok_or_else(|| failed(format!("document {} has no record", doc.0)))?
            .name;
        let base = write
            .base
            .map(RevisionId::from_str)
            .transpose()
            .map_err(|error| failed(error.to_string()))?;
        let base_fields = match base.clone() {
            Some(base) => self
                .blocking(move |store| {
                    store
                        .describe(doc, &base)
                        .map_err(|error| error.to_string())
                })
                .await
                .map_err(failed)?
                .map(|info| info.fields)
                .unwrap_or_default(),
            None => Map::new(),
        };
        let fields = live_fields(&base_fields, write.at);
        let blob = write.blob;
        let authors = write.authors.to_vec();
        let commit_fields = fields.clone();
        let commit_base = base.clone();
        let outcome = write_claimed(
            &self.blobs,
            Some(&self.db),
            Arc::clone(&self.store),
            doc,
            blob,
            Origin::EditorSave,
            move |store| {
                store.write(&Checkpoint {
                    doc,
                    file_name: &name,
                    blob,
                    base: commit_base.as_ref(),
                    authors: &authors,
                    fields: &commit_fields,
                })
            },
        )
        .await;
        let revision = match outcome {
            Ok(Outcome::Written(revision) | Outcome::Unchanged(revision)) => revision,
            Err(WriteError::Stale { current }) => {
                return Err(LiveWriteError::Stale {
                    current: current.map(|current| current.to_string()),
                });
            }
            Err(WriteError::Failed(error)) => return Err(failed(error)),
        };
        let wanted = revision.clone();
        let described = self
            .blocking(move |store| {
                store
                    .describe(doc, &wanted)
                    .map_err(|error| error.to_string())
            })
            .await;
        let text = revision.to_string();
        let checkpoint = NewLiveCheckpoint {
            revision: &text,
            through: write.at,
            sha256: blob.sha256,
            session: write.session,
            session_data: write.session_data,
            major_reason: None,
            major_person: None,
            created_at: write.created_at,
        };
        let unrecorded =
            |described: Option<RevisionInfo>, error: String| LiveWriteError::Unrecorded {
                revision: text.clone(),
                fields: fields.clone(),
                described: described.map(Box::new),
                error,
            };
        match described {
            Ok(Some(info)) => match self
                .reconcile_live_checkpoint(doc, write.base, &checkpoint, &info, &fields)
                .await
            {
                Ok(()) => Ok(text.clone()),
                Err(error) => Err(unrecorded(Some(info), error.to_string())),
            },
            Ok(None) => Err(unrecorded(None, "Git does not describe it".to_owned())),
            Err(error) => Err(unrecorded(None, error)),
        }
    }

    async fn write_live_forensic(
        &self,
        doc: DocId,
        write: &LiveWrite<'_>,
        agreed: &str,
    ) -> Result<ForensicWritten, LiveWriteError> {
        let failed = |error: String| LiveWriteError::Failed(error);
        let name = self
            .db
            .document(doc)
            .await
            .map_err(|error| failed(error.to_string()))?
            .ok_or_else(|| failed(format!("document {} has no record", doc.0)))?
            .name;
        let base = write
            .base
            .map(RevisionId::from_str)
            .transpose()
            .map_err(|error| failed(error.to_string()))?;
        let agreed_id = RevisionId::from_str(agreed).map_err(|error| failed(error.to_string()))?;
        let wanted = agreed_id.clone();
        let agreed_info = self
            .blocking(move |store| {
                store
                    .describe(doc, &wanted)
                    .map_err(|error| error.to_string())
            })
            .await
            .map_err(failed)?
            .ok_or_else(|| failed(format!("{agreed} is not a revision of the document")))?;
        let agreed_blob = agreed_info
            .blob
            .ok_or_else(|| failed(format!("{agreed} has no content-store bytes")))?;
        let mut fields = live_fields(&agreed_info.fields, write.at);
        fields.insert("forensic_of".to_owned(), Value::from(agreed));
        let blob = write.blob;
        let authors = write.authors.to_vec();
        let commit_base = base.clone();
        let outcome = write_claimed(
            &self.blobs,
            Some(&self.db),
            Arc::clone(&self.store),
            doc,
            blob,
            Origin::EditorSave,
            {
                let name = name.clone();
                move |store| {
                    store.write(&Checkpoint {
                        doc,
                        file_name: &name,
                        blob,
                        base: commit_base.as_ref(),
                        authors: &authors,
                        fields: &fields,
                    })
                }
            },
        )
        .await;
        let forensic = match outcome {
            Ok(Outcome::Written(revision) | Outcome::Unchanged(revision)) => revision,
            Err(WriteError::Stale { current }) => {
                return Err(LiveWriteError::Stale {
                    current: current.map(|current| current.to_string()),
                });
            }
            Err(WriteError::Failed(error)) => return Err(failed(error)),
        };
        self.record_index(doc, base.as_ref(), &forensic).await;
        // The agreed checkpoint becomes current again, so the forensic bytes stay history.
        let mut restored_fields = agreed_info.fields.clone();
        restored_fields.insert("restores".to_owned(), Value::from(agreed));
        let restored_authors = agreed_info.authors.clone();
        let restore_base = forensic.clone();
        let restored = write_claimed(
            &self.blobs,
            Some(&self.db),
            Arc::clone(&self.store),
            doc,
            agreed_blob,
            Origin::EditorSave,
            move |store| {
                store.write(&Checkpoint {
                    doc,
                    file_name: &name,
                    blob: agreed_blob,
                    base: Some(&restore_base),
                    authors: &restored_authors,
                    fields: &restored_fields,
                })
            },
        )
        .await;
        let current = match restored {
            Ok(Outcome::Written(revision) | Outcome::Unchanged(revision)) => {
                self.record_index(doc, Some(&forensic), &revision).await;
                revision.to_string()
            }
            Err(error) => {
                eprintln!(
                    "sharecraft-server: the agreed checkpoint {agreed} of document {} could not \
                     be restored after forensic revision {forensic}: {error:?}",
                    doc.0
                );
                forensic.to_string()
            }
        };
        Ok(ForensicWritten {
            forensic: forensic.to_string(),
            current,
        })
    }

    async fn record_live_checkpoint(
        &self,
        doc: DocId,
        checkpoint: &NewLiveCheckpoint<'_>,
    ) -> Result<(), DbError> {
        self.db.record_live_checkpoint(doc, checkpoint).await
    }

    async fn reconcile_live_checkpoint(
        &self,
        doc: DocId,
        parent: Option<&str>,
        checkpoint: &NewLiveCheckpoint<'_>,
        described: &RevisionInfo,
        expected_fields: &Map<String, Value>,
    ) -> Result<(), DbError> {
        LiveStore::reconcile_live_checkpoint(
            &self.db,
            doc,
            parent,
            checkpoint,
            described,
            expected_fields,
        )
        .await
    }

    async fn verify_live_checkpoint(
        &self,
        doc: DocId,
        revision: &str,
        through: Seq,
        at: OffsetDateTime,
    ) -> Result<(), DbError> {
        self.db
            .verify_live_checkpoint(doc, revision, through, at)
            .await
    }

    async fn live_return_tombstone(
        &self,
        doc: DocId,
        session: Uuid,
        participant: u32,
    ) -> Result<Option<LiveReturnTombstoneRow>, DbError> {
        self.db
            .live_return_tombstone(doc, session, participant)
            .await
    }

    async fn start_live_session(
        &self,
        doc: DocId,
        session: Uuid,
        format: &str,
        at: OffsetDateTime,
    ) -> Result<(), DbError> {
        self.db.start_live_session(doc, session, format, at).await
    }

    async fn add_live_participant(
        &self,
        doc: DocId,
        session: Uuid,
        client: ClientId,
        user: UserId,
        at: OffsetDateTime,
    ) -> Result<u32, DbError> {
        self.db
            .add_live_participant(doc, session, client, user, at)
            .await
    }

    async fn end_live_place(
        &self,
        session: Uuid,
        number: u32,
        reason: GoneReason,
        at: OffsetDateTime,
    ) -> Result<(), DbError> {
        self.db.end_live_place(session, number, reason, at).await
    }

    async fn append_live_change(
        &self,
        doc: DocId,
        change: &NewLiveChange<'_>,
    ) -> Result<(), DbError> {
        self.db.append_live_change(doc, change).await
    }

    async fn cancel_live_entry(&self, doc: DocId, cancel: &NewLiveCancel) -> Result<(), DbError> {
        self.db.cancel_live_entry(doc, cancel).await
    }

    async fn consume_live_local(
        &self,
        doc: DocId,
        client: ClientId,
        user: UserId,
        local: u64,
        reason: &str,
        at: OffsetDateTime,
    ) -> Result<(), DbError> {
        self.db
            .consume_live_local(doc, client, user, local, reason, at)
            .await
    }

    async fn live_local(
        &self,
        doc: DocId,
        client: ClientId,
        local: u64,
    ) -> Result<LiveLocal, DbError> {
        self.db.live_local(doc, client, local).await
    }

    async fn end_live_session(&self, session: Uuid, at: OffsetDateTime) -> Result<(), DbError> {
        self.db.end_live_session(session, at).await
    }
}

#[cfg(test)]
pub(crate) use memory::MemStore;

#[cfg(test)]
mod memory {
    use std::collections::{HashMap, HashSet};
    use std::sync::{Arc, Mutex, MutexGuard};

    use records::{
        LiveCheckpointRow, LiveEntryRow, LiveParticipantRow, LiveSessionDataRow, LiveSessionRow,
    };

    use super::*;

    #[derive(Clone, Default)]
    pub(crate) struct MemStore {
        inner: Arc<Mutex<State>>,
    }

    #[derive(Default)]
    struct State {
        docs: HashMap<DocId, MemoryDoc>,
        fail_next: bool,
        fail_after_next: bool,
        last_authors: Vec<Author>,
        /// While set, each checkpoint write waits for one permit before it commits.
        write_gate: Option<Arc<tokio::sync::Semaphore>>,
    }

    #[derive(Clone)]
    struct MemoryCheckpoint {
        row: LiveCheckpointRow,
        created_at: OffsetDateTime,
    }

    struct MemoryDoc {
        head: Seq,
        last_participant: u32,
        format: Option<String>,
        verified_through: Seq,
        checkpoints: Vec<MemoryCheckpoint>,
        entries: Vec<LiveEntryRow>,
        unsaved_entries: u64,
        unsaved_bytes: u64,
        sessions: HashSet<Uuid>,
        open_session: Option<LiveSessionRow>,
        participants: Vec<LiveParticipantRow>,
        clients: HashMap<ClientId, (UserId, u64)>,
        locals: HashMap<(ClientId, u64), LiveLocal>,
        return_tombstones: Vec<LiveReturnTombstoneRow>,
        current_revision: Option<String>,
        revisions_written: u64,
        /// Every revision written, oldest first, with the SHA-256 of its content.
        revisions: Vec<(String, [u8; 32])>,
    }

    impl Default for MemoryDoc {
        fn default() -> Self {
            Self {
                head: Seq(0),
                last_participant: 0,
                format: None,
                verified_through: Seq(0),
                checkpoints: Vec::new(),
                entries: Vec::new(),
                unsaved_entries: 0,
                unsaved_bytes: 0,
                sessions: HashSet::new(),
                open_session: None,
                participants: Vec::new(),
                clients: HashMap::new(),
                locals: HashMap::new(),
                return_tombstones: Vec::new(),
                current_revision: None,
                revisions_written: 0,
                revisions: Vec::new(),
            }
        }
    }

    impl MemStore {
        pub(crate) fn new() -> Self {
            Self::default()
        }

        pub(crate) fn fail_next(&self) {
            if let Ok(mut state) = self.inner.lock() {
                state.fail_next = true;
            }
        }

        pub(crate) fn fail_after_next(&self) {
            if let Ok(mut state) = self.inner.lock() {
                state.fail_after_next = true;
            }
        }

        /// Makes `revision` the document's current revision, as an outside save would.
        pub(crate) fn set_current_revision(&self, doc: DocId, revision: String) {
            if let Ok(mut state) = self.inner.lock() {
                state.docs.entry(doc).or_default().current_revision = Some(revision);
            }
        }

        /// The authors credited by the last checkpoint written.
        pub(crate) fn last_authors(&self) -> Vec<Author> {
            self.inner
                .lock()
                .map(|state| state.last_authors.clone())
                .unwrap_or_default()
        }

        /// Makes every later checkpoint write wait for a permit of the returned gate, as a
        /// slow Git write would.
        pub(crate) fn hold_checkpoint_writes(&self) -> Arc<tokio::sync::Semaphore> {
            let gate = Arc::new(tokio::sync::Semaphore::new(0));
            if let Ok(mut state) = self.inner.lock() {
                state.write_gate = Some(Arc::clone(&gate));
            }
            gate
        }

        /// Every revision written for `doc`, oldest first, with its content's SHA-256.
        pub(crate) fn revisions(&self, doc: DocId) -> Vec<(String, [u8; 32])> {
            self.inner
                .lock()
                .ok()
                .and_then(|state| state.docs.get(&doc).map(|memory| memory.revisions.clone()))
                .unwrap_or_default()
        }

        async fn wait_for_gate(&self) {
            let gate = self
                .inner
                .lock()
                .ok()
                .and_then(|state| state.write_gate.clone());
            if let Some(gate) = gate
                && let Ok(permit) = gate.acquire().await
            {
                permit.forget();
            }
        }

        pub(crate) fn entry_count(&self) -> usize {
            self.inner
                .lock()
                .map(|state| state.docs.values().map(|doc| doc.entries.len()).sum())
                .unwrap_or_default()
        }

        fn state(&self) -> Result<MutexGuard<'_, State>, DbError> {
            self.inner
                .lock()
                .map_err(|_| DbError::new("memory live store lock poisoned"))
        }

        fn write(&self) -> Result<MutexGuard<'_, State>, DbError> {
            let mut state = self.state()?;
            if state.fail_next {
                state.fail_next = false;
                return Err(DbError::new("injected live store failure"));
            }
            Ok(state)
        }

        fn finish_write(mut state: MutexGuard<'_, State>) -> Result<(), DbError> {
            if state.fail_after_next {
                state.fail_after_next = false;
                return Err(DbError::new(
                    "injected live store failure after durable write",
                ));
            }
            Ok(())
        }
    }

    impl LiveStore for MemStore {
        async fn current_revision(&self, doc: DocId) -> Result<Option<String>, DbError> {
            Ok(self
                .state()?
                .docs
                .get(&doc)
                .and_then(|memory| memory.current_revision.clone()))
        }

        async fn write_live_checkpoint(
            &self,
            doc: DocId,
            write: &LiveWrite<'_>,
        ) -> Result<String, LiveWriteError> {
            self.wait_for_gate().await;
            let revision = {
                let mut state = self
                    .write()
                    .map_err(|error| LiveWriteError::Failed(error.to_string()))?;
                state.last_authors = write.authors.to_vec();
                let memory = state.docs.entry(doc).or_default();
                if write.base.map(str::to_owned) != memory.current_revision {
                    return Err(LiveWriteError::Stale {
                        current: memory.current_revision.clone(),
                    });
                }
                memory.revisions_written = memory.revisions_written.saturating_add(1);
                let revision = format!("{:040x}", memory.revisions_written);
                memory.current_revision = Some(revision.clone());
                memory.revisions.push((revision.clone(), write.blob.sha256));
                revision
            };
            self.record_live_checkpoint(
                doc,
                &NewLiveCheckpoint {
                    revision: &revision,
                    through: write.at,
                    sha256: write.blob.sha256,
                    session: write.session,
                    session_data: write.session_data,
                    major_reason: None,
                    major_person: None,
                    created_at: write.created_at,
                },
            )
            .await
            .map_err(|error| LiveWriteError::Unrecorded {
                revision: revision.clone(),
                fields: live_fields(&Map::new(), write.at),
                described: None,
                error: error.to_string(),
            })?;
            Ok(revision)
        }

        async fn write_live_forensic(
            &self,
            doc: DocId,
            write: &LiveWrite<'_>,
            agreed: &str,
        ) -> Result<ForensicWritten, LiveWriteError> {
            self.wait_for_gate().await;
            let mut state = self
                .write()
                .map_err(|error| LiveWriteError::Failed(error.to_string()))?;
            let memory = state.docs.entry(doc).or_default();
            if write.base.map(str::to_owned) != memory.current_revision {
                return Err(LiveWriteError::Stale {
                    current: memory.current_revision.clone(),
                });
            }
            let agreed_sha = memory
                .revisions
                .iter()
                .find(|(revision, _)| revision == agreed)
                .map(|(_, sha)| *sha)
                .ok_or_else(|| LiveWriteError::Failed(format!("{agreed} is not a revision")))?;
            let mut written = Vec::new();
            for sha in [write.blob.sha256, agreed_sha] {
                memory.revisions_written = memory.revisions_written.saturating_add(1);
                let revision = format!("{:040x}", memory.revisions_written);
                memory.revisions.push((revision.clone(), sha));
                written.push(revision);
            }
            let current = written.pop().unwrap_or_default();
            let forensic = written.pop().unwrap_or_default();
            memory.current_revision = Some(current.clone());
            Ok(ForensicWritten { forensic, current })
        }

        async fn load_live(&self, doc: DocId) -> Result<LiveLoaded, DbError> {
            let mut state = self.state()?;
            let memory = state.docs.entry(doc).or_default();
            let verified_checkpoint = memory
                .checkpoints
                .iter()
                .filter(|checkpoint| checkpoint.row.verified)
                .max_by_key(|checkpoint| checkpoint.row.through)
                .map(|checkpoint| checkpoint.row.clone());
            let mut unverified_checkpoints = memory
                .checkpoints
                .iter()
                .filter(|checkpoint| {
                    !checkpoint.row.verified
                        && (checkpoint.row.through > memory.verified_through
                            || (memory.verified_through == Seq(0)
                                && checkpoint.row.through == Seq(0)))
                })
                .map(|checkpoint| checkpoint.row.clone())
                .collect::<Vec<_>>();
            unverified_checkpoints.sort_by_key(|checkpoint| checkpoint.through);
            let entries = memory
                .entries
                .iter()
                .filter(|entry| match entry {
                    LiveEntryRow::Change { seq, .. } | LiveEntryRow::Cancel { seq, .. } => {
                        *seq > memory.verified_through
                    }
                })
                .cloned()
                .collect();
            let mut newest_session_data = HashMap::<Uuid, LiveSessionDataRow>::new();
            for checkpoint in memory
                .checkpoints
                .iter()
                .filter(|checkpoint| !checkpoint.row.session_data.is_empty())
            {
                let row = LiveSessionDataRow {
                    session: checkpoint.row.session,
                    through: checkpoint.row.through,
                    session_data: checkpoint.row.session_data.clone(),
                };
                newest_session_data
                    .entry(row.session)
                    .and_modify(|current| {
                        if row.through > current.through {
                            *current = row.clone();
                        }
                    })
                    .or_insert(row);
            }
            let mut retained_session_data = newest_session_data.into_values().collect::<Vec<_>>();
            retained_session_data.sort_by_key(|row| row.through);
            Ok(LiveLoaded {
                head: memory.head,
                last_participant: memory.last_participant,
                format: memory.format.clone(),
                verified_through: memory.verified_through,
                verified_checkpoint,
                unverified_checkpoints,
                entries,
                unsaved_entries: memory.unsaved_entries,
                unsaved_bytes: memory.unsaved_bytes,
                open_session: memory.open_session.clone(),
                participants: memory.participants.clone(),
                clients: memory
                    .clients
                    .iter()
                    .map(|(client, (user, next))| (*client, *user, *next))
                    .collect(),
                retained_session_data,
            })
        }

        async fn record_live_checkpoint(
            &self,
            doc: DocId,
            checkpoint: &NewLiveCheckpoint<'_>,
        ) -> Result<(), DbError> {
            let mut state = self.write()?;
            let memory = state.docs.entry(doc).or_default();
            if checkpoint.through > memory.head {
                return Err(DbError::new("checkpoint position exceeds the live head"));
            }
            if !memory.sessions.contains(&checkpoint.session) {
                return Err(DbError::new(
                    "checkpoint session does not belong to the document",
                ));
            }
            let row = LiveCheckpointRow {
                revision: checkpoint.revision.to_owned(),
                through: checkpoint.through,
                sha256: checkpoint.sha256,
                verified: false,
                session: checkpoint.session,
                session_data: checkpoint.session_data.to_vec(),
                major_reason: checkpoint.major_reason.map(str::to_owned),
                major_person: checkpoint.major_person,
            };
            if let Some(existing) = memory.checkpoints.iter().find(|existing| {
                existing.row.revision == row.revision || existing.row.through == row.through
            }) {
                let mut stored = existing.row.clone();
                stored.verified = false;
                if stored != row || existing.created_at != checkpoint.created_at {
                    return Err(DbError::new("checkpoint revision or position conflicts"));
                }
            } else {
                memory.checkpoints.push(MemoryCheckpoint {
                    row,
                    created_at: checkpoint.created_at,
                });
            }
            Self::finish_write(state)
        }

        async fn reconcile_live_checkpoint(
            &self,
            doc: DocId,
            _parent: Option<&str>,
            checkpoint: &NewLiveCheckpoint<'_>,
            described: &RevisionInfo,
            expected_fields: &Map<String, Value>,
        ) -> Result<(), DbError> {
            validate_checkpoint_description(doc, checkpoint, described, expected_fields)?;
            self.record_live_checkpoint(doc, checkpoint).await
        }

        async fn verify_live_checkpoint(
            &self,
            doc: DocId,
            revision: &str,
            through: Seq,
            _at: OffsetDateTime,
        ) -> Result<(), DbError> {
            let mut state = self.write()?;
            let memory = state.docs.entry(doc).or_default();
            if through > memory.head {
                return Err(DbError::new("checkpoint position exceeds the live head"));
            }
            let checkpoint = memory
                .checkpoints
                .iter_mut()
                .find(|checkpoint| {
                    checkpoint.row.revision == revision && checkpoint.row.through == through
                })
                .ok_or_else(|| DbError::new("no checkpoint at the exact revision and position"))?;
            checkpoint.row.verified = true;
            if through > memory.verified_through {
                memory.verified_through = through;
                memory.unsaved_entries = memory
                    .entries
                    .iter()
                    .filter(|entry| match entry {
                        LiveEntryRow::Change { seq, .. } | LiveEntryRow::Cancel { seq, .. } => {
                            *seq > through
                        }
                    })
                    .count()
                    .try_into()
                    .unwrap_or(u64::MAX);
                memory.unsaved_bytes = memory
                    .entries
                    .iter()
                    .filter_map(|entry| match entry {
                        LiveEntryRow::Change { seq, payload, .. } if *seq > through => {
                            Some(payload.len())
                        }
                        _ => None,
                    })
                    .sum::<usize>()
                    .try_into()
                    .unwrap_or(u64::MAX);
            }
            Self::finish_write(state)
        }

        async fn live_return_tombstone(
            &self,
            doc: DocId,
            session: Uuid,
            participant: u32,
        ) -> Result<Option<LiveReturnTombstoneRow>, DbError> {
            Ok(self
                .state()?
                .docs
                .get(&doc)
                .and_then(|memory| {
                    memory
                        .return_tombstones
                        .iter()
                        .find(|row| row.session == session && row.participant == participant)
                })
                .cloned())
        }

        async fn start_live_session(
            &self,
            doc: DocId,
            session: Uuid,
            format: &str,
            at: OffsetDateTime,
        ) -> Result<(), DbError> {
            let mut state = self.write()?;
            let memory = state.docs.entry(doc).or_default();
            if memory.open_session.is_some() {
                return Err(DbError::new("live session already open"));
            }
            memory.open_session = Some(LiveSessionRow {
                id: session,
                format: format.to_owned(),
                first_seq: Seq(memory.head.0 + 1),
                started_at: at,
                participants: Vec::new(),
            });
            memory.sessions.insert(session);
            Ok(())
        }

        async fn add_live_participant(
            &self,
            doc: DocId,
            session: Uuid,
            client: ClientId,
            user: UserId,
            at: OffsetDateTime,
        ) -> Result<u32, DbError> {
            let mut state = self.write()?;
            let memory = state.docs.entry(doc).or_default();
            if memory.open_session.as_ref().map(|open| open.id) != Some(session) {
                return Err(DbError::new("live session is not open"));
            }
            memory.last_participant = memory
                .last_participant
                .checked_add(1)
                .ok_or_else(|| DbError::new("participant numbers exhausted"))?;
            let number = memory.last_participant;
            if number == 0 {
                return Err(DbError::new("participant zero is reserved"));
            }
            if let Some(open) = &mut memory.open_session {
                let participant = LiveParticipantRow {
                    number,
                    client,
                    user,
                    joined_at: at,
                    gone: None,
                };
                open.participants.push(participant.clone());
                memory.participants.push(participant);
            }
            Ok(number)
        }

        async fn end_live_place(
            &self,
            session: Uuid,
            number: u32,
            reason: GoneReason,
            _at: OffsetDateTime,
        ) -> Result<(), DbError> {
            let mut state = self.write()?;
            let memory = state
                .docs
                .values_mut()
                .find(|doc| doc.open_session.as_ref().map(|open| open.id) == Some(session))
                .ok_or_else(|| DbError::new("live place does not exist"))?;
            let place = memory
                .open_session
                .as_mut()
                .and_then(|open| open.participants.iter_mut().find(|p| p.number == number))
                .ok_or_else(|| DbError::new("live place does not exist"))?;
            place.gone.get_or_insert(reason);
            let historical = memory
                .participants
                .iter_mut()
                .find(|place| place.number == number)
                .ok_or_else(|| DbError::new("historical live place does not exist"))?;
            historical.gone.get_or_insert(reason);
            Ok(())
        }

        async fn append_live_change(
            &self,
            doc: DocId,
            change: &NewLiveChange<'_>,
        ) -> Result<(), DbError> {
            let mut state = self.write()?;
            let memory = state.docs.entry(doc).or_default();
            if change.seq.0 != memory.head.0 + 1 {
                return Err(DbError::new("change does not follow live head"));
            }
            if !matches!(
                memory.locals.get(&(change.client, change.local)),
                None | Some(LiveLocal::Unseen)
            ) {
                return Err(DbError::new("local already used"));
            }
            memory.head = change.seq;
            memory.format = Some(change.format.to_owned());
            memory.entries.push(LiveEntryRow::Change {
                seq: change.seq,
                session: change.session,
                client: change.client,
                local: change.local,
                participant: change.participant,
                user: change.user,
                seen: change.seen,
                at: change.at,
                seed: change.seed,
                format: change.format.to_owned(),
                payload: change.payload.to_vec(),
                witnesses: change.witnesses.to_vec(),
                cancelled_by: None,
            });
            memory.unsaved_entries = memory.unsaved_entries.saturating_add(1);
            memory.unsaved_bytes = memory
                .unsaved_bytes
                .saturating_add(u64::try_from(change.payload.len()).unwrap_or(u64::MAX));
            memory.locals.insert(
                (change.client, change.local),
                LiveLocal::Stored { seq: change.seq },
            );
            let next = change.local.saturating_add(1);
            memory
                .clients
                .entry(change.client)
                .and_modify(|(_, current)| *current = (*current).max(next))
                .or_insert((change.user, next));
            Ok(())
        }

        async fn cancel_live_entry(
            &self,
            doc: DocId,
            cancel: &NewLiveCancel,
        ) -> Result<(), DbError> {
            let mut state = self.write()?;
            let memory = state.docs.entry(doc).or_default();
            if cancel.seq.0 != memory.head.0 + 1 {
                return Err(DbError::new("cancel does not follow live head"));
            }
            if memory.open_session.as_ref().map(|open| open.id) != Some(cancel.session) {
                return Err(DbError::new("live session is not open"));
            }
            let Some(index) = memory.entries.iter().position(|entry| {
                matches!(entry, LiveEntryRow::Change { seq, cancelled_by: None, .. } if *seq == cancel.cancels)
            }) else {
                return Err(DbError::new("cancel target is not an uncancelled change"));
            };
            let Some(LiveEntryRow::Change {
                seq: target_seq,
                payload,
                cancelled_by,
                ..
            }) = memory.entries.get_mut(index)
            else {
                return Err(DbError::new("cancel target is not a change"));
            };
            if *target_seq > memory.verified_through {
                memory.unsaved_bytes = memory
                    .unsaved_bytes
                    .saturating_sub(u64::try_from(payload.len()).unwrap_or(u64::MAX));
            }
            payload.clear();
            *cancelled_by = Some(cancel.seq);
            memory.entries.push(LiveEntryRow::Cancel {
                seq: cancel.seq,
                session: cancel.session,
                at: cancel.at,
                seed: cancel.seed,
                cancels: cancel.cancels,
                reports: cancel.reports,
            });
            memory.unsaved_entries = memory.unsaved_entries.saturating_add(1);
            memory.head = cancel.seq;
            Ok(())
        }

        async fn consume_live_local(
            &self,
            doc: DocId,
            client: ClientId,
            user: UserId,
            local: u64,
            reason: &str,
            _at: OffsetDateTime,
        ) -> Result<(), DbError> {
            let mut state = self.write()?;
            let memory = state.docs.entry(doc).or_default();
            memory
                .locals
                .entry((client, local))
                .or_insert_with(|| LiveLocal::Refused {
                    reason: reason.to_owned(),
                });
            let next = local.saturating_add(1);
            memory
                .clients
                .entry(client)
                .and_modify(|(_, current)| *current = (*current).max(next))
                .or_insert((user, next));
            Ok(())
        }

        async fn live_local(
            &self,
            doc: DocId,
            client: ClientId,
            local: u64,
        ) -> Result<LiveLocal, DbError> {
            Ok(self
                .state()?
                .docs
                .get(&doc)
                .and_then(|memory| memory.locals.get(&(client, local)))
                .cloned()
                .unwrap_or(LiveLocal::Unseen))
        }

        async fn end_live_session(&self, session: Uuid, at: OffsetDateTime) -> Result<(), DbError> {
            let mut state = self.write()?;
            let memory = state
                .docs
                .values_mut()
                .find(|doc| doc.sessions.contains(&session))
                .ok_or_else(|| DbError::new("live session does not exist"))?;
            if let Some(open) = memory.open_session.take().filter(|open| open.id == session) {
                for participant in open.participants {
                    if memory
                        .return_tombstones
                        .iter()
                        .any(|row| row.session == session && row.participant == participant.number)
                    {
                        continue;
                    }
                    memory.return_tombstones.push(LiveReturnTombstoneRow {
                        session,
                        participant: participant.number,
                        client: participant.client,
                        user: participant.user,
                        ended_at: at,
                        final_seq: memory.head,
                    });
                }
            }
            Ok(())
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]
mod tests {
    use collab_core::ids::{ClientId, DocId, Seq, UserId};
    use records::{NewLiveCancel, NewLiveChange, NewLiveCheckpoint};
    use revisions::{BlobRef, RevisionInfo};
    use serde_json::{Map, Value, json};
    use time::OffsetDateTime;
    use uuid::Uuid;

    use super::{LiveStore, MemStore};

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn checkpoint<'a>(
        revision: &'a str,
        session: Uuid,
        session_data: &'a [u8],
    ) -> NewLiveCheckpoint<'a> {
        NewLiveCheckpoint {
            revision,
            through: Seq(0),
            sha256: [9; 32],
            session,
            session_data,
            major_reason: None,
            major_person: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn checkpoint_fields(through: Seq) -> Map<String, Value> {
        serde_json::from_value(json!({
            "title": "kept base field",
            "verified": false,
            "live_through": through.0,
        }))
        .unwrap()
    }

    fn described_revision(
        doc: DocId,
        checkpoint: &NewLiveCheckpoint<'_>,
        fields: Map<String, Value>,
    ) -> RevisionInfo {
        RevisionInfo {
            revision: checkpoint.revision.parse().unwrap(),
            doc,
            committed_at: 0,
            authors: Vec::new(),
            file_name: "book.gridcraft".into(),
            fields,
            blob: Some(BlobRef {
                sha256: checkpoint.sha256,
                size: 123,
            }),
            size: 123,
        }
    }

    async fn started_store() -> (MemStore, DocId, Uuid) {
        let store = MemStore::new();
        let doc = DocId(id(100));
        let session = id(200);
        store
            .start_live_session(doc, session, "gridcraft/1", OffsetDateTime::UNIX_EPOCH)
            .await
            .unwrap();
        (store, doc, session)
    }

    #[tokio::test]
    async fn restart_loads_session_data_with_the_base() {
        let (store, doc, session) = started_store().await;
        let expected = b"checkpoint identity table";
        store
            .record_live_checkpoint(doc, &checkpoint("revision-one", session, expected))
            .await
            .unwrap();
        store
            .verify_live_checkpoint(doc, "revision-one", Seq(0), OffsetDateTime::UNIX_EPOCH)
            .await
            .unwrap();

        let restarted = store.load_live(doc).await.unwrap();
        let base = restarted.verified_checkpoint.unwrap();
        assert_eq!(base.revision, "revision-one");
        assert_eq!(base.through, Seq(0));
        assert_eq!(base.session_data, expected);
    }

    #[tokio::test]
    async fn checkpoint_commit_reconciles_after_unknown_db_outcome() {
        let (store, doc, session) = started_store().await;
        let checkpoint = checkpoint(
            "1111111111111111111111111111111111111111",
            session,
            b"identities",
        );
        let fields = checkpoint_fields(checkpoint.through);
        let described = described_revision(doc, &checkpoint, fields.clone());
        store.fail_next();
        assert!(
            store
                .record_live_checkpoint(doc, &checkpoint)
                .await
                .is_err()
        );
        assert!(
            store
                .load_live(doc)
                .await
                .unwrap()
                .unverified_checkpoints
                .is_empty(),
            "a failure before the durable operation stores nothing"
        );

        store.fail_after_next();
        assert!(
            store
                .record_live_checkpoint(doc, &checkpoint)
                .await
                .is_err(),
            "the caller sees an unknown outcome after the durable write"
        );

        store
            .reconcile_live_checkpoint(doc, None, &checkpoint, &described, &fields)
            .await
            .expect("reconcile repeats the exact durable operation");
        let restarted = store.load_live(doc).await.unwrap();
        assert!(restarted.verified_checkpoint.is_none());
        assert_eq!(restarted.unverified_checkpoints.len(), 1);
        assert_eq!(
            restarted.unverified_checkpoints[0].revision,
            checkpoint.revision
        );

        store.fail_after_next();
        assert!(
            store
                .verify_live_checkpoint(
                    doc,
                    checkpoint.revision,
                    Seq(0),
                    OffsetDateTime::UNIX_EPOCH,
                )
                .await
                .is_err(),
            "the caller sees an unknown outcome after durable verification"
        );
        assert!(
            store
                .load_live(doc)
                .await
                .unwrap()
                .verified_checkpoint
                .is_some(),
            "the verification itself was durable"
        );
        store
            .verify_live_checkpoint(doc, checkpoint.revision, Seq(0), OffsetDateTime::UNIX_EPOCH)
            .await
            .expect("reconcile repeats the exact durable verification");
    }

    #[tokio::test]
    async fn database_reconcile_records_a_missing_revision_row_before_the_checkpoint() {
        use crate::checkpoint::Checkpointer;
        use records::test_support::TestDb;
        use records::{Editor, NewUser};
        use revisions::{Author, Checkpoint, Outcome, RevisionStore};

        let test = TestDb::create().await;
        let db = test.db.clone();
        let ann = db
            .create_user(
                None,
                NewUser {
                    email: "ann@test.invalid",
                    name: "Ann",
                    password: "a long enough password",
                    is_admin: false,
                },
            )
            .await
            .unwrap()
            .id;
        let doc = DocId(Uuid::new_v4());
        db.create_document(ann, doc, "book.xlsx", Editor::GridCraft)
            .await
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let library = RevisionStore::open(&crate::library_path(dir.path())).unwrap();
        let blob = crate::blobs::test_blobs(dir.path())
            .store()
            .write_from(&b"workbook bytes at zero"[..], u64::MAX)
            .unwrap();
        let fields = checkpoint_fields(Seq(0));
        let authors = [Author {
            id: ann.0.to_string(),
            name: "Ann".to_owned(),
        }];
        let Outcome::Written(written) = library
            .write(&Checkpoint {
                doc,
                file_name: "book.xlsx",
                blob,
                base: None,
                authors: &authors,
                fields: &fields,
            })
            .unwrap()
        else {
            panic!("the checkpoint must write a revision");
        };
        // Git has the revision; the database outcome was lost, so the index has no row.
        assert!(db.revisions(doc).await.unwrap().is_empty());
        let session = Uuid::new_v4();
        db.start_live_session(doc, session, "gridcraft/1", OffsetDateTime::UNIX_EPOCH)
            .await
            .unwrap();
        let revision = written.to_string();
        let checkpoint = NewLiveCheckpoint {
            sha256: blob.sha256,
            ..checkpoint(&revision, session, b"identities")
        };
        let described = library.describe(doc, &written).unwrap().unwrap();
        assert!(db.record_live_checkpoint(doc, &checkpoint).await.is_err());

        LiveStore::reconcile_live_checkpoint(&db, doc, None, &checkpoint, &described, &fields)
            .await
            .expect("reconcile records the revision row and the checkpoint together");
        LiveStore::reconcile_live_checkpoint(&db, doc, None, &checkpoint, &described, &fields)
            .await
            .expect("reconcile is repeatable");

        let loaded = LiveStore::load_live(&db, doc).await.unwrap();
        assert_eq!(loaded.unverified_checkpoints.len(), 1);
        assert_eq!(loaded.unverified_checkpoints[0].revision, revision);
        assert_eq!(
            db.revisions(doc)
                .await
                .unwrap()
                .iter()
                .map(|row| row.revision.clone())
                .collect::<Vec<_>>(),
            [revision]
        );
    }

    #[tokio::test]
    async fn end_session_makes_return_tombstone_lookup_available() {
        let (store, doc, session) = started_store().await;
        let client = ClientId(id(300));
        let user = UserId(id(400));
        let participant = store
            .add_live_participant(doc, session, client, user, OffsetDateTime::UNIX_EPOCH)
            .await
            .unwrap();
        let ended_at = OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(1);

        store.end_live_session(session, ended_at).await.unwrap();

        let tombstone = store
            .live_return_tombstone(doc, session, participant)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(tombstone.client, client);
        assert_eq!(tombstone.user, user);
        assert_eq!(tombstone.ended_at, ended_at);
        assert_eq!(tombstone.final_seq, Seq(0));
    }

    #[tokio::test]
    async fn cancelling_before_the_verified_base_keeps_suffix_byte_total() {
        let (store, doc, session) = started_store().await;
        let client = ClientId(id(300));
        let user = UserId(id(400));
        let participant = store
            .add_live_participant(doc, session, client, user, OffsetDateTime::UNIX_EPOCH)
            .await
            .unwrap();
        let change = |seq, local, payload| NewLiveChange {
            seq: Seq(seq),
            session,
            client,
            local,
            participant,
            user,
            seen: Seq(seq - 1),
            at: OffsetDateTime::UNIX_EPOCH,
            seed: 1,
            format: "gridcraft/1",
            payload,
            witnesses: &[],
        };
        store
            .append_live_change(doc, &change(1, 1, b"0123456789"))
            .await
            .unwrap();
        let mut base = checkpoint("revision-one", session, b"identities");
        base.through = Seq(1);
        store.record_live_checkpoint(doc, &base).await.unwrap();
        store
            .verify_live_checkpoint(doc, "revision-one", Seq(1), OffsetDateTime::UNIX_EPOCH)
            .await
            .unwrap();
        store
            .append_live_change(doc, &change(2, 2, b"new"))
            .await
            .unwrap();

        store
            .cancel_live_entry(
                doc,
                &NewLiveCancel {
                    seq: Seq(3),
                    session,
                    cancels: Seq(1),
                    reports: 2,
                    at: OffsetDateTime::UNIX_EPOCH,
                    seed: 2,
                },
            )
            .await
            .unwrap();

        let loaded = store.load_live(doc).await.unwrap();
        assert_eq!(loaded.unsaved_entries, 2);
        assert_eq!(loaded.unsaved_bytes, 3);
    }
}
