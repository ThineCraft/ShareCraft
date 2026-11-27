//! Read-only access to file shares and bounded directory listings.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use collab_core::ids::NodeId;
use records::{
    SharePath, SourceConfig, SourceStatus, approved_root, external_tag, hidden_by_default,
    reserved_name, windows_safe_name,
};
use tokio::task::JoinSet;

use crate::AppState;
use crate::blobs::Held;
use crate::source::{ContentMode, EntryKind, Listing, Look, SourceCaps, StorageSource, TreeEntry};
use crate::source_sync::{Hidden, SyncError};

const HIDDEN_ATTRIBUTE: u32 = 0x2;
const SYSTEM_ATTRIBUTE: u32 = 0x4;
const OFFLINE_ATTRIBUTES: u32 = 0x1000 | 0x40000 | 0x400000;
const MAX_RELATIVE_CHARS: usize = 400;

/// What an entry in a share is without following it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsKind {
    File,
    Dir,
    /// A symbolic link, junction or other name-surrogate reparse point.
    Link,
    Other,
}

/// One child returned by a single-folder listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FsEntry {
    /// `None` when the operating-system name is not Unicode.
    pub name: Option<String>,
    pub kind: FsKind,
    pub size: u64,
    pub mtime_ns: i64,
    pub attributes: u32,
}

/// Metadata read without following a link.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FsStat {
    pub kind: FsKind,
    pub size: u64,
    pub mtime_ns: i64,
    pub attributes: u32,
}

/// The Phase 1 file-system seam. It deliberately has no write operation.
///
/// Every operation receives the approved root and performs its own canonical containment check,
/// so a caller cannot accidentally list or open through an unchecked ancestor link. There remains
/// a small accepted check-then-use window if an ancestor is replaced after canonicalization and
/// before the operating-system operation; closing it needs handle-relative APIs that require
/// unsafe code or another crate. Canonicalization is therefore done immediately before use.
///
/// Hard-linked files are a known residual risk: stable Rust exposes no Windows link-count API, so
/// this seam cannot identify two names for the same file object without unsafe code or a new crate.
pub trait ShareFs: Send + Sync + 'static {
    /// Lists one folder within `approved_root`, never follows its final link, and stops at `budget`.
    fn list(&self, approved_root: &Path, dir: &Path, budget: &FsBudget)
    -> io::Result<Vec<FsEntry>>;
    /// Reads metadata and never follows a link.
    fn stat(&self, approved_root: &Path, path: &Path) -> io::Result<FsStat>;
    /// Opens one contained file read-only.
    fn open(&self, approved_root: &Path, path: &Path) -> io::Result<Box<dyn Read + Send>>;
    /// Resolves links and refuses a result outside `approved_root`.
    fn canonical(&self, approved_root: &Path, path: &Path) -> io::Result<PathBuf>;
    /// The file-system identity of a contained `path` (read only), so two names for one folder
    /// can be told apart from two folders: a case-insensitive CIFS mount on Linux gives
    /// `/mnt/s/Projects` and `/mnt/s/projects` different canonical paths. `None` where the
    /// platform offers no stable identity; callers then rely on path comparisons alone.
    fn identity(&self, _approved_root: &Path, _path: &Path) -> io::Result<Option<FileIdentity>> {
        Ok(None)
    }
}

/// What identifies a file or folder on its file system, whatever name reaches it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FileIdentity {
    pub device: u64,
    pub inode: u64,
}

/// A deadline and shared entry allowance used by all folder reads in one listing.
#[derive(Clone)]
pub struct FsBudget {
    remaining: Arc<AtomicUsize>,
    deadline: Instant,
}

impl FsBudget {
    #[must_use]
    pub fn new(max_entries: usize, deadline: Instant) -> Self {
        Self {
            remaining: Arc::new(AtomicUsize::new(max_entries)),
            deadline,
        }
    }

    /// Claims room for one enumerated entry.
    pub fn claim(&self) -> io::Result<()> {
        if Instant::now() >= self.deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "listing timed out"));
        }
        self.remaining
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .map(|_| ())
            .map_err(|_| io::Error::new(io::ErrorKind::FileTooLarge, "entry limit exceeded"))
    }
}

/// The operating-system implementation of [`ShareFs`].
pub struct OsFs;

