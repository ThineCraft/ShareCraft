//! First-use reads of share-backed files, their scan gate, and the bounded content cache.

use std::io::{self, ErrorKind};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use collab_core::ids::{DocId, NodeId};
use records::{
    ExternalItem, NewEvent, ScanState, SharePath, Unavailable, approved_root, external_tag,
};
use revisions::{BlobError, BlobRef};
use serde_json::json;
use time::OffsetDateTime;

use crate::AppState;
use crate::blobs::{CacheEviction, Held};
use crate::share::{Flight, FsKind, FsStat, ShareFs};

pub const SHARE_READS: usize = 4;
pub const READ_WAIT: Duration = Duration::from_secs(20);
pub const SCAN_WAIT: Duration = Duration::from_secs(20);
pub const STAT_WAIT: Duration = Duration::from_secs(5);
pub const OFFLINE_FOR: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExternalError {
    Gone,
    Denied,
    Offline,
    Changing,
    TooLarge,
    Busy,
    NotApproved,
    Held(ScanState),
    Failed,
}

pub struct Ready {
    pub blob: BlobRef,
    pub held: Held,
    pub revision: String,
}

#[derive(Clone, Copy)]
enum FsError {
    Gone,
    Denied,
    Offline,
    Escaped,
    Failed,
}

struct ReadLease {
    _read: tokio::sync::OwnedSemaphorePermit,
    _flight: Arc<Flight>,
}

struct LeaseReader {
    inner: Box<dyn io::Read + Send>,
    _lease: Arc<ReadLease>,
}

impl io::Read for LeaseReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

