//! Persistence boundary for the live room.

use std::future::Future;
use std::str::FromStr;
use std::sync::Arc;

use collab_core::ids::{ClientId, DocId, Seq, UserId};
use records::{
    Db, DbError, GoneReason, LiveCheckpointWriter, LiveLoaded, LiveLocal, LiveReturnTombstoneRow,
    NewLiveCancel, NewLiveChange, NewLiveCheckpoint, Origin,
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
    /// The place and client that uploaded it.
    pub writer: Option<LiveCheckpointWriter>,
    /// The major-version reason and person, when the checkpoint is a major version (C23).
    pub major: Option<(&'a str, UserId)>,
}

/// What a major-version write keeps in the history (C22, C23, S4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MajorKind {
    /// The room's last state, uploaded, kept below the outside revision that replaced the
    /// file; that revision is then stored again as current and becomes the live base.
    Outside,
    /// An expired returning client's own copy, uploaded, kept below the current revision,
    /// which is then stored again as current.
    Archive,
    /// No upload: the current revision is stored again, unchanged, marked major.
    Mark,
}

/// A major-version write on top of the document's current revision.
pub(crate) struct MajorWrite<'a> {
    pub kind: MajorKind,
    /// The reason recorded on the revision stored again as current.
    pub reason: &'a str,
    pub person: UserId,
    /// The document's current revision: everything is written on top of it.
    pub current: &'a str,
    /// The uploaded bytes, for [`MajorKind::Outside`] and [`MajorKind::Archive`].
    pub blob: Option<BlobRef>,
    /// Credited on the uploaded bytes.
    pub authors: &'a [Author],
    pub at: Seq,
    pub session: Uuid,
    pub created_at: OffsetDateTime,
}

/// What a major-version write left in the history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MajorWritten {
    /// The revision holding the uploaded bytes, when there were any.
    pub kept: Option<String>,
    /// The current revision afterwards: the earlier current file stored again.
    pub current: String,
}

/// The reason recorded on uploaded bytes a major write keeps.
fn kept_reason(kind: MajorKind) -> &'static str {
    match kind {
        MajorKind::Outside => "outside_replacement",
        MajorKind::Archive | MajorKind::Mark => "returning_client_archive",
    }
}

/// Live and major facts, which a revision stored again or kept must not inherit.
const LIVE_KEYS: [&str; 8] = [
    "verified",
    "live_through",
    "major",
    "major_reason",
    "major_person",
    "restores",
    "forensic_of",
    "replaced_by",
];

/// `base` without live facts, marked as a major version for `reason` and `person`.
pub(crate) fn major_fields(
    base: &Map<String, Value>,
    reason: &str,
    person: UserId,
) -> Map<String, Value> {
    let mut fields = base.clone();
    for key in LIVE_KEYS {
        fields.remove(key);
    }
    fields.insert("major".to_owned(), Value::Bool(true));
    fields.insert("major_reason".to_owned(), Value::from(reason));
    fields.insert("major_person".to_owned(), Value::from(person.0.to_string()));
    fields
}

/// The fields of `current` stored again, unchanged, as a major version that names the revision
/// whose bytes it holds (the one `current` itself restores, if any).
fn restored_fields(
    current: &str,
    current_fields: &Map<String, Value>,
    reason: &str,
    person: UserId,
) -> Map<String, Value> {
    let origin = current_fields
        .get("restores")
        .and_then(Value::as_str)
        .unwrap_or(current)
        .to_owned();
    let mut fields = major_fields(current_fields, reason, person);
    fields.insert("restores".to_owned(), Value::from(origin));
    fields
}

/// The fields of the uploaded bytes a major write keeps.
fn kept_fields(
    kind: MajorKind,
    current_fields: &Map<String, Value>,
    write: &MajorWrite<'_>,
) -> Map<String, Value> {
    let mut fields = major_fields(current_fields, kept_reason(kind), write.person);
    if kind == MajorKind::Outside {
        fields.insert("live_through".to_owned(), Value::from(write.at.0));
        fields.insert("verified".to_owned(), Value::Bool(false));
        fields.insert("replaced_by".to_owned(), Value::from(write.current));
    }
    fields
}