impl ShareFs for OsFs {
    fn list(
        &self,
        approved_root: &Path,
        dir: &Path,
        budget: &FsBudget,
    ) -> io::Result<Vec<FsEntry>> {
        canonical_within(approved_root, dir)?;
        let dir_meta = fs::symlink_metadata(dir)?;
        if kind(&dir_meta) != FsKind::Dir {
            return Err(io::Error::new(io::ErrorKind::NotADirectory, "not a folder"));
        }
        let canonical = canonical_within(approved_root, dir)?;
        let mut entries = Vec::new();
        for answer in fs::read_dir(canonical)? {
            let entry = answer?;
            budget.claim()?;
            let meta = fs::symlink_metadata(entry.path())?;
            entries.push(FsEntry {
                name: entry.file_name().into_string().ok(),
                kind: kind(&meta),
                size: meta.len(),
                mtime_ns: modified_ns(&meta),
                attributes: attributes(&meta),
            });
        }
        Ok(entries)
    }

    fn stat(&self, approved_root: &Path, path: &Path) -> io::Result<FsStat> {
        canonical_within(approved_root, path)?;
        let meta = fs::symlink_metadata(path)?;
        Ok(FsStat {
            kind: kind(&meta),
            size: meta.len(),
            mtime_ns: modified_ns(&meta),
            attributes: attributes(&meta),
        })
    }

    fn open(&self, approved_root: &Path, path: &Path) -> io::Result<Box<dyn Read + Send>> {
        canonical_within(approved_root, path)?;
        let meta = fs::symlink_metadata(path)?;
        if kind(&meta) != FsKind::File {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a file"));
        }
        let canonical = canonical_within(approved_root, path)?;
        fs::File::open(canonical).map(|file| Box::new(file) as Box<dyn Read + Send>)
    }

    fn canonical(&self, approved_root: &Path, path: &Path) -> io::Result<PathBuf> {
        canonical_within(approved_root, path)
    }

    fn identity(&self, approved_root: &Path, path: &Path) -> io::Result<Option<FileIdentity>> {
        // Where there is no identity to read, nothing touches the share: each lookup here runs
        // inside a connect's timed canonical lookup.
        if cfg!(not(unix)) {
            return Ok(None);
        }
        let canonical = canonical_within(approved_root, path)?;
        os_identity(&canonical)
    }
}

/// On Unix the device and inode numbers.
#[cfg(unix)]
fn os_identity(path: &Path) -> io::Result<Option<FileIdentity>> {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::metadata(path)?;
    Ok(Some(FileIdentity {
        device: meta.dev(),
        inode: meta.ino(),
    }))
}

/// Elsewhere none: Windows canonical paths already come back in on-disk case, and its file
/// index is not stable on every file system (and reading it needs unsafe code).
#[cfg(not(unix))]
fn os_identity(_path: &Path) -> io::Result<Option<FileIdentity>> {
    Ok(None)
}

fn canonical_within(approved_root: &Path, path: &Path) -> io::Result<PathBuf> {
    let canonical_root = fs::canonicalize(approved_root)?;
    let canonical_path = fs::canonicalize(path)?;
    let root = canonical_share_path(&canonical_root)?;
    let candidate = canonical_share_path(&canonical_path)?;
    if candidate.is_within(&root) {
        Ok(canonical_path)
    } else {
        Err(path_refused())
    }
}

/// Converts an operating-system canonical path to the typed representation used for share
/// containment. Windows canonicalisation adds a device prefix, which is safe to remove only
/// after the operating system has resolved the path.
pub fn canonical_share_path(path: &Path) -> io::Result<SharePath> {
    let text = path.to_str().ok_or_else(path_refused)?;
    let normal = if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        text.to_string()
    };
    SharePath::parse(&normal).map_err(|_| path_refused())
}

fn path_refused() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, "path refused")
}

fn kind(meta: &fs::Metadata) -> FsKind {
    let kind = meta.file_type();
    if kind.is_symlink() {
        FsKind::Link
    } else if kind.is_file() {
        FsKind::File
    } else if kind.is_dir() {
        FsKind::Dir
    } else {
        FsKind::Other
    }
}

fn modified_ns(meta: &fs::Metadata) -> i64 {
    let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    match modified.duration_since(UNIX_EPOCH) {
        Ok(elapsed) => i64::try_from(elapsed.as_nanos()).unwrap_or(i64::MAX),
        Err(before) => {
            let magnitude = i64::try_from(before.duration().as_nanos()).unwrap_or(i64::MAX);
            magnitude.saturating_neg()
        }
    }
}

