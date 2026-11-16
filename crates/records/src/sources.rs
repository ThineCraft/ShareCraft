//! Connected libraries: a library that mirrors a branch of a repository. The records here are
//! the connection (`library_sources`), which node holds which repository path (`source_items`)
//! and the sealed secrets the connection uses (`secrets`). The mirroring itself is the
//! server's; PostgreSQL holds only what it needs to resume.
//!
//! A secret is stored as ciphertext only. The key that seals it lives outside the database.

use collab_core::ids::{NodeId, UserId};
use sqlx::Row;
use sqlx::postgres::PgRow;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::libraries::ensure_library_row;
use crate::{AccessError, Db, DbError};

/// PostgreSQL's code for a row a check constraint refuses.
const CHECK_VIOLATION: &str = "23514";
/// The shortest and longest time between two polls of a source, in seconds.
pub const MIN_POLL_SECONDS: u32 = 60;
pub const MAX_POLL_SECONDS: u32 = 86_400;
/// The longest `last_error` the record keeps, in characters.
pub const MAX_SOURCE_ERROR_CHARS: usize = 300;

/// The setting that caps how many files a connected library may mirror (an integer from 1;
/// 50 000 by default). Read by the sync.
pub const SOURCE_MAX_FILES: &str = "source_max_files";
/// The setting that caps how many bytes a connected library may mirror (an integer from 1;
/// 2 GiB by default). Read by the sync.
pub const SOURCE_MAX_BYTES: &str = "source_max_bytes";
/// The default of [`SOURCE_MAX_FILES`].
pub const DEFAULT_SOURCE_MAX_FILES: i64 = 50_000;
/// The default of [`SOURCE_MAX_BYTES`].
pub const DEFAULT_SOURCE_MAX_BYTES: i64 = 2_147_483_648;

/// The id of a stored secret.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SecretId(pub Uuid);

/// An encrypted value as the `secrets` table holds it. Never plaintext.
#[derive(Clone, PartialEq, Eq)]
pub struct SealedRow {
    /// Which key sealed it.
    pub key_id: u32,
    pub nonce: [u8; 12],
    /// The ciphertext with its authentication tag; never empty.
    pub ciphertext: Vec<u8>,
}

impl std::fmt::Debug for SealedRow {
    /// Shows the key and sizes only, as a precaution.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SealedRow")
            .field("key_id", &self.key_id)
            .field("ciphertext_len", &self.ciphertext.len())
            .finish_non_exhaustive()
    }
}

/// What a library is connected to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    Github,
}

impl SourceKind {
    /// The word for the kind, as stored.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            SourceKind::Github => "github",
        }
    }

    /// The kind named `word`.
    #[must_use]
    pub fn parse(word: &str) -> Option<SourceKind> {
        match word {
            "github" => Some(SourceKind::Github),
            _ => None,
        }
    }
}

/// Where a connection stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceState {
    /// Made, and not yet read.
    Connecting,
    /// In step with the branch as of the last check.
    Ok,
    /// A sync is running.
    Syncing,
    /// The repository, branch or token no longer works; no retry until someone changes it.
    NotConnected,
    /// Waiting for the host's rate limit to reset (see `retry_at`).
    RateLimited,
    /// The last sync failed; it is retried after `retry_at`.
    Failed,
}

impl SourceState {
    /// The word for the state, as stored.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            SourceState::Connecting => "connecting",
            SourceState::Ok => "ok",
            SourceState::Syncing => "syncing",
            SourceState::NotConnected => "not_connected",
            SourceState::RateLimited => "rate_limited",
            SourceState::Failed => "failed",
        }
    }

    /// The state named `word`.
    #[must_use]
    pub fn parse(word: &str) -> Option<SourceState> {
        match word {
            "connecting" => Some(SourceState::Connecting),
            "ok" => Some(SourceState::Ok),
            "syncing" => Some(SourceState::Syncing),
            "not_connected" => Some(SourceState::NotConnected),
            "rate_limited" => Some(SourceState::RateLimited),
            "failed" => Some(SourceState::Failed),
            _ => None,
        }
    }
}

/// A connection to make.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewSource {
    pub library: NodeId,
    pub kind: SourceKind,
    pub owner: String,
    pub repo: String,
    pub branch: String,
    /// A folder of the repository; empty for the whole repository.
    pub folder: String,
    /// The sealed access token, for a private repository.
    pub token: Option<SecretId>,
    /// Seconds between polls, from 60 to 86 400.
    pub poll_seconds: u32,
    /// Who connected it; the sync is attributed to them.
    pub connected_by: UserId,
}

