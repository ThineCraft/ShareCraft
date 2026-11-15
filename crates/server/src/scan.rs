//! The scan worker: sends new content to the organisation's malware scanner over ICAP (see
//! [`crate::icap`]) and records what it says (see [`records::ScanState`]).
//!
//! It wakes whenever content is claimed, and every few seconds, and scans what is due: pending
//! content, and failed content whose retry time has come (after 1, 5, 15 and then every 60
//! minutes). At most two scans run at once (fewer if the scanner's `Max-Connections` says so),
//! and the same content is never scanned twice at once. Content over the scanner's size limit
//! is never sent; it is recorded `too_large`. Each scan records `content.scanned`. A scan
//! streams the file from the content store; nothing is held in memory. With no scanner
//! configured, nothing is scanned.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use records::{DueScan, ScanResult, ScanState, ScannerSettings};
use revisions::Content;
use time::OffsetDateTime;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;

use crate::AppState;
use crate::api::ApiError;
use crate::icap::{Client, Verdict};

/// The most scans run at once.
pub const MAX_SCANS: usize = 2;
/// How often the worker looks for due content when nothing wakes it.
const POLL: Duration = Duration::from_secs(5);
/// The most contents taken from the queue at a time.
const BATCH: i64 = 32;

/// The scan worker's shared state.
pub(crate) struct Scanner {
    /// Taken by each running scan.
    running: Arc<Semaphore>,
    /// The contents being scanned, or waiting for a turn.
    in_flight: Mutex<HashSet<[u8; 32]>>,
    /// Told when a setting changes or content is queued, to look again at once.
    wake: tokio::sync::Notify,
}

impl Default for Scanner {
    fn default() -> Self {
        Scanner {
            running: Arc::new(Semaphore::new(MAX_SCANS)),
            in_flight: Mutex::new(HashSet::new()),
            wake: tokio::sync::Notify::new(),
        }
    }
}

impl Scanner {
    /// Asks the worker to look for due content at once.
    pub(crate) fn wake(&self) {
        self.wake.notify_one();
    }

    /// Takes `sha256` to scan; `false` if it is being scanned already.
    fn take(&self, sha256: [u8; 32]) -> bool {
        self.in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(sha256)
    }

    fn done(&self, sha256: &[u8; 32]) {
        self.in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(sha256);
    }
}

/// Starts the scans due as of `now` and waits for them: content over the size limit is
/// recorded `too_large`, the rest is sent to the scanner. Answers how many it started. Does
/// nothing when no scanner is configured. Content already being scanned is left to that scan.
pub async fn scan_once(state: &AppState, now: OffsetDateTime) -> usize {
    let started = start_due(state, now).await;
    let count = started.len();
    for scan in started {
        if let Err(e) = scan.await {
            eprintln!("sharecraft-server: a scan did not finish: {e}");
        }
    }
    count
}

/// Starts the scans due as of `now`, each on a task of its own; their handles.
async fn start_due(state: &AppState, now: OffsetDateTime) -> Vec<JoinHandle<()>> {
    let db = state.db();
    let settings = match db.scanner_settings().await {
        Ok(settings) => settings,
        Err(e) => {
            eprintln!("sharecraft-server: database failure: {e}");
            return Vec::new();
        }
    };
    let Some(url) = settings.url.clone() else {
        return Vec::new();
    };
    let due = match db.due_scans(now, BATCH).await {
        Ok(due) => due,
        Err(e) => {
            eprintln!("sharecraft-server: database failure: {e}");
            return Vec::new();
        }
    };
    let scanner = state.scanner();
    let due: Vec<DueScan> = due
        .into_iter()
        .filter(|item| scanner.take(item.blob.sha256))
        .collect();
    if due.is_empty() {
        return Vec::new();
    }
    let client = Client::new(url, Duration::from_secs(settings.timeout_seconds));
    let settings = Arc::new(settings);
    due.into_iter()
        .map(|item| {
            let state = state.clone();
            let client = client.clone();
            let settings = Arc::clone(&settings);
            tokio::spawn(async move {
                let sha256 = item.blob.sha256;
                scan_one(&state, &client, &settings, item, now).await;
                state.scanner().done(&sha256);
            })
        })
        .collect()
}