/// Materialises one live share-backed document. Ordinary and GitHub documents answer `None`.
pub async fn materialise(state: &AppState, node: NodeId) -> Result<Option<Ready>, ExternalError> {
    let Some(mut item) = state
        .db()
        .external_item(node)
        .await
        .map_err(database_failed)?
    else {
        return Ok(None);
    };
    let settings = state.db().share_settings().await.map_err(database_failed)?;
    let location = SharePath::parse(&item.location).map_err(|_| ExternalError::NotApproved)?;
    let root = approved_root(&location, &settings.roots)
        .cloned()
        .ok_or(ExternalError::NotApproved)?;
    let file = location
        .join(&item.repository_path)
        .map_err(|_| ExternalError::NotApproved)?;
    let root_path = root.to_path_buf();
    let file_path = file.to_path_buf();

    let flight = Arc::new(state.shares().flight(node).await);
    item = state
        .db()
        .external_item(node)
        .await
        .map_err(database_failed)?
        .ok_or(ExternalError::Gone)?;

    if state.shares().offline(item.library) {
        return cached_while_offline(state, &item).await.map(Some);
    }

    let fs = state.shares().fs();
    let first = match timed_stat(Arc::clone(&fs), root_path.clone(), file_path.clone(), None).await
    {
        Ok(stat) => stat,
        Err(FsError::Offline) => {
            state.shares().mark_offline(item.library, OFFLINE_FOR);
            return cached_while_offline(state, &item).await.map(Some);
        }
        Err(error) => return stat_refusal(state, &item, error).await,
    };
    if first.kind != FsKind::File {
        mark_gone_and_wake(state, &item).await?;
        return Err(ExternalError::Gone);
    }

    let observed_tag = external_tag(first.size, first.mtime_ns);
    if observed_tag != item.tag {
        let _changed = state
            .db()
            .set_external_stat(node, &item.tag, first.size, first.mtime_ns)
            .await
            .map_err(database_failed)?;
        state.sources().wake(item.library);
        item = state
            .db()
            .external_item(node)
            .await
            .map_err(database_failed)?
            .ok_or(ExternalError::Gone)?;
    }

    if let Some(ready) = cached(state, &item).await? {
        return Ok(Some(ready));
    }

    let cache_max = state.shares().cache_max_bytes(settings.cache_max_bytes);
    if first.size > state.config().max_file_bytes || first.size > cache_max / 2 {
        state
            .db()
            .set_external_unavailable(node, &item.tag, Some(Unavailable::TooLarge))
            .await
            .map_err(database_failed)?;
        return Err(ExternalError::TooLarge);
    }
    let read = state
        .shares()
        .read_turn()
        .await
        .ok_or(ExternalError::Busy)?;
    let lease = Arc::new(ReadLease {
        _read: read,
        _flight: Arc::clone(&flight),
    });

    let mut attempt = 0;
    let (held, old_hash) = loop {
        match canonical(
            Arc::clone(&fs),
            root_path.clone(),
            file_path.clone(),
            Some(Arc::clone(&lease)),
        )
        .await
        {
            Ok(_) => {}
            Err(error) => {
                return stat_refusal(state, &item, error).await;
            }
        }
        let before = match timed_stat(
            Arc::clone(&fs),
            root_path.clone(),
            file_path.clone(),
            Some(Arc::clone(&lease)),
        )
        .await
        {
            Ok(stat) if stat.kind == FsKind::File => stat,
            Ok(_) => {
                mark_gone_and_wake(state, &item).await?;
                return Err(ExternalError::Gone);
            }
            Err(error) => return stat_refusal(state, &item, error).await,
        };
        let before_tag = external_tag(before.size, before.mtime_ns);
        if before_tag != item.tag {
            let _ = state
                .db()
                .set_external_stat(node, &item.tag, before.size, before.mtime_ns)
                .await
                .map_err(database_failed)?;
            state.sources().wake(item.library);
            item = state
                .db()
                .external_item(node)
                .await
                .map_err(database_failed)?
                .ok_or(ExternalError::Gone)?;
            if item.tag != before_tag {
                if attempt == 1 {
                    return Err(ExternalError::Changing);
                }
                attempt += 1;
                continue;
            }
        }
        if before.size > state.config().max_file_bytes || before.size > cache_max / 2 {
            state
                .db()
                .set_external_unavailable(node, &item.tag, Some(Unavailable::TooLarge))
                .await
                .map_err(database_failed)?;
            return Err(ExternalError::TooLarge);
        }
        let old_hash = item.sha256;
        let reader = match open(
            Arc::clone(&fs),
            root_path.clone(),
            file_path.clone(),
            Some(Arc::clone(&lease)),
        )
        .await
        {
            Ok(reader) => reader,
            Err(FsError::Gone) => {
                mark_gone_and_wake(state, &item).await?;
                return Err(ExternalError::Gone);
            }
            Err(FsError::Escaped) => {
                escape_event(state, &item);
                mark_gone_and_wake(state, &item).await?;
                return Err(ExternalError::Gone);
            }
            Err(FsError::Denied) => {
                state
                    .db()
                    .set_external_unavailable(node, &item.tag, Some(Unavailable::Denied))
                    .await
                    .map_err(database_failed)?;
                return Err(ExternalError::Denied);
            }
            Err(FsError::Offline) => {
                state.shares().mark_offline(item.library, OFFLINE_FOR);
                return Err(ExternalError::Offline);
            }
            Err(FsError::Failed) => return Err(ExternalError::Failed),
        };
        let reader = Box::new(LeaseReader {
            inner: reader,
            _lease: Arc::clone(&lease),
        });
        let held = match state.blobs().write_reader(reader, before.size).await {
            Ok(held) => held,
            Err(BlobError::TooLarge { .. }) if attempt == 0 => {
                attempt += 1;
                continue;
            }
            Err(error) => return Err(blob_failed(error)),
        };
        let after = match timed_stat(
            Arc::clone(&fs),
            root_path.clone(),
            file_path.clone(),
            Some(Arc::clone(&lease)),
        )
        .await
        {
            Ok(stat) => stat,
            Err(error) => {
                discard(state, held).await;
                return stat_refusal(state, &item, error).await;
            }
        };
        let torn = held.blob().size != before.size
            || after.kind != FsKind::File
            || after.size != before.size
            || after.mtime_ns != before.mtime_ns;
        if !torn {
            break (held, old_hash);
        }
        discard(state, held).await;
        if attempt == 1 {
            return Err(ExternalError::Changing);
        }
        attempt += 1;
    };
    drop(lease);

    let blob = held.blob();
    let now = OffsetDateTime::now_utc();
    state
        .db()
        .pin_cached(&blob, now)
        .await
        .map_err(database_failed)?;
    state.blobs().claimed().notify_one();
    let recorded = state
        .db()
        .set_external_content(node, &item.tag, &blob.sha256, now)
        .await
        .map_err(database_failed)?;
    if !recorded {
        state.sources().wake(item.library);
    } else if old_hash.is_some_and(|old| old != blob.sha256) {
        eprintln!(
            "sharecraft-server: share content changed without a new stat for node {}",
            node.0
        );
        state.events().emit(NewEvent {
            kind: "source.content_changed",
            actor: None,
            subject: None,
            document: Some(DocId(node.0)),
            node: Some(item.library),
            detail: json!({ "path": item.repository_path }),
        });
    }
    if let Err(error) = scan_gate(state, &blob.sha256).await {
        drop(held);
        let _ = evict(state).await;
        return Err(error);
    }
    let _ = evict(state).await;
    Ok(Some(Ready {
        blob,
        held,
        revision: item.revision,
    }))
}