#[cfg(windows)]
fn attributes(meta: &fs::Metadata) -> u32 {
    use std::os::windows::fs::MetadataExt;
    meta.file_attributes()
}

#[cfg(not(windows))]
fn attributes(_meta: &fs::Metadata) -> u32 {
    0
}

/// Shared file-share read state.
pub struct Shares {
    fs: RwLock<Arc<dyn ShareFs>>,
    read: Arc<tokio::sync::Semaphore>,
    flights: Arc<Mutex<HashMap<NodeId, Arc<tokio::sync::Mutex<()>>>>>,
    offline_until: Mutex<HashMap<NodeId, Instant>>,
    read_wait: RwLock<Duration>,
    operation_timeout: RwLock<Duration>,
    cache_max_bytes_for_tests: Mutex<Option<u64>>,
    listing_progress: Mutex<HashMap<NodeId, Arc<AtomicUsize>>>,
    lookups: Arc<tokio::sync::Semaphore>,
    listing_took_for_tests: Mutex<Option<Duration>>,
}

/// How many canonical-path lookups a connect may have in flight across the whole server. A
/// lookup holds its permit for its whole blocking call, also after its caller stopped waiting,
/// so a hung share server can never tie up more than this many blocking threads.
pub const SHARE_LOOKUPS: usize = 8;

impl Default for Shares {
    fn default() -> Self {
        Self::new(Arc::new(OsFs))
    }
}

impl Shares {
    /// Makes share state around a file-system implementation.
    #[must_use]
    pub fn new(fs: Arc<dyn ShareFs>) -> Self {
        Self {
            fs: RwLock::new(fs),
            read: Arc::new(tokio::sync::Semaphore::new(crate::external::SHARE_READS)),
            flights: Arc::new(Mutex::new(HashMap::new())),
            offline_until: Mutex::new(HashMap::new()),
            read_wait: RwLock::new(crate::external::READ_WAIT),
            operation_timeout: RwLock::new(crate::external::OPERATION_WAIT),
            cache_max_bytes_for_tests: Mutex::new(None),
            listing_progress: Mutex::new(HashMap::new()),
            lookups: Arc::new(tokio::sync::Semaphore::new(SHARE_LOOKUPS)),
            listing_took_for_tests: Mutex::new(None),
        }
    }

    /// The server-wide bound on canonical-path lookups in flight; see [`SHARE_LOOKUPS`].
    #[must_use]
    pub fn lookups(&self) -> Arc<tokio::sync::Semaphore> {
        Arc::clone(&self.lookups)
    }

    /// The current file-system implementation.
    #[must_use]
    pub fn fs(&self) -> Arc<dyn ShareFs> {
        self.fs
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Replaces the file-system implementation. Used by tests.
    pub fn set_fs(&self, fs: Arc<dyn ShareFs>) {
        *self
            .fs
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = fs;
    }

    /// Files accepted so far by a share listing currently in progress.
    #[must_use]
    pub fn listing_progress(&self, library: NodeId) -> Option<usize> {
        self.listing_progress
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&library)
            .map(|count| count.load(Ordering::SeqCst))
    }