/// A connection's configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceConfig {
    pub library: NodeId,
    pub kind: SourceKind,
    pub owner: String,
    pub repo: String,
    pub branch: String,
    pub folder: String,
    pub token: Option<SecretId>,
    pub webhook_secret: Option<SecretId>,
    pub poll_seconds: u32,
    pub connected_by: UserId,
}

/// A connection's state and sync bookmark.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceStatus {
    pub state: SourceState,
    /// The commit last mirrored.
    pub last_commit: Option<String>,
    /// The entity tag of the last answer about the branch, for a conditional request.
    pub etag: Option<String>,
    pub last_checked_at: Option<OffsetDateTime>,
    pub last_synced_at: Option<OffsetDateTime>,
    /// A plain sentence of ours about the last failure, at most 300 characters; never text
    /// from a response.
    pub last_error: Option<String>,
    /// No attempt is made before this time.
    pub retry_at: Option<OffsetDateTime>,
    /// Files mirrored.
    pub files: i64,
    /// Bytes mirrored.
    pub bytes: i64,
    /// Entries not mirrored.
    pub hidden: i64,
}

/// A new status for [`Db::set_source_status`]: every field is written as given.
pub type StatusUpdate = SourceStatus;

/// What [`Db::change_source`] changes; `None` leaves a field alone. For the two secrets,
/// `Some(None)` takes the secret away. Changing the branch or folder clears the sync bookmark
/// (`last_commit` and `etag`), as what was mirrored no longer says where the new place stands.
/// The secrets replaced or removed are the caller's to delete.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourceChange {
    pub branch: Option<String>,
    pub folder: Option<String>,
    pub poll_seconds: Option<u32>,
    pub token: Option<Option<SecretId>>,
    pub webhook_secret: Option<Option<SecretId>>,
}

/// A node that holds a repository path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceItem {
    pub node: NodeId,
    /// The path in the repository, below the mirrored folder.
    pub path: String,
    /// The Git blob of a file; `None` for a folder.
    pub blob_sha: Option<String>,
    pub size: i64,
}

/// The turn to sync one library (see [`Db::try_lock_source`]); dropping it gives the turn up.
pub struct SourceLock {
    _tx: sqlx::Transaction<'static, sqlx::Postgres>,
}

impl std::fmt::Debug for SourceLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SourceLock")
    }
}

const SOURCE_COLUMNS: &str = "library_id, kind, owner, repo, branch, folder, token_secret, \
    webhook_secret, poll_seconds, connected_by, state, last_commit, etag, last_checked_at, \
    last_synced_at, last_error, retry_at, files, bytes, hidden";

fn decode(what: &str, value: &str) -> sqlx::Error {
    sqlx::Error::Decode(format!("unknown {what} {value:?}").into())
}

fn source_from_row(row: &PgRow) -> Result<(SourceConfig, SourceStatus), sqlx::Error> {
    let kind: String = row.try_get("kind")?;
    let state: String = row.try_get("state")?;
    let poll: i32 = row.try_get("poll_seconds")?;
    let token: Option<Uuid> = row.try_get("token_secret")?;
    let webhook: Option<Uuid> = row.try_get("webhook_secret")?;
    let config = SourceConfig {
        library: NodeId(row.try_get("library_id")?),
        kind: SourceKind::parse(&kind).ok_or_else(|| decode("source kind", &kind))?,
        owner: row.try_get("owner")?,
        repo: row.try_get("repo")?,
        branch: row.try_get("branch")?,
        folder: row.try_get("folder")?,
        token: token.map(SecretId),
        webhook_secret: webhook.map(SecretId),
        poll_seconds: u32::try_from(poll).map_err(|e| sqlx::Error::Decode(e.into()))?,
        connected_by: UserId(row.try_get("connected_by")?),
    };
    let status = SourceStatus {
        state: SourceState::parse(&state).ok_or_else(|| decode("source state", &state))?,
        last_commit: row.try_get("last_commit")?,
        etag: row.try_get("etag")?,
        last_checked_at: row.try_get("last_checked_at")?,
        last_synced_at: row.try_get("last_synced_at")?,
        last_error: row.try_get("last_error")?,
        retry_at: row.try_get("retry_at")?,
        files: row.try_get("files")?,
        bytes: row.try_get("bytes")?,
        hidden: row.try_get("hidden")?,
    };
    Ok((config, status))
}

fn poll_ok(seconds: u32) -> bool {
    (MIN_POLL_SECONDS..=MAX_POLL_SECONDS).contains(&seconds)
}