async fn cached(state: &AppState, item: &ExternalItem) -> Result<Option<Ready>, ExternalError> {
    let Some(sha256) = item.sha256 else {
        return Ok(None);
    };
    let now = OffsetDateTime::now_utc();
    if !state
        .db()
        .touch_cached(&sha256, now)
        .await
        .map_err(database_failed)?
    {
        return Ok(None);
    }
    let blob = BlobRef {
        sha256,
        size: item.size,
    };
    let Some(held) = state.blobs().hold_stored(blob).await else {
        state
            .db()
            .unpin_cached(&sha256)
            .await
            .map_err(database_failed)?;
        return Ok(None);
    };
    if let Err(error) = scan_gate(state, &sha256).await {
        drop(held);
        let _ = evict(state).await;
        return Err(error);
    }
    Ok(Some(Ready {
        blob,
        held,
        revision: item.revision.clone(),
    }))
}

async fn cached_while_offline(
    state: &AppState,
    item: &ExternalItem,
) -> Result<Ready, ExternalError> {
    let Some(sha256) = item.sha256 else {
        return Err(ExternalError::Offline);
    };
    let blob = BlobRef {
        sha256,
        size: item.size,
    };
    let Some(held) = state.blobs().hold_stored(blob).await else {
        return Err(ExternalError::Offline);
    };
    state
        .db()
        .pin_cached(&blob, OffsetDateTime::now_utc())
        .await
        .map_err(database_failed)?;
    if let Err(error) = scan_gate(state, &sha256).await {
        drop(held);
        let _ = evict(state).await;
        return Err(error);
    }
    Ok(Ready {
        blob,
        held,
        revision: item.revision.clone(),
    })
}

async fn scan_gate(state: &AppState, sha256: &[u8; 32]) -> Result<(), ExternalError> {
    let deadline = tokio::time::Instant::now() + SCAN_WAIT;
    loop {
        match state
            .db()
            .availability(sha256)
            .await
            .map_err(database_failed)?
        {
            None => return Ok(()),
            Some(ScanState::Pending) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Some(held) => return Err(ExternalError::Held(held)),
        }
    }
}

/// Evicts cold cache pins until the configured byte bound is met. Held content is re-pinned.
pub async fn evict(state: &AppState) -> usize {
    let settings = match state.db().share_settings().await {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!("sharecraft-server: cannot read share cache settings: {error}");
            return 0;
        }
    };
    let maximum = state.shares().cache_max_bytes(settings.cache_max_bytes);
    let mut removed = 0;
    let mut held_seen = std::collections::HashSet::new();
    loop {
        let bytes = match state.db().cached_bytes().await {
            Ok(bytes) => bytes,
            Err(error) => {
                eprintln!("sharecraft-server: cannot count share cache bytes: {error}");
                return removed;
            }
        };
        if bytes <= maximum {
            return removed;
        }
        let cold = match state.db().coldest_cached(100).await {
            Ok(cold) => cold,
            Err(error) => {
                eprintln!("sharecraft-server: cannot list share cache entries: {error}");
                return removed;
            }
        };
        if cold.is_empty() {
            return removed;
        }
        let mut progress = false;
        let mut new_held = false;
        for blob in cold {
            match state.blobs().evict_cached(state.db(), blob).await {
                Ok(CacheEviction::Held) => {
                    new_held |= held_seen.insert(blob.sha256);
                }
                Ok(CacheEviction::Removed) => {
                    progress = true;
                    removed += 1;
                }
                Ok(CacheEviction::Missing) => progress = true,
                Err(error) => eprintln!("sharecraft-server: cannot evict cached content: {error}"),
            }
            if state
                .db()
                .cached_bytes()
                .await
                .is_ok_and(|bytes| bytes <= maximum)
            {
                return removed;
            }
        }
        if !progress && !new_held {
            eprintln!(
                "sharecraft-server: share cache remains over its limit because every entry is held"
            );
            return removed;
        }
    }
}

async fn discard(state: &AppState, held: Held) {
    let blob = held.blob();
    let already_cached = state
        .db()
        .touch_cached(&blob.sha256, OffsetDateTime::now_utc())
        .await
        .unwrap_or(false);
    if already_cached {
        drop(held);
        return;
    }
    if let Err(error) = state
        .db()
        .pin_cached(&blob, OffsetDateTime::now_utc())
        .await
    {
        eprintln!("sharecraft-server: cannot register discarded share content: {error}");
        drop(held);
        return;
    }
    drop(held);
    if let Err(error) = state.db().unpin_cached(&blob.sha256).await {
        eprintln!("sharecraft-server: cannot unpin discarded share content: {error}");
        return;
    }
    if let Err(error) = state
        .blobs()
        .erase_if_unreferenced(state.db(), &blob.sha256)
        .await
    {
        eprintln!("sharecraft-server: cannot erase discarded share content: {error}");
    }
}