    fn begin_listing(&self, library: NodeId) -> ListingProgress<'_> {
        let count = Arc::new(AtomicUsize::new(0));
        self.listing_progress
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(library, Arc::clone(&count));
        ListingProgress {
            owner: self,
            library,
            count,
        }
    }

    /// Takes the single-flight turn for one external node. Its map entry disappears when the
    /// last holder or waiter drops, including when a request is cancelled.
    pub(crate) async fn flight(&self, node: NodeId) -> Flight {
        let lock = {
            let mut flights = self
                .flights
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            Arc::clone(
                flights
                    .entry(node)
                    .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
            )
        };
        let mut flight = Flight {
            node,
            lock,
            guard: None,
            flights: Arc::clone(&self.flights),
        };
        flight.guard = Some(Arc::clone(&flight.lock).lock_owned().await);
        flight
    }

    /// [`Shares::flight`], waited for no longer than a read turn is; `None` if it did not come
    /// free in time (a read of the node, perhaps stuck in the operating system, holds it).
    pub(crate) async fn flight_within(&self, node: NodeId) -> Option<Flight> {
        let wait = *self
            .read_wait
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        tokio::time::timeout(wait, self.flight(node)).await.ok()
    }

    pub(crate) async fn read_turn(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        let wait = *self
            .read_wait
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        tokio::time::timeout(wait, Arc::clone(&self.read).acquire_owned())
            .await
            .ok()
            .and_then(Result::ok)
    }

    pub(crate) fn offline(&self, library: NodeId) -> bool {
        let now = Instant::now();
        let mut offline = self
            .offline_until
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        offline.retain(|_, until| *until > now);
        offline.get(&library).is_some_and(|until| *until > now)
    }

    pub(crate) fn mark_offline(&self, library: NodeId, for_time: Duration) {
        self.offline_until
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(library, Instant::now() + for_time);
    }

    pub(crate) fn operation_timeout(&self) -> Duration {
        *self
            .operation_timeout
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) fn cache_max_bytes(&self, configured: u64) -> u64 {
        self.cache_max_bytes_for_tests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .unwrap_or(configured)
    }

    /// Sets the Busy wait without changing its 20-second production default.
    #[doc(hidden)]
    pub fn set_read_wait(&self, wait: Duration) {
        *self
            .read_wait
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = wait;
    }

    /// Sets the caller-side share operation deadline without changing its 30-second production
    /// default. Safe Rust cannot cancel a blocked operating-system read: up to four stuck OS
    /// operations can therefore occupy the read permits until the OS releases them. Cancelling
    /// those operations would require unsafe code or a new crate.
    #[doc(hidden)]
    pub fn set_operation_timeout(&self, timeout: Duration) {
        *self
            .operation_timeout
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = timeout;
    }

    /// Overrides cache accounting for integration tests without relaxing persisted settings.
    #[doc(hidden)]
    pub fn set_cache_max_bytes_for_tests(&self, bytes: u64) {
        *self
            .cache_max_bytes_for_tests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(bytes);
    }

    /// Makes every later share listing report that it took `took`, whatever it really took, so
    /// tests can drive the spacing after a slow listing without waiting that long.
    #[doc(hidden)]
    pub fn set_listing_took_for_tests(&self, took: Duration) {
        *self
            .listing_took_for_tests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(took);
    }

    /// How long a listing that really took `took` is taken to have lasted.
    fn listing_took(&self, took: Duration) -> Duration {
        self.listing_took_for_tests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .unwrap_or(took)
    }

    /// Number of node-flight entries. Used to prove cancellation cleanup in integration tests.
    #[doc(hidden)]
    pub fn flight_count_for_tests(&self) -> usize {
        self.flights
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    /// Reports the node-flight references so integration tests can observe a queued waiter.
    #[doc(hidden)]
    pub fn flight_references_for_tests(&self, node: NodeId) -> usize {
        self.flights
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&node)
            .map_or(0, Arc::strong_count)
    }
}

struct ListingProgress<'a> {
    owner: &'a Shares,
    library: NodeId,
    count: Arc<AtomicUsize>,
}

impl ListingProgress<'_> {
    fn counter(&self) -> &AtomicUsize {
        &self.count
    }
}

impl Drop for ListingProgress<'_> {
    fn drop(&mut self) {
        let mut progress = self
            .owner
            .listing_progress
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if progress
            .get(&self.library)
            .is_some_and(|current| Arc::ptr_eq(current, &self.count))
        {
            progress.remove(&self.library);
        }
    }
}

pub(crate) struct Flight {
    node: NodeId,
    lock: Arc<tokio::sync::Mutex<()>>,
    guard: Option<tokio::sync::OwnedMutexGuard<()>>,
    flights: Arc<Mutex<HashMap<NodeId, Arc<tokio::sync::Mutex<()>>>>>,
}

impl Drop for Flight {
    fn drop(&mut self) {
        self.guard.take();
        let mut flights = self
            .flights
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if Arc::strong_count(&self.lock) == 2
            && flights
                .get(&self.node)
                .is_some_and(|entry| Arc::ptr_eq(entry, &self.lock))
        {
            flights.remove(&self.node);
        }
    }
}

/// Limits applied to one complete listing.
pub struct ListLimits {
    pub max_files: usize,
    pub max_time: Duration,
    pub at_once: usize,
    pub max_depth: usize,
}

