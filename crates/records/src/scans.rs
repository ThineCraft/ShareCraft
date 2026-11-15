//! Malware scanning results, which belong to the content (the `blobs` row), not to the
//! documents naming it, so the same bytes are scanned once.
//!
//! New content is `pending` when a scanner is configured and `unscanned` when not; the scan
//! worker moves it on to `clean`, `infected` (with the threat's name), `failed` (retried after
//! 1, 5, 15 and then every 60 minutes) or `too_large`. An administrator may release content
//! (`released`). Content is available when it is clean, released or unscanned. Content an
//! upload named (`scan_hold`: an upload naming it sets the hold) is held while pending, and
//! when it failed or is too large unless the matching setting lets it through. Content no
//! upload ever named (an editor's save, content queued by "scan existing files") stays
//! available whatever its scan state, until it is found infected: a scanner outage never
//! locks what the editors saved. Infected content is never available. Clearing the scanner
//! setting releases nothing: held content stays held, as when the scanner is down. Content
//! stored while no scanner is configured is `unscanned`, and available.
//!
//! A `blobs` row is written here in a short transaction of its own, never while holding any
//! other lock.

use std::collections::HashMap;
use std::fmt;

use collab_core::ids::{DocId, UserId};
use revisions::{BlobRef, hex, parse_hex};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;

use crate::events::{NewEvent, record};
use crate::{AccessError, Db, DbError};

/// The scanner's address, `icap://host[:port]/service`; empty for none.
pub const SCANNER_URL: &str = "scanner_url";
/// How long the scanner may take over a whole scan, in seconds (5 to 3600; 120).
pub const SCANNER_TIMEOUT_SECONDS: &str = "scanner_timeout_seconds";
/// The largest content sent to the scanner, in bytes (2 GiB by default).
pub const SCANNER_MAX_BYTES: &str = "scanner_max_bytes";
/// `"hold"` (the default) or `"allow"`: whether content the scanner failed on is available.
pub const SCANNER_UNAVAILABLE: &str = "scanner_unavailable";
/// `"hold"` (the default) or `"allow"`: whether content over the scanner's limit is available.
pub const SCANNER_TOO_LARGE: &str = "scanner_too_large";
/// Every scanner setting.
pub const SCANNER_SETTINGS: [&str; 5] = [
    SCANNER_URL,
    SCANNER_TIMEOUT_SECONDS,
    SCANNER_MAX_BYTES,
    SCANNER_UNAVAILABLE,
    SCANNER_TOO_LARGE,
];

/// The default port of an ICAP service.
pub const ICAP_PORT: u16 = 1344;
/// The longest a threat's name (or a failure's description) is kept.
pub const MAX_SCAN_DETAIL: usize = 200;
/// The shortest and longest reason an administrator gives for a release.
pub const RELEASE_REASON: std::ops::RangeInclusive<usize> = 3..=500;

/// Where a scan of content stands (the column `blobs.scan`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanState {
    /// Waiting for the scanner.
    Pending,
    Clean,
    /// The scanner found a threat.
    Infected,
    /// The scanner could not be asked, or did not answer; retried.
    Failed,
    /// Over the scanner's size limit.
    TooLarge,
    /// Stored before scanning, or while no scanner was configured.
    Unscanned,
    /// An administrator let infected or held content through.
    Released,
}

impl ScanState {
    /// The state as the column holds it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ScanState::Pending => "pending",
            ScanState::Clean => "clean",
            ScanState::Infected => "infected",
            ScanState::Failed => "failed",
            ScanState::TooLarge => "too_large",
            ScanState::Unscanned => "unscanned",
            ScanState::Released => "released",
        }
    }

    /// The state the column text names.
    #[must_use]
    pub fn from_column(text: &str) -> Option<ScanState> {
        Some(match text {
            "pending" => ScanState::Pending,
            "clean" => ScanState::Clean,
            "infected" => ScanState::Infected,
            "failed" => ScanState::Failed,
            "too_large" => ScanState::TooLarge,
            "unscanned" => ScanState::Unscanned,
            "released" => ScanState::Released,
            _ => return None,
        })
    }
}

