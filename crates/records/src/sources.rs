//! Connected libraries: a library that mirrors a branch of a repository. The records here are
//! the connection (`library_sources`), which node holds which repository path (`source_items`)
//! and the sealed secrets the connection uses (`secrets`). The mirroring itself is the
//! server's; PostgreSQL holds only what it needs to resume.
//!
//! A secret is stored as ciphertext only. The key that seals it lives outside the database.

use collab_core::ids::{DocId, NodeId, UserId};
use serde_json::json;
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Postgres, QueryBuilder, Row};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::events::{NewEvent, record};
use crate::libraries::ensure_library_row;
use crate::quotas::recompute_site_usage;
use crate::s3_endpoints::{clean_prefix, prefixes_overlap, s3_location, valid_bucket};
use crate::{AccessError, Db, DbError, Editor, SharePath};

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

/// A stored secret: sealed bytes, or a note that an outside provider keeps the value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoredSecret {
    /// Sealed under this server's key, kept here.
    Local(SealedRow),
    /// Kept by the outside provider, under a name made from the secret's id.
    External,
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
    Share,
    S3,
}

impl SourceKind {
    /// The word for the kind, as stored.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            SourceKind::Github => "github",
            SourceKind::Share => "share",
            SourceKind::S3 => "s3",
        }
    }

    /// The kind named `word`.
    #[must_use]
    pub fn parse(word: &str) -> Option<SourceKind> {
        match word {
            "github" => Some(SourceKind::Github),
            "share" => Some(SourceKind::Share),
            "s3" => Some(SourceKind::S3),
            _ => None,
        }
    }
}

/// Whether a file-share source may be changed through ShareCraft.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceMode {
    ReadWrite,
    ReadOnly,
}

impl SourceMode {
    /// The word for the mode, as stored.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            SourceMode::ReadWrite => "read_write",
            SourceMode::ReadOnly => "read_only",
        }
    }

    /// The mode named `word`.
    #[must_use]
    pub fn parse(word: &str) -> Option<SourceMode> {
        match word {
            "read_write" => Some(SourceMode::ReadWrite),
            "read_only" => Some(SourceMode::ReadOnly),
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
    /// The approved root of a file-share source; absent for GitHub.
    pub location: Option<String>,
    pub mode: SourceMode,
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
    pub location: Option<String>,
    pub mode: SourceMode,
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
    /// Source-specific detail behind `hidden`; empty for sources without detail.
    pub hidden_detail: serde_json::Value,
    /// The next scheduled source check, when the source has its own schedule.
    pub next_check_at: Option<OffsetDateTime>,
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

/// A file-share listing entry to attach to an existing tree node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExternalEntry {
    pub node: NodeId,
    pub path: String,
    pub repository_path: String,
    pub size: u64,
    pub mtime_ns: i64,
    /// The entry's own change tag; `None` stores [`external_tag`] of `size` and `mtime_ns`.
    pub tag: Option<String>,
}

/// Why the bytes of an external item cannot currently be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unavailable {
    Denied,
    Gone,
    TooLarge,
    Locked,
    /// The object is archived (offline) in the store and must be restored first.
    Archived,
}

/// Whether versioning is on in an S3 bucket, as probed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Versioning {
    Enabled,
    Suspended,
    None,
    Unknown,
}

impl Versioning {
    /// The word for the state, as stored.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Versioning::Enabled => "enabled",
            Versioning::Suspended => "suspended",
            Versioning::None => "none",
            Versioning::Unknown => "unknown",
        }
    }

    /// The state named `word`.
    #[must_use]
    pub fn parse(word: &str) -> Option<Versioning> {
        match word {
            "enabled" => Some(Versioning::Enabled),
            "suspended" => Some(Versioning::Suspended),
            "none" => Some(Versioning::None),
            "unknown" => Some(Versioning::Unknown),
            _ => None,
        }
    }
}

/// A yes, a no, or not known yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tri {
    Yes,
    No,
    Unknown,
}

impl Tri {
    /// The word for the answer, as stored.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Tri::Yes => "yes",
            Tri::No => "no",
            Tri::Unknown => "unknown",
        }
    }

    /// The answer named `word`.
    #[must_use]
    pub fn parse(word: &str) -> Option<Tri> {
        match word {
            "yes" => Some(Tri::Yes),
            "no" => Some(Tri::No),
            "unknown" => Some(Tri::Unknown),
            _ => None,
        }
    }
}

/// Where the bucket of an S3-backed library is, and what was learned about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct S3Place {
    /// The name of an approved endpoint.
    pub endpoint: String,
    pub bucket: String,
    /// Empty, or ends in `/`.
    pub prefix: String,
    pub access_key_id: String,
    pub versioning: Versioning,
    /// Whether the store honours conditional requests.
    pub conditional: Tri,
}

/// Requests and bytes counted against an S3 library in one month.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub list: u64,
    pub read: u64,
    pub write: u64,
    pub bytes_down: u64,
    pub bytes_up: u64,
    pub cache_hits: u64,
}

/// What an S3 library may use in a month.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budget {
    pub list_requests: u64,
    pub read_requests: u64,
    pub bytes_down: Option<u64>,
}

/// What a change tag says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TagParts {
    pub mtime_ns: i64,
    /// The first 16 hex digits of the SHA-256 of the object's ETag; absent in a file-share tag.
    pub etag: Option<String>,
    pub archived: bool,
}

/// The change tag of an S3 object: `x{size}-{ns}~{16 hex}` and `!a` when it is archived. The
/// 16 hex digits are the start of the SHA-256 of the ETag exactly as received.
#[must_use]
pub fn object_tag(size: u64, modified_s: i64, etag: &str, archived: bool) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    let digest = Sha256::digest(etag.as_bytes());
    let mut tag = format!("x{size}-{}~", modified_s.saturating_mul(1_000_000_000));
    for byte in digest.iter().take(8) {
        let _ = write!(tag, "{byte:02x}");
    }
    if archived {
        tag.push_str("!a");
    }
    tag
}

/// Reads a change tag of either form (`x5-1700…` and `x5-1700…~ab12…!a`) for an item of `size`
/// bytes. `None` when it is neither.
#[must_use]
pub fn external_tag_parts(tag: &str, size: u64) -> Option<TagParts> {
    let rest = tag
        .strip_prefix('x')?
        .strip_prefix(size.to_string().as_str())?
        .strip_prefix('-')?;
    let (rest, archived) = match rest.strip_suffix("!a") {
        Some(rest) => (rest, true),
        None => (rest, false),
    };
    let (time, etag) = match rest.split_once('~') {
        Some((time, digest)) => {
            let hex = digest.len() == 16
                && digest
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
            if !hex {
                return None;
            }
            (time, Some(digest.to_owned()))
        }
        None => (rest, None),
    };
    if archived && etag.is_none() {
        return None;
    }
    Some(TagParts {
        mtime_ns: time.parse().ok()?,
        etag,
        archived,
    })
}