/// A complete, safe listing of one share root.
#[derive(Debug)]
pub struct ShareListing {
    pub entries: Vec<TreeEntry>,
    pub hidden: Hidden,
    pub unreadable: Vec<String>,
    pub temps: Vec<(String, i64)>,
    pub took: Duration,
}

/// Why a complete listing could not be produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListError {
    Unreachable,
    Denied,
    NotAFolder,
    TooManyFiles(usize),
    TooSlow,
    TooDeep,
    /// The library is being disconnected; the walk gave up.
    Stopped,
}

/// How often a listing waiting for a folder looks whether it was asked to stop.
const STOP_POLL: Duration = Duration::from_millis(50);

/// A connected file share as a storage source.
pub(crate) struct ShareSource;

impl StorageSource for ShareSource {
    type Seen = ();

    fn content(&self) -> ContentMode {
        ContentMode::External
    }

    async fn look(
        &self,
        state: &AppState,
        config: &SourceConfig,
        _status: &SourceStatus,
        _dirty: bool,
    ) -> Result<Look<Self::Seen>, SyncError> {
        let settings = state.db().share_settings().await.map_err(database_failed)?;
        let location = config.location.as_deref().ok_or_else(not_approved)?;
        let root = SharePath::parse(location).map_err(|_| not_approved())?;
        let approved = approved_root(&root, &settings.roots).ok_or_else(not_approved)?;
        let limits = ListLimits {
            max_files: settings.max_files,
            max_time: Duration::from_secs(settings.list_seconds),
            at_once: 4,
            max_depth: 100,
        };
        signed_in(state, &root).await?;
        let progress = state.shares().begin_listing(config.library);
        let library = config.library;
        let stopped = || state.sources().is_stopping(library);
        let list = || {
            list_share_progress(
                state.shares().fs(),
                approved,
                &root,
                &limits,
                Some(progress.counter()),
                Some(&stopped),
            )
        };
        let mut listed = list().await;
        // A refusal with a stored login may be a connection gone stale (the server restarted,
        // the session timed out): sign in again once, then list once more, then report.
        if matches!(listed, Err(ListError::Denied)) && signed_in_again(state, &root).await? {
            listed = list().await;
        }
        drop(progress);
        let mut listed = listed.map_err(|error| listing_failed(error, &settings))?;

        // A folder that shows nothing at all while the library holds items is far more often
        // an unmounted mount point (an empty local folder) or a share that came back empty
        // than a folder whose every file was deleted: nothing is removed on its word.
        if listed.entries.is_empty()
            && listed.unreadable.is_empty()
            && listed.hidden.total() == 0
            && !state
                .db()
                .source_items(config.library)
                .await
                .map_err(database_failed)?
                .is_empty()
        {
            return Err(SyncError::Failed(EMPTY_LISTING.to_owned()));
        }

        let mut protected = HashSet::new();
        if !listed.unreadable.is_empty() {
            let known = state
                .db()
                .source_items(config.library)
                .await
                .map_err(database_failed)?;
            for item in known {
                let protected_by_unreadable = listed.unreadable.iter().any(|folder| {
                    item.repository_path == *folder
                        || item
                            .repository_path
                            .strip_prefix(folder)
                            .is_some_and(|tail| tail.starts_with('/'))
                });
                if !protected_by_unreadable {
                    continue;
                }
                protected.insert(item.node);
                if listed
                    .entries
                    .iter()
                    .any(|entry| entry.path == item.repository_path)
                {
                    continue;
                }
                listed.entries.push(TreeEntry {
                    path: item.repository_path,
                    kind: if item.blob_sha.is_some() {
                        EntryKind::File
                    } else {
                        EntryKind::Dir
                    },
                    sha: item.blob_sha.unwrap_or_default(),
                    size: u64::try_from(item.size).unwrap_or(0),
                });
            }
            listed
                .entries
                .sort_by(|left, right| left.path.cmp(&right.path));
        }

        Ok(Look::Listing(Box::new(Listing {
            entries: listed.entries,
            protected,
            bookmark: String::new(),
            etag: None,
            credit: None,
            hidden: listed.hidden,
            took: state.shares().listing_took(listed.took),
            seen: (),
        })))
    }

    async fn list(
        &self,
        _state: &AppState,
        _listing: &mut Listing<Self::Seen>,
    ) -> Result<(), SyncError> {
        Ok(())
    }