/// Scans one content, taking a turn first, and records the result as of `now` (or as of when
/// it finished, if later).
async fn scan_one(
    state: &AppState,
    client: &Client,
    settings: &ScannerSettings,
    item: DueScan,
    now: OffsetDateTime,
) {
    let blob = item.blob;
    let result = if blob.size > settings.max_bytes {
        ScanResult::TooLarge
    } else {
        let Ok(_turn) = Arc::clone(&state.scanner().running).acquire_owned().await else {
            return;
        };
        let name = item.name.unwrap_or_else(|| blob.hex());
        scan_stored(state, client, blob, &name).await
    };
    // As of when it finished, unless the caller is looking ahead.
    let at = now.max(OffsetDateTime::now_utc());
    match state.db().record_scan(&blob.sha256, &result, at).await {
        Ok(_) => {}
        Err(e) => eprintln!("sharecraft-server: cannot record a scan: {e}"),
    }
}

/// Sends the stored content `blob`, named `name`, to the scanner.
async fn scan_stored(
    state: &AppState,
    client: &Client,
    blob: revisions::BlobRef,
    name: &str,
) -> ScanResult {
    let blobs = Arc::clone(state.blobs());
    let opened = tokio::task::spawn_blocking(move || blobs.store().open_blob(&blob.sha256)).await;
    let file = match opened {
        Ok(Ok(Some(file))) => tokio::fs::File::from_std(file),
        Ok(Ok(None)) => return ScanResult::Failed("the content is not in the store".to_owned()),
        Ok(Err(e)) => return ScanResult::Failed(format!("cannot open the content: {e}")),
        Err(e) => return ScanResult::Failed(format!("cannot open the content: {e}")),
    };
    let options = match client.options().await {
        Ok(options) => options,
        Err(e) => return ScanResult::Failed(e.to_string()),
    };
    match client.scan_with(&options, file, blob.size, name).await {
        Ok(Verdict::Clean) => ScanResult::Clean,
        Ok(Verdict::Infected(threat)) => ScanResult::Infected(threat),
        Err(e) => ScanResult::Failed(e.to_string()),
    }
}

/// The scan worker, running until dropped.
pub(crate) struct Worker {
    task: JoinHandle<()>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Starts the scan worker: it starts the scans due whenever content is claimed or a setting
/// changes, and every few seconds, without waiting for them to finish.
pub(crate) fn start_worker(state: AppState) -> Worker {
    let task = tokio::spawn(async move {
        loop {
            // Started scans run on their own tasks; the in-flight set keeps them single.
            drop(start_due(&state, OffsetDateTime::now_utc()).await);
            let blobs = Arc::clone(state.blobs());
            tokio::select! {
                () = blobs.claimed().notified() => {}
                () = state.scanner().wake.notified() => {}
                () = tokio::time::sleep(POLL) => {}
            }
        }
    });
    Worker { task }
}

/// Refuses with `423 held {scan}` the content `content` unless it is available (see
/// [`records::ScanState`]). Content kept inline in Git, saved before the content store, is
/// always available.
pub(crate) async fn require_available(state: &AppState, content: &Content) -> Result<(), ApiError> {
    let Content::Blob(blob) = content else {
        return Ok(());
    };
    match state.db().availability(&blob.sha256).await {
        Ok(None) => Ok(()),
        Ok(Some(scan)) => Err(ApiError::Held(scan)),
        Err(e) => Err(ApiError::unavailable(&e)),
    }
}

/// The scan state holding the document `doc`'s current version, if it is held; `None` when it
/// is available or has no version.
pub(crate) async fn held_state(
    state: &AppState,
    doc: collab_core::ids::DocId,
) -> Result<Option<ScanState>, ApiError> {
    state
        .db()
        .scan_mark(doc)
        .await
        .map(|mark| mark.map(|m| m.state))
        .map_err(|e| ApiError::unavailable(&e))
}
