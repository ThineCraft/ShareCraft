//! Writes to a connected library's store (a bucket first): the changes ShareCraft has accepted
//! but not yet written, a library's write state, and its mode.
//!
//! A save is accepted once its content is stored in the external-content cache and a row in
//! `source_writes` names it. That row is a reference to the content just as a cache pin is: the
//! content is neither released nor evicted nor unpinned while it exists. The row goes only when
//! the store holds the content ([`Db::finish_write`]) or the change is withdrawn
//! ([`Db::drop_write`]). Nothing here knows which kind of store a library is.

use collab_core::ids::{NodeId, UserId};
use revisions::{BlobRef, hex, parse_hex};
use sqlx::Row;
use sqlx::postgres::PgRow;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::sources::{SourceItem, SourceKind, SourceMode, item_from_row, new_external_revision};
use crate::{AccessError, Db, DbError};

/// How long a change waits after it was first staged before it is written, unless it is
/// flushed: a burst of saves of one file is written once.
pub const COALESCE: time::Duration = time::Duration::seconds(30);

/// The longest error text kept, in characters.
const MAX_ERROR_CHARS: usize = 300;

/// Whether a library may be written to now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteState {
    Ok,
    /// A write was refused by the store: the library is read-only until that is fixed.
    Blocked,
}

impl WriteState {
    /// The word for the state, as stored.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            WriteState::Ok => "ok",
            WriteState::Blocked => "blocked",
        }
    }

    /// The state named `word`.
    #[must_use]
    pub fn parse(word: &str) -> Option<WriteState> {
        match word {
            "ok" => Some(WriteState::Ok),
            "blocked" => Some(WriteState::Blocked),
            _ => None,
        }
    }
}

/// Where a pending write stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PendingState {
    /// Waiting for the scanner's verdict, the throttle or its turn.
    Scanning,
    /// Cleared to write.
    Ready,
    /// The last attempt failed; it is tried again at `next_try_at`.
    Failed,
}

impl PendingState {
    /// The word for the state, as stored.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            PendingState::Scanning => "scanning",
            PendingState::Ready => "ready",
            PendingState::Failed => "failed",
        }
    }

    /// The state named `word`.
    #[must_use]
    pub fn parse(word: &str) -> Option<PendingState> {
        match word {
            "scanning" => Some(PendingState::Scanning),
            "ready" => Some(PendingState::Ready),
            "failed" => Some(PendingState::Failed),
            _ => None,
        }
    }
}

/// A change to stage; see [`Db::stage_write`].
#[derive(Clone, Copy, Debug)]
pub struct NewPendingWrite<'a> {
    pub node: NodeId,
    pub library: NodeId,
    /// The path below the library's place, as `source_items.repository_path` writes it.
    pub path: &'a str,
    /// The new content, pinned in the cache by the caller.
    pub blob: BlobRef,
    /// The tag the change is based on; `None`: the file is new and must not exist.
    pub base_tag: Option<&'a str>,
    pub by: UserId,
    /// Write without waiting for the throttle.
    pub flush: bool,
    pub now: OffsetDateTime,
}

/// A change accepted and not yet written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingWrite {
    pub node: NodeId,
    pub library: NodeId,
    pub path: String,
    pub blob: BlobRef,
    pub revision: String,
    pub base_tag: Option<String>,
    pub by: UserId,
    pub state: PendingState,
    pub error: Option<String>,
    pub attempts: u32,
    pub next_try_at: Option<OffsetDateTime>,
    pub flush: bool,
    pub first_staged_at: OffsetDateTime,
    pub staged_at: OffsetDateTime,
}

const COLUMNS: &str = "node_id, library_id, path, sha256, size, revision, base_tag, by_user, \
                       state, error, attempts, next_try_at, flush, first_staged_at, staged_at";

fn decode(what: &str, value: &str) -> sqlx::Error {
    sqlx::Error::Decode(format!("unknown {what} {value:?}").into())
}