    async fn caps(&self, state: &AppState) -> Result<SourceCaps, SyncError> {
        let settings = state.db().share_settings().await.map_err(database_failed)?;
        Ok(SourceCaps {
            max_files: settings.max_files,
            max_bytes: None,
        })
    }

    async fn fetch(
        &self,
        _state: &AppState,
        _config: &SourceConfig,
        _listing: &Listing<Self::Seen>,
        _caps: SourceCaps,
        _needed: &HashMap<String, u64>,
    ) -> Result<HashMap<String, Held>, SyncError> {
        Ok(HashMap::new())
    }
}

/// Signs in to the share of `root` with its server's stored login, if it has one.
///
/// # Errors
/// The library's plain error: a refused login stops it until an administrator sets the login
/// again (so a wrong password is not tried over and over); anything else is tried again later.
pub(crate) async fn signed_in(state: &AppState, root: &SharePath) -> Result<(), SyncError> {
    state
        .share_logins()
        .ensure(state, root)
        .await
        .map_err(|failure| login_failed(failure, root))
}

/// After a refusal: when the server of `root` has a stored login, drops the connection to its
/// share and signs in again, answering `true` (read once more). `false` when there is no
/// stored login, so the refusal stands.
///
/// # Errors
/// As [`signed_in`].
pub(crate) async fn signed_in_again(state: &AppState, root: &SharePath) -> Result<bool, SyncError> {
    let (Some(server), Some(remote)) = (root.server(), crate::share_login::remote_of(root)) else {
        return Ok(false);
    };
    let has_login = state
        .db()
        .share_login(&server)
        .await
        .map_err(database_failed)?
        .is_some();
    if !has_login {
        return Ok(false);
    }
    state.share_logins().invalidate(&remote);
    signed_in(state, root).await?;
    Ok(true)
}

fn login_failed(failure: crate::share_login::LoginFailure, root: &SharePath) -> SyncError {
    use crate::share_login::LoginFailure;
    let server = root.server().unwrap_or_default();
    let message = crate::share_login::failure_sentence(failure, &server);
    match failure {
        LoginFailure::Refused => SyncError::NotConnected(message),
        LoginFailure::Conflict
        | LoginFailure::Unreachable
        | LoginFailure::SecretStoreUnavailable
        | LoginFailure::Other(_) => SyncError::Failed(message),
    }
}

/// The failure recorded when a listing finds nothing at all while the library holds items.
pub const EMPTY_LISTING: &str = "The folder is empty or not mounted; nothing was removed.";

fn not_approved() -> SyncError {
    SyncError::NotConnected(
        "This folder is no longer approved for ShareCraft. An administrator can approve it again."
            .to_owned(),
    )
}

fn database_failed(error: impl std::fmt::Display) -> SyncError {
    eprintln!("sharecraft-server: database failure: {error}");
    SyncError::Failed("The database could not be read, so nothing was changed.".to_owned())
}

fn listing_failed(error: ListError, settings: &records::ShareSettings) -> SyncError {
    let message = match error {
        ListError::Denied => {
            "The server's account may not read this folder, so nothing was changed.".to_owned()
        }
        ListError::TooManyFiles(_) => format!(
            "The folder holds more than {} files, the most this server shows for one library, so nothing was changed.",
            settings.max_files
        ),
        ListError::TooSlow => format!(
            "Listing the folder took longer than {} seconds, so nothing was changed.",
            settings.list_seconds
        ),
        ListError::Stopped => {
            "The library is being disconnected, so its listing stopped and nothing was changed."
                .to_owned()
        }
        ListError::Unreachable | ListError::NotAFolder | ListError::TooDeep => {
            "The share could not be reached, so nothing was changed.".to_owned()
        }
    };
    SyncError::Failed(message)
}

/// Maps an operating-system listing failure to a stable category.
#[must_use]
pub fn kind_of_failure(error: &io::Error) -> ListError {
    match error.kind() {
        io::ErrorKind::PermissionDenied => ListError::Denied,
        _ => ListError::Unreachable,
    }
}

struct FolderAnswer {
    relative: String,
    path: PathBuf,
    depth: usize,
    root: bool,
    answer: io::Result<Vec<FsEntry>>,
}

fn blocking_permits() -> &'static Arc<tokio::sync::Semaphore> {
    static PERMITS: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    PERMITS.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(4)))
}