impl fmt::Display for ScanState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Everything recorded about the scanning of one content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanInfo {
    pub state: ScanState,
    /// The threat's name when infected; what went wrong when failed.
    pub detail: Option<String>,
    /// Failed attempts so far.
    pub attempts: u32,
    /// When a pending or failed scan is next due.
    pub next_scan_at: Option<OffsetDateTime>,
    /// When the scanner last answered (or failed).
    pub scanned_at: Option<OffsetDateTime>,
    /// Whether an upload names it, so it is held while pending.
    pub hold: bool,
    pub size: u64,
}

/// Where new content comes from, as a claim states it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// An upload, a new version uploaded, or a single-request upload: held until scanned.
    Upload,
    /// A save from an editor (or a copy of content already available): scanned afterwards,
    /// never held.
    EditorSave,
}

/// What the scanner said, as the worker records it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScanResult {
    Clean,
    /// The threat's name.
    Infected(String),
    /// What went wrong.
    Failed(String),
    TooLarge,
}

impl ScanResult {
    fn state(&self) -> ScanState {
        match self {
            ScanResult::Clean => ScanState::Clean,
            ScanResult::Infected(_) => ScanState::Infected,
            ScanResult::Failed(_) => ScanState::Failed,
            ScanResult::TooLarge => ScanState::TooLarge,
        }
    }

    fn detail(&self) -> Option<String> {
        match self {
            ScanResult::Infected(text) | ScanResult::Failed(text) => Some(clean_detail(text)),
            ScanResult::Clean | ScanResult::TooLarge => None,
        }
    }
}

/// `text` as a threat name or failure is kept: printable characters only, trimmed, at most
/// [`MAX_SCAN_DETAIL`] characters; never empty.
#[must_use]
pub fn clean_detail(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .chars()
        .take(MAX_SCAN_DETAIL)
        .collect();
    if cleaned.is_empty() {
        "unnamed".to_owned()
    } else {
        cleaned
    }
}

/// How long after its `attempts`-th failure content is scanned again: 1, 5, 15, then 60
/// minutes.
#[must_use]
pub fn retry_after(attempts: u32) -> time::Duration {
    match attempts {
        0 | 1 => time::Duration::minutes(1),
        2 => time::Duration::minutes(5),
        3 => time::Duration::minutes(15),
        _ => time::Duration::minutes(60),
    }
}

/// An ICAP scanner's address: `icap://host[:port]/service`. It never carries credentials.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScannerUrl {
    /// A host name, an IPv4 address, or an IPv6 address in brackets.
    host: String,
    port: u16,
    service: String,
}

impl ScannerUrl {
    /// The address `text` names; `None` unless it is `icap://host[:port]/service` with a host
    /// name or address, a port from 1 to 65535 (1344 when absent), no user name or password,
    /// and a service of letters, digits and `-._~/`.
    #[must_use]
    pub fn parse(text: &str) -> Option<ScannerUrl> {
        let rest = text.strip_prefix("icap://")?;
        let (authority, service) = rest.split_once('/')?;
        let service_ok = !service.is_empty()
            && service.len() <= 200
            && service
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-._~/".contains(&b));
        if !service_ok || authority.contains('@') {
            return None;
        }
        let (host, port) = if let Some(after) = authority.strip_prefix('[') {
            let (inside, tail) = after.split_once(']')?;
            let ok = !inside.is_empty()
                && inside
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() || b == b':' || b == b'.');
            if !ok {
                return None;
            }
            let port = if tail.is_empty() {
                None
            } else {
                Some(tail.strip_prefix(':')?)
            };
            (format!("[{inside}]"), port)
        } else {
            let (host, port) = match authority.split_once(':') {
                Some((host, port)) => (host, Some(port)),
                None => (authority, None),
            };
            let ok = !host.is_empty()
                && host.len() <= 253
                && host
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.');
            if !ok {
                return None;
            }
            (host.to_owned(), port)
        };
        let port = match port {
            None => ICAP_PORT,
            Some(text) => {
                if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                let port: u16 = text.parse().ok()?;
                if port == 0 {
                    return None;
                }
                port
            }
        };
        Some(ScannerUrl {
            host,
            port,
            service: service.to_owned(),
        })
    }

    /// The host as written, IPv6 addresses in brackets.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The host to connect to: without brackets.
    #[must_use]
    pub fn connect_host(&self) -> &str {
        self.host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(&self.host)
    }

    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The service, without its leading `/`.
    #[must_use]
    pub fn service(&self) -> &str {
        &self.service
    }
}