/// The verified base an outside replacement leaves: the outside file stored again.
fn adopted_base<'a>(
    revision: &'a str,
    sha256: [u8; 32],
    write: &MajorWrite<'_>,
) -> NewLiveCheckpoint<'a> {
    NewLiveCheckpoint {
        revision,
        through: write.at,
        sha256,
        session: write.session,
        session_data: &[],
        major_reason: Some("outside_current"),
        major_person: Some(write.person),
        writer: None,
        created_at: write.created_at,
    }
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
        "restores",
        "forensic_of",
        "replaced_by",
    ] {
        fields.remove(key);
    }
    fields.insert("verified".to_owned(), Value::Bool(false));
    fields.insert("live_through".to_owned(), Value::from(at.0));
    fields
}

/// The fields of the checkpoint `write`: [`live_fields`], marked major when it is one.
pub(crate) fn checkpoint_fields(
    base: &Map<String, Value>,
    write: &LiveWrite<'_>,
) -> Map<String, Value> {
    let mut fields = live_fields(base, write.at);
    if let Some((reason, person)) = write.major {
        fields.insert("major".to_owned(), Value::Bool(true));
        fields.insert("major_reason".to_owned(), Value::from(reason));
        fields.insert("major_person".to_owned(), Value::from(person.0.to_string()));
    }
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

    /// Writes a major version on top of `write.current` (see [`MajorKind`]). After an outside
    /// replacement it also records the outside file, stored again, as the verified live base
    /// at `write.at`.
    fn write_live_major(
        &self,
        doc: DocId,
        write: &MajorWrite<'_>,
    ) -> impl Future<Output = Result<MajorWritten, LiveWriteError>> + Send;

    /// Records `checkpoint`, a revision that replaced the live file from outside, as the
    /// verified catch-up base at its position.
    fn adopt_live_base(
        &self,
        doc: DocId,
        checkpoint: &NewLiveCheckpoint<'_>,
        at: OffsetDateTime,
    ) -> impl Future<Output = Result<(), DbError>> + Send;

    /// Git's description of `revision` of `doc`, for a checkpoint whose write could not
    /// describe it; `None` when Git does not know it.
    fn describe_live_revision(
        &self,
        doc: DocId,
        revision: &str,
    ) -> impl Future<Output = Result<Option<RevisionInfo>, String>> + Send;

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

    async fn write_live_major(
        &self,
        _doc: DocId,
        _write: &MajorWrite<'_>,
    ) -> Result<MajorWritten, LiveWriteError> {
        Err(LiveWriteError::Failed(
            "the database alone cannot commit a major version".to_owned(),
        ))
    }

    async fn adopt_live_base(
        &self,
        doc: DocId,
        checkpoint: &NewLiveCheckpoint<'_>,
        at: OffsetDateTime,
    ) -> Result<(), DbError> {
        Db::adopt_live_base(self, doc, checkpoint, at).await
    }

    async fn describe_live_revision(
        &self,
        _doc: DocId,
        _revision: &str,
    ) -> Result<Option<RevisionInfo>, String> {
        Err("the database alone cannot read the library".to_owned())
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
        let fields = checkpoint_fields(&base_fields, write);
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
            major_reason: write.major.map(|(reason, _)| reason),
            major_person: write.major.map(|(_, person)| person),
            writer: write.writer,
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

    async fn write_live_major(
        &self,
        doc: DocId,
        write: &MajorWrite<'_>,
    ) -> Result<MajorWritten, LiveWriteError> {
        let failed = |error: String| LiveWriteError::Failed(error);
        let name = self
            .db
            .document(doc)
            .await
            .map_err(|error| failed(error.to_string()))?
            .ok_or_else(|| failed(format!("document {} has no record", doc.0)))?
            .name;
        let current_id =
            RevisionId::from_str(write.current).map_err(|error| failed(error.to_string()))?;
        let wanted = current_id.clone();
        let current_info = self
            .blocking(move |store| {
                store
                    .describe(doc, &wanted)
                    .map_err(|error| error.to_string())
            })
            .await
            .map_err(failed)?
            .ok_or_else(|| {
                failed(format!(
                    "{} is not a revision of the document",
                    write.current
                ))
            })?;
        let current_blob = current_info
            .blob
            .ok_or_else(|| failed(format!("{} has no content-store bytes", write.current)))?;
        // Commits `blob` on top of `base`; a base that is no longer current is stale.
        let commit = |blob: BlobRef,
                      base: RevisionId,
                      authors: Vec<Author>,
                      fields: Map<String, Value>| {
            let name = name.clone();
            async move {
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
                            base: Some(&base),
                            authors: &authors,
                            fields: &fields,
                        })
                    },
                )
                .await;
                match outcome {
                    Ok(Outcome::Written(revision) | Outcome::Unchanged(revision)) => Ok(revision),
                    Err(WriteError::Stale { current }) => Err(LiveWriteError::Stale {
                        current: current.map(|current| current.to_string()),
                    }),
                    Err(WriteError::Failed(error)) => Err(LiveWriteError::Failed(error)),
                }
            }
        };
        let mut parent = current_id.clone();
        let kept = match (write.kind, write.blob) {
            (MajorKind::Outside | MajorKind::Archive, Some(blob)) => {
                let fields = kept_fields(write.kind, &current_info.fields, write);
                let kept = commit(blob, parent.clone(), write.authors.to_vec(), fields).await?;
                self.record_index(doc, Some(&parent), &kept).await;
                parent = kept.clone();
                Some(kept)
            }
            (MajorKind::Mark, None) => None,
            _ => {
                return Err(failed(
                    "a major write's bytes do not match its kind".to_owned(),
                ));
            }
        };
        let fields = restored_fields(
            write.current,
            &current_info.fields,
            write.reason,
            write.person,
        );
        let restored = commit(
            current_blob,
            parent.clone(),
            current_info.authors.clone(),
            fields,
        )
        .await;
        let current = match restored {
            Ok(revision) => {
                self.record_index(doc, Some(&parent), &revision).await;
                revision.to_string()
            }
            // Nothing was written: the current file moved on.
            Err(stale @ LiveWriteError::Stale { .. }) if kept.is_none() => return Err(stale),
            Err(error) => {
                // The uploaded bytes are history now; with the current file not stored again
                // on top, they are what the document shows. Reported; never silently adopted.
                return Err(LiveWriteError::Failed(format!(
                    "the current file {} could not be stored again on top of {parent}: {error:?}",
                    write.current
                )));
            }
        };
        if write.kind == MajorKind::Outside {
            self.db
                .adopt_live_base(
                    doc,
                    &adopted_base(&current, current_blob.sha256, write),
                    write.created_at,
                )
                .await
                .map_err(|error| failed(error.to_string()))?;
        }
        Ok(MajorWritten {
            kept: kept.map(|kept| kept.to_string()),
            current,
        })
    }

    async fn adopt_live_base(
        &self,
        doc: DocId,
        checkpoint: &NewLiveCheckpoint<'_>,
        at: OffsetDateTime,
    ) -> Result<(), DbError> {
        self.db.adopt_live_base(doc, checkpoint, at).await
    }

    async fn describe_live_revision(
        &self,
        doc: DocId,
        revision: &str,
    ) -> Result<Option<RevisionInfo>, String> {
        let revision = RevisionId::from_str(revision).map_err(|error| error.to_string())?;
        self.blocking(move |store| {
            store
                .describe(doc, &revision)
                .map_err(|error| error.to_string())
        })
        .await
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
        /// Describes still to fail.
        fail_describes: u32,
        describe_calls: usize,
        /// The next checkpoint write panics.
        panic_next_write: bool,
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
        /// The fields of each checkpoint revision written.
        revision_fields: HashMap<String, Map<String, Value>>,
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
                revision_fields: HashMap::new(),
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

        /// Makes the next `count` descriptions of a revision fail, as an unreachable Git would.
        pub(crate) fn fail_describes(&self, count: u32) {
            if let Ok(mut state) = self.inner.lock() {
                state.fail_describes = count;
            }
        }

        /// How many times a revision was described.
        pub(crate) fn describe_calls(&self) -> usize {
            self.inner
                .lock()
                .map(|state| state.describe_calls)
                .unwrap_or_default()
        }

        /// Makes the next checkpoint write panic.
        pub(crate) fn panic_next_write(&self) {
            if let Ok(mut state) = self.inner.lock() {
                state.panic_next_write = true;
            }
        }

        /// Writes a revision of `sha256` as the current file, as an outside save would.
        pub(crate) fn write_outside(&self, doc: DocId, sha256: [u8; 32]) -> String {
            let mut state = self
                .inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let memory = state.docs.entry(doc).or_default();
            memory.revisions_written = memory.revisions_written.saturating_add(1);
            let revision = format!("{:040x}", memory.revisions_written);
            memory.revisions.push((revision.clone(), sha256));
            memory.current_revision = Some(revision.clone());
            revision
        }

        /// The fields `revision` of `doc` was written with.
        pub(crate) fn revision_fields(&self, doc: DocId, revision: &str) -> Map<String, Value> {
            self.inner
                .lock()
                .ok()
                .and_then(|state| {
                    state
                        .docs
                        .get(&doc)
                        .and_then(|memory| memory.revision_fields.get(revision).cloned())
                })
                .unwrap_or_default()
        }

        /// Forgets the places of ended sessions, as the 30-day sweep does; their tombstones
        /// stay.
        pub(crate) fn forget_ended_places(&self, doc: DocId) {
            if let Ok(mut state) = self.inner.lock()
                && let Some(memory) = state.docs.get_mut(&doc)
            {
                let open = memory
                    .open_session
                    .as_ref()
                    .map(|session| {
                        session
                            .participants
                            .iter()
                            .map(|place| place.number)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                memory
                    .participants
                    .retain(|place| open.contains(&place.number));
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

        /// The identity bytes held by `doc`'s checkpoints.
        pub(crate) fn session_data_bytes(&self, doc: DocId) -> usize {
            self.inner
                .lock()
                .ok()
                .and_then(|state| {
                    state.docs.get(&doc).map(|memory| {
                        memory
                            .checkpoints
                            .iter()
                            .map(|checkpoint| checkpoint.row.session_data.len())
                            .sum()
                    })
                })
                .unwrap_or_default()
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

    /// The in-memory form of `Db::adopt_live_base`.
    fn adopt_in(memory: &mut MemoryDoc, checkpoint: &NewLiveCheckpoint<'_>) -> Result<(), DbError> {
        if checkpoint.through > memory.head {
            return Err(DbError::new("base position exceeds the live head"));
        }
        memory.checkpoints.retain(|kept| {
            !(kept.row.revision == checkpoint.revision
                || kept.row.through >= checkpoint.through
                || (!kept.row.verified && kept.row.major_reason.is_none()))
        });
        memory.checkpoints.push(MemoryCheckpoint {
            row: LiveCheckpointRow {
                revision: checkpoint.revision.to_owned(),
                through: checkpoint.through,
                sha256: checkpoint.sha256,
                verified: true,
                session: checkpoint.session,
                session_data: checkpoint.session_data.to_vec(),
                major_reason: checkpoint.major_reason.map(str::to_owned),
                major_person: checkpoint.major_person,
                writer: checkpoint.writer,
            },
            created_at: checkpoint.created_at,
        });
        if checkpoint.through > memory.verified_through {
            memory.verified_through = checkpoint.through;
            let through = checkpoint.through;
            let after = |entry: &LiveEntryRow| match entry {
                LiveEntryRow::Change { seq, .. } | LiveEntryRow::Cancel { seq, .. } => {
                    *seq > through
                }
            };
            memory.unsaved_entries = memory
                .entries
                .iter()
                .filter(|entry| after(entry))
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
        Ok(())
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
            let panic_now = self
                .inner
                .lock()
                .map(|mut state| std::mem::take(&mut state.panic_next_write))
                .unwrap_or_default();
            assert!(!panic_now, "injected checkpoint write panic");
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
                memory
                    .revision_fields
                    .insert(revision.clone(), checkpoint_fields(&Map::new(), write));
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
                    major_reason: write.major.map(|(reason, _)| reason),
                    major_person: write.major.map(|(_, person)| person),
                    writer: write.writer,
                    created_at: write.created_at,
                },
            )
            .await
            .map_err(|error| LiveWriteError::Unrecorded {
                revision: revision.clone(),
                fields: checkpoint_fields(&Map::new(), write),
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
            let mut forensic_fields = live_fields(&Map::new(), write.at);
            forensic_fields.insert("forensic_of".to_owned(), Value::from(agreed));
            memory
                .revision_fields
                .insert(forensic.clone(), forensic_fields);
            let mut restored = memory
                .revision_fields
                .get(agreed)
                .cloned()
                .unwrap_or_default();
            restored.insert("restores".to_owned(), Value::from(agreed));
            memory.revision_fields.insert(current.clone(), restored);
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
                writer: checkpoint.writer,
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
                let verified_through = memory.verified_through;
                let counted = |row: &LiveCheckpointRow| {
                    !row.verified
                        && row.major_reason.is_none()
                        && (row.through > verified_through
                            || (verified_through == Seq(0) && row.through == Seq(0)))
                };
                let mut newest = memory
                    .checkpoints
                    .iter()
                    .filter(|checkpoint| counted(&checkpoint.row))
                    .map(|checkpoint| checkpoint.row.through)
                    .collect::<Vec<_>>();
                newest.sort_unstable_by(|a, b| b.cmp(a));
                let dropped = newest
                    .into_iter()
                    .skip(records::MAX_UNVERIFIED_CHECKPOINTS)
                    .collect::<HashSet<_>>();
                memory.checkpoints.retain(|checkpoint| {
                    !(counted(&checkpoint.row) && dropped.contains(&checkpoint.row.through))
                });
            }
            Self::finish_write(state)
        }

        async fn write_live_major(
            &self,
            doc: DocId,
            write: &MajorWrite<'_>,
        ) -> Result<MajorWritten, LiveWriteError> {
            self.wait_for_gate().await;
            let mut state = self
                .write()
                .map_err(|error| LiveWriteError::Failed(error.to_string()))?;
            let memory = state.docs.entry(doc).or_default();
            if memory.current_revision.as_deref() != Some(write.current) {
                return Err(LiveWriteError::Stale {
                    current: memory.current_revision.clone(),
                });
            }
            let current_sha = memory
                .revisions
                .iter()
                .find(|(revision, _)| revision == write.current)
                .map(|(_, sha)| *sha)
                .ok_or_else(|| {
                    LiveWriteError::Failed(format!("{} is not a revision", write.current))
                })?;
            let current_fields = memory
                .revision_fields
                .get(write.current)
                .cloned()
                .unwrap_or_default();
            let push = |memory: &mut MemoryDoc, sha: [u8; 32], fields: Map<String, Value>| {
                memory.revisions_written = memory.revisions_written.saturating_add(1);
                let revision = format!("{:040x}", memory.revisions_written);
                memory.revisions.push((revision.clone(), sha));
                memory.revision_fields.insert(revision.clone(), fields);
                revision
            };
            let kept = match (write.kind, write.blob) {
                (MajorKind::Outside | MajorKind::Archive, Some(blob)) => Some(push(
                    memory,
                    blob.sha256,
                    kept_fields(write.kind, &current_fields, write),
                )),
                (MajorKind::Mark, None) => None,
                _ => {
                    return Err(LiveWriteError::Failed(
                        "a major write's bytes do not match its kind".to_owned(),
                    ));
                }
            };
            let current = push(
                memory,
                current_sha,
                restored_fields(write.current, &current_fields, write.reason, write.person),
            );
            memory.current_revision = Some(current.clone());
            if write.kind == MajorKind::Outside {
                adopt_in(memory, &adopted_base(&current, current_sha, write))
                    .map_err(|error| LiveWriteError::Failed(error.to_string()))?;
            }
            state.last_authors = write.authors.to_vec();
            Ok(MajorWritten { kept, current })
        }

        async fn adopt_live_base(
            &self,
            doc: DocId,
            checkpoint: &NewLiveCheckpoint<'_>,
            _at: OffsetDateTime,
        ) -> Result<(), DbError> {
            let mut state = self.write()?;
            let memory = state.docs.entry(doc).or_default();
            adopt_in(memory, checkpoint)?;
            Self::finish_write(state)
        }

        async fn describe_live_revision(
            &self,
            doc: DocId,
            revision: &str,
        ) -> Result<Option<RevisionInfo>, String> {
            let mut state = self.inner.lock().map_err(|_| "lock poisoned".to_owned())?;
            state.describe_calls = state.describe_calls.saturating_add(1);
            if state.fail_describes > 0 {
                state.fail_describes -= 1;
                return Err("injected describe failure".to_owned());
            }
            let Some(memory) = state.docs.get(&doc) else {
                return Ok(None);
            };
            let Some((_, sha256)) = memory.revisions.iter().find(|(id, _)| id == revision) else {
                return Ok(None);
            };
            Ok(Some(RevisionInfo {
                revision: RevisionId::from_str(revision).map_err(|error| error.to_string())?,
                doc,
                committed_at: 0,
                authors: Vec::new(),
                file_name: "book.gridcraft".to_owned(),
                fields: memory
                    .revision_fields
                    .get(revision)
                    .cloned()
                    .unwrap_or_default(),
                blob: Some(BlobRef {
                    sha256: *sha256,
                    size: 3,
                }),
                size: 3,
            }))
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
            let verified_session = checkpoint.row.session;
            let rows = memory
                .checkpoints
                .iter()
                .map(|checkpoint| (checkpoint.row.session, checkpoint.row.through))
                .collect::<Vec<_>>();
            for checkpoint in &mut memory.checkpoints {
                let row = &mut checkpoint.row;
                let superseded_in_session = rows
                    .iter()
                    .any(|(session, at)| *session == row.session && *at > row.through);
                if !row.verified
                    && row.through < through
                    && (row.session == verified_session || superseded_in_session)
                {
                    row.session_data.clear();
                }
            }
            memory.checkpoints.retain(|checkpoint| {
                checkpoint.row.verified
                    || checkpoint.row.through >= through
                    || !checkpoint.row.session_data.is_empty()
            });
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
            writer: None,
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

    /// A library that commits a set number of writes, then fails every later one.
    struct FailsAfter {
        inner: revisions::RevisionStore,
        writes_left: std::sync::atomic::AtomicUsize,
    }

    impl crate::checkpoint::Checkpointer for FailsAfter {
        fn write(
            &self,
            c: &revisions::Checkpoint<'_>,
        ) -> Result<revisions::Outcome, revisions::CheckpointError> {
            use std::sync::atomic::Ordering;
            if self
                .writes_left
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                    left.checked_sub(1)
                })
                .is_err()
            {
                return Err(revisions::CheckpointError::Git(revisions::GitError::new(
                    "injected restore failure",
                )));
            }
            crate::checkpoint::Checkpointer::write(&self.inner, c)
        }

        fn load(
            &self,
            doc: DocId,
        ) -> Result<Option<revisions::Stored>, revisions::CheckpointError> {
            crate::checkpoint::Checkpointer::load(&self.inner, doc)
        }

        fn load_at(
            &self,
            doc: DocId,
            revision: &revisions::RevisionId,
        ) -> Result<Option<revisions::Stored>, revisions::CheckpointError> {
            crate::checkpoint::Checkpointer::load_at(&self.inner, doc, revision)
        }

        fn describe(
            &self,
            doc: DocId,
            revision: &revisions::RevisionId,
        ) -> Result<Option<RevisionInfo>, revisions::CheckpointError> {
            crate::checkpoint::Checkpointer::describe(&self.inner, doc, revision)
        }

        fn history(&self, doc: DocId) -> Result<Vec<RevisionInfo>, revisions::CheckpointError> {
            crate::checkpoint::Checkpointer::history(&self.inner, doc)
        }

        fn documents(&self) -> Result<Vec<DocId>, revisions::GitError> {
            crate::checkpoint::Checkpointer::documents(&self.inner)
        }
    }

    #[tokio::test]
    async fn database_forensic_write_keeps_both_commits_and_restores_the_agreed_file() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        use records::test_support::TestDb;
        use records::{Editor, NewUser};
        use revisions::{Author, RevisionStore};

        use super::{LiveDb, LiveWrite};
        use crate::checkpoint::Checkpointer;

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
        let library = Arc::new(FailsAfter {
            inner: RevisionStore::open(&crate::library_path(dir.path())).unwrap(),
            // The agreed checkpoint, then a forensic revision and its restore.
            writes_left: AtomicUsize::new(3),
        });
        let blobs = crate::blobs::test_blobs(dir.path());
        let live = LiveDb {
            db: db.clone(),
            store: library.clone(),
            blobs: blobs.clone(),
        };
        let session = Uuid::new_v4();
        db.start_live_session(doc, session, "gridcraft/1", OffsetDateTime::UNIX_EPOCH)
            .await
            .unwrap();
        let authors = [Author {
            id: ann.0.to_string(),
            name: "Ann".to_owned(),
        }];
        let blob = |bytes: &'static [u8]| blobs.store().write_from(bytes, u64::MAX).unwrap();
        let agreed_blob = blob(b"agreed workbook");
        let agreed = live
            .write_live_checkpoint(
                doc,
                &LiveWrite {
                    base: None,
                    blob: agreed_blob,
                    authors: &authors,
                    at: Seq(0),
                    session,
                    session_data: b"identities",
                    created_at: OffsetDateTime::UNIX_EPOCH,
                    writer: None,
                    major: None,
                },
            )
            .await
            .unwrap();

        let forensic_blob = blob(b"disagreeing workbook");
        let written = live
            .write_live_forensic(
                doc,
                &LiveWrite {
                    base: Some(&agreed),
                    blob: forensic_blob,
                    authors: &authors,
                    at: Seq(0),
                    session,
                    session_data: b"",
                    created_at: OffsetDateTime::UNIX_EPOCH,
                    writer: None,
                    major: None,
                },
                &agreed,
            )
            .await
            .unwrap();
        assert_ne!(written.forensic, written.current);
        let describe = |revision: &str| {
            library
                .describe(doc, &revision.parse().unwrap())
                .unwrap()
                .unwrap()
        };
        let forensic = describe(&written.forensic);
        assert_eq!(forensic.blob.unwrap().sha256, forensic_blob.sha256);
        assert_eq!(forensic.fields.get("forensic_of"), Some(&json!(agreed)));
        let restored = describe(&written.current);
        assert_eq!(restored.blob.unwrap().sha256, agreed_blob.sha256);
        assert_eq!(restored.fields.get("restores"), Some(&json!(agreed)));
        assert_eq!(
            library.load(doc).unwrap().unwrap().revision.to_string(),
            written.current,
            "the agreed bytes are the current file again"
        );
        let index = db.revisions(doc).await.unwrap();
        assert_eq!(
            index
                .iter()
                .map(|row| (row.revision.clone(), row.parent.clone()))
                .collect::<Vec<_>>(),
            [
                (agreed.clone(), None),
                (written.forensic.clone(), Some(agreed.clone())),
                (written.current.clone(), Some(written.forensic.clone())),
            ]
        );
        let loaded = LiveStore::load_live(&db, doc).await.unwrap();
        assert_eq!(
            loaded
                .unverified_checkpoints
                .iter()
                .map(|checkpoint| checkpoint.revision.clone())
                .collect::<Vec<_>>(),
            std::slice::from_ref(&agreed),
            "forensic bytes never become a checkpoint"
        );

        // A second forensic upload whose restore cannot be written: the forensic revision
        // stays current and recorded.
        library.writes_left.store(1, Ordering::SeqCst);
        let base = written.current.clone();
        let again = live
            .write_live_forensic(
                doc,
                &LiveWrite {
                    base: Some(&base),
                    blob: blob(b"another disagreeing workbook"),
                    authors: &authors,
                    at: Seq(0),
                    session,
                    session_data: b"",
                    created_at: OffsetDateTime::UNIX_EPOCH,
                    writer: None,
                    major: None,
                },
                &agreed,
            )
            .await
            .unwrap();
        assert_eq!(again.current, again.forensic);
        assert_eq!(
            library.load(doc).unwrap().unwrap().revision.to_string(),
            again.forensic
        );
        let index = db.revisions(doc).await.unwrap();
        assert_eq!(index.len(), 4);
        assert_eq!(index[3].revision, again.forensic);
        assert_eq!(index[3].parent.as_deref(), Some(base.as_str()));
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

    #[tokio::test]
    async fn database_outside_recovery_keeps_live_state_below_the_outside_file() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicUsize;

        use records::test_support::TestDb;
        use records::{Editor, NewUser};
        use revisions::{Author, RevisionStore};

        use super::{LiveDb, LiveWrite, MajorKind, MajorWrite};
        use crate::checkpoint::Checkpointer;

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
        let library = Arc::new(FailsAfter {
            inner: RevisionStore::open(&crate::library_path(dir.path())).unwrap(),
            writes_left: AtomicUsize::new(100),
        });
        let blobs = crate::blobs::test_blobs(dir.path());
        let live = LiveDb {
            db: db.clone(),
            store: library.clone(),
            blobs: blobs.clone(),
        };
        let session = Uuid::new_v4();
        db.start_live_session(doc, session, "gridcraft/1", OffsetDateTime::UNIX_EPOCH)
            .await
            .unwrap();
        let authors = [Author {
            id: ann.0.to_string(),
            name: "Ann".to_owned(),
        }];
        let blob = |bytes: &'static [u8]| blobs.store().write_from(bytes, u64::MAX).unwrap();
        let live_blob = blob(b"live workbook");
        let checkpoint = live
            .write_live_checkpoint(
                doc,
                &LiveWrite {
                    base: None,
                    blob: live_blob,
                    authors: &authors,
                    at: Seq(0),
                    session,
                    session_data: b"identities",
                    created_at: OffsetDateTime::UNIX_EPOCH,
                    writer: None,
                    major: None,
                },
            )
            .await
            .unwrap();
        // Someone saves another file over it.
        let outside_blob = blob(b"outside workbook");
        let outside = live
            .write_live_checkpoint(
                doc,
                &LiveWrite {
                    base: Some(&checkpoint),
                    blob: outside_blob,
                    authors: &authors,
                    at: Seq(0),
                    session,
                    session_data: b"",
                    created_at: OffsetDateTime::UNIX_EPOCH,
                    writer: None,
                    major: None,
                },
            )
            .await;
        // A second checkpoint at the same position is refused as metadata, but Git has it:
        // whatever the record says, the current file is the outside one.
        let outside = match outside {
            Ok(revision) => revision,
            Err(super::LiveWriteError::Unrecorded { revision, .. }) => revision,
            Err(other) => panic!("{other:?}"),
        };

        let last_state = blob(b"room's last state");
        let written = live
            .write_live_major(
                doc,
                &MajorWrite {
                    kind: MajorKind::Outside,
                    reason: "outside_current",
                    person: ann,
                    current: &outside,
                    blob: Some(last_state),
                    authors: &authors,
                    at: Seq(0),
                    session,
                    created_at: OffsetDateTime::UNIX_EPOCH,
                },
            )
            .await
            .unwrap();
        let describe = |revision: &str| {
            library
                .describe(doc, &revision.parse().unwrap())
                .unwrap()
                .unwrap()
        };
        let kept = describe(written.kept.as_deref().unwrap());
        assert_eq!(kept.blob.unwrap().sha256, last_state.sha256);
        assert_eq!(kept.fields.get("major"), Some(&json!(true)));
        assert_eq!(
            kept.fields.get("major_reason"),
            Some(&json!("outside_replacement"))
        );
        let current = describe(&written.current);
        assert_eq!(current.blob.unwrap().sha256, outside_blob.sha256);
        assert_eq!(current.fields.get("restores"), Some(&json!(outside)));
        assert_eq!(
            current.fields.get("major_reason"),
            Some(&json!("outside_current"))
        );
        assert_eq!(
            library.load(doc).unwrap().unwrap().revision.to_string(),
            written.current,
            "the outside file is current again"
        );
        let loaded = LiveStore::load_live(&db, doc).await.unwrap();
        let base = loaded.verified_checkpoint.unwrap();
        assert_eq!(base.revision, written.current);
        assert!(loaded.unverified_checkpoints.is_empty());

        // Stale against a moved head: nothing is written.
        let before = library.history(doc).unwrap().len();
        let stale = live
            .write_live_major(
                doc,
                &MajorWrite {
                    kind: MajorKind::Mark,
                    reason: "returning_client_before",
                    person: ann,
                    current: &outside,
                    blob: None,
                    authors: &[],
                    at: Seq(0),
                    session,
                    created_at: OffsetDateTime::UNIX_EPOCH,
                },
            )
            .await;
        assert!(
            matches!(stale, Err(super::LiveWriteError::Stale { .. })),
            "{stale:?}"
        );
        assert_eq!(library.history(doc).unwrap().len(), before);
    }
}
