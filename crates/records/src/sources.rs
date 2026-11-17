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
use crate::quotas::recompute_site_usage;
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

/// The result of conditionally moving a worker-selected source to `syncing`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceSyncTransition {
    /// The source was still eligible under the generation the worker read and now is syncing.
    Started,
    /// The source generation moved after the worker read it.
    Superseded,
    /// The source no longer exists or is no longer eligible for an automatic sync.
    Ineligible,
}

/// What [`Db::change_source`] changes; `None` leaves a field alone. For the two secrets,
/// `Some(None)` takes the secret away. Changing the branch or folder clears the sync bookmark
/// (`last_commit` and `etag`), as what was mirrored no longer says where the new place stands.
/// The secrets replaced or removed are the caller's to delete: [`Db::change_source`] answers
/// them.
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
    /// The collision-adjusted path displayed in the library.
    pub path: String,
    /// The original path in the repository, below the mirrored folder. This is its identity.
    pub repository_path: String,
    /// The Git blob of a file; `None` for a folder.
    pub blob_sha: Option<String>,
    pub size: i64,
}

/// Where a tracked source item's node actually is. A missing node has no name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceItemLocation {
    pub node: NodeId,
    pub parent: Option<NodeId>,
    pub name: Option<String>,
    pub deleted: bool,
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