impl fmt::Display for ScannerUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "icap://{}:{}/{}", self.host, self.port, self.service)
    }
}

/// The scanner settings, as read together.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScannerSettings {
    /// The scanner; `None` when none is configured.
    pub url: Option<ScannerUrl>,
    /// The text of `scanner_url` as set.
    pub url_text: String,
    pub timeout_seconds: u64,
    pub max_bytes: u64,
    /// `scanner_unavailable` is `allow`.
    pub unavailable_allow: bool,
    /// `scanner_too_large` is `allow`.
    pub too_large_allow: bool,
}

impl Default for ScannerSettings {
    fn default() -> Self {
        ScannerSettings {
            url: None,
            url_text: String::new(),
            timeout_seconds: 120,
            max_bytes: 2 << 30,
            unavailable_allow: false,
            too_large_allow: false,
        }
    }
}

impl ScannerSettings {
    /// Whether content in `state`, held for uploads when `hold`, is available.
    #[must_use]
    pub fn available(&self, state: ScanState, hold: bool) -> bool {
        self.allows(state, hold)
    }

    /// Whether content in `state` may be offered and copied as a template ("New from
    /// template"): as [`ScannerSettings::available`], except that pending content never is,
    /// held or not, so a copy from a template is never made before its content is judged.
    #[must_use]
    pub fn offers_template(&self, state: ScanState) -> bool {
        state != ScanState::Pending && self.allows(state, true)
    }

    fn allows(&self, state: ScanState, hold: bool) -> bool {
        match state {
            ScanState::Clean | ScanState::Released | ScanState::Unscanned => true,
            ScanState::Infected => false,
            ScanState::Pending => !hold,
            ScanState::Failed => !hold || self.unavailable_allow,
            ScanState::TooLarge => !hold || self.too_large_allow,
        }
    }
}

/// Whether `value` is acceptable for the scanner setting `key` (see [`ScannerSettings`]).
#[must_use]
pub fn valid_scanner_setting(key: &str, value: &Value) -> bool {
    match key {
        SCANNER_URL => value
            .as_str()
            .is_some_and(|text| text.is_empty() || ScannerUrl::parse(text).is_some()),
        SCANNER_TIMEOUT_SECONDS => value.as_i64().is_some_and(|s| (5..=3600).contains(&s)),
        SCANNER_MAX_BYTES => value.as_i64().is_some_and(|n| n > 0),
        SCANNER_UNAVAILABLE | SCANNER_TOO_LARGE => {
            matches!(value.as_str(), Some("hold" | "allow"))
        }
        _ => false,
    }
}

/// Content due for the scanner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DueScan {
    pub blob: BlobRef,
    /// The name of a document naming it, to give the scanner; `None` if there is none yet.
    pub name: Option<String>,
}

/// A document whose current version names quarantined or held content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeldDocument {
    pub id: DocId,
    pub name: String,
    /// From the site down, names joined with ` / `.
    pub path: String,
    /// Whether it is in the recycle bin.
    pub deleted: bool,
}