fn write_from_row(row: &PgRow) -> Result<PendingWrite, sqlx::Error> {
    let sha256: String = row.try_get("sha256")?;
    let size: i64 = row.try_get("size")?;
    let state: String = row.try_get("state")?;
    let attempts: i32 = row.try_get("attempts")?;
    Ok(PendingWrite {
        node: NodeId(row.try_get("node_id")?),
        library: NodeId(row.try_get("library_id")?),
        path: row.try_get("path")?,
        blob: BlobRef {
            sha256: parse_hex(&sha256).ok_or_else(|| decode("content hash", &sha256))?,
            size: u64::try_from(size).map_err(|e| sqlx::Error::Decode(e.into()))?,
        },
        revision: row.try_get("revision")?,
        base_tag: row.try_get("base_tag")?,
        by: UserId(row.try_get("by_user")?),
        state: PendingState::parse(&state).ok_or_else(|| decode("write state", &state))?,
        error: row.try_get("error")?,
        attempts: u32::try_from(attempts).unwrap_or(0),
        next_try_at: row.try_get("next_try_at")?,
        flush: row.try_get("flush")?,
        first_staged_at: row.try_get("first_staged_at")?,
        staged_at: row.try_get("staged_at")?,
    })
}

/// `text` cut to [`MAX_ERROR_CHARS`] characters.
fn cut(text: &str) -> String {
    text.chars().take(MAX_ERROR_CHARS).collect()
}

fn column(bytes: u64) -> i64 {
    i64::try_from(bytes).unwrap_or(i64::MAX)
}

