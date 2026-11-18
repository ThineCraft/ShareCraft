//! Read-only access to file shares and bounded directory listings.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use collab_core::ids::NodeId;
use records::{SharePath, external_tag, hidden_by_default, reserved_name, windows_safe_name};
use tokio::task::JoinSet;

use crate::source::{EntryKind, TreeEntry};
use crate::source_sync::Hidden;

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
pub trait ShareFs: Send + Sync + 'static {
    /// Lists one folder and never follows a link.
    fn list(&self, dir: &Path) -> io::Result<Vec<FsEntry>>;
    /// Reads metadata and never follows a link.
    fn stat(&self, path: &Path) -> io::Result<FsStat>;
    /// Opens one file read-only.
    fn open(&self, path: &Path) -> io::Result<Box<dyn Read + Send>>;
    /// Resolves links for a containment check.
    fn canonical(&self, path: &Path) -> io::Result<PathBuf>;
}

/// The operating-system implementation of [`ShareFs`].
pub struct OsFs;

impl ShareFs for OsFs {
    fn list(&self, dir: &Path) -> io::Result<Vec<FsEntry>> {
        let dir_meta = fs::symlink_metadata(dir)?;
        if kind(&dir_meta) != FsKind::Dir {
            return Err(io::Error::new(io::ErrorKind::NotADirectory, "not a folder"));
        }
        fs::read_dir(dir)?
            .map(|answer| {
                let entry = answer?;
                let meta = fs::symlink_metadata(entry.path())?;
                Ok(FsEntry {
                    name: entry.file_name().into_string().ok(),
                    kind: kind(&meta),
                    size: meta.len(),
                    mtime_ns: modified_ns(&meta),
                    attributes: attributes(&meta),
                })
            })
            .collect()
    }

    fn stat(&self, path: &Path) -> io::Result<FsStat> {
        let meta = fs::symlink_metadata(path)?;
        Ok(FsStat {
            kind: kind(&meta),
            size: meta.len(),
            mtime_ns: modified_ns(&meta),
            attributes: attributes(&meta),
        })
    }

    fn open(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
        fs::File::open(path).map(|file| Box::new(file) as Box<dyn Read + Send>)
    }

    fn canonical(&self, path: &Path) -> io::Result<PathBuf> {
        fs::canonicalize(path)
    }
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
    #[allow(dead_code)]
    read: Arc<tokio::sync::Semaphore>,
    #[allow(dead_code)]
    flights: Mutex<HashMap<NodeId, Arc<tokio::sync::Mutex<()>>>>,
    #[allow(dead_code)]
    offline_until: Mutex<HashMap<NodeId, Instant>>,
}

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
            read: Arc::new(tokio::sync::Semaphore::new(4)),
            flights: Mutex::new(HashMap::new()),
            offline_until: Mutex::new(HashMap::new()),
        }
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

/// Walks a share breadth first on blocking threads and returns only a complete listing.
pub async fn list_share(
    fs: Arc<dyn ShareFs>,
    root: &SharePath,
    limits: &ListLimits,
) -> Result<ShareListing, ListError> {
    let started = Instant::now();
    let root_path = root.to_path_buf();
    let stat_fs = Arc::clone(&fs);
    let stat_path = root_path.clone();
    let stat = wait_for(
        started,
        limits.max_time,
        tokio::task::spawn_blocking(move || stat_fs.stat(&stat_path)),
    )
    .await?;
    let stat = stat.map_err(|error| kind_of_failure(&error))?;
    if stat.kind != FsKind::Dir {
        return Err(ListError::NotAFolder);
    }

    let mut queue = VecDeque::from([(root_path, String::new(), 0_usize, true)]);
    let mut entries = Vec::new();
    let mut hidden = Hidden::default();
    let mut unreadable = Vec::new();
    let mut temps = Vec::new();
    let mut files = 0_usize;
    let at_once = limits.at_once.max(1);

    while !queue.is_empty() {
        if started.elapsed() > limits.max_time {
            return Err(ListError::TooSlow);
        }
        let mut turns = JoinSet::new();
        for _ in 0..at_once {
            let Some((path, relative, depth, root)) = queue.pop_front() else {
                break;
            };
            let turn_fs = Arc::clone(&fs);
            turns.spawn_blocking(move || FolderAnswer {
                answer: turn_fs.list(&path),
                path,
                relative,
                depth,
                root,
            });
        }

        while !turns.is_empty() {
            let remaining = limits.max_time.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Err(ListError::TooSlow);
            }
            let joined = tokio::time::timeout(remaining, turns.join_next())
                .await
                .map_err(|_| ListError::TooSlow)?;
            let Some(joined) = joined else {
                break;
            };
            let folder = joined.map_err(|_| ListError::Unreachable)?;
            let children = match folder.answer {
                Ok(children) => children,
                Err(error) if folder.root => {
                    if error.kind() == io::ErrorKind::NotADirectory {
                        return Err(ListError::NotAFolder);
                    }
                    return Err(kind_of_failure(&error));
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
                        files = files.saturating_add(1);
                        if files > limits.max_files {
                            return Err(ListError::TooManyFiles(files));
                        }
                        entries.push(TreeEntry {
                            path: relative,
                            kind: EntryKind::File,
                            sha: external_tag(child.size, child.mtime_ns),
                            size: child.size,
                        });
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