/// Writes every source row belonging to a tree batch through the tree projection's existing
/// transaction. Folders use `items`; external documents use `external` so their revision and
/// modified time are established atomically with their node and document rows.
pub(crate) async fn record_source_items_with(
    conn: &mut PgConnection,
    library: NodeId,
    items: &[SourceItem],
    external: &[ExternalEntry],
) -> Result<(), DbError> {
    if !items.is_empty() {
        let mut query = QueryBuilder::<Postgres>::new(
            "insert into source_items \
             (library_id, path, repository_path, node_id, blob_sha, size) ",
        );
        query.push_values(items, |mut values, item| {
            values
                .push_bind(library.0)
                .push_bind(&item.path)
                .push_bind(&item.repository_path)
                .push_bind(item.node.0)
                .push_bind(&item.blob_sha)
                .push_bind(item.size);
        });
        query.build().execute(&mut *conn).await?;
    }
    if !external.is_empty() {
        let values: Vec<_> = external
            .iter()
            .map(|entry| {
                let size = i64::try_from(entry.size).map_err(DbError::new)?;
                let modified =
                    OffsetDateTime::from_unix_timestamp_nanos(i128::from(entry.mtime_ns))
                        .map_err(DbError::new)?;
                let tag = entry_tag(entry);
                let unavailable = tag_unavailable(&tag, entry.size);
                Ok((
                    entry,
                    tag,
                    size,
                    new_external_revision(),
                    modified,
                    unavailable,
                ))
            })
            .collect::<Result<Vec<_>, DbError>>()?;
        let mut query = QueryBuilder::<Postgres>::new(
            "insert into source_items \
             (library_id, path, repository_path, node_id, blob_sha, size, revision, modified_at, \
              unavailable) ",
        );
        query.push_values(
            &values,
            |mut row, (entry, tag, size, revision, modified, unavailable)| {
                row.push_bind(library.0)
                    .push_bind(&entry.path)
                    .push_bind(&entry.repository_path)
                    .push_bind(entry.node.0)
                    .push_bind(tag)
                    .push_bind(*size)
                    .push_bind(revision)
                    .push_bind(*modified)
                    .push_bind(*unavailable);
            },
        );
        query.build().execute(&mut *conn).await?;
    }
    Ok(())
}

impl Unavailable {
    fn as_str(self) -> &'static str {
        match self {
            Unavailable::Denied => "denied",
            Unavailable::Gone => "gone",
            Unavailable::TooLarge => "too_large",
            Unavailable::Locked => "locked",
            Unavailable::Archived => "archived",
        }
    }

    fn parse(word: &str) -> Option<Unavailable> {
        match word {
            "denied" => Some(Unavailable::Denied),
            "gone" => Some(Unavailable::Gone),
            "too_large" => Some(Unavailable::TooLarge),
            "locked" => Some(Unavailable::Locked),
            "archived" => Some(Unavailable::Archived),
            _ => None,
        }
    }
}

/// A live document backed by a file-share or S3 source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExternalItem {
    pub library: NodeId,
    pub node: NodeId,
    /// The kind of source that holds it: a file share or S3.
    pub kind: SourceKind,
    /// The stored location text of the source.
    pub location: String,
    pub repository_path: String,
    pub size: u64,
    pub mtime_ns: i64,
    pub tag: String,
    pub revision: String,
    pub sha256: Option<[u8; 32]>,
    pub unavailable: Option<Unavailable>,
    /// The object version the item was last read at, where the store keeps versions.
    pub version: Option<String>,
}

/// The compact change-detection tag stored in `source_items.blob_sha`.
#[must_use]
pub fn external_tag(size: u64, mtime_ns: i64) -> String {
    format!("x{size}-{mtime_ns}")
}

/// A random, lower-case 40-hex revision id for pages and file sessions.
#[must_use]
pub fn new_external_revision() -> String {
    let random = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    random.chars().take(40).collect()
}

/// The turn to sync one library (see [`Db::try_lock_source`]); dropping it gives the turn up.
pub struct SourceLock {
    _tx: sqlx::Transaction<'static, sqlx::Postgres>,
}

/// The organisation-wide turn that serialises approved-root changes with file-share
/// connection commits. Dropping it rolls its otherwise empty transaction back and releases
/// the PostgreSQL advisory lock.
pub struct ShareConnectionLock {
    tx: sqlx::Transaction<'static, sqlx::Postgres>,
}

impl std::fmt::Debug for ShareConnectionLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ShareConnectionLock")
    }
}

impl std::fmt::Debug for SourceLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SourceLock")
    }
}

const SOURCE_COLUMNS: &str = "library_id, kind, owner, repo, branch, folder, location, mode, \
    token_secret, webhook_secret, poll_seconds, connected_by, state, last_commit, etag, \
    last_checked_at, last_synced_at, last_error, retry_at, files, bytes, hidden, hidden_detail, \
    next_check_at";

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
    let mode: String = row.try_get("mode")?;
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
        location: row.try_get("location")?,
        mode: SourceMode::parse(&mode).ok_or_else(|| decode("source mode", &mode))?,
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
        hidden_detail: row.try_get("hidden_detail")?,
        next_check_at: row.try_get("next_check_at")?,
    };
    Ok((config, status))
}

fn poll_ok(seconds: u32) -> bool {
    (MIN_POLL_SECONDS..=MAX_POLL_SECONDS).contains(&seconds)
}

