//! Resumable upload sessions: one row per file being uploaded a chunk at a time.
//!
//! A session belongs to the person who started it and makes either a new file in a library or
//! folder, or a new version of a file document made from a base revision. Its row records what
//! the uploader declared (name, size, SHA-256) and how many bytes of its part file are written
//! and flushed (`received`). The bytes themselves stay in the part file on disk; the server
//! hashes them as they arrive. A person has at most [`UPLOADS_PER_PERSON`] sessions open, and a
//! session that has received nothing for a day is swept.

use collab_core::ids::{DocId, NodeId, UserId};
use sqlx::Row;
use sqlx::postgres::PgRow;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{Db, DbError};

/// How many upload sessions one person may have open at once.
pub const UPLOADS_PER_PERSON: i64 = 5;

/// What an upload session makes when it finishes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UploadTarget {
    /// A new file in this library or folder.
    NewFile { parent: NodeId },
    /// A new version of this file document.
    NewVersion { doc: DocId },
}

/// An upload session as its row holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UploadSession {
    pub id: Uuid,
    /// Who started it; nobody else may see or use it.
    pub owner: UserId,
    /// The site the file goes into (for its quota).
    pub site: NodeId,
    pub target: UploadTarget,
    /// The file's name: as asked for a new file, the document's for a new version.
    pub name: String,
    /// The declared size of the whole file, in bytes.
    pub size: u64,
    /// The declared SHA-256 of the whole file.
    pub sha256: [u8; 32],
    /// How many bytes of the part file are written and flushed.
    pub received: u64,
    /// For a new version, the revision it was made from (none if the document has none).
    pub base: Option<String>,
    pub created_at: OffsetDateTime,
    /// When the session started or last received a chunk.
    pub touched_at: OffsetDateTime,
}

/// A session to start; see [`Db::start_upload`].
#[derive(Clone, Copy, Debug)]
pub struct NewUpload<'a> {
    pub id: Uuid,
    pub owner: UserId,
    pub site: NodeId,
    pub target: UploadTarget,
    pub name: &'a str,
    pub size: u64,
    pub sha256: [u8; 32],
    pub base: Option<&'a str>,
}