/// Maps a refused check to `InvalidSetting`.
fn refused(e: sqlx::Error) -> AccessError {
    match e {
        sqlx::Error::Database(d) if d.code().as_deref() == Some(CHECK_VIOLATION) => {
            AccessError::InvalidSetting
        }
        other => other.into(),
    }
}

impl Db {
    /// Stores a sealed secret and returns its id. A secret with no ciphertext is refused: only
    /// sealed bytes belong here.
    pub async fn put_secret(&self, s: &SealedRow) -> Result<SecretId, DbError> {
        if s.ciphertext.is_empty() {
            return Err(DbError::new("a secret must be sealed: no ciphertext"));
        }
        let id = Uuid::new_v4();
        sqlx::query("insert into secrets (id, key_id, nonce, ciphertext) values ($1, $2, $3, $4)")
            .bind(id)
            .bind(i32::try_from(s.key_id).map_err(DbError::new)?)
            .bind(s.nonce.as_slice())
            .bind(&s.ciphertext)
            .execute(self.pool())
            .await?;
        Ok(SecretId(id))
    }

    /// The sealed secret `id`, if there is one.
    pub async fn get_secret(&self, id: SecretId) -> Result<Option<SealedRow>, DbError> {
        let row = sqlx::query("select key_id, nonce, ciphertext from secrets where id = $1")
            .bind(id.0)
            .fetch_optional(self.pool())
            .await?;
        row.map(|row| {
            let key_id: i32 = row.try_get("key_id")?;
            let nonce: Vec<u8> = row.try_get("nonce")?;
            Ok::<_, sqlx::Error>(SealedRow {
                key_id: u32::try_from(key_id).map_err(|e| sqlx::Error::Decode(e.into()))?,
                nonce: nonce
                    .try_into()
                    .map_err(|_| sqlx::Error::Decode("a nonce of the wrong length".into()))?,
                ciphertext: row.try_get("ciphertext")?,
            })
        })
        .transpose()
        .map_err(DbError::from)
    }