async fn timed_stat(
    fs: Arc<dyn ShareFs>,
    root: PathBuf,
    file: PathBuf,
    hold: Option<Arc<ReadLease>>,
) -> Result<FsStat, FsError> {
    let task = tokio::task::spawn_blocking(move || {
        let _hold = hold;
        fs.stat(&root, &file)
    });
    match tokio::time::timeout(STAT_WAIT, task).await {
        Ok(Ok(Ok(stat))) => Ok(stat),
        Ok(Ok(Err(error))) => Err(classify(&error)),
        Ok(Err(error)) => {
            eprintln!("sharecraft-server: share stat task failed: {error}");
            Err(FsError::Failed)
        }
        Err(_) => Err(FsError::Offline),
    }
}

async fn canonical(
    fs: Arc<dyn ShareFs>,
    root: PathBuf,
    file: PathBuf,
    hold: Option<Arc<ReadLease>>,
) -> Result<PathBuf, FsError> {
    tokio::task::spawn_blocking(move || {
        let _hold = hold;
        fs.canonical(&root, &file)
    })
    .await
    .map_err(|error| {
        eprintln!("sharecraft-server: share canonical task failed: {error}");
        FsError::Failed
    })?
    .map_err(|error| classify(&error))
}

async fn open(
    fs: Arc<dyn ShareFs>,
    root: PathBuf,
    file: PathBuf,
    hold: Option<Arc<ReadLease>>,
) -> Result<Box<dyn io::Read + Send>, FsError> {
    tokio::task::spawn_blocking(move || {
        let _hold = hold;
        fs.open(&root, &file)
    })
    .await
    .map_err(|error| {
        eprintln!("sharecraft-server: share open task failed: {error}");
        FsError::Failed
    })?
    .map_err(|error| classify(&error))
}

fn classify(error: &io::Error) -> FsError {
    if error.to_string() == "path refused" {
        return FsError::Escaped;
    }
    match error.kind() {
        ErrorKind::NotFound | ErrorKind::NotADirectory | ErrorKind::InvalidInput => FsError::Gone,
        ErrorKind::PermissionDenied => FsError::Denied,
        ErrorKind::TimedOut
        | ErrorKind::ConnectionAborted
        | ErrorKind::ConnectionReset
        | ErrorKind::NotConnected
        | ErrorKind::HostUnreachable
        | ErrorKind::NetworkUnreachable => FsError::Offline,
        _ => {
            eprintln!("sharecraft-server: file share read failed: {error}");
            FsError::Failed
        }
    }
}

async fn stat_refusal(
    state: &AppState,
    item: &ExternalItem,
    error: FsError,
) -> Result<Option<Ready>, ExternalError> {
    match error {
        FsError::Gone | FsError::Escaped => {
            if matches!(error, FsError::Escaped) {
                escape_event(state, item);
            }
            mark_gone_and_wake(state, item).await?;
            Err(ExternalError::Gone)
        }
        FsError::Denied => {
            state
                .db()
                .set_external_unavailable(item.node, &item.tag, Some(Unavailable::Denied))
                .await
                .map_err(database_failed)?;
            Err(ExternalError::Denied)
        }
        FsError::Offline => {
            state.shares().mark_offline(item.library, OFFLINE_FOR);
            cached_while_offline(state, item).await.map(Some)
        }
        FsError::Failed => Err(ExternalError::Failed),
    }
}

async fn mark_gone_and_wake(state: &AppState, item: &ExternalItem) -> Result<(), ExternalError> {
    state
        .db()
        .set_external_unavailable(item.node, &item.tag, Some(Unavailable::Gone))
        .await
        .map_err(database_failed)?;
    state.sources().wake(item.library);
    Ok(())
}

fn escape_event(state: &AppState, item: &ExternalItem) {
    state.events().emit(NewEvent {
        kind: "source.escape_refused",
        actor: None,
        subject: None,
        document: Some(DocId(item.node.0)),
        node: Some(item.library),
        detail: json!({ "path": item.repository_path }),
    });
}

fn database_failed(error: impl std::fmt::Display) -> ExternalError {
    eprintln!("sharecraft-server: external content database failure: {error}");
    ExternalError::Failed
}

fn blob_failed(error: BlobError) -> ExternalError {
    match error {
        BlobError::TooLarge { .. } => ExternalError::Changing,
        BlobError::Io(error) => {
            eprintln!("sharecraft-server: cannot store external content: {error}");
            ExternalError::Failed
        }
        BlobError::Mismatch | BlobError::Missing(_) => {
            eprintln!("sharecraft-server: cannot store external content: {error}");
            ExternalError::Failed
        }
    }
}