/// A [`SourceItem`] from a row of `node_id, path, repository_path, blob_sha, size`.
pub(crate) fn item_from_row(row: &PgRow) -> Result<SourceItem, sqlx::Error> {
    Ok(SourceItem {
        node: NodeId(row.try_get("node_id")?),
        path: row.try_get("path")?,
        repository_path: row.try_get("repository_path")?,
        blob_sha: row.try_get("blob_sha")?,
        size: row.try_get("size")?,
    })
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

fn encode_sha256(bytes: &[u8; 32]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

fn decode_sha256(hex: &str) -> Result<[u8; 32], sqlx::Error> {
    if hex.len() != 64 {
        return Err(decode("SHA-256", hex));
    }
    let bytes = hex
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let digits =
                std::str::from_utf8(pair).map_err(|error| sqlx::Error::Decode(error.into()))?;
            u8::from_str_radix(digits, 16).map_err(|error| sqlx::Error::Decode(error.into()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    bytes.try_into().map_err(|_| decode("SHA-256", hex))
}

fn usage_from_row(row: &PgRow) -> Result<Usage, sqlx::Error> {
    let count = |column: &str| -> Result<u64, sqlx::Error> {
        let value: i64 = row.try_get(column)?;
        u64::try_from(value).map_err(|error| sqlx::Error::Decode(error.into()))
    };
    Ok(Usage {
        list: count("list_requests")?,
        read: count("read_requests")?,
        write: count("write_requests")?,
        bytes_down: count("bytes_down")?,
        bytes_up: count("bytes_up")?,
        cache_hits: count("cache_hits")?,
    })
}

/// The tag stored for `entry`: its own, else the one built from its size and time.
fn entry_tag(entry: &ExternalEntry) -> String {
    entry
        .tag
        .clone()
        .unwrap_or_else(|| external_tag(entry.size, entry.mtime_ns))
}

/// `Some("archived")` for the tag of an archived object.
fn tag_unavailable(tag: &str, size: u64) -> Option<&'static str> {
    external_tag_parts(tag, size)
        .is_some_and(|parts| parts.archived)
        .then_some("archived")
}

impl Db {
    /// Takes the organisation-wide file-share connection/settings turn, waiting at most `wait`
    /// for it. `None` when another holder kept it for longer: the caller answers with a
    /// retryable refusal instead of holding a pool connection indefinitely. The wait is enforced
    /// by PostgreSQL's `lock_timeout`, local to this turn's transaction.
    pub async fn lock_share_connections(
        &self,
        wait: std::time::Duration,
    ) -> Result<Option<ShareConnectionLock>, DbError> {
        let mut tx = self.pool().begin().await?;
        let millis = wait.as_millis().clamp(1, u128::from(u32::MAX));
        sqlx::query("select set_config('lock_timeout', $1, true)")
            .bind(format!("{millis}ms"))
            .execute(&mut *tx)
            .await?;
        match sqlx::query("select pg_advisory_xact_lock(hashtextextended('share-connection', 0))")
            .execute(&mut *tx)
            .await
        {
            Ok(_) => {}
            Err(sqlx::Error::Database(error)) if error.code().as_deref() == Some("55P03") => {
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        }
        // The timeout bounded only the wait for this turn; later statements on it are not
        // subject to it.
        sqlx::query("select set_config('lock_timeout', '0', true)")
            .execute(&mut *tx)
            .await?;
        Ok(Some(ShareConnectionLock { tx }))
    }

    /// The lexical locations of every connected share, read without the connection turn. A
    /// caller resolves them outside the turn and compares again under it with
    /// [`Db::share_locations_locked`].
    pub async fn share_locations(&self) -> Result<Vec<String>, DbError> {
        Ok(sqlx::query_scalar(
            "select location from library_sources \
             where kind = 'share' and location is not null",
        )
        .fetch_all(self.pool())
        .await?)
    }

    /// Every connected share's library with its lexical location, for waking the shares of
    /// one file server.
    pub async fn share_sources(&self) -> Result<Vec<(NodeId, String)>, DbError> {
        let rows: Vec<(Uuid, String)> = sqlx::query_as(
            "select library_id, location from library_sources \
             where kind = 'share' and location is not null order by library_id",
        )
        .fetch_all(self.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(|(library, location)| (NodeId(library), location))
            .collect())
    }

    /// The lexical locations of the other connected shares while this turn is held.
    pub async fn share_locations_locked(
        &self,
        lock: &mut ShareConnectionLock,
        except: NodeId,
    ) -> Result<Vec<String>, DbError> {
        Ok(sqlx::query_scalar(
            "select location from library_sources \
             where kind = 'share' and library_id <> $1 and location is not null",
        )
        .bind(except.0)
        .fetch_all(&mut *lock.tx)
        .await?)
    }

    /// Makes every connected share that stopped (`not_connected`, for example because its
    /// folder was no longer approved) due at once, as [`Db::requeue_source`] does, and returns
    /// their libraries so the caller can wake them. The approved roots changed: a stop may be over.
    pub async fn requeue_stopped_shares(&self) -> Result<Vec<NodeId>, DbError> {
        let ids: Vec<Uuid> = sqlx::query_scalar(
            "update library_sources set state = 'connecting', last_checked_at = null, \
                 next_check_at = null, retry_at = null, last_error = null \
             where kind = 'share' and state = 'not_connected' \
             returning library_id",
        )
        .fetch_all(self.pool())
        .await?;
        Ok(ids.into_iter().map(NodeId).collect())
    }

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

    /// The sealed secret `id`, if there is one and it is kept here. A secret kept by an outside
    /// provider is not held here: asking for it is an error (see [`Db::get_stored_secret`]).
    pub async fn get_secret(&self, id: SecretId) -> Result<Option<SealedRow>, DbError> {
        match self.get_stored_secret(id).await? {
            None => Ok(None),
            Some(StoredSecret::Local(sealed)) => Ok(Some(sealed)),
            Some(StoredSecret::External) => {
                Err(DbError::new("a secret is kept by an outside provider"))
            }
        }
    }

    /// The secret `id` as the table holds it: sealed bytes, or a note that an outside provider
    /// has the value. `None` when there is no such secret or it has been forgotten.
    pub async fn get_stored_secret(&self, id: SecretId) -> Result<Option<StoredSecret>, DbError> {
        let row = sqlx::query(
            "select provider, key_id, nonce, ciphertext from secrets \
             where id = $1 and forgotten_at is null",
        )
        .bind(id.0)
        .fetch_optional(self.pool())
        .await?;
        row.map(|row| {
            let provider: String = row.try_get("provider")?;
            if provider == "external" {
                return Ok::<_, sqlx::Error>(StoredSecret::External);
            }
            let key_id: Option<i32> = row.try_get("key_id")?;
            let nonce: Option<Vec<u8>> = row.try_get("nonce")?;
            let ciphertext: Option<Vec<u8>> = row.try_get("ciphertext")?;
            let (Some(key_id), Some(nonce), Some(ciphertext)) = (key_id, nonce, ciphertext) else {
                return Err(sqlx::Error::Decode("a sealed secret with no bytes".into()));
            };
            Ok(StoredSecret::Local(SealedRow {
                key_id: u32::try_from(key_id).map_err(|e| sqlx::Error::Decode(e.into()))?,
                nonce: nonce
                    .try_into()
                    .map_err(|_| sqlx::Error::Decode("a nonce of the wrong length".into()))?,
                ciphertext,
            }))
        })
        .transpose()
        .map_err(DbError::from)
    }

    /// Records a secret whose value an outside provider keeps, and returns its id: the name at
    /// the provider is made from it. The row holds no bytes.
    pub async fn put_external_secret(&self) -> Result<SecretId, DbError> {
        let id = Uuid::new_v4();
        sqlx::query("insert into secrets (id, provider) values ($1, 'external')")
            .bind(id)
            .execute(self.pool())
            .await?;
        Ok(SecretId(id))
    }

    /// Forgets the secret `id`: a sealed one is deleted at once (a secret a connection still
    /// uses cannot be, the database refuses); one kept by an outside provider is marked
    /// forgotten, for [`Db::forgotten_secrets`] to list until the provider has deleted its value
    /// and [`Db::delete_secret`] removes the row. Nothing happens if there is none.
    pub async fn forget_secret(&self, id: SecretId) -> Result<(), DbError> {
        sqlx::query("delete from secrets where id = $1 and provider = 'local'")
            .bind(id.0)
            .execute(self.pool())
            .await?;
        sqlx::query(
            "update secrets set forgotten_at = now() \
             where id = $1 and provider = 'external' and forgotten_at is null",
        )
        .bind(id.0)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Up to `limit` secrets that were forgotten but whose value the provider may still hold,
    /// the oldest first.
    pub async fn forgotten_secrets(&self, limit: i64) -> Result<Vec<SecretId>, DbError> {
        let ids: Vec<Uuid> = sqlx::query_scalar(
            "select id from secrets where forgotten_at is not null \
             order by forgotten_at, id limit $1",
        )
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        Ok(ids.into_iter().map(SecretId).collect())
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
        self.connect_source_inner(s, true, None).await
    }

    /// Connects a library to a bucket as [`Db::connect_source`] connects the other kinds, in
    /// one transaction that also records `place`. `s.kind` must be `S3`, its location must be
    /// [`s3_location`] of `place`, and its token the sealed secret key; the bucket must be a
    /// valid name and the prefix clean (`InvalidSetting` otherwise). `Exists` when another
    /// library holds the same endpoint and bucket under an overlapping prefix; the overlap is
    /// judged under an organisation-wide lock, so two connections made at once cannot both win.
    pub async fn connect_s3_source(
        &self,
        s: &NewSource,
        place: &S3Place,
    ) -> Result<(), AccessError> {
        if s.kind != SourceKind::S3 {
            return Err(AccessError::InvalidSetting);
        }
        self.connect_source_inner(s, true, Some(place)).await
    }

    /// Connects while the caller holds [`ShareConnectionLock`]. The caller has already checked
    /// canonical exclusivity; the lexical check remains as defence in depth.
    pub async fn connect_source_locked(
        &self,
        _lock: &mut ShareConnectionLock,
        s: &NewSource,
    ) -> Result<(), AccessError> {
        self.connect_source_inner(s, false, None).await
    }

    async fn connect_source_inner(
        &self,
        s: &NewSource,
        take_share_lock: bool,
        s3: Option<&S3Place>,
    ) -> Result<(), AccessError> {
        if !poll_ok(s.poll_seconds) {
            return Err(AccessError::InvalidSetting);
        }
        let share_location = match s.kind {
            SourceKind::Github | SourceKind::S3 => None,
            SourceKind::Share => Some(
                SharePath::parse(s.location.as_deref().ok_or(AccessError::InvalidSetting)?)
                    .map_err(|_| AccessError::InvalidSetting)?,
            ),
        };
        // An S3 connection is made only through `connect_s3_source`, with a place that
        // agrees with its location; no other kind has one.
        match (s.kind, s3) {
            (SourceKind::S3, Some(place)) => {
                let location = s3_location(&place.endpoint, &place.bucket, &place.prefix);
                if place.endpoint.is_empty()
                    || !valid_bucket(&place.bucket)
                    || clean_prefix(&place.prefix).as_deref() != Some(place.prefix.as_str())
                    || s.location.as_deref() != Some(location.as_str())
                    || s.token.is_none()
                {
                    return Err(AccessError::InvalidSetting);
                }
            }
            (SourceKind::S3, None) | (_, Some(_)) => return Err(AccessError::InvalidSetting),
            _ => {}
        }
        let mut tx = self.pool().begin().await?;
        if let Some(place) = s3 {
            sqlx::query("select pg_advisory_xact_lock(hashtextextended('s3-connection', 0))")
                .execute(&mut *tx)
                .await?;
            // The endpoint's approval is read again under the lock, and its setting row is
            // held `FOR SHARE` until the commit: a settings change that removes the endpoint
            // either commits first (and is seen here) or waits for this connection to commit.
            if !place.endpoint.starts_with("aws:") {
                let approved: Option<serde_json::Value> =
                    sqlx::query_scalar("select value from settings where key = $1 for share")
                        .bind(crate::s3_endpoints::S3_ENDPOINTS)
                        .fetch_optional(&mut *tx)
                        .await?;
                let named = approved
                    .as_ref()
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|entries| {
                        entries.iter().any(|entry| {
                            entry.get("name").and_then(serde_json::Value::as_str)
                                == Some(place.endpoint.as_str())
                        })
                    });
                if !named {
                    return Err(AccessError::S3EndpointNotApproved);
                }
            }
            let prefixes: Vec<String> = sqlx::query_scalar(
                "select prefix from source_s3 \
                 where endpoint = $1 and bucket = $2 and library_id <> $3",
            )
            .bind(&place.endpoint)
            .bind(&place.bucket)
            .bind(s.library.0)
            .fetch_all(&mut *tx)
            .await?;
            if prefixes
                .iter()
                .any(|other| prefixes_overlap(&place.prefix, other))
            {
                return Err(AccessError::Exists);
            }
        }
        if let Some(share_location) = &share_location {
            if take_share_lock {
                sqlx::query(
                    "select pg_advisory_xact_lock(hashtextextended('share-connection', 0))",
                )
                .execute(&mut *tx)
                .await?;
            }
            let locations: Vec<Option<String>> = sqlx::query_scalar(
                "select location from library_sources \
                 where kind = 'share' and library_id <> $1",
            )
            .bind(s.library.0)
            .fetch_all(&mut *tx)
            .await?;
            for location in locations {
                let location =
                    SharePath::parse(location.as_deref().ok_or(AccessError::InvalidSetting)?)
                        .map_err(|_| AccessError::InvalidSetting)?;
                if share_location.is_within(&location) || location.is_within(share_location) {
                    return Err(AccessError::ShareLocationInUse);
                }
            }
        }
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
        let (owner, repo, branch, folder) = match s.kind {
            SourceKind::Github => (
                s.owner.as_str(),
                s.repo.as_str(),
                s.branch.as_str(),
                s.folder.as_str(),
            ),
            SourceKind::Share | SourceKind::S3 => ("", "", "", ""),
        };
        sqlx::query(
            "insert into library_sources (library_id, kind, owner, repo, branch, folder, \
                 location, mode, token_secret, poll_seconds, connected_by) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
        )
        .bind(s.library.0)
        .bind(s.kind.as_str())
        .bind(owner)
        .bind(repo)
        .bind(branch)
        .bind(folder)
        .bind(&s.location)
        .bind(s.mode.as_str())
        .bind(s.token.map(|t| t.0))
        .bind(i32::try_from(s.poll_seconds).map_err(|_| AccessError::InvalidSetting)?)
        .bind(s.connected_by.0)
        .execute(&mut *tx)
        .await
        .map_err(refused)?;
        if let Some(place) = s3 {
            sqlx::query(
                "insert into source_s3 \
                     (library_id, endpoint, bucket, prefix, access_key_id, versioning, \
                      conditional) \
                 values ($1, $2, $3, $4, $5, $6, $7)",
            )
            .bind(s.library.0)
            .bind(&place.endpoint)
            .bind(&place.bucket)
            .bind(&place.prefix)
            .bind(&place.access_key_id)
            .bind(place.versioning.as_str())
            .bind(place.conditional.as_str())
            .execute(&mut *tx)
            .await
            .map_err(refused)?;
        }
        recompute_site_usage(&mut tx, site).await?;
        tx.commit().await?;
        Ok(())
    }

    /// How many libraries are connected through each S3 endpoint, by the endpoint's name.
    pub async fn s3_endpoints_in_use(&self) -> Result<Vec<(String, i64)>, DbError> {
        Ok(sqlx::query_as(
            "select endpoint, count(*) from source_s3 group by endpoint order by endpoint",
        )
        .fetch_all(self.pool())
        .await?)
    }

    /// Whether another library holds `bucket` at `endpoint` under a prefix overlapping
    /// `prefix`: the rule [`Db::connect_s3_source`] refuses with `Exists`, read without a lock
    /// so a route can refuse before it asks the store anything (the connect judges it again).
    pub async fn s3_place_taken(
        &self,
        endpoint: &str,
        bucket: &str,
        prefix: &str,
    ) -> Result<bool, DbError> {
        let prefixes: Vec<String> =
            sqlx::query_scalar("select prefix from source_s3 where endpoint = $1 and bucket = $2")
                .bind(endpoint)
                .bind(bucket)
                .fetch_all(self.pool())
                .await?;
        Ok(prefixes.iter().any(|other| prefixes_overlap(prefix, other)))
    }

    /// Where the bucket of the S3 library `library` is; `None` if it is not connected to one.
    pub async fn s3_place(&self, library: NodeId) -> Result<Option<S3Place>, DbError> {
        let row = sqlx::query(
            "select endpoint, bucket, prefix, access_key_id, versioning, conditional \
             from source_s3 where library_id = $1",
        )
        .bind(library.0)
        .fetch_optional(self.pool())
        .await?;
        row.map(|row| {
            let versioning: String = row.try_get("versioning")?;
            let conditional: String = row.try_get("conditional")?;
            Ok::<_, sqlx::Error>(S3Place {
                endpoint: row.try_get("endpoint")?,
                bucket: row.try_get("bucket")?,
                prefix: row.try_get("prefix")?,
                access_key_id: row.try_get("access_key_id")?,
                versioning: Versioning::parse(&versioning)
                    .ok_or_else(|| decode("versioning", &versioning))?,
                conditional: Tri::parse(&conditional)
                    .ok_or_else(|| decode("conditional support", &conditional))?,
            })
        })
        .transpose()
        .map_err(DbError::from)
    }

    /// Records what a probe learned about the bucket of `library`. Nothing happens if it is
    /// not an S3 library.
    pub async fn set_s3_probe(
        &self,
        library: NodeId,
        versioning: Versioning,
        conditional: Tri,
    ) -> Result<(), DbError> {
        sqlx::query("update source_s3 set versioning = $2, conditional = $3 where library_id = $1")
            .bind(library.0)
            .bind(versioning.as_str())
            .bind(conditional.as_str())
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// Replaces the access key of the S3 library `library`: the key id and the sealed secret
    /// together, in one transaction. Moves the generation on and clears `last_error` and the
    /// retry time, so a library that could not connect is tried again at once. Answers the
    /// secret it displaced, for the caller to delete. `NotFound` if `library` is not connected
    /// to a bucket; `InvalidSetting` for a key id the record refuses.
    pub async fn set_s3_key(
        &self,
        library: NodeId,
        access_key_id: &str,
        secret: SecretId,
    ) -> Result<Option<SecretId>, AccessError> {
        let mut tx = self.pool().begin().await?;
        let current: Option<(Option<Uuid>,)> = sqlx::query_as(
            "select s.token_secret from library_sources s \
             join source_s3 b on b.library_id = s.library_id \
             where s.library_id = $1 and s.kind = 's3' for update of s",
        )
        .bind(library.0)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((old,)) = current else {
            return Err(AccessError::NotFound);
        };
        sqlx::query("update source_s3 set access_key_id = $2 where library_id = $1")
            .bind(library.0)
            .bind(access_key_id)
            .execute(&mut *tx)
            .await
            .map_err(refused)?;
        sqlx::query(
            "update library_sources set token_secret = $2, generation = generation + 1, \
                 last_error = null, retry_at = null, last_checked_at = null, \
                 next_check_at = null, \
                 state = case when state = 'not_connected' then 'connecting' else state end \
             where library_id = $1",
        )
        .bind(library.0)
        .bind(secret.0)
        .execute(&mut *tx)
        .await?;
        // A denied mark came from the old key; the new key gets to try again.
        sqlx::query(
            "update source_items set unavailable = null \
             where library_id = $1 and unavailable = 'denied'",
        )
        .bind(library.0)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(old.filter(|old| *old != secret.0).map(SecretId))
    }

    /// Adds `delta` to the counts of `library` for the month of `month` and answers the
    /// month's totals after the addition. One statement: two additions made at once both
    /// count. The first request of a month makes its row.
    pub async fn add_source_usage(
        &self,
        library: NodeId,
        month: time::Date,
        delta: &Usage,
    ) -> Result<Usage, DbError> {
        let month = month.replace_day(1).map_err(DbError::new)?;
        let count = |n: u64| i64::try_from(n).map_err(DbError::new);
        let row = sqlx::query(
            "insert into source_usage \
                 (library_id, month, list_requests, read_requests, write_requests, \
                  bytes_down, bytes_up, cache_hits) \
             values ($1, $2, $3, $4, $5, $6, $7, $8) \
             on conflict (library_id, month) do update set \
                 list_requests = source_usage.list_requests + excluded.list_requests, \
                 read_requests = source_usage.read_requests + excluded.read_requests, \
                 write_requests = source_usage.write_requests + excluded.write_requests, \
                 bytes_down = source_usage.bytes_down + excluded.bytes_down, \
                 bytes_up = source_usage.bytes_up + excluded.bytes_up, \
                 cache_hits = source_usage.cache_hits + excluded.cache_hits \
             returning list_requests, read_requests, write_requests, bytes_down, bytes_up, \
                       cache_hits",
        )
        .bind(library.0)
        .bind(month)
        .bind(count(delta.list)?)
        .bind(count(delta.read)?)
        .bind(count(delta.write)?)
        .bind(count(delta.bytes_down)?)
        .bind(count(delta.bytes_up)?)
        .bind(count(delta.cache_hits)?)
        .fetch_one(self.pool())
        .await?;
        Ok(usage_from_row(&row)?)
    }

    /// The counts of `library` for the month of `month`; all zero when nothing was counted.
    pub async fn usage(&self, library: NodeId, month: time::Date) -> Result<Usage, DbError> {
        let month = month.replace_day(1).map_err(DbError::new)?;
        let row = sqlx::query(
            "select list_requests, read_requests, write_requests, bytes_down, bytes_up, \
                    cache_hits \
             from source_usage where library_id = $1 and month = $2",
        )
        .bind(library.0)
        .bind(month)
        .fetch_optional(self.pool())
        .await?;
        Ok(row
            .as_ref()
            .map(usage_from_row)
            .transpose()?
            .unwrap_or_default())
    }

    /// Marks the budget warning of the month as given; true the first time only.
    pub async fn mark_usage_warned(
        &self,
        library: NodeId,
        month: time::Date,
    ) -> Result<bool, DbError> {
        let month = month.replace_day(1).map_err(DbError::new)?;
        let done = sqlx::query(
            "insert into source_usage (library_id, month, warned) values ($1, $2, true) \
             on conflict (library_id, month) do update set warned = true \
             where not source_usage.warned",
        )
        .bind(library.0)
        .bind(month)
        .execute(self.pool())
        .await?;
        Ok(done.rows_affected() > 0)
    }

    /// What `library` may use in a month: its own budget where it has one, else the
    /// organisation's defaults (see [`Db::s3_settings`]).
    pub async fn budget(&self, library: NodeId) -> Result<Budget, DbError> {
        let own: Option<(Option<i64>, Option<i64>, Option<i64>)> = sqlx::query_as(
            "select budget_list, budget_read, budget_bytes from source_s3 where library_id = $1",
        )
        .bind(library.0)
        .fetch_optional(self.pool())
        .await?;
        let settings = self.s3_settings().await?;
        let (list, read, bytes) = own.unwrap_or_default();
        let own = |value: Option<i64>| value.and_then(|n| u64::try_from(n).ok());
        Ok(Budget {
            list_requests: own(list).unwrap_or(settings.list_requests),
            read_requests: own(read).unwrap_or(settings.read_requests),
            bytes_down: own(bytes).or(settings.bytes_down),
        })
    }

    /// Sets the budget of the S3 library `library`. `NotFound` if it is not connected to a
    /// bucket; `InvalidSetting` for a figure the record cannot hold.
    pub async fn set_budget(&self, library: NodeId, b: &Budget) -> Result<(), AccessError> {
        let count = |n: u64| i64::try_from(n).map_err(|_| AccessError::InvalidSetting);
        let done = sqlx::query(
            "update source_s3 set budget_list = $2, budget_read = $3, budget_bytes = $4 \
             where library_id = $1",
        )
        .bind(library.0)
        .bind(count(b.list_requests)?)
        .bind(count(b.read_requests)?)
        .bind(b.bytes_down.map(count).transpose()?)
        .execute(self.pool())
        .await?;
        if done.rows_affected() == 0 {
            return Err(AccessError::NotFound);
        }
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
                 files = $9, bytes = $10, hidden = $11, hidden_detail = $12, \
                 next_check_at = $13 \
             where library_id = $1 and ($14::bigint is null or generation = $14)",
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
        .bind(&u.hidden_detail)
        .bind(u.next_check_at)
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
                 next_check_at = null, retry_at = null, last_error = null, \
                 etag = coalesce($2, etag) \
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
                 next_check_at = case when state = 'rate_limited' and retry_at > $3 \
                                      then next_check_at else null end, \
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
                 last_checked_at = null, next_check_at = null, \
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

    /// Atomically records the file-share metadata for `e`. A new content tag receives a new
    /// external revision and forgets content learned for the old tag. If reconcile restored the
    /// node from Git after a crashed creation batch, its missing document row is restored as an
    /// ordinary external file in this transaction too.
    pub async fn record_external_item(
        &self,
        library: NodeId,
        e: &ExternalEntry,
    ) -> Result<(), DbError> {
        let size = i64::try_from(e.size).map_err(DbError::new)?;
        let modified = OffsetDateTime::from_unix_timestamp_nanos(i128::from(e.mtime_ns))
            .map_err(DbError::new)?;
        let tag = entry_tag(e);
        let archived = tag_unavailable(&tag, e.size);
        let revision = new_external_revision();
        let mut tx = self.pool().begin().await?;
        sqlx::query(
            "delete from source_items where library_id = $1 and node_id = $2 \
             and repository_path <> $3",
        )
        .bind(library.0)
        .bind(e.node.0)
        .bind(&e.repository_path)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "insert into source_items \
                 (library_id, path, repository_path, node_id, blob_sha, size, revision, \
                  modified_at, unavailable) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
             on conflict (library_id, repository_path) do update set \
                 path = excluded.path, node_id = excluded.node_id, size = excluded.size, \
                 modified_at = excluded.modified_at, \
                 revision = case when source_items.blob_sha is not distinct from excluded.blob_sha \
                                 then source_items.revision else excluded.revision end, \
                 content_sha256 = case when source_items.blob_sha is not distinct from excluded.blob_sha \
                                       then source_items.content_sha256 else null end, \
                 read_at = case when source_items.blob_sha is not distinct from excluded.blob_sha \
                                then source_items.read_at else null end, \
                 unavailable = coalesce(excluded.unavailable, \
                     case when source_items.blob_sha is not distinct from excluded.blob_sha \
                          then source_items.unavailable else null end), \
                 version_id = case when source_items.blob_sha is not distinct from excluded.blob_sha \
                                   then source_items.version_id else null end, \
                 blob_sha = excluded.blob_sha, missing_since = null",
        )
        .bind(library.0)
        .bind(&e.path)
        .bind(&e.repository_path)
        .bind(e.node.0)
        .bind(tag)
        .bind(size)
        .bind(revision)
        .bind(modified)
        .bind(archived)
        .execute(&mut *tx)
        .await?;
        let restored: Option<(String, Option<Uuid>)> = sqlx::query_as(
            "select name, created_by from nodes where id = $1 and kind = 'document' \
             and not exists (select 1 from documents where id = $1)",
        )
        .bind(e.node.0)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some((name, created_by)) = restored {
            let editor = Editor::for_file_name(&name);
            let inserted = sqlx::query(
                "insert into documents (id, name, editor, created_by) values ($1, $2, $3, $4) \
                 on conflict (id) do nothing",
            )
            .bind(e.node.0)
            .bind(&name)
            .bind(editor.id())
            .bind(created_by)
            .execute(&mut *tx)
            .await?;
            if inserted.rows_affected() == 1 {
                record(
                    &mut tx,
                    NewEvent {
                        kind: "document.created",
                        actor: created_by.map(UserId),
                        subject: None,
                        document: Some(DocId(e.node.0)),
                        node: Some(e.node),
                        detail: json!({
                            "name": name,
                            "editor": editor.id(),
                            "uploaded": true,
                            "bytes": e.size,
                        }),
                    },
                )
                .await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    /// The external metadata for a live share-backed document.
    pub async fn external_item(&self, node: NodeId) -> Result<Option<ExternalItem>, DbError> {
        let row = sqlx::query(
            "select i.library_id, i.node_id, s.kind, s.location, i.repository_path, i.size, \
                    i.blob_sha, i.revision, i.content_sha256, i.unavailable, i.version_id \
             from source_items i \
             join library_sources s on s.library_id = i.library_id \
                  and s.kind in ('share', 's3') \
             join nodes n on n.id = i.node_id \
             where i.node_id = $1 and n.kind = 'document' and n.deleted_at is null",
        )
        .bind(node.0)
        .fetch_optional(self.pool())
        .await?;
        row.map(|row| {
            let size: i64 = row.try_get("size")?;
            let size = u64::try_from(size).map_err(|error| sqlx::Error::Decode(error.into()))?;
            let tag: String = row.try_get("blob_sha")?;
            let kind: String = row.try_get("kind")?;
            let sha: Option<String> = row.try_get("content_sha256")?;
            let unavailable: Option<String> = row.try_get("unavailable")?;
            let parts =
                external_tag_parts(&tag, size).ok_or_else(|| decode("external tag", &tag))?;
            Ok::<_, sqlx::Error>(ExternalItem {
                library: NodeId(row.try_get("library_id")?),
                node: NodeId(row.try_get("node_id")?),
                kind: SourceKind::parse(&kind).ok_or_else(|| decode("source kind", &kind))?,
                location: row.try_get("location")?,
                repository_path: row.try_get("repository_path")?,
                size,
                mtime_ns: parts.mtime_ns,
                tag,
                version: row.try_get("version_id")?,
                revision: row.try_get("revision")?,
                sha256: sha.as_deref().map(decode_sha256).transpose()?,
                unavailable: unavailable
                    .as_deref()
                    .map(|word| {
                        Unavailable::parse(word)
                            .ok_or_else(|| decode("external unavailability", word))
                    })
                    .transpose()?,
            })
        })
        .transpose()
        .map_err(DbError::from)
    }

    /// Records materialised content only if `tag` is still the item's current tag, returning its
    /// revision. Content that changed without a new tag atomically receives a new revision.
    pub async fn set_external_content(
        &self,
        node: NodeId,
        tag: &str,
        sha256: &[u8; 32],
        at: OffsetDateTime,
    ) -> Result<Option<String>, DbError> {
        let revision = sqlx::query_scalar(
            "update source_items set \
                 revision = case when content_sha256 is not null and content_sha256 <> $3 \
                                 then $5 else revision end, \
                 content_sha256 = $3, read_at = $4, unavailable = null \
             where node_id = $1 and blob_sha = $2 returning revision",
        )
        .bind(node.0)
        .bind(tag)
        .bind(encode_sha256(sha256))
        .bind(at)
        .bind(new_external_revision())
        .fetch_optional(self.pool())
        .await?;
        Ok(revision)
    }

    /// Moves an external item's stat and revision only if `old_tag` is still current.
    pub async fn set_external_stat(
        &self,
        node: NodeId,
        old_tag: &str,
        size: u64,
        mtime_ns: i64,
    ) -> Result<bool, DbError> {
        self.set_external_seen(node, old_tag, &external_tag(size, mtime_ns), size)
            .await
    }

    /// Moves an external item to what was seen last (`new_tag`, of an object of `size` bytes)
    /// only if `old_tag` is still current, in one statement: a new revision, the content
    /// learned for the old tag and its version forgotten, and the item unavailable as
    /// archived exactly when `new_tag` says so. Whether it moved.
    pub async fn set_external_seen(
        &self,
        node: NodeId,
        old_tag: &str,
        new_tag: &str,
        size: u64,
    ) -> Result<bool, DbError> {
        let parts = external_tag_parts(new_tag, size)
            .ok_or_else(|| DbError::new("an external item was seen with an invalid change tag"))?;
        let stored_size = i64::try_from(size).map_err(DbError::new)?;
        let modified = OffsetDateTime::from_unix_timestamp_nanos(i128::from(parts.mtime_ns))
            .map_err(DbError::new)?;
        let done = sqlx::query(
            "update source_items set blob_sha = $3, size = $4, modified_at = $5, revision = $6, \
                 content_sha256 = null, read_at = null, unavailable = $7, missing_since = null, \
                 version_id = null \
             where node_id = $1 and blob_sha = $2",
        )
        .bind(node.0)
        .bind(old_tag)
        .bind(new_tag)
        .bind(stored_size)
        .bind(modified)
        .bind(new_external_revision())
        .bind(parts.archived.then_some("archived"))
        .execute(self.pool())
        .await?;
        Ok(done.rows_affected() > 0)
    }

    /// Records the object version an item was read at, only if `tag` is still current.
    pub async fn set_external_version(
        &self,
        node: NodeId,
        tag: &str,
        version: Option<&str>,
    ) -> Result<bool, DbError> {
        let done = sqlx::query(
            "update source_items set version_id = $3 where node_id = $1 and blob_sha = $2",
        )
        .bind(node.0)
        .bind(tag)
        .bind(version)
        .execute(self.pool())
        .await?;
        Ok(done.rows_affected() > 0)
    }

    /// Changes an external item's availability only if `tag` is still current.
    pub async fn set_external_unavailable(
        &self,
        node: NodeId,
        tag: &str,
        reason: Option<Unavailable>,
    ) -> Result<bool, DbError> {
        let done = sqlx::query(
            "update source_items set unavailable = $3 where node_id = $1 and blob_sha = $2",
        )
        .bind(node.0)
        .bind(tag)
        .bind(reason.map(Unavailable::as_str))
        .execute(self.pool())
        .await?;
        Ok(done.rows_affected() > 0)
    }

    /// Notes the listed nodes missing and answers those whose grace period has elapsed.
    pub async fn note_missing(
        &self,
        library: NodeId,
        nodes: &[NodeId],
        now: OffsetDateTime,
        grace: time::Duration,
    ) -> Result<Vec<NodeId>, DbError> {
        let ids: Vec<Uuid> = nodes.iter().map(|node| node.0).collect();
        let cutoff = now - grace;
        let missing: Vec<Uuid> = sqlx::query_scalar(
            "with noted as ( \
                 update source_items set missing_since = coalesce(missing_since, $3) \
                 where library_id = $1 and node_id = any($2) returning node_id, missing_since \
             ) select node_id from noted where missing_since <= $4 order by node_id",
        )
        .bind(library.0)
        .bind(&ids)
        .bind(now)
        .bind(cutoff)
        .fetch_all(self.pool())
        .await?;
        Ok(missing.into_iter().map(NodeId).collect())
    }

    /// Clears the missing marker from every tracked item except `still_missing`.
    pub async fn clear_missing_except(
        &self,
        library: NodeId,
        still_missing: &[NodeId],
    ) -> Result<(), DbError> {
        let ids: Vec<Uuid> = still_missing.iter().map(|node| node.0).collect();
        sqlx::query(
            "update source_items set missing_since = null \
             where library_id = $1 and not (node_id = any($2))",
        )
        .bind(library.0)
        .bind(&ids)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Records share-listing scheduling and hidden detail at one generation.
    pub async fn record_listing(
        &self,
        library: NodeId,
        generation: i64,
        next_check_at: Option<OffsetDateTime>,
        hidden_detail: &serde_json::Value,
    ) -> Result<bool, DbError> {
        let done = sqlx::query(
            "update library_sources set next_check_at = $3, hidden_detail = $4 \
             where library_id = $1 and generation = $2",
        )
        .bind(library.0)
        .bind(generation)
        .bind(next_check_at)
        .bind(hidden_detail)
        .execute(self.pool())
        .await?;
        Ok(done.rows_affected() > 0)
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
            .map(item_from_row)
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
                     revision = case when source_items.blob_sha is not distinct from excluded.blob_sha \
                                     then source_items.revision else null end, \
                     modified_at = case when source_items.blob_sha is not distinct from excluded.blob_sha \
                                        then source_items.modified_at else null end, \
                     content_sha256 = case when source_items.blob_sha is not distinct from excluded.blob_sha \
                                           then source_items.content_sha256 else null end, \
                     read_at = case when source_items.blob_sha is not distinct from excluded.blob_sha \
                                    then source_items.read_at else null end, \
                     unavailable = case when source_items.blob_sha is not distinct from excluded.blob_sha \
                                        then source_items.unavailable else null end, \
                     missing_since = case when source_items.blob_sha is not distinct from excluded.blob_sha \
                                         then source_items.missing_since else null end, \
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
                 revision = case when source_items.blob_sha is not distinct from excluded.blob_sha \
                                 then source_items.revision else null end, \
                 modified_at = case when source_items.blob_sha is not distinct from excluded.blob_sha \
                                    then source_items.modified_at else null end, \
                 content_sha256 = case when source_items.blob_sha is not distinct from excluded.blob_sha \
                                       then source_items.content_sha256 else null end, \
                 read_at = case when source_items.blob_sha is not distinct from excluded.blob_sha \
                                then source_items.read_at else null end, \
                 unavailable = case when source_items.blob_sha is not distinct from excluded.blob_sha \
                                    then source_items.unavailable else null end, \
                 missing_since = case when source_items.blob_sha is not distinct from excluded.blob_sha \
                                     then source_items.missing_since else null end, \
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
               and (coalesce(s.next_check_at, \
                             s.last_checked_at + make_interval(secs => s.poll_seconds)) is null \
                    or coalesce(s.next_check_at, \
                                s.last_checked_at + make_interval(secs => s.poll_seconds)) <= $1) \
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
    pub async fn disconnect_source(&self, library: NodeId) -> Result<Vec<SecretId>, AccessError> {
        let mut tx = self.pool().begin().await?;
        let secrets = disconnect_in(&mut tx, library).await?;
        tx.commit().await?;
        Ok(secrets)
    }

    /// [`Db::disconnect_source`] for `actor`, whose `manage_access` on `library` is judged again
    /// in the same transaction as the disconnect: a revocation after the route's own check
    /// still stops it. `NotFound` when `actor` cannot see the library; the access's refusal
    /// when they see it without `manage_access`.
    pub async fn disconnect_source_as(
        &self,
        actor: UserId,
        library: NodeId,
    ) -> Result<Vec<SecretId>, AccessError> {
        let mut tx = self.pool().begin().await?;
        match crate::scopes::access_in(&mut tx, actor, library).await? {
            None => return Err(AccessError::NotFound),
            Some(held) if !held.can_manage_access() => return Err(held.refusal()),
            Some(_) => {}
        }
        let secrets = disconnect_in(&mut tx, library).await?;
        tx.commit().await?;
        Ok(secrets)
    }
}

/// Removes the connection of `library` and its item rows on `conn`, returning its secrets.
/// `WritesPending` while changes accepted for the store are not yet written: they would be
/// lost with it.
async fn disconnect_in(
    conn: &mut PgConnection,
    library: NodeId,
) -> Result<Vec<SecretId>, AccessError> {
    let row: Option<(Option<Uuid>, Option<Uuid>, Uuid)> = sqlx::query_as(
        "select s.token_secret, s.webhook_secret, n.site_id from library_sources s \
         join nodes n on n.id = s.library_id where s.library_id = $1 for update of s",
    )
    .bind(library.0)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((token, webhook, site)) = row else {
        return Ok(Vec::new());
    };
    let pending: i64 = sqlx::query_scalar(
        "select (select count(*) from source_writes where library_id = $1) \
                  + (select count(*) from source_multipart where library_id = $1)",
    )
    .bind(library.0)
    .fetch_one(&mut *conn)
    .await?;
    if pending > 0 {
        return Err(AccessError::WritesPending);
    }
    // What the library holds is forgotten with the connection; the objects stay in the bucket.
    sqlx::query("delete from source_held where library_id = $1")
        .bind(library.0)
        .execute(&mut *conn)
        .await?;
    sqlx::query("delete from source_items where library_id = $1")
        .bind(library.0)
        .execute(&mut *conn)
        .await?;
    sqlx::query("delete from library_sources where library_id = $1")
        .bind(library.0)
        .execute(&mut *conn)
        .await?;
    recompute_site_usage(conn, NodeId(site)).await?;
    Ok([token, webhook]
        .into_iter()
        .flatten()
        .map(SecretId)
        .collect())
}