/// Why an upload session was not started.
#[derive(Debug, thiserror::Error)]
pub enum UploadError {
    /// The person already has [`UPLOADS_PER_PERSON`] sessions open.
    #[error("too many uploads under way")]
    TooMany,
    #[error(transparent)]
    Db(#[from] DbError),
}

/// A byte count as the database stores it; counts beyond `i64::MAX` cannot occur in a file
/// system.
fn column(bytes: u64) -> i64 {
    i64::try_from(bytes).unwrap_or(i64::MAX)
}

/// A byte count read from the database, where the checks keep it from being negative.
fn bytes_of(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

/// The columns [`session_from_row`] reads.
macro_rules! columns {
    () => {
        "id, owner, site_id, parent_id, doc_id, name, size, sha256, received, base_revision, \
         created_at, touched_at"
    };
}

fn session_from_row(row: &PgRow) -> Result<UploadSession, sqlx::Error> {
    let parent: Option<Uuid> = row.try_get("parent_id")?;
    let doc: Option<Uuid> = row.try_get("doc_id")?;
    let target = match (parent, doc) {
        (Some(parent), None) => UploadTarget::NewFile {
            parent: NodeId(parent),
        },
        (None, Some(doc)) => UploadTarget::NewVersion { doc: DocId(doc) },
        _ => {
            return Err(sqlx::Error::Decode(
                "an upload session names both a parent and a document, or neither".into(),
            ));
        }
    };
    let sha256: Vec<u8> = row.try_get("sha256")?;
    let sha256: [u8; 32] = sha256
        .try_into()
        .map_err(|_| sqlx::Error::Decode("an upload's SHA-256 is not 32 bytes".into()))?;
    Ok(UploadSession {
        id: row.try_get("id")?,
        owner: UserId(row.try_get("owner")?),
        site: NodeId(row.try_get("site_id")?),
        target,
        name: row.try_get("name")?,
        size: bytes_of(row.try_get("size")?),
        sha256,
        received: bytes_of(row.try_get("received")?),
        base: row.try_get("base_revision")?,
        created_at: row.try_get("created_at")?,
        touched_at: row.try_get("touched_at")?,
    })
}

impl Db {
    /// Starts the upload session `new`, with nothing received, unless its owner already has
    /// [`UPLOADS_PER_PERSON`] open. The owner's row is locked while counting, so two sessions
    /// started at once cannot both take the last place. The caller has checked the person may
    /// upload there.
    pub async fn start_upload(&self, new: NewUpload<'_>) -> Result<UploadSession, UploadError> {
        let mut tx = self.pool().begin().await.map_err(DbError::from)?;
        sqlx::query("select id from users where id = $1 for update")
            .bind(new.owner.0)
            .fetch_one(&mut *tx)
            .await
            .map_err(DbError::from)?;
        let open: i64 = sqlx::query_scalar("select count(*) from upload_sessions where owner = $1")
            .bind(new.owner.0)
            .fetch_one(&mut *tx)
            .await
            .map_err(DbError::from)?;
        if open >= UPLOADS_PER_PERSON {
            return Err(UploadError::TooMany);
        }
        let (parent, doc) = match new.target {
            UploadTarget::NewFile { parent } => (Some(parent.0), None),
            UploadTarget::NewVersion { doc } => (None, Some(doc.0)),
        };
        let row = sqlx::query(concat!(
            "insert into upload_sessions \
                 (id, owner, site_id, parent_id, doc_id, name, size, sha256, base_revision) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9) returning ",
            columns!()
        ))
        .bind(new.id)
        .bind(new.owner.0)
        .bind(new.site.0)
        .bind(parent)
        .bind(doc)
        .bind(new.name)
        .bind(column(new.size))
        .bind(&new.sha256[..])
        .bind(new.base)
        .fetch_one(&mut *tx)
        .await
        .map_err(DbError::from)?;
        let session = session_from_row(&row).map_err(DbError::from)?;
        tx.commit().await.map_err(DbError::from)?;
        Ok(session)
    }

    /// The upload session `id`, whoever it belongs to. Callers check the owner.
    pub async fn upload_session(&self, id: Uuid) -> Result<Option<UploadSession>, DbError> {
        let row = sqlx::query(concat!(
            "select ",
            columns!(),
            " from upload_sessions where id = $1"
        ))
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        Ok(row.as_ref().map(session_from_row).transpose()?)
    }

    /// Records that the first `received` bytes of session `id`'s part file are written and
    /// flushed, and that it received a chunk now. Whether the session still exists.
    pub async fn record_upload_progress(&self, id: Uuid, received: u64) -> Result<bool, DbError> {
        let done = sqlx::query(
            "update upload_sessions set received = $2, touched_at = now() where id = $1",
        )
        .bind(id)
        .bind(column(received))
        .execute(self.pool())
        .await?;
        Ok(done.rows_affected() > 0)
    }

    /// Ends the upload session `id`: finished, abandoned or expired. Whether it existed.
    pub async fn end_upload(&self, id: Uuid) -> Result<bool, DbError> {
        let done = sqlx::query("delete from upload_sessions where id = $1")
            .bind(id)
            .execute(self.pool())
            .await?;
        Ok(done.rows_affected() > 0)
    }

    /// The upload sessions that have received nothing since `before` (nor started after it),
    /// oldest first.
    pub async fn idle_uploads(&self, before: OffsetDateTime) -> Result<Vec<Uuid>, DbError> {
        let ids = sqlx::query_scalar(
            "select id from upload_sessions where touched_at <= $1 order by touched_at, id",
        )
        .bind(before)
        .fetch_all(self.pool())
        .await?;
        Ok(ids)
    }
}