/// Walks a share breadth first on blocking threads and returns only a complete listing.
pub async fn list_share(
    fs: Arc<dyn ShareFs>,
    approved_root: &SharePath,
    root: &SharePath,
    limits: &ListLimits,
) -> Result<ShareListing, ListError> {
    list_share_progress(fs, approved_root, root, limits, None, None).await
}

/// [`list_share`], counting accepted files in `progress`, and giving up with
/// [`ListError::Stopped`] before any further folder is read once `stopped` answers `true`.
async fn list_share_progress(
    fs: Arc<dyn ShareFs>,
    approved_root: &SharePath,
    root: &SharePath,
    limits: &ListLimits,
    progress: Option<&AtomicUsize>,
    stopped: Option<&(dyn Fn() -> bool + Sync)>,
) -> Result<ShareListing, ListError> {
    let stop_asked = || stopped.is_some_and(|stopped| stopped());
    let started = Instant::now();
    let deadline = started.checked_add(limits.max_time).unwrap_or(started);
    let budget = FsBudget::new(limits.max_files, deadline);
    let approved_path = approved_root.to_path_buf();
    let root_path = root.to_path_buf();
    let stat_fs = Arc::clone(&fs);
    let stat_approved = approved_path.clone();
    let stat_path = root_path.clone();
    let stat_permit =
        unless_stopped(acquire_permit(started, limits.max_time), &stop_asked).await??;
    let stat = unless_stopped(
        wait_for(
            started,
            limits.max_time,
            tokio::task::spawn_blocking(move || {
                let _permit = stat_permit;
                stat_fs.stat(&stat_approved, &stat_path)
            }),
        ),
        &stop_asked,
    )
    .await??;
    let stat = stat.map_err(|error| kind_of_failure(&error))?;
    if stat.kind != FsKind::Dir {
        return Err(ListError::NotAFolder);
    }

    let mut queue = VecDeque::from([(root_path, String::new(), 0_usize, true)]);
    let mut entries = Vec::new();
    let mut hidden = Hidden::default();
    let mut unreadable = Vec::new();
    let mut temps = Vec::new();
    let at_once = limits.at_once.max(1);

    while !queue.is_empty() {
        if stop_asked() {
            return Err(ListError::Stopped);
        }
        if started.elapsed() > limits.max_time {
            return Err(ListError::TooSlow);
        }
        let mut turns = JoinSet::new();
        for _ in 0..at_once {
            let Some((path, relative, depth, root)) = queue.pop_front() else {
                break;
            };
            let permit =
                unless_stopped(acquire_permit(started, limits.max_time), &stop_asked).await??;
            let turn_fs = Arc::clone(&fs);
            let turn_approved = approved_path.clone();
            let turn_budget = budget.clone();
            turns.spawn_blocking(move || FolderAnswer {
                answer: {
                    let _permit = permit;
                    turn_fs.list(&turn_approved, &path, &turn_budget)
                },
                path,
                relative,
                depth,
                root,
            });
        }

        while !turns.is_empty() {
            // Waited for in short steps, so a stop is noticed while a slow folder is read.
            let joined = loop {
                if stop_asked() {
                    return Err(ListError::Stopped);
                }
                let remaining = limits.max_time.saturating_sub(started.elapsed());
                if remaining.is_zero() {
                    return Err(ListError::TooSlow);
                }
                if let Ok(joined) =
                    tokio::time::timeout(remaining.min(STOP_POLL), turns.join_next()).await
                {
                    break joined;
                }
            };
            let Some(joined) = joined else {
                break;
            };
            let folder = joined.map_err(|_| ListError::Unreachable)?;
            let children = match folder.answer {
                Ok(children) => children,
                Err(error) if error.kind() == io::ErrorKind::TimedOut => {
                    return Err(ListError::TooSlow);
                }
                Err(error) if error.kind() == io::ErrorKind::FileTooLarge => {
                    return Err(ListError::TooManyFiles(limits.max_files.saturating_add(1)));
                }
                Err(error) if folder.root => {
                    if error.kind() == io::ErrorKind::NotADirectory {
                        return Err(ListError::NotAFolder);
                    }
                    return Err(kind_of_failure(&error));
                }
                Err(error) if is_path_refusal(&error) => {
                    hidden.symlinks = hidden.symlinks.saturating_add(1);
                    continue;
                }
                Err(_) => {
                    hidden.unreadable = hidden.unreadable.saturating_add(1);
                    unreadable.push(folder.relative);
                    continue;
                }
            };

            for child in children {
                if started.elapsed() > limits.max_time {
                    return Err(ListError::TooSlow);
                }
                let Some(name) = child.name else {
                    hidden.unnameable = hidden.unnameable.saturating_add(1);
                    continue;
                };
                let relative = if folder.relative.is_empty() {
                    name.clone()
                } else {
                    format!("{}/{name}", folder.relative)
                };
                if reserved_name(&name) {
                    if name.to_ascii_lowercase().starts_with(".sharecraft-tmp-")
                        && child.kind == FsKind::File
                    {
                        temps.push((relative, child.mtime_ns));
                    }
                    continue;
                }
                if child.kind == FsKind::Other {
                    continue;
                }
                if child.kind == FsKind::Link {
                    hidden.symlinks = hidden.symlinks.saturating_add(1);
                    continue;
                }
                if child.attributes & (HIDDEN_ATTRIBUTE | SYSTEM_ATTRIBUTE) != 0
                    || hidden_by_default(&name)
                {
                    hidden.system = hidden.system.saturating_add(1);
                    continue;
                }
                if child.kind == FsKind::File && child.attributes & OFFLINE_ATTRIBUTES != 0 {
                    hidden.offline = hidden.offline.saturating_add(1);
                    continue;
                }
                if !windows_safe_name(&name) {
                    hidden.unnameable = hidden.unnameable.saturating_add(1);
                    continue;
                }
                if relative.chars().count() > MAX_RELATIVE_CHARS {
                    hidden.too_long = hidden.too_long.saturating_add(1);
                    continue;
                }
                let depth = folder.depth.saturating_add(1);
                if depth > limits.max_depth {
                    return Err(ListError::TooDeep);
                }
                match child.kind {
                    FsKind::File => {
                        entries.push(TreeEntry {
                            path: relative,
                            kind: EntryKind::File,
                            sha: external_tag(child.size, child.mtime_ns),
                            size: child.size,
                        });
                        if let Some(progress) = progress {
                            progress.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                    FsKind::Dir => {
                        entries.push(TreeEntry {
                            path: relative.clone(),
                            kind: EntryKind::Dir,
                            sha: String::new(),
                            size: 0,
                        });
                        queue.push_back((folder.path.join(name), relative, depth, false));
                    }
                    FsKind::Link | FsKind::Other => {}
                }
            }
        }
    }

    entries.sort_by(|left, right| left.path.cmp(&right.path));
    unreadable.sort();
    temps.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(ShareListing {
        entries,
        hidden,
        unreadable,
        temps,
        took: started.elapsed(),
    })
}

/// Waits for `future`, looking every [`STOP_POLL`] whether the listing was asked to stop; then
/// [`ListError::Stopped`] (the future is dropped; a blocking call it started finishes on its
/// own thread).
async fn unless_stopped<F: std::future::Future>(
    future: F,
    stop_asked: &impl Fn() -> bool,
) -> Result<F::Output, ListError> {
    tokio::pin!(future);
    loop {
        if stop_asked() {
            return Err(ListError::Stopped);
        }
        if let Ok(output) = tokio::time::timeout(STOP_POLL, &mut future).await {
            return Ok(output);
        }
    }
}

async fn acquire_permit(
    started: Instant,
    maximum: Duration,
) -> Result<tokio::sync::OwnedSemaphorePermit, ListError> {
    let remaining = maximum.saturating_sub(started.elapsed());
    if remaining.is_zero() {
        return Err(ListError::TooSlow);
    }
    tokio::time::timeout(remaining, Arc::clone(blocking_permits()).acquire_owned())
        .await
        .map_err(|_| ListError::TooSlow)?
        .map_err(|_| ListError::Unreachable)
}

fn is_path_refusal(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::PermissionDenied && error.to_string() == "path refused"
}

async fn wait_for<T>(
    started: Instant,
    maximum: Duration,
    task: tokio::task::JoinHandle<io::Result<T>>,
) -> Result<io::Result<T>, ListError> {
    let remaining = maximum.saturating_sub(started.elapsed());
    if remaining.is_zero() {
        return Err(ListError::TooSlow);
    }
    tokio::time::timeout(remaining, task)
        .await
        .map_err(|_| ListError::TooSlow)?
        .map_err(|_| ListError::Unreachable)
}