impl Db {
    /// Changes the mode of the connection of `library`. Whether it changed: a real change
    /// advances the generation (a sync under way records nothing of its result). A GitHub source
    /// is never read and write (`InvalidSetting`), and a library with pending writes is never
    /// made read-only (`WritesPending`): the row of the connection is locked `FOR UPDATE` and
    /// the pending rows counted in the same transaction, so a write staged meanwhile (which
    /// holds the row `FOR KEY SHARE` through its reference) is either counted or waits. A
    /// library with no connection changes nothing.
    pub async fn set_source_mode(
        &self,
        library: NodeId,
        mode: SourceMode,
    ) -> Result<bool, AccessError> {
        let mut tx = self.pool().begin().await?;
        let row: Option<(String, String)> = sqlx::query_as(
            "select kind, mode from library_sources where library_id = $1 for update",
        )
        .bind(library.0)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((kind, current)) = row else {
            return Ok(false);
        };
        if mode == SourceMode::ReadWrite && SourceKind::parse(&kind) == Some(SourceKind::Github) {
            return Err(AccessError::InvalidSetting);
        }
        if current == mode.as_str() {
            return Ok(false);
        }
        if mode == SourceMode::ReadOnly {
            let pending: i64 =
                sqlx::query_scalar("select count(*) from source_writes where library_id = $1")
                    .bind(library.0)
                    .fetch_one(&mut *tx)
                    .await?;
            if pending > 0 {
                return Err(AccessError::WritesPending);
            }
        }
        sqlx::query(
            "update library_sources set mode = $2, generation = generation + 1 \
             where library_id = $1",
        )
        .bind(library.0)
        .bind(mode.as_str())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Whether `library` may be written to, and why not; `None` if it has no connection.
    pub async fn write_state(
        &self,
        library: NodeId,
    ) -> Result<Option<(WriteState, Option<String>)>, DbError> {
        let row: Option<(String, Option<String>)> = sqlx::query_as(
            "select write_state, write_error from library_sources where library_id = $1",
        )
        .bind(library.0)
        .fetch_optional(self.pool())
        .await?;
        row.map(|(state, error)| {
            WriteState::parse(&state)
                .map(|state| (state, error))
                .ok_or_else(|| DbError::new(format!("unknown write state {state:?}")))
        })
        .transpose()
    }

    /// Records the write state of `library`, with `error` (cut to 300 characters) as its reason.
    /// Whether the state or the reason changed; nothing happens if there is no connection.
    pub async fn set_write_state(
        &self,
        library: NodeId,
        state: WriteState,
        error: Option<&str>,
    ) -> Result<bool, DbError> {
        let done = sqlx::query(
            "update library_sources set write_state = $2, write_error = $3 \
             where library_id = $1 \
               and (write_state <> $2 or write_error is distinct from $3)",
        )
        .bind(library.0)
        .bind(state.as_str())
        .bind(error.map(cut))
        .execute(self.pool())
        .await?;
        Ok(done.rows_affected() > 0)
    }

    /// Accepts the change `w`, in one transaction, and answers the revision it was given. The
    /// content's `blobs` row is locked `FOR SHARE` (the caller pinned it with
    /// [`Db::pin_cached`]), so no erasure runs meanwhile. A node with a pending row keeps its
    /// first base tag and first staging time; the content, size, author, staging time and
    /// revision are replaced, a request to flush is kept once made, and the attempt starts over.
    /// A tracked node's `source_items.revision` moves with it, so the editor's view and the
    /// page agree that the file changed. The library's connection row is locked `FOR SHARE` and
    /// its mode judged in the same transaction as the insert, so a switch to read-only cannot
    /// slip between: a library that is read-only (or has no connection) refuses with
    /// `LibraryReadOnly`, and a switch that wins the lock first is seen here.
    pub async fn stage_write(&self, w: &NewPendingWrite<'_>) -> Result<String, AccessError> {
        let revision = new_external_revision();
        let mut tx = self.pool().begin().await?;
        let mode: Option<String> =
            sqlx::query_scalar("select mode from library_sources where library_id = $1 for share")
                .bind(w.library.0)
                .fetch_optional(&mut *tx)
                .await?;
        if mode.as_deref() != Some(SourceMode::ReadWrite.as_str()) {
            return Err(AccessError::LibraryReadOnly);
        }
        sqlx::query("select sha256 from blobs where sha256 = $1 for share")
            .bind(w.blob.hex())
            .fetch_one(&mut *tx)
            .await?;
        sqlx::query(
            "insert into source_writes \
                 (node_id, library_id, path, sha256, size, revision, base_tag, by_user, state, \
                  error, attempts, next_try_at, flush, first_staged_at, staged_at) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, 'scanning', null, 0, null, $9, $10, $10) \
             on conflict (node_id) do update set \
                 sha256 = excluded.sha256, size = excluded.size, revision = excluded.revision, \
                 by_user = excluded.by_user, staged_at = excluded.staged_at, \
                 flush = source_writes.flush or excluded.flush, \
                 state = 'scanning', error = null, attempts = 0, next_try_at = null",
        )
        .bind(w.node.0)
        .bind(w.library.0)
        .bind(w.path)
        .bind(w.blob.hex())
        .bind(column(w.blob.size))
        .bind(&revision)
        .bind(w.base_tag)
        .bind(w.by.0)
        .bind(w.flush)
        .bind(w.now)
        .execute(&mut *tx)
        .await?;
        sqlx::query("update source_items set revision = $2 where node_id = $1")
            .bind(w.node.0)
            .bind(&revision)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(revision)
    }

    /// The pending write of `node`, if it has one.
    pub async fn pending_write(&self, node: NodeId) -> Result<Option<PendingWrite>, DbError> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "select {COLUMNS} from source_writes where node_id = $1"
        )))
        .bind(node.0)
        .fetch_optional(self.pool())
        .await?;
        Ok(row.as_ref().map(write_from_row).transpose()?)
    }

    /// Every pending write of `library`, oldest first.
    pub async fn pending_writes_of(&self, library: NodeId) -> Result<Vec<PendingWrite>, DbError> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "select {COLUMNS} from source_writes where library_id = $1 \
             order by first_staged_at, node_id"
        )))
        .bind(library.0)
        .fetch_all(self.pool())
        .await?;
        Ok(rows
            .iter()
            .map(write_from_row)
            .collect::<Result<_, sqlx::Error>>()?)
    }

    /// How many of the pending writes of `library` are being saved, and how many have failed.
    pub async fn pending_counts(&self, library: NodeId) -> Result<(u64, u64), DbError> {
        let (saving, failed): (i64, i64) = sqlx::query_as(
            "select count(*) filter (where state <> 'failed'), \
                    count(*) filter (where state = 'failed') \
             from source_writes where library_id = $1",
        )
        .bind(library.0)
        .fetch_one(self.pool())
        .await?;
        Ok((
            u64::try_from(saving).unwrap_or(0),
            u64::try_from(failed).unwrap_or(0),
        ))
    }

    /// At most `limit` pending writes whose retry time is null or passed and that are flushed or
    /// were first staged [`COALESCE`] or more before `now`, oldest first. Whether the content
    /// has been scanned is the caller's question ([`Db::availability`]), not this one's.
    pub async fn due_writes(
        &self,
        now: OffsetDateTime,
        limit: i64,
    ) -> Result<Vec<PendingWrite>, DbError> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "select {COLUMNS} from source_writes \
             where coalesce(next_try_at, '-infinity') <= $1 \
               and (flush or first_staged_at <= $2) \
             order by first_staged_at, node_id limit $3"
        )))
        .bind(now)
        .bind(now - COALESCE)
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        Ok(rows
            .iter()
            .map(write_from_row)
            .collect::<Result<_, sqlx::Error>>()?)
    }

    /// Records that the content `sha256` of `node` is in the store now, under `tag`, as one
    /// compare-and-set in one transaction (the row is locked first). A tracked node's item gets
    /// the new tag, size and modification time; when the row still holds `sha256` the item also
    /// takes the content as read now, forgets what made it unavailable and the object version
    /// read, keeps the row's revision, and the row is deleted (`true`). A new file gets its
    /// item, with the path and revision of the row. If a newer stage arrived meanwhile, the row
    /// stays, its base tag becomes `tag` (the store holds that now), and the answer is `false`.
    pub async fn finish_write(
        &self,
        node: NodeId,
        sha256: &[u8; 32],
        tag: &str,
        size: u64,
        modified: OffsetDateTime,
    ) -> Result<bool, DbError> {
        let mut tx = self.pool().begin().await?;
        let row: Option<(Uuid, String, String, String)> = sqlx::query_as(
            "select library_id, path, sha256, revision from source_writes \
             where node_id = $1 for update",
        )
        .bind(node.0)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((library, path, held, revision)) = row else {
            return Ok(false);
        };
        let current = held == hex(sha256);
        let tracked = sqlx::query(
            "update source_items set blob_sha = $2, size = $3, modified_at = $4 \
             where node_id = $1",
        )
        .bind(node.0)
        .bind(tag)
        .bind(column(size))
        .bind(modified)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0;
        if !tracked {
            sqlx::query(
                "insert into source_items \
                     (library_id, path, repository_path, node_id, blob_sha, size, revision, \
                      modified_at) \
                 values ($1, $2, $2, $3, $4, $5, $6, $7) \
                 on conflict (library_id, repository_path) do update set \
                     node_id = excluded.node_id, blob_sha = excluded.blob_sha, \
                     size = excluded.size, revision = excluded.revision, \
                     modified_at = excluded.modified_at",
            )
            .bind(library)
            .bind(&path)
            .bind(node.0)
            .bind(tag)
            .bind(column(size))
            .bind(&revision)
            .bind(modified)
            .execute(&mut *tx)
            .await?;
        }
        if !current {
            // The item's content is the old one no longer: forget it, so a read fetches the
            // bucket's, also if the newer stage is withdrawn.
            sqlx::query(
                "update source_items set content_sha256 = null, read_at = null where node_id = $1",
            )
            .bind(node.0)
            .execute(&mut *tx)
            .await?;
            sqlx::query("update source_writes set base_tag = $2 where node_id = $1")
                .bind(node.0)
                .bind(tag)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            return Ok(false);
        }
        sqlx::query(
            "update source_items set content_sha256 = $2, read_at = now(), unavailable = null, \
                 missing_since = null, version_id = null, revision = $3 \
             where node_id = $1",
        )
        .bind(node.0)
        .bind(hex(sha256))
        .bind(&revision)
        .execute(&mut *tx)
        .await?;
        sqlx::query("delete from source_writes where node_id = $1")
            .bind(node.0)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Records that writing the content `sha256` of `node` failed with `error` (cut to 300
    /// characters): the row is failed, counts the attempt and waits until `next_try_at`. Whether
    /// the row still holds that content.
    pub async fn fail_write(
        &self,
        node: NodeId,
        sha256: &[u8; 32],
        error: &str,
        next_try_at: OffsetDateTime,
    ) -> Result<bool, DbError> {
        let done = sqlx::query(
            "update source_writes set state = 'failed', error = $3, attempts = attempts + 1, \
                 next_try_at = $4 \
             where node_id = $1 and sha256 = $2",
        )
        .bind(node.0)
        .bind(hex(sha256))
        .bind(cut(error))
        .bind(next_try_at)
        .execute(self.pool())
        .await?;
        Ok(done.rows_affected() > 0)
    }

    /// Makes the pending write of `node` due at `now` (as a person's Retry does): it is
    /// scanning again, flushed so the throttle does not hold it, and keeps its last error until
    /// it is written. Whether there is a row.
    pub async fn retry_write(&self, node: NodeId, now: OffsetDateTime) -> Result<bool, DbError> {
        let done = sqlx::query(
            "update source_writes set state = 'scanning', next_try_at = $2, flush = true \
             where node_id = $1",
        )
        .bind(node.0)
        .bind(now)
        .execute(self.pool())
        .await?;
        Ok(done.rows_affected() > 0)
    }

    /// Asks for the pending write of `node` not to wait for the throttle (a session ended, or
    /// the person closed the file). A retry time still holds it back, so `_now` is not used.
    /// Whether there is a row.
    pub async fn flush_write(&self, node: NodeId, _now: OffsetDateTime) -> Result<bool, DbError> {
        let done = sqlx::query("update source_writes set flush = true where node_id = $1")
            .bind(node.0)
            .execute(self.pool())
            .await?;
        Ok(done.rows_affected() > 0)
    }

    /// Withdraws the pending write of `node` if it still holds the content `sha256`. A tracked
    /// node's item gets a fresh revision (the staged one is withdrawn; its content is the
    /// store's again). Whether it was withdrawn.
    pub async fn drop_write(&self, node: NodeId, sha256: &[u8; 32]) -> Result<bool, DbError> {
        let mut tx = self.pool().begin().await?;
        let done = sqlx::query("delete from source_writes where node_id = $1 and sha256 = $2")
            .bind(node.0)
            .bind(hex(sha256))
            .execute(&mut *tx)
            .await?;
        if done.rows_affected() == 0 {
            return Ok(false);
        }
        sqlx::query("update source_items set revision = $2 where node_id = $1")
            .bind(node.0)
            .bind(new_external_revision())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// The tracking row of `node` in its connected library, if it has one.
    pub async fn source_item_of(&self, node: NodeId) -> Result<Option<SourceItem>, DbError> {
        let row = sqlx::query(
            "select node_id, path, repository_path, blob_sha, size from source_items \
             where node_id = $1",
        )
        .bind(node.0)
        .fetch_optional(self.pool())
        .await?;
        Ok(row.as_ref().map(item_from_row).transpose()?)
    }
}