/// Content an administrator may release or delete: infected, or held by a failed scan or the
/// size limit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuarantineEntry {
    pub sha256: String,
    pub state: ScanState,
    /// The threat's name when infected.
    pub threat: Option<String>,
    pub size: u64,
    pub scanned_at: Option<OffsetDateTime>,
    /// The documents whose current version names it.
    pub documents: Vec<HeldDocument>,
}

/// How a held document is marked: its content's state, and the threat when infected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanMark {
    pub state: ScanState,
    pub threat: Option<String>,
}

impl ScanMark {
    /// Whether the document is quarantined (its current version infected) rather than held.
    #[must_use]
    pub fn quarantined(&self) -> bool {
        self.state == ScanState::Infected
    }
}

fn state_of(text: &str) -> Result<ScanState, DbError> {
    ScanState::from_column(text).ok_or_else(|| DbError::new(format!("unknown scan state {text}")))
}

fn size_of(size: i64) -> u64 {
    u64::try_from(size).unwrap_or(0)
}

type MarkRow = (uuid::Uuid, String, Option<String>, bool);

impl Db {
    /// The scanner settings; defaults for any not set.
    pub async fn scanner_settings(&self) -> Result<ScannerSettings, DbError> {
        let rows: Vec<(String, Value)> =
            sqlx::query_as("select key, value from settings where key = any($1)")
                .bind(SCANNER_SETTINGS.to_vec())
                .fetch_all(self.pool())
                .await?;
        let mut settings = ScannerSettings::default();
        for (key, value) in rows {
            match key.as_str() {
                SCANNER_URL => {
                    let text = value.as_str().unwrap_or_default().to_owned();
                    settings.url = ScannerUrl::parse(&text);
                    settings.url_text = text;
                }
                SCANNER_TIMEOUT_SECONDS => {
                    if let Some(seconds) = value.as_u64() {
                        settings.timeout_seconds = seconds;
                    }
                }
                SCANNER_MAX_BYTES => {
                    if let Some(bytes) = value.as_u64() {
                        settings.max_bytes = bytes;
                    }
                }
                SCANNER_UNAVAILABLE => settings.unavailable_allow = value == "allow",
                SCANNER_TOO_LARGE => settings.too_large_allow = value == "allow",
                _ => {}
            }
        }
        Ok(settings)
    }

    /// What is recorded about the scanning of the content `sha256`; `None` when it has no row
    /// (content stored before the content store's rows, which counts as unscanned).
    pub async fn scan_state(&self, sha256: &[u8; 32]) -> Result<Option<ScanInfo>, DbError> {
        type Row = (
            String,
            Option<String>,
            i32,
            Option<OffsetDateTime>,
            Option<OffsetDateTime>,
            bool,
            i64,
        );
        let row: Option<Row> = sqlx::query_as(
            "select scan, scan_detail, scan_attempts, next_scan_at, scanned_at, scan_hold, size \
             from blobs where sha256 = $1",
        )
        .bind(hex(sha256))
        .fetch_optional(self.pool())
        .await?;
        let Some((state, detail, attempts, next_scan_at, scanned_at, hold, size)) = row else {
            return Ok(None);
        };
        Ok(Some(ScanInfo {
            state: state_of(&state)?,
            detail,
            attempts: u32::try_from(attempts).unwrap_or(0),
            next_scan_at,
            scanned_at,
            hold,
            size: size_of(size),
        }))
    }

    /// `None` when the content `sha256` is available (see the module documentation), or the
    /// state that holds it.
    pub async fn availability(&self, sha256: &[u8; 32]) -> Result<Option<ScanState>, DbError> {
        let Some(info) = self.scan_state(sha256).await? else {
            return Ok(None);
        };
        if info.state == ScanState::Clean || info.state == ScanState::Unscanned {
            return Ok(None);
        }
        let settings = self.scanner_settings().await?;
        Ok((!settings.available(info.state, info.hold)).then_some(info.state))
    }

