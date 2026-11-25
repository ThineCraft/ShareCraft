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

/// What a library holds in its holding folder for a deleted bin entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Held {
    pub entry: NodeId,
    pub library: NodeId,
    /// Library-relative; ends with `/`.
    pub root: String,
    pub objects: u64,
    pub held_at: OffsetDateTime,
    pub retry_at: Option<OffsetDateTime>,
}

/// A multipart upload started and not yet completed or aborted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenMultipart {
    pub library: NodeId,
    pub node: NodeId,
    pub key: String,
    pub upload_id: String,
    pub started_at: OffsetDateTime,
}

fn held_from_row(row: &PgRow) -> Result<Held, sqlx::Error> {
    let objects: i64 = row.try_get("objects")?;
    Ok(Held {
        entry: NodeId(row.try_get("entry_id")?),
        library: NodeId(row.try_get("library_id")?),
        root: row.try_get("root")?,
        objects: u64::try_from(objects).map_err(|e| sqlx::Error::Decode(e.into()))?,
        held_at: row.try_get("held_at")?,
        retry_at: row.try_get("retry_at")?,
    })
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
            let pending: i64 = sqlx::query_scalar(
                "select (select count(*) from source_writes where library_id = $1) \
                      + (select count(*) from source_multipart where library_id = $1)",
            )
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
    /// were first staged [`COALESCE`] or more before `now`, oldest first. Rows of a library whose
    /// writes are blocked are left out: they wait for the probe that unblocks it, and never take
    /// the places of other libraries' rows. Whether the content has been scanned is the
    /// caller's question ([`Db::availability`]), not this one's.
    pub async fn due_writes(
        &self,
        now: OffsetDateTime,
        limit: i64,
    ) -> Result<Vec<PendingWrite>, DbError> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "select {COLUMNS} from source_writes w \
             where coalesce(w.next_try_at, '-infinity') <= $1 \
               and (w.flush or w.first_staged_at <= $2) \
               and not exists (select 1 from library_sources s \
                               where s.library_id = w.library_id \
                                 and s.write_state = 'blocked') \
             order by w.first_staged_at, w.node_id limit $3"
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
        // A row that is not failed is due at once; a failed one keeps its retry time.
        let done = sqlx::query(
            "update source_writes set flush = true, \
             next_try_at = case when state = 'failed' then next_try_at else null end \
             where node_id = $1",
        )
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

    /// Moves the pending write of `node` holding the content `sha256` to `path` (a new file
    /// written under another name because its own was taken meanwhile). Whether it moved.
    pub async fn repath_write(
        &self,
        node: NodeId,
        sha256: &[u8; 32],
        path: &str,
    ) -> Result<bool, DbError> {
        let done =
            sqlx::query("update source_writes set path = $3 where node_id = $1 and sha256 = $2")
                .bind(node.0)
                .bind(hex(sha256))
                .bind(path)
                .execute(self.pool())
                .await?;
        Ok(done.rows_affected() > 0)
    }

    /// Puts off the pending write of `node` holding the content `sha256` until `until`, without
    /// counting an attempt or recording an error (its content is still being scanned, or a
    /// listing holds its library). Whether the row still holds that content.
    pub async fn defer_write(
        &self,
        node: NodeId,
        sha256: &[u8; 32],
        until: OffsetDateTime,
    ) -> Result<bool, DbError> {
        let done = sqlx::query(
            "update source_writes set next_try_at = $3 where node_id = $1 and sha256 = $2",
        )
        .bind(node.0)
        .bind(hex(sha256))
        .bind(until)
        .execute(self.pool())
        .await?;
        Ok(done.rows_affected() > 0)
    }

    /// The libraries whose writes are blocked and that have pending writes.
    pub async fn blocked_with_writes(&self) -> Result<Vec<NodeId>, DbError> {
        let ids: Vec<Uuid> = sqlx::query_scalar(
            "select s.library_id from library_sources s \
             where s.write_state = 'blocked' \
               and exists (select 1 from source_writes w where w.library_id = s.library_id) \
             order by s.library_id",
        )
        .fetch_all(self.pool())
        .await?;
        Ok(ids.into_iter().map(NodeId).collect())
    }

    /// Records that the objects of the bin entry `entry` are held under `root` of `library`
    /// (`objects` of them), replacing an earlier record of the entry.
    pub async fn hold_record(
        &self,
        entry: NodeId,
        library: NodeId,
        root: &str,
        objects: u64,
        now: OffsetDateTime,
    ) -> Result<(), DbError> {
        sqlx::query(
            "insert into source_held (entry_id, library_id, root, objects, held_at, retry_at) \
             values ($1, $2, $3, $4, $5, null) \
             on conflict (entry_id) do update set library_id = excluded.library_id, \
                 root = excluded.root, objects = excluded.objects, \
                 held_at = excluded.held_at, retry_at = null",
        )
        .bind(entry.0)
        .bind(library.0)
        .bind(root)
        .bind(column(objects))
        .bind(now)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// The hold of the bin entry `entry`, whatever its retry time holds.
    pub async fn held(&self, entry: NodeId) -> Result<Option<Held>, DbError> {
        let row = sqlx::query(
            "select entry_id, library_id, root, objects, held_at, retry_at from source_held \
             where entry_id = $1",
        )
        .bind(entry.0)
        .fetch_optional(self.pool())
        .await?;
        Ok(row.as_ref().map(held_from_row).transpose()?)
    }

    /// The bin entries `library` holds objects for, oldest hold first.
    pub async fn held_entries_of(&self, library: NodeId) -> Result<Vec<NodeId>, DbError> {
        let ids: Vec<Uuid> = sqlx::query_scalar(
            "select entry_id from source_held where library_id = $1 order by held_at, entry_id",
        )
        .bind(library.0)
        .fetch_all(self.pool())
        .await?;
        Ok(ids.into_iter().map(NodeId).collect())
    }

    /// The S3 libraries connected read and write whose writes are not blocked, by id.
    pub async fn writable_s3_libraries(&self) -> Result<Vec<NodeId>, DbError> {
        let ids: Vec<Uuid> = sqlx::query_scalar(
            "select library_id from library_sources \
             where kind = 's3' and mode = 'read_write' and write_state <> 'blocked' \
             order by library_id",
        )
        .fetch_all(self.pool())
        .await?;
        Ok(ids.into_iter().map(NodeId).collect())
    }

    /// Forgets the hold of `entry`. Whether there was one.
    pub async fn forget_held(&self, entry: NodeId) -> Result<bool, DbError> {
        let done = sqlx::query("delete from source_held where entry_id = $1")
            .bind(entry.0)
            .execute(self.pool())
            .await?;
        Ok(done.rows_affected() > 0)
    }

    /// Puts off the next try at releasing the hold of `entry` until `until`. Whether there is
    /// a hold.
    pub async fn defer_held(&self, entry: NodeId, until: OffsetDateTime) -> Result<bool, DbError> {
        let done = sqlx::query("update source_held set retry_at = $2 where entry_id = $1")
            .bind(entry.0)
            .bind(until)
            .execute(self.pool())
            .await?;
        Ok(done.rows_affected() > 0)
    }

    /// Records that a multipart upload `upload_id` of `key` (for `node`) was started in
    /// `library`.
    pub async fn note_multipart(
        &self,
        library: NodeId,
        node: NodeId,
        key: &str,
        upload_id: &str,
        now: OffsetDateTime,
    ) -> Result<(), DbError> {
        sqlx::query(
            "insert into source_multipart (library_id, upload_id, node_id, key, started_at) \
             values ($1, $2, $3, $4, $5) \
             on conflict (library_id, upload_id) do update set \
                 node_id = excluded.node_id, key = excluded.key, started_at = excluded.started_at",
        )
        .bind(library.0)
        .bind(upload_id)
        .bind(node.0)
        .bind(key)
        .bind(now)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Forgets the multipart upload `upload_id` (completed or aborted). Whether there was one.
    pub async fn forget_multipart(
        &self,
        library: NodeId,
        upload_id: &str,
    ) -> Result<bool, DbError> {
        let done =
            sqlx::query("delete from source_multipart where library_id = $1 and upload_id = $2")
                .bind(library.0)
                .bind(upload_id)
                .execute(self.pool())
                .await?;
        Ok(done.rows_affected() > 0)
    }

    /// At most `limit` multipart uploads started before `before`, oldest first.
    pub async fn stale_multiparts(
        &self,
        before: OffsetDateTime,
        limit: i64,
    ) -> Result<Vec<OpenMultipart>, DbError> {
        let rows: Vec<(Uuid, Uuid, String, String, OffsetDateTime)> = sqlx::query_as(
            "select library_id, node_id, key, upload_id, started_at from source_multipart \
             where started_at < $1 order by started_at, library_id, upload_id limit $2",
        )
        .bind(before)
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(
                |(library, node, key, upload_id, started_at)| OpenMultipart {
                    library: NodeId(library),
                    node: NodeId(node),
                    key,
                    upload_id,
                    started_at,
                },
            )
            .collect())
    }

    /// How many pending writes of `library` are for `repository_path`, or for anything below it
    /// when it ends with `/`. The match is literal.
    pub async fn writes_below(
        &self,
        library: NodeId,
        repository_path: &str,
    ) -> Result<u64, DbError> {
        let folder = repository_path.ends_with('/');
        let count: i64 = sqlx::query_scalar(
            "select count(*) from source_writes where library_id = $1 \
               and (path = $2 or ($3 and left(path, char_length($2)) = $2))",
        )
        .bind(library.0)
        .bind(repository_path)
        .bind(folder)
        .fetch_one(self.pool())
        .await?;
        Ok(u64::try_from(count).unwrap_or(0))
    }

    /// Moves the tracked item `from` of `library` to `to` (both repository paths), and
    /// everything below it when `from` ends with `/`, in one transaction. Answers how many
    /// rows changed. Both the repository path and the shown path are rewritten; a shown path
    /// that does not start with `from` (a case-collision suffix keeps its own tail, so a path
    /// that does not share the prefix is left as it is). A target already taken is a `DbError`
    /// and nothing changes.
    pub async fn move_source_items(
        &self,
        library: NodeId,
        from: &str,
        to: &str,
    ) -> Result<u64, DbError> {
        let folder = from.ends_with('/');
        let mut tx = self.pool().begin().await?;
        let rows: Vec<(Uuid, String, String)> = sqlx::query_as(
            "select node_id, repository_path, path from source_items \
             where library_id = $1 \
               and (repository_path = $2 \
                    or ($3 and left(repository_path, char_length($2)) = $2)) \
             order by repository_path for update",
        )
        .bind(library.0)
        .bind(from)
        .bind(folder)
        .fetch_all(&mut *tx)
        .await?;
        let rebased = |text: &str| -> Option<String> {
            text.strip_prefix(from).map(|rest| format!("{to}{rest}"))
        };
        let mut ids = Vec::with_capacity(rows.len());
        let mut temps = Vec::with_capacity(rows.len());
        let mut repository = Vec::with_capacity(rows.len());
        let mut shown = Vec::with_capacity(rows.len());
        for (node, old_repository, old_path) in &rows {
            ids.push(*node);
            temps.push(format!("\u{1}moving/{node}"));
            repository.push(rebased(old_repository).unwrap_or_else(|| old_repository.clone()));
            shown.push(rebased(old_path).unwrap_or_else(|| old_path.clone()));
        }
        // Two steps, so rows trading places inside the subtree never meet halfway: the unique
        // keys are not deferrable. A target taken by a row outside fails the second step, and
        // the transaction is dropped unfinished.
        sqlx::query(
            "update source_items i set repository_path = t.temp, path = t.temp \
             from unnest($2::uuid[], $3::text[]) as t(id, temp) \
             where i.library_id = $1 and i.node_id = t.id",
        )
        .bind(library.0)
        .bind(&ids)
        .bind(&temps)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "update source_items i set repository_path = t.repository, path = t.shown \
             from unnest($2::uuid[], $3::text[], $4::text[]) as t(id, repository, shown) \
             where i.library_id = $1 and i.node_id = t.id",
        )
        .bind(library.0)
        .bind(&ids)
        .bind(&repository)
        .bind(&shown)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(u64::try_from(rows.len()).unwrap_or(0))
    }

    /// Records what the store holds for the tracked `node` after ShareCraft moved it inside the
    /// store: the copy's tag `new_tag`, `size` and modification time, only if `old_tag` is still
    /// current. The content is the same, so the revision, the content read and its time are
    /// kept (an editor's base stays valid); the object version read is forgotten, since the
    /// copy is another object. Whether it changed.
    pub async fn retag_source_item(
        &self,
        node: NodeId,
        old_tag: &str,
        new_tag: &str,
        size: u64,
        modified: OffsetDateTime,
    ) -> Result<bool, DbError> {
        let done = sqlx::query(
            "update source_items set blob_sha = $3, size = $4, modified_at = $5, \
                 version_id = null, missing_since = null \
             where node_id = $1 and blob_sha = $2",
        )
        .bind(node.0)
        .bind(old_tag)
        .bind(new_tag)
        .bind(column(size))
        .bind(modified)
        .execute(self.pool())
        .await?;
        Ok(done.rows_affected() > 0)
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