    /// Deletes the secret `id`; nothing happens if there is none. A secret a connection still
    /// uses cannot be deleted (the database refuses).
    pub async fn delete_secret(&self, id: SecretId) -> Result<(), DbError> {
        sqlx::query("delete from secrets where id = $1")
            .bind(id.0)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// Connects a library to a source, in state `connecting`. `NotFound` unless it is a live
    /// library; `InvalidSetting` for a poll time outside 60 to 86 400 seconds;
    /// `LibraryNotEmpty` if anything not in the bin is in it; `SourceExists` if it is connected
    /// already.
    pub async fn connect_source(&self, s: &NewSource) -> Result<(), AccessError> {
        if !poll_ok(s.poll_seconds) {
            return Err(AccessError::InvalidSetting);
        }
        let mut tx = self.pool().begin().await?;
        let node: Option<(String, Option<OffsetDateTime>)> =
            sqlx::query_as("select kind, deleted_at from nodes where id = $1 for update")
                .bind(s.library.0)
                .fetch_optional(&mut *tx)
                .await?;
        match node {
            Some((kind, None)) if kind == "library" => {}
            _ => return Err(AccessError::NotFound),
        }
        ensure_library_row(&mut tx, s.library).await?;
        let exists: bool = sqlx::query_scalar(
            "select exists (select 1 from library_sources where library_id = $1)",
        )
        .bind(s.library.0)
        .fetch_one(&mut *tx)
        .await?;
        if exists {
            return Err(AccessError::SourceExists);
        }
        let occupied: bool = sqlx::query_scalar(
            "select exists (select 1 from nodes where parent_id = $1 and deleted_at is null)",
        )
        .bind(s.library.0)
        .fetch_one(&mut *tx)
        .await?;
        if occupied {
            return Err(AccessError::LibraryNotEmpty);
        }
        sqlx::query(
            "insert into library_sources (library_id, kind, owner, repo, branch, folder, \
                 token_secret, poll_seconds, connected_by) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(s.library.0)
        .bind(s.kind.as_str())
        .bind(&s.owner)
        .bind(&s.repo)
        .bind(&s.branch)
        .bind(&s.folder)
        .bind(s.token.map(|t| t.0))
        .bind(i32::try_from(s.poll_seconds).map_err(|_| AccessError::InvalidSetting)?)
        .bind(s.connected_by.0)
        .execute(&mut *tx)
        .await
        .map_err(refused)?;
        tx.commit().await?;
        Ok(())
    }

    /// The configuration and status of the connection of `library`, if it has one.
    pub async fn source_of(
        &self,
        library: NodeId,
    ) -> Result<Option<(SourceConfig, SourceStatus)>, DbError> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "select {SOURCE_COLUMNS} from library_sources where library_id = $1"
        )))
        .bind(library.0)
        .fetch_optional(self.pool())
        .await?;
        Ok(row.as_ref().map(source_from_row).transpose()?)
    }

    /// The connected library that contains `node` (or is `node`), if it is in one.
    pub async fn source_library_of(&self, node: NodeId) -> Result<Option<NodeId>, DbError> {
        let Some(library) = self.library_of(node).await? else {
            return Ok(None);
        };
        let connected: bool = sqlx::query_scalar(
            "select exists (select 1 from library_sources where library_id = $1)",
        )
        .bind(library.0)
        .fetch_one(self.pool())
        .await?;
        Ok(connected.then_some(library))
    }

    /// Writes the status of the connection of `library`; every field is written as given. A
    /// `last_error` is cut to 300 characters. Nothing happens if the library has no connection.
    pub async fn set_source_status(
        &self,
        library: NodeId,
        u: &StatusUpdate,
    ) -> Result<(), DbError> {
        let error: Option<String> = u
            .last_error
            .as_ref()
            .map(|e| e.chars().take(MAX_SOURCE_ERROR_CHARS).collect());
        sqlx::query(
            "update library_sources set state = $2, last_commit = $3, etag = $4, \
                 last_checked_at = $5, last_synced_at = $6, last_error = $7, retry_at = $8, \
                 files = $9, bytes = $10, hidden = $11 \
             where library_id = $1",
        )
        .bind(library.0)
        .bind(u.state.as_str())
        .bind(&u.last_commit)
        .bind(&u.etag)
        .bind(u.last_checked_at)
        .bind(u.last_synced_at)
        .bind(error)
        .bind(u.retry_at)
        .bind(u.files)
        .bind(u.bytes)
        .bind(u.hidden)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Changes the connection of `library` as `c` says. `NotFound` if it has none;
    /// `InvalidSetting` for a poll time outside 60 to 86 400 seconds. A changed branch or
    /// folder clears `last_commit` and `etag`.
    pub async fn change_source(
        &self,
        library: NodeId,
        c: &SourceChange,
    ) -> Result<(), AccessError> {
        if c.poll_seconds.is_some_and(|s| !poll_ok(s)) {
            return Err(AccessError::InvalidSetting);
        }
        let poll = c
            .poll_seconds
            .map(|s| i32::try_from(s).map_err(|_| AccessError::InvalidSetting))
            .transpose()?;
        let mut tx = self.pool().begin().await?;
        let current: Option<(String, String)> = sqlx::query_as(
            "select branch, folder from library_sources where library_id = $1 for update",
        )
        .bind(library.0)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((branch, folder)) = current else {
            return Err(AccessError::NotFound);
        };
        let moved = c.branch.as_ref().is_some_and(|b| *b != branch)
            || c.folder.as_ref().is_some_and(|f| *f != folder);
        sqlx::query(
            "update library_sources set \
                 branch = coalesce($2, branch), \
                 folder = coalesce($3, folder), \
                 poll_seconds = coalesce($4, poll_seconds), \
                 token_secret = case when $5 then $6 else token_secret end, \
                 webhook_secret = case when $7 then $8 else webhook_secret end, \
                 last_commit = case when $9 then null else last_commit end, \
                 etag = case when $9 then null else etag end \
             where library_id = $1",
        )
        .bind(library.0)
        .bind(&c.branch)
        .bind(&c.folder)
        .bind(poll)
        .bind(c.token.is_some())
        .bind(c.token.flatten().map(|t| t.0))
        .bind(c.webhook_secret.is_some())
        .bind(c.webhook_secret.flatten().map(|t| t.0))
        .bind(moved)
        .execute(&mut *tx)
        .await
        .map_err(refused)?;
        tx.commit().await?;
        Ok(())
    }

    /// The items of `library`, by path.
    pub async fn source_items(&self, library: NodeId) -> Result<Vec<SourceItem>, DbError> {
        let rows = sqlx::query(
            "select node_id, path, blob_sha, size from source_items \
             where library_id = $1 order by path",
        )
        .bind(library.0)
        .fetch_all(self.pool())
        .await?;
        rows.iter()
            .map(|row| {
                Ok(SourceItem {
                    node: NodeId(row.try_get("node_id")?),
                    path: row.try_get("path")?,
                    blob_sha: row.try_get("blob_sha")?,
                    size: row.try_get("size")?,
                })
            })
            .collect::<Result<_, sqlx::Error>>()
            .map_err(DbError::from)
    }

    /// Records `items` for `library`: a path already recorded takes the new node, blob and
    /// size. In one transaction, so a node held at two paths refuses the whole batch.
    pub async fn upsert_source_items(
        &self,
        library: NodeId,
        items: &[SourceItem],
    ) -> Result<(), DbError> {
        let mut tx = self.pool().begin().await?;
        for item in items {
            sqlx::query(
                "insert into source_items (library_id, path, node_id, blob_sha, size) \
                 values ($1, $2, $3, $4, $5) \
                 on conflict (library_id, path) do update set node_id = excluded.node_id, \
                     blob_sha = excluded.blob_sha, size = excluded.size",
            )
            .bind(library.0)
            .bind(&item.path)
            .bind(item.node.0)
            .bind(&item.blob_sha)
            .bind(item.size)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Forgets the items held by `nodes` in `library`. The nodes themselves stay.
    pub async fn remove_source_items(
        &self,
        library: NodeId,
        nodes: &[NodeId],
    ) -> Result<(), DbError> {
        let ids: Vec<Uuid> = nodes.iter().map(|n| n.0).collect();
        sqlx::query("delete from source_items where library_id = $1 and node_id = any($2)")
            .bind(library.0)
            .bind(&ids)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// Up to `limit` live connected libraries that are due for a check at `now`, the longest
    /// waiting first: in state `connecting`, `ok`, `rate_limited` or `failed` (not `syncing`,
    /// and not `not_connected`, which waits for someone to change it), whose `retry_at` has
    /// passed or is unset, and which were last checked at least `poll_seconds` ago or never.
    pub async fn due_sources(
        &self,
        now: OffsetDateTime,
        limit: i64,
    ) -> Result<Vec<NodeId>, DbError> {
        let ids: Vec<Uuid> = sqlx::query_scalar(
            "select s.library_id from library_sources s join nodes n on n.id = s.library_id \
             where s.state in ('connecting', 'ok', 'rate_limited', 'failed') \
               and n.deleted_at is null \
               and (s.retry_at is null or s.retry_at <= $1) \
               and (s.last_checked_at is null \
                    or s.last_checked_at + make_interval(secs => s.poll_seconds) <= $1) \
             order by s.last_checked_at asc nulls first, s.library_id \
             limit $2",
        )
        .bind(now)
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        Ok(ids.into_iter().map(NodeId).collect())
    }

    /// Takes the turn to sync `library`, if nobody holds it: a transaction-scoped PostgreSQL
    /// advisory lock, held until the answer is dropped (its transaction then ends and the lock
    /// goes, also when the process dies). `None` while someone else holds it.
    pub async fn try_lock_source(&self, library: NodeId) -> Result<Option<SourceLock>, DbError> {
        let mut tx = self.pool().begin().await?;
        let got: bool = sqlx::query_scalar(
            "select pg_try_advisory_xact_lock(hashtextextended('source-sync:' || $1::text, 0))",
        )
        .bind(library.0)
        .fetch_one(&mut *tx)
        .await?;
        Ok(got.then_some(SourceLock { _tx: tx }))
    }

    /// Puts every connection left in state `syncing` and last checked before `before` (or
    /// never) back to `connecting`, so it is due again: a sync that stopped with its process
    /// left it so. Returns how many.
    pub async fn reset_stale_syncs(&self, before: OffsetDateTime) -> Result<u64, DbError> {
        let done = sqlx::query(
            "update library_sources set state = 'connecting' \
             where state = 'syncing' and (last_checked_at is null or last_checked_at < $1)",
        )
        .bind(before)
        .execute(self.pool())
        .await?;
        Ok(done.rows_affected())
    }

    /// Ends the connection of `library`: removes its items and its connection, keeping every
    /// node, and returns the ids of the secrets it used, which the caller deletes. Empty if it
    /// had no connection.
    pub async fn disconnect_source(&self, library: NodeId) -> Result<Vec<SecretId>, DbError> {
        let mut tx = self.pool().begin().await?;
        let row: Option<(Option<Uuid>, Option<Uuid>)> = sqlx::query_as(
            "select token_secret, webhook_secret from library_sources \
             where library_id = $1 for update",
        )
        .bind(library.0)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((token, webhook)) = row else {
            return Ok(Vec::new());
        };
        sqlx::query("delete from source_items where library_id = $1")
            .bind(library.0)
            .execute(&mut *tx)
            .await?;
        sqlx::query("delete from library_sources where library_id = $1")
            .bind(library.0)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok([token, webhook]
            .into_iter()
            .flatten()
            .map(SecretId)
            .collect())
    }
}