    /// `None` when the content `sha256` may be copied as a template (see
    /// [`ScannerSettings::offers_template`]: as available, but never while pending), or the
    /// state that keeps it back.
    pub async fn template_availability(
        &self,
        sha256: &[u8; 32],
    ) -> Result<Option<ScanState>, DbError> {
        let Some(info) = self.scan_state(sha256).await? else {
            return Ok(None);
        };
        if info.state == ScanState::Clean || info.state == ScanState::Unscanned {
            return Ok(None);
        }
        let settings = self.scanner_settings().await?;
        Ok((!settings.offers_template(info.state)).then_some(info.state))
    }

    /// The documents among `docs` whose current version's content may not be offered as a
    /// template (see [`ScannerSettings::offers_template`]), each with the state that keeps it
    /// back; the others are left out.
    pub async fn templates_kept_back(
        &self,
        docs: &[DocId],
    ) -> Result<HashMap<DocId, ScanState>, DbError> {
        if docs.is_empty() {
            return Ok(HashMap::new());
        }
        let ids: Vec<uuid::Uuid> = docs.iter().map(|d| d.0).collect();
        let rows: Vec<(uuid::Uuid, String)> = sqlx::query_as(
            "select d.id, b.scan from documents d \
             join blob_refs r on r.doc_id = d.id and r.revision = d.revision and not r.pending \
             join blobs b on b.sha256 = r.sha256 \
             where d.id = any($1) and b.scan not in ('clean', 'released', 'unscanned')",
        )
        .bind(&ids)
        .fetch_all(self.pool())
        .await?;
        if rows.is_empty() {
            return Ok(HashMap::new());
        }
        let settings = self.scanner_settings().await?;
        let mut kept = HashMap::new();
        for (id, state) in rows {
            let state = state_of(&state)?;
            if !settings.offers_template(state) {
                kept.insert(DocId(id), state);
            }
        }
        Ok(kept)
    }

    /// Whether the content `sha256` is available (see the module documentation).
    pub async fn is_available(&self, sha256: &[u8; 32]) -> Result<bool, DbError> {
        Ok(self.availability(sha256).await?.is_none())
    }

    /// The held documents among `docs`, by their current version's content, with how each is
    /// marked; available ones are left out.
    pub async fn scan_marks(&self, docs: &[DocId]) -> Result<HashMap<DocId, ScanMark>, DbError> {
        if docs.is_empty() {
            return Ok(HashMap::new());
        }
        let ids: Vec<uuid::Uuid> = docs.iter().map(|d| d.0).collect();
        let rows: Vec<MarkRow> = sqlx::query_as(
            "select d.id, b.scan, b.scan_detail, b.scan_hold from documents d \
             join blob_refs r on r.doc_id = d.id and r.revision = d.revision and not r.pending \
             join blobs b on b.sha256 = r.sha256 \
             where d.id = any($1) and b.scan not in ('clean', 'released', 'unscanned')",
        )
        .bind(&ids)
        .fetch_all(self.pool())
        .await?;
        if rows.is_empty() {
            return Ok(HashMap::new());
        }
        let settings = self.scanner_settings().await?;
        let mut marks = HashMap::new();
        for (id, state, detail, hold) in rows {
            let state = state_of(&state)?;
            if settings.available(state, hold) {
                continue;
            }
            let threat = (state == ScanState::Infected).then_some(detail).flatten();
            marks.insert(DocId(id), ScanMark { state, threat });
        }
        Ok(marks)
    }

    /// How the document `doc` is marked when its current version is held; `None` when it is
    /// available (or has no version).
    pub async fn scan_mark(&self, doc: DocId) -> Result<Option<ScanMark>, DbError> {
        Ok(self.scan_marks(&[doc]).await?.remove(&doc))
    }