/// `from … where …` for the connections whose library is live in a live site that is not
/// archived and whose retry time (`$1` is now) has passed or is unset; the caller adds more
/// conditions with `and`.
const SYNCABLE: &str = "from library_sources s \
    join nodes n on n.id = s.library_id \
    join nodes site on site.id = n.site_id \
    left join sites st on st.id = n.site_id \
    where n.deleted_at is null and site.deleted_at is null and st.archived_at is null \
      and (s.retry_at is null or s.retry_at <= $1)";

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
        let node: Option<(String, Option<OffsetDateTime>, Uuid)> =
            sqlx::query_as("select kind, deleted_at, site_id from nodes where id = $1 for update")
                .bind(s.library.0)
                .fetch_optional(&mut *tx)
                .await?;
        let site = match node {
            Some((kind, None, site)) if kind == "library" => NodeId(site),
            _ => return Err(AccessError::NotFound),
        };
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
        recompute_site_usage(&mut tx, site).await?;
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
    ///
    /// One statement: the walk up to the library (as [`Db::library_of`]) and its connection.
    pub async fn source_library_of(&self, node: NodeId) -> Result<Option<NodeId>, DbError> {
        let found: Option<Uuid> = sqlx::query_scalar(
            "with recursive up(id, parent_id, kind, depth) as ( \
                 select id, parent_id, kind, 0 from nodes where id = $1 \
                 union all \
                 select n.id, n.parent_id, n.kind, up.depth + 1 \
                 from nodes n join up on n.id = up.parent_id \
                 where up.kind <> 'library' and up.depth < 10000 \
             ) select up.id from up join library_sources s on s.library_id = up.id \
               where up.kind = 'library' limit 1",
        )
        .bind(node.0)
        .fetch_optional(self.pool())
        .await?;
        Ok(found.map(NodeId))
    }

    /// Writes the status of the connection of `library`; every field is written as given. A
    /// `last_error` is cut to 300 characters. Nothing happens if the library has no connection.
    pub async fn set_source_status(
        &self,
        library: NodeId,
        u: &StatusUpdate,
    ) -> Result<(), DbError> {
        self.write_status(library, u, None).await?;
        Ok(())
    }

    /// [`Db::set_source_status`] only while the connection's generation is still `generation`
    /// (see [`Db::source_generation`]): whether it was written.
    pub async fn set_source_status_if(
        &self,
        library: NodeId,
        u: &StatusUpdate,
        generation: i64,
    ) -> Result<bool, DbError> {
        self.write_status(library, u, Some(generation)).await
    }

    async fn write_status(
        &self,
        library: NodeId,
        u: &StatusUpdate,
        generation: Option<i64>,
    ) -> Result<bool, DbError> {
        let error: Option<String> = u
            .last_error
            .as_ref()
            .map(|e| e.chars().take(MAX_SOURCE_ERROR_CHARS).collect());
        let done = sqlx::query(
            "update library_sources set state = $2, last_commit = $3, etag = $4, \
                 last_checked_at = $5, last_synced_at = $6, last_error = $7, retry_at = $8, \
                 files = $9, bytes = $10, hidden = $11 \
             where library_id = $1 and ($12::bigint is null or generation = $12)",
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
        .bind(generation)
        .execute(self.pool())
        .await?;
        Ok(done.rows_affected() > 0)
    }

    /// The generation of the connection of `library`, if it has one: moved on by every
    /// [`Db::change_source`] and [`Db::bump_source_generation`]. A sync reads it as it starts
    /// and records its result only if it has not moved (see [`Db::set_source_status_if`]).
    pub async fn source_generation(&self, library: NodeId) -> Result<Option<i64>, DbError> {
        Ok(
            sqlx::query_scalar("select generation from library_sources where library_id = $1")
                .bind(library.0)
                .fetch_optional(self.pool())
                .await?,
        )
    }

    /// Moves the generation of the connection of `library` on, so a sync under way records
    /// nothing of its result; whether there is a connection.
    pub async fn bump_source_generation(&self, library: NodeId) -> Result<bool, DbError> {
        let done = sqlx::query(
            "update library_sources set generation = generation + 1 where library_id = $1",
        )
        .bind(library.0)
        .execute(self.pool())
        .await?;
        Ok(done.rows_affected() > 0)
    }

    /// Marks the connection of `library` as syncing, checked at `now`, if its generation is
    /// still `generation`; whether it was.
    pub async fn mark_source_syncing(
        &self,
        library: NodeId,
        now: OffsetDateTime,
        generation: i64,
    ) -> Result<bool, DbError> {
        let done = sqlx::query(
            "update library_sources set state = 'syncing', last_checked_at = $2 \
             where library_id = $1 and generation = $3",
        )
        .bind(library.0)
        .bind(now)
        .bind(generation)
        .execute(self.pool())
        .await?;
        Ok(done.rows_affected() > 0)
    }

    /// Moves `library` to `syncing` for a worker only if the generation and every automatic
    /// eligibility predicate still hold in this statement.
    pub async fn mark_source_syncing_if_eligible(
        &self,
        library: NodeId,
        now: OffsetDateTime,
        generation: i64,
    ) -> Result<SourceSyncTransition, DbError> {
        let result: i32 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "with updated as (\
                 update library_sources set state = 'syncing', last_checked_at = $1 \
                  where library_id = $2 and generation = $3 \
                    and exists(select 1 {SYNCABLE} \
                         and s.library_id = $2 \
                         and s.state in ('connecting', 'ok', 'rate_limited', 'failed')) \
                  returning 1\
             ) \
             select case \
                 when exists(select 1 from updated) then 1 \
                 when exists(select 1 from library_sources \
                              where library_id = $2 and generation <> $3) then -1 \
                 else 0 \
             end"
        )))
        .bind(now)
        .bind(library.0)
        .bind(generation)
        .fetch_one(self.pool())
        .await?;
        Ok(match result {
            1 => SourceSyncTransition::Started,
            -1 => SourceSyncTransition::Superseded,
            _ => SourceSyncTransition::Ineligible,
        })
    }

    /// Makes the connection of `library` due at once: state `connecting`, never checked, no
    /// retry time or error. The bookmark stays, except that `etag`, when given, replaces the
    /// recorded one (a marker that the library may be half way between two commits). Nothing
    /// happens if the library has no connection.
    pub async fn requeue_source(&self, library: NodeId, etag: Option<&str>) -> Result<(), DbError> {
        sqlx::query(
            "update library_sources set state = 'connecting', last_checked_at = null, \
                 retry_at = null, last_error = null, etag = coalesce($2, etag) \
             where library_id = $1",
        )
        .bind(library.0)
        .bind(etag)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Makes a source due unless it is under an active rate limit. Poll cadence and webhook
    /// secret changes use this: neither changes what GitHub request is authorized or addressed,
    /// so waking before `retry_at` would only repeat a request GitHub has already refused.
    pub async fn requeue_source_preserving_rate_limit(
        &self,
        library: NodeId,
        etag: Option<&str>,
        now: OffsetDateTime,
    ) -> Result<(), DbError> {
        sqlx::query(
            "update library_sources set \
                 state = case when state = 'rate_limited' and retry_at > $3 \
                              then state else 'connecting' end, \
                 last_checked_at = case when state = 'rate_limited' and retry_at > $3 \
                                        then last_checked_at else null end, \
                 retry_at = case when state = 'rate_limited' and retry_at > $3 \
                                 then retry_at else null end, \
                 last_error = case when state = 'rate_limited' and retry_at > $3 \
                                    then last_error else null end, \
                 etag = coalesce($2, etag) where library_id = $1",
        )
        .bind(library.0)
        .bind(etag)
        .bind(now)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Records a rate limit independently of a sync generation: the retry time can only move
    /// later, and `etag`, when given, marks a possibly half-applied sync. This is used when an
    /// owner changed or requeued the connection while the rate-limited request was in flight.
    pub async fn rate_limit_source(
        &self,
        library: NodeId,
        until: OffsetDateTime,
        error: &str,
        etag: Option<&str>,
    ) -> Result<(), DbError> {
        let error: String = error.chars().take(MAX_SOURCE_ERROR_CHARS).collect();
        sqlx::query(
            "update library_sources set state = 'rate_limited', \
                 retry_at = greatest(coalesce(retry_at, $2), $2), last_error = $3, \
                 etag = coalesce($4, etag) where library_id = $1",
        )
        .bind(library.0)
        .bind(until)
        .bind(error)
        .bind(etag)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Changes the connection of `library` as `c` says. `NotFound` if it has none;
    /// `InvalidSetting` for a poll time outside 60 to 86 400 seconds. A changed branch or
    /// folder clears `last_commit` and `etag`. Every change moves the generation on (see
    /// [`Db::source_generation`]). Answers the secrets the change replaced or removed, read
    /// under the connection's row lock, for the caller to delete: of two changes made at once
    /// each answers what it displaced, so none is left behind.
    pub async fn change_source(
        &self,
        library: NodeId,
        c: &SourceChange,
    ) -> Result<Vec<SecretId>, AccessError> {
        if c.poll_seconds.is_some_and(|s| !poll_ok(s)) {
            return Err(AccessError::InvalidSetting);
        }
        let poll = c
            .poll_seconds
            .map(|s| i32::try_from(s).map_err(|_| AccessError::InvalidSetting))
            .transpose()?;
        let mut tx = self.pool().begin().await?;
        type Current = (String, String, Option<Uuid>, Option<Uuid>);
        let current: Option<Current> = sqlx::query_as(
            "select branch, folder, token_secret, webhook_secret from library_sources \
             where library_id = $1 for update",
        )
        .bind(library.0)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((branch, folder, token, webhook)) = current else {
            return Err(AccessError::NotFound);
        };
        let displaced = |old: Option<Uuid>, new: Option<Option<SecretId>>| match new {
            Some(new) if old != new.map(|id| id.0) => old.map(SecretId),
            _ => None,
        };
        let gone: Vec<SecretId> = [
            displaced(token, c.token),
            displaced(webhook, c.webhook_secret),
        ]
        .into_iter()
        .flatten()
        .collect();
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
                 etag = case when $9 then null else etag end, \
                 generation = generation + 1 \
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
        Ok(gone)
    }

    /// The items of `library`, by path.
    pub async fn source_items(&self, library: NodeId) -> Result<Vec<SourceItem>, DbError> {
        let rows = sqlx::query(
            "select node_id, path, repository_path, blob_sha, size from source_items \
             where library_id = $1 order by repository_path",
        )
        .bind(library.0)
        .fetch_all(self.pool())
        .await?;
        rows.iter()
            .map(|row| {
                Ok(SourceItem {
                    node: NodeId(row.try_get("node_id")?),
                    path: row.try_get("path")?,
                    repository_path: row.try_get("repository_path")?,
                    blob_sha: row.try_get("blob_sha")?,
                    size: row.try_get("size")?,
                })
            })
            .collect::<Result<_, sqlx::Error>>()
            .map_err(DbError::from)
    }

    /// The actual parent, name and liveness of every tracked node, in one read.
    pub async fn source_item_locations(
        &self,
        library: NodeId,
    ) -> Result<Vec<SourceItemLocation>, DbError> {
        let rows = sqlx::query(
            "select i.node_id, n.parent_id, n.name, \
                    coalesce(n.deleted_at is not null, true) as deleted \
             from source_items i left join nodes n on n.id = i.node_id \
             where i.library_id = $1",
        )
        .bind(library.0)
        .fetch_all(self.pool())
        .await?;
        rows.iter()
            .map(|row| {
                Ok(SourceItemLocation {
                    node: NodeId(row.try_get("node_id")?),
                    parent: row.try_get::<Option<Uuid>, _>("parent_id")?.map(NodeId),
                    name: row.try_get("name")?,
                    deleted: row.try_get("deleted")?,
                })
            })
            .collect::<Result<_, sqlx::Error>>()
            .map_err(DbError::from)
    }

    /// The nodes of the items of `library` that are live: not deleted (in the bin or purged)
    /// and not missing. Reads only.
    pub async fn live_source_nodes(&self, library: NodeId) -> Result<Vec<NodeId>, DbError> {
        let ids: Vec<Uuid> = sqlx::query_scalar(
            "select i.node_id from source_items i join nodes n on n.id = i.node_id \
             where i.library_id = $1 and n.deleted_at is null",
        )
        .bind(library.0)
        .fetch_all(self.pool())
        .await?;
        Ok(ids.into_iter().map(NodeId).collect())
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
                "insert into source_items \
                     (library_id, path, repository_path, node_id, blob_sha, size) \
                 values ($1, $2, $3, $4, $5, $6) \
                 on conflict (library_id, repository_path) do update set \
                     path = excluded.path, node_id = excluded.node_id, \
                     blob_sha = excluded.blob_sha, size = excluded.size",
            )
            .bind(library.0)
            .bind(&item.path)
            .bind(&item.repository_path)
            .bind(item.node.0)
            .bind(&item.blob_sha)
            .bind(item.size)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Atomically makes `item` the sole tracking row held by its node. Repository path is the
    /// identity; displayed path is only a collision-adjusted location.
    pub async fn replace_source_item(
        &self,
        library: NodeId,
        item: &SourceItem,
    ) -> Result<(), DbError> {
        let mut tx = self.pool().begin().await?;
        sqlx::query(
            "delete from source_items where library_id = $1 and node_id = $2 \
             and repository_path <> $3",
        )
        .bind(library.0)
        .bind(item.node.0)
        .bind(&item.repository_path)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "insert into source_items \
                 (library_id, path, repository_path, node_id, blob_sha, size) \
             values ($1, $2, $3, $4, $5, $6) \
             on conflict (library_id, repository_path) do update set \
                 path = excluded.path, node_id = excluded.node_id, \
                 blob_sha = excluded.blob_sha, size = excluded.size",
        )
        .bind(library.0)
        .bind(&item.path)
        .bind(&item.repository_path)
        .bind(item.node.0)
        .bind(&item.blob_sha)
        .bind(item.size)
        .execute(&mut *tx)
        .await?;
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
    /// A library in the recycle bin, or in a site that is in the bin or archived, is never due.
    pub async fn due_sources(
        &self,
        now: OffsetDateTime,
        limit: i64,
    ) -> Result<Vec<NodeId>, DbError> {
        let ids: Vec<Uuid> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "select s.library_id {SYNCABLE} \
               and s.state in ('connecting', 'ok', 'rate_limited', 'failed') \
               and (s.last_checked_at is null \
                    or s.last_checked_at + make_interval(secs => s.poll_seconds) <= $1) \
             order by s.last_checked_at asc nulls first, s.library_id \
             limit $2"
        )))
        .bind(now)
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        Ok(ids.into_iter().map(NodeId).collect())
    }

    /// Those of `libraries` that may be synced at `now` without waiting for their poll time
    /// (a wake: a webhook delivery, a change): as [`Db::due_sources`] judges, but also in state
    /// `syncing` (a sync asked for while one runs is tried again after it) and whatever the time
    /// since the last check. A connection that stopped (`not_connected`) or waits for a retry
    /// time still waits.
    pub async fn wakeable_sources(
        &self,
        now: OffsetDateTime,
        libraries: &[NodeId],
    ) -> Result<Vec<NodeId>, DbError> {
        if libraries.is_empty() {
            return Ok(Vec::new());
        }
        let wanted: Vec<Uuid> = libraries.iter().map(|l| l.0).collect();
        let ids: Vec<Uuid> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "select s.library_id {SYNCABLE} \
               and s.state in ('connecting', 'ok', 'rate_limited', 'failed', 'syncing') \
               and s.library_id = any($2) \
             order by s.library_id"
        )))
        .bind(now)
        .bind(&wanted)
        .fetch_all(self.pool())
        .await?;
        Ok(ids.into_iter().map(NodeId).collect())
    }

    /// Whether `library` is eligible to begin a sync at `now`, checked in one statement: its
    /// library and site are live, the site is not archived, its retry time has passed, and its
    /// state is one the worker may run. `rate_limited` becomes eligible when that retry passes.
    pub async fn source_is_syncable(
        &self,
        library: NodeId,
        now: OffsetDateTime,
    ) -> Result<bool, DbError> {
        let eligible: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "select exists(select 1 {SYNCABLE} \
               and s.library_id = $2 \
               and s.state in ('connecting', 'ok', 'rate_limited', 'failed'))"
        )))
        .bind(now)
        .bind(library.0)
        .fetch_one(self.pool())
        .await?;
        Ok(eligible)
    }

    /// Whether `library` is live and waiting for another sync to finish. Such a wake is retried
    /// soon; an archived, deleted, stopped or backed-off source is not.
    pub async fn source_sync_is_busy(
        &self,
        library: NodeId,
        now: OffsetDateTime,
    ) -> Result<bool, DbError> {
        let busy: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "select exists(select 1 {SYNCABLE} \
               and s.library_id = $2 and s.state = 'syncing')"
        )))
        .bind(now)
        .bind(library.0)
        .fetch_one(self.pool())
        .await?;
        Ok(busy)
    }

    /// Sets the retry time of the connection of `library` to `until`, if its last sync failed
    /// (state `failed`) and nothing changed it since; whether it did.
    pub async fn back_off_source(
        &self,
        library: NodeId,
        until: OffsetDateTime,
    ) -> Result<bool, DbError> {
        let done = sqlx::query(
            "update library_sources set retry_at = $2 \
             where library_id = $1 and state = 'failed'",
        )
        .bind(library.0)
        .bind(until)
        .execute(self.pool())
        .await?;
        Ok(done.rows_affected() > 0)
    }

    /// Replaces the recorded `etag` of the connection of `library` with `etag` (a marker that
    /// the library may be about to be half way between two commits), leaving everything else.
    pub async fn mark_source_etag(&self, library: NodeId, etag: &str) -> Result<(), DbError> {
        sqlx::query("update library_sources set etag = $2 where library_id = $1")
            .bind(library.0)
            .bind(etag)
            .execute(self.pool())
            .await?;
        Ok(())
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

    /// Puts each connection left in state `syncing` and last checked before `before` (or never)
    /// back to `connecting` when its source advisory lock can be taken, so it is due again: a
    /// sync that stopped with its process left it so. A source still holding its lock is skipped.
    /// Returns how many.
    pub async fn reset_stale_syncs(&self, before: OffsetDateTime) -> Result<u64, DbError> {
        let stale: Vec<Uuid> = sqlx::query_scalar(
            "select library_id from library_sources \
             where state = 'syncing' and (last_checked_at is null or last_checked_at < $1) \
             order by library_id",
        )
        .bind(before)
        .fetch_all(self.pool())
        .await?;
        let mut reset = 0_u64;
        for library in stale {
            let mut tx = self.pool().begin().await?;
            let got: bool = sqlx::query_scalar(
                "select pg_try_advisory_xact_lock(\
                    hashtextextended('source-sync:' || $1::text, 0))",
            )
            .bind(library)
            .fetch_one(&mut *tx)
            .await?;
            if !got {
                tx.rollback().await?;
                continue;
            }
            let done = sqlx::query(
                "update library_sources set state = 'connecting' \
                 where library_id = $2 and state = 'syncing' \
                   and (last_checked_at is null or last_checked_at < $1)",
            )
            .bind(before)
            .bind(library)
            .execute(&mut *tx)
            .await?;
            reset += done.rows_affected();
            tx.commit().await?;
        }
        Ok(reset)
    }

    /// Ends the connection of `library`: removes its items and its connection, keeping every
    /// node, and returns the ids of the secrets it used, which the caller deletes. Empty if it
    /// had no connection.
    pub async fn disconnect_source(&self, library: NodeId) -> Result<Vec<SecretId>, DbError> {
        let mut tx = self.pool().begin().await?;
        let row: Option<(Option<Uuid>, Option<Uuid>, Uuid)> = sqlx::query_as(
            "select s.token_secret, s.webhook_secret, n.site_id from library_sources s \
             join nodes n on n.id = s.library_id where s.library_id = $1 for update of s",
        )
        .bind(library.0)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((token, webhook, site)) = row else {
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
        recompute_site_usage(&mut tx, NodeId(site)).await?;
        tx.commit().await?;
        Ok([token, webhook]
            .into_iter()
            .flatten()
            .map(SecretId)
            .collect())
    }
}
