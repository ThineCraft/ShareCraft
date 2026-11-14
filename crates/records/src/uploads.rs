//! Resumable upload sessions: one row per file being uploaded a chunk at a time.
//!
//! A session belongs to the person who started it and makes either a new file in a library or
//! folder, or a new version of a file document made from a base revision. Its row records what
//! the uploader declared (name, size, SHA-256) and how many bytes of its part file are written
//! and flushed (`received`). The bytes themselves stay in the part file on disk; the server
//! hashes them as they arrive. A person has at most [`UPLOADS_PER_PERSON`] sessions open, and a
//! session that has received nothing for a day is swept. While open, a session holds its
//! declared size reserved against its site's quota (see [`crate::Reservation`]); ending it
//! releases that, and claiming it for its finish ([`Db::claim_upload`]) keeps the bytes
//! reserved for the commit.

use collab_core::ids::{DocId, NodeId, UserId};
use sqlx::Row;
use sqlx::postgres::PgRow;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::quotas::{change_reserved, fits, locked_usage};
use crate::{Db, DbError, QuotaError, Reservation};

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

/// What to do when a new file's name is taken where it goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NameConflict {
    /// Save the upload as a new version of the file there.
    Replace,
    /// Make the new file with the first free name `name (2)`, `name (3)` ….
    KeepBoth,
    /// Make nothing.
    Skip,
}

impl NameConflict {
    /// The choice as the API and the database name it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            NameConflict::Replace => "replace",
            NameConflict::KeepBoth => "keep_both",
            NameConflict::Skip => "skip",
        }
    }

    /// The choice named `text`, if it is one.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "replace" => Some(NameConflict::Replace),
            "keep_both" => Some(NameConflict::KeepBoth),
            "skip" => Some(NameConflict::Skip),
            _ => None,
        }
    }
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
    /// For a new file, what to do if its name is taken, if chosen when it started.
    pub on_conflict: Option<NameConflict>,
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
    pub on_conflict: Option<NameConflict>,
}

/// Why an upload session was not started.
#[derive(Debug, thiserror::Error)]
pub enum UploadError {
    /// The person already has [`UPLOADS_PER_PERSON`] sessions open.
    #[error("too many uploads under way")]
    TooMany,
    /// The site has no room for the declared size under its quota.
    #[error("the site's storage quota would be exceeded")]
    QuotaExceeded,
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
         on_conflict, created_at, touched_at"
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
    let on_conflict: Option<String> = row.try_get("on_conflict")?;
    let on_conflict = match on_conflict {
        Some(text) => Some(NameConflict::parse(&text).ok_or_else(|| {
            sqlx::Error::Decode("an upload's name conflict choice is unknown".into())
        })?),
        None => None,
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
        on_conflict,
        created_at: row.try_get("created_at")?,
        touched_at: row.try_get("touched_at")?,
    })
}

impl Db {
    /// Starts the upload session `new`, with nothing received, unless its owner already has
    /// [`UPLOADS_PER_PERSON`] open (`TooMany`) or its site has no room for its declared size
    /// under its quota (`QuotaExceeded`); the size is reserved in the site while the session is
    /// open. The owner's row is locked while counting, then the site's usage row, so two
    /// sessions started at once cannot both take the last place or the last bytes. The caller
    /// has checked the person may upload there.
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
        let usage = locked_usage(&mut tx, new.site)
            .await
            .map_err(DbError::from)?;
        if !fits(usage.used, usage.reserved, usage.quota, new.size) {
            return Err(UploadError::QuotaExceeded);
        }
        change_reserved(&mut tx, new.site, column(new.size))
            .await
            .map_err(DbError::from)?;
        let (parent, doc) = match new.target {
            UploadTarget::NewFile { parent } => (Some(parent.0), None),
            UploadTarget::NewVersion { doc } => (None, Some(doc.0)),
        };
        let row = sqlx::query(concat!(
            "insert into upload_sessions \
                 (id, owner, site_id, parent_id, doc_id, name, size, sha256, base_revision, \
                  on_conflict) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) returning ",
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
        .bind(new.on_conflict.map(NameConflict::as_str))
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

    /// Ends the upload session `id`, abandoned, refused or expired, releasing its reservation.
    /// Whether it existed. The site's usage row is locked before the session's row, as
    /// [`Db::claim_upload`] locks them.
    pub async fn end_upload(&self, id: Uuid) -> Result<bool, DbError> {
        let Some(site) = self.upload_site(id).await? else {
            return Ok(false);
        };
        let mut tx = self.pool().begin().await?;
        locked_usage(&mut tx, site).await?;
        let size: Option<i64> =
            sqlx::query_scalar("delete from upload_sessions where id = $1 returning size")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
        let Some(size) = size else {
            return Ok(false);
        };
        change_reserved(&mut tx, site, -size).await?;
        tx.commit().await?;
        Ok(true)
    }

    /// The site of upload session `id`, if it exists.
    async fn upload_site(&self, id: Uuid) -> Result<Option<NodeId>, DbError> {
        let site: Option<Uuid> =
            sqlx::query_scalar("select site_id from upload_sessions where id = $1")
                .bind(id)
                .fetch_optional(self.pool())
                .await?;
        Ok(site.map(NodeId))
    }

    /// Ends the upload session `id` for its finish, keeping its declared size reserved for the
    /// commit: the reservation answered, which the caller commits or releases. Refused
    /// (`Exceeded`), leaving the session open, if the site has no room for it now: what the
    /// site uses, with what is reserved for commits under way (not for other open sessions)
    /// and this session's size, would go over its quota, which may have been lowered since it
    /// started. `None` if the session is gone. Two sessions finishing at once take turns on the
    /// site's usage row, so they cannot go over the quota together.
    pub async fn claim_upload(&self, id: Uuid) -> Result<Option<Reservation>, QuotaError> {
        let Some(site) = self.upload_site(id).await? else {
            return Ok(None);
        };
        let mut tx = self.pool().begin().await?;
        let usage = locked_usage(&mut tx, site).await?;
        let size: Option<i64> =
            sqlx::query_scalar("select size from upload_sessions where id = $1 for update")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
        let Some(size) = size else {
            return Ok(None);
        };
        let open: i64 = sqlx::query_scalar(
            "select coalesce(sum(size), 0)::bigint from upload_sessions where site_id = $1",
        )
        .bind(site.0)
        .fetch_one(&mut *tx)
        .await?;
        let committing = usage.reserved.saturating_sub(bytes_of(open));
        if !fits(usage.used, committing, usage.quota, bytes_of(size)) {
            return Err(QuotaError::Exceeded);
        }
        sqlx::query("delete from upload_sessions where id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(Some(Reservation::new(site, bytes_of(size))))
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