    /// Records what the scanner said of the content `sha256` as of `now`, with
    /// `content.scanned {sha256, result, detail}`, if it is still pending or failed (a release
    /// meanwhile wins); whether it was recorded. A failure is retried after
    /// [`retry_after`] its count of failures.
    pub async fn record_scan(
        &self,
        sha256: &[u8; 32],
        result: &ScanResult,
        now: OffsetDateTime,
    ) -> Result<bool, DbError> {
        let hash = hex(sha256);
        let mut tx = self.pool().begin().await?;
        let row: Option<(String, i32)> =
            sqlx::query_as("select scan, scan_attempts from blobs where sha256 = $1 for update")
                .bind(&hash)
                .fetch_optional(&mut *tx)
                .await?;
        let Some((state, attempts)) = row else {
            return Ok(false);
        };
        if !matches!(state_of(&state)?, ScanState::Pending | ScanState::Failed) {
            return Ok(false);
        }
        let detail = result.detail();
        let (attempts, next) = match result {
            ScanResult::Failed(_) => {
                let attempts = attempts.saturating_add(1);
                let next = now + retry_after(u32::try_from(attempts).unwrap_or(u32::MAX));
                (attempts, Some(next))
            }
            _ => (attempts, None),
        };
        sqlx::query(
            "update blobs set scan = $2, scan_detail = $3, scanned_at = $4, scan_attempts = $5, \
             next_scan_at = $6 where sha256 = $1",
        )
        .bind(&hash)
        .bind(result.state().as_str())
        .bind(&detail)
        .bind(now)
        .bind(attempts)
        .bind(next)
        .execute(&mut *tx)
        .await?;
        record(
            &mut tx,
            NewEvent {
                kind: "content.scanned",
                actor: None,
                subject: None,
                document: None,
                node: None,
                detail: json!({
                    "sha256": hash,
                    "result": result.state().as_str(),
                    "detail": detail,
                }),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// At most `limit` contents due for the scanner as of `now`: pending or failed, due, and
    /// named by some version (or a claim) or pinned by a brand image; held content (an upload's) before any other, so the
    /// backlog "scan existing files" queues never keeps uploads waiting, then the longest
    /// waiting first.
    pub async fn due_scans(
        &self,
        now: OffsetDateTime,
        limit: i64,
    ) -> Result<Vec<DueScan>, DbError> {
        let rows: Vec<(String, i64, Option<String>)> = sqlx::query_as(
            "select b.sha256, b.size, \
                    (select d.name from blob_refs r join documents d on d.id = r.doc_id \
                     where r.sha256 = b.sha256 order by r.id desc limit 1) \
             from blobs b \
             where b.scan in ('pending', 'failed') \
               and coalesce(b.next_scan_at, '-infinity') <= $1 \
               and (exists (select 1 from blob_refs r where r.sha256 = b.sha256) \
                    or exists (select 1 from brand_refs p where p.sha256 = b.sha256)) \
             order by b.scan_hold desc, b.next_scan_at nulls first, b.sha256 limit $2",
        )
        .bind(now)
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(sha256, size, name)| {
                Some(DueScan {
                    blob: BlobRef {
                        sha256: parse_hex(&sha256)?,
                        size: u64::try_from(size).ok()?,
                    },
                    name,
                })
            })
            .collect())
    }

    /// Queues every unscanned content that some version names, or a brand image pins, for the
    /// scanner; how many. It stays available while it waits (its hold cleared: it was
    /// available before), and is quarantined if the scanner finds it infected.
    pub async fn queue_unscanned(&self) -> Result<u64, DbError> {
        let done = sqlx::query(
            "update blobs b set scan = 'pending', next_scan_at = now(), scan_attempts = 0, \
                 scan_hold = false \
             where b.scan = 'unscanned' \
               and (exists (select 1 from blob_refs r where r.sha256 = b.sha256) \
                    or exists (select 1 from brand_refs p where p.sha256 = b.sha256))",
        )
        .execute(self.pool())
        .await?;
        Ok(done.rows_affected())
    }

    /// Whether `actor` is an enabled administrator.
    async fn is_enabled_admin(&self, actor: UserId) -> Result<bool, DbError> {
        let admin: Option<bool> =
            sqlx::query_scalar("select is_admin and disabled_at is null from users where id = $1")
                .bind(actor.0)
                .fetch_optional(self.pool())
                .await?;
        Ok(admin == Some(true))
    }

    /// Releases the content `sha256` for `actor`, an enabled administrator (`Forbidden`), who
    /// gives `reason` (3 to 500 characters once trimmed; `InvalidSetting` otherwise): content
    /// that is infected, failed, too large or pending is marked `released` and recorded with
    /// `content.released {sha256, reason, from}`. Whether it was held (`false` for content
    /// with no row, or already available).
    pub async fn release(
        &self,
        sha256: &[u8; 32],
        actor: UserId,
        reason: &str,
    ) -> Result<bool, AccessError> {
        let reason = reason.trim();
        if !RELEASE_REASON.contains(&reason.chars().count()) {
            return Err(AccessError::InvalidSetting);
        }
        if !self.is_enabled_admin(actor).await? {
            return Err(AccessError::Forbidden);
        }
        let hash = hex(sha256);
        let mut tx = self.pool().begin().await?;
        let before: Option<String> = sqlx::query_scalar(
            "select scan from blobs where sha256 = $1 \
               and scan in ('infected', 'failed', 'too_large', 'pending') for update",
        )
        .bind(&hash)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(before) = before else {
            return Ok(false);
        };
        sqlx::query("update blobs set scan = 'released', next_scan_at = null where sha256 = $1")
            .bind(&hash)
            .execute(&mut *tx)
            .await?;
        record(
            &mut tx,
            NewEvent {
                kind: "content.released",
                actor: Some(actor),
                subject: None,
                document: None,
                node: None,
                detail: json!({ "sha256": hash, "reason": reason, "from": before }),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// The live or binned documents whose current version names the content `sha256`.
    pub async fn documents_naming(&self, sha256: &[u8; 32]) -> Result<Vec<HeldDocument>, DbError> {
        let rows: Vec<(uuid::Uuid, String, bool)> = sqlx::query_as(
            "select n.id, n.name, n.deleted_at is not null from documents d \
             join blob_refs r on r.doc_id = d.id and r.revision = d.revision and not r.pending \
             join nodes n on n.id = d.id \
             where r.sha256 = $1 order by n.name, n.id",
        )
        .bind(hex(sha256))
        .fetch_all(self.pool())
        .await?;
        let mut documents = Vec::with_capacity(rows.len());
        for (id, name, deleted) in rows {
            let path: Option<String> = sqlx::query_scalar(
                "with recursive up (id, parent_id, name, depth) as ( \
                     select id, parent_id, name, 0 from nodes where id = $1 \
                     union all \
                     select n.id, n.parent_id, n.name, up.depth + 1 \
                     from nodes n join up on n.id = up.parent_id where up.depth < 1000 \
                 ) select string_agg(name, ' / ' order by depth desc) from up",
            )
            .bind(id)
            .fetch_one(self.pool())
            .await?;
            documents.push(HeldDocument {
                id: DocId(id),
                name,
                path: path.unwrap_or_default(),
                deleted,
            });
        }
        Ok(documents)
    }

    /// The organisation's quarantine, for administrators: content that is infected, held by a
    /// failed scan or the size limit, or held while pending (an upload waiting for the
    /// scanner, or stuck after the scanner setting was cleared), that some document's current
    /// version names, each with those documents and their paths. The most recently scanned
    /// first.
    pub async fn quarantine_list(&self) -> Result<Vec<QuarantineEntry>, DbError> {
        type Row = (
            String,
            String,
            Option<String>,
            i64,
            Option<OffsetDateTime>,
            bool,
        );
        let rows: Vec<Row> = sqlx::query_as(
            "select b.sha256, b.scan, b.scan_detail, b.size, b.scanned_at, b.scan_hold \
             from blobs b where b.scan in ('infected', 'failed', 'too_large') \
                or (b.scan = 'pending' and b.scan_hold) \
             order by b.scanned_at desc nulls last, b.sha256",
        )
        .fetch_all(self.pool())
        .await?;
        let settings = self.scanner_settings().await?;
        let mut entries = Vec::new();
        for (sha256, state, detail, size, scanned_at, hold) in rows {
            let state = state_of(&state)?;
            if settings.available(state, hold) {
                continue;
            }
            let Some(hash) = parse_hex(&sha256) else {
                continue;
            };
            let documents = self.documents_naming(&hash).await?;
            if documents.is_empty() {
                continue;
            }
            entries.push(QuarantineEntry {
                sha256,
                state,
                threat: (state == ScanState::Infected).then_some(detail).flatten(),
                size: size_of(size),
                scanned_at,
                documents,
            });
        }
        Ok(entries)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn scanner_urls() {
        let url = ScannerUrl::parse("icap://scanner.example.org:11344/av/scan").unwrap();
        assert_eq!(url.host(), "scanner.example.org");
        assert_eq!(url.port(), 11344);
        assert_eq!(url.service(), "av/scan");
        let url = ScannerUrl::parse("icap://[::1]/scan").unwrap();
        assert_eq!(url.connect_host(), "::1");
        assert_eq!(url.port(), ICAP_PORT);
        assert_eq!(url.to_string(), "icap://[::1]:1344/scan");
        for bad in [
            "",
            "icap://",
            "icap://host",
            "icap://host/",
            "icaps://host/scan",
            "icap://a:b@host/scan",
            "icap://host:/scan",
            "icap://host:0/scan",
            "icap://host:70000/scan",
            "icap://ho st/scan",
            "icap://host/sc an",
            "icap://host/scan?x=1",
            "icap://[zz]/scan",
        ] {
            assert!(ScannerUrl::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn retries_slow_down() {
        let minutes: Vec<i64> = (1..=6).map(|n| retry_after(n).whole_minutes()).collect();
        assert_eq!(minutes, [1, 5, 15, 60, 60, 60]);
    }

    #[test]
    fn availability_follows_the_settings() {
        let configured = ScannerSettings {
            url: ScannerUrl::parse("icap://scanner/x"),
            ..ScannerSettings::default()
        };
        assert!(configured.available(ScanState::Pending, false));
        assert!(!configured.available(ScanState::Pending, true));
        // Content no upload named (an editor's save, queued existing content) stays available
        // when its scan fails or it is too large; held content does not.
        assert!(configured.available(ScanState::Failed, false));
        assert!(configured.available(ScanState::TooLarge, false));
        assert!(!configured.available(ScanState::Failed, true));
        assert!(!configured.available(ScanState::TooLarge, true));
        assert!(!configured.available(ScanState::Infected, false));
        // Templates are offered only once judged, held or not.
        assert!(!configured.offers_template(ScanState::Failed));
        assert!(configured.available(ScanState::Released, true));
        let allowing = ScannerSettings {
            unavailable_allow: true,
            too_large_allow: true,
            ..configured
        };
        assert!(allowing.available(ScanState::Failed, true));
        assert!(allowing.available(ScanState::TooLarge, true));
        assert!(!allowing.available(ScanState::Infected, false));
        // Clearing the scanner releases nothing.
        let none = ScannerSettings::default();
        assert!(!none.available(ScanState::Pending, true));
        assert!(none.available(ScanState::Pending, false));
        assert!(!none.available(ScanState::Failed, true));
        assert!(!none.available(ScanState::TooLarge, true));
        assert!(!none.available(ScanState::Infected, true));
        assert!(none.available(ScanState::Unscanned, true));
    }

    #[test]
    fn details_are_cleaned() {
        assert_eq!(clean_detail(" Eicar\r\nTest "), "Eicar  Test");
        assert_eq!(clean_detail("\n"), "unnamed");
        assert_eq!(clean_detail(&"x".repeat(500)).len(), MAX_SCAN_DETAIL);
    }
}
