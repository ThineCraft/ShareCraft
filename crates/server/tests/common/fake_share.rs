//! An in-memory file share used by the file-share integration tests.

use std::collections::{HashMap, HashSet};
use std::io::{self, Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use server::share::{FsEntry, FsKind, FsStat, ShareFs};

#[derive(Clone)]
pub struct FakeShare {
    inner: Arc<Inner>,
}

struct Inner {
    root: PathBuf,
    nodes: Mutex<HashMap<PathBuf, Node>>,
    denied: Mutex<HashSet<PathBuf>>,
    offline: AtomicBool,
    slow_open: Mutex<HashMap<PathBuf, Duration>>,
    slow_list: Mutex<Duration>,
    vanish_on_open: Mutex<HashSet<PathBuf>>,
    change_during_read: Mutex<HashMap<PathBuf, Vec<u8>>>,
    truncate_read: Mutex<HashMap<PathBuf, usize>>,
    lists: AtomicUsize,
    stats: AtomicUsize,
    opens: Mutex<HashMap<PathBuf, usize>>,
    opened_paths: Mutex<Vec<PathBuf>>,
}

#[derive(Clone)]
struct Node {
    name: Option<String>,
    kind: FsKind,
    bytes: Vec<u8>,
    mtime_ns: i64,
    attributes: u32,
    target: Option<PathBuf>,
}

impl FakeShare {
    pub fn new(root: &str) -> Self {
        let root = PathBuf::from(root);
        let mut nodes = HashMap::new();
        nodes.insert(
            root.clone(),
            Node::dir(root.file_name().map(|n| n.to_string_lossy().into())),
        );
        Self {
            inner: Arc::new(Inner {
                root,
                nodes: Mutex::new(nodes),
                denied: Mutex::new(HashSet::new()),
                offline: AtomicBool::new(false),
                slow_open: Mutex::new(HashMap::new()),
                slow_list: Mutex::new(Duration::ZERO),
                vanish_on_open: Mutex::new(HashSet::new()),
                change_during_read: Mutex::new(HashMap::new()),
                truncate_read: Mutex::new(HashMap::new()),
                lists: AtomicUsize::new(0),
                stats: AtomicUsize::new(0),
                opens: Mutex::new(HashMap::new()),
                opened_paths: Mutex::new(Vec::new()),
            }),
        }
    }

    pub fn root(&self) -> PathBuf {
        self.inner.root.clone()
    }

    pub fn put(&self, path: &str, bytes: impl AsRef<[u8]>) {
        self.put_at(path, bytes, 1);
    }

    pub fn put_at(&self, path: &str, bytes: impl AsRef<[u8]>, mtime_ns: i64) {
        let path = self.path(path);
        self.ensure_parents(&path);
        self.nodes().insert(
            path.clone(),
            Node {
                name: path.file_name().map(|n| n.to_string_lossy().into_owned()),
                kind: FsKind::File,
                bytes: bytes.as_ref().to_vec(),
                mtime_ns,
                attributes: 0,
                target: None,
            },
        );
    }

    pub fn put_unnameable(&self, path: &str, bytes: impl AsRef<[u8]>) {
        self.put(path, bytes);
        if let Some(node) = self.nodes().get_mut(&self.path(path)) {
            node.name = None;
        }
    }

    pub fn mkdir(&self, path: &str) {
        let path = self.path(path);
        self.ensure_parents(&path);
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
        self.nodes().insert(path, Node::dir(name));
    }

    pub fn remove(&self, path: &str) {
        let path = self.path(path);
        self.nodes()
            .retain(|candidate, _| !candidate.starts_with(&path));
    }

    pub fn rename(&self, from: &str, to: &str) {
        let from = self.path(from);
        let to = self.path(to);
        let moved: Vec<(PathBuf, Node)> = self
            .nodes()
            .iter()
            .filter(|(path, _)| path.starts_with(&from))
            .map(|(path, node)| (path.clone(), node.clone()))
            .collect();
        let mut nodes = self.nodes();
        for (old, _) in &moved {
            nodes.remove(old);
        }
        for (old, mut node) in moved {
            let suffix = old.strip_prefix(&from).unwrap_or(Path::new(""));
            let new = to.join(suffix);
            node.name = new.file_name().map(|n| n.to_string_lossy().into_owned());
            nodes.insert(new, node);
        }
    }

    pub fn link(&self, path: &str, target: &str) {
        let path = self.path(path);
        self.ensure_parents(&path);
        self.nodes().insert(
            path.clone(),
            Node {
                name: path.file_name().map(|n| n.to_string_lossy().into_owned()),
                kind: FsKind::Link,
                bytes: Vec::new(),
                mtime_ns: 1,
                attributes: 0,
                target: Some(self.path(target)),
            },
        );
    }

    pub fn set_attributes(&self, path: &str, bits: u32) {
        if let Some(node) = self.nodes().get_mut(&self.path(path)) {
            node.attributes = bits;
        }
    }

    pub fn deny(&self, path: &str) {
        self.denied().insert(self.path(path));
    }

    pub fn offline(&self, offline: bool) {
        self.inner.offline.store(offline, Ordering::SeqCst);
    }

    pub fn slow_open(&self, path: &str, duration: Duration) {
        self.slow_opens().insert(self.path(path), duration);
    }

    pub fn slow_list(&self, duration: Duration) {
        *self.slow_listing() = duration;
    }

    pub fn vanish_on_open(&self, path: &str) {
        self.vanishing().insert(self.path(path));
    }

    pub fn change_during_read(&self, path: &str, new_bytes: impl AsRef<[u8]>) {
        self.changes()
            .insert(self.path(path), new_bytes.as_ref().to_vec());
    }

    pub fn truncate_read(&self, path: &str, at: usize) {
        self.truncations().insert(self.path(path), at);
    }

    pub fn lists(&self) -> usize {
        self.inner.lists.load(Ordering::SeqCst)
    }

    pub fn stats(&self) -> usize {
        self.inner.stats.load(Ordering::SeqCst)
    }

    pub fn opens(&self, path: &str) -> usize {
        self.opens_map().get(&self.path(path)).copied().unwrap_or(0)
    }

    pub fn opened_paths(&self) -> Vec<PathBuf> {
        self.inner
            .opened_paths
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn path(&self, path: &str) -> PathBuf {
        let candidate = Path::new(path);
        if candidate.is_absolute() || candidate.starts_with(&self.inner.root) {
            candidate.to_path_buf()
        } else {
            self.inner.root.join(path.replace('/', "\\"))
        }
    }

    fn ensure_parents(&self, path: &Path) {
        let mut missing = Vec::new();
        let mut parent = path.parent();
        let nodes = self.nodes();
        while let Some(candidate) = parent {
            if nodes.contains_key(candidate) {
                break;
            }
            missing.push(candidate.to_path_buf());
            parent = candidate.parent();
        }
        drop(nodes);
        let mut nodes = self.nodes();
        for dir in missing.into_iter().rev() {
            let name = dir.file_name().map(|n| n.to_string_lossy().into_owned());
            nodes.insert(dir, Node::dir(name));
        }
    }

    fn nodes(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, Node>> {
        self.inner.nodes.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn denied(&self) -> std::sync::MutexGuard<'_, HashSet<PathBuf>> {
        self.inner.denied.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn slow_opens(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, Duration>> {
        self.inner
            .slow_open
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn slow_listing(&self) -> std::sync::MutexGuard<'_, Duration> {
        self.inner
            .slow_list
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn vanishing(&self) -> std::sync::MutexGuard<'_, HashSet<PathBuf>> {
        self.inner
            .vanish_on_open
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn changes(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, Vec<u8>>> {
        self.inner
            .change_during_read
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn truncations(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, usize>> {
        self.inner
            .truncate_read
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn opens_map(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, usize>> {
        self.inner.opens.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn unavailable(&self, path: &Path) -> io::Result<()> {
        if self.inner.offline.load(Ordering::SeqCst) {
            return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "offline"));
        }
        if self.denied().contains(path) {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "denied"));
        }
        Ok(())
    }
}

impl Node {
    fn dir(name: Option<String>) -> Self {
        Self {
            name,
            kind: FsKind::Dir,
            bytes: Vec::new(),
            mtime_ns: 1,
            attributes: 0,
            target: None,
        }
    }

    fn stat(&self) -> FsStat {
        FsStat {
            kind: self.kind,
            size: self.bytes.len() as u64,
            mtime_ns: self.mtime_ns,
            attributes: self.attributes,
        }
    }
}

impl ShareFs for FakeShare {
    fn list(&self, dir: &Path) -> io::Result<Vec<FsEntry>> {
        self.inner.lists.fetch_add(1, Ordering::SeqCst);
        self.unavailable(dir)?;
        let delay = *self.slow_listing();
        if !delay.is_zero() {
            std::thread::sleep(delay);
        }
        let nodes = self.nodes();
        let Some(parent) = nodes.get(dir) else {
            return Err(io::Error::new(io::ErrorKind::NotFound, "missing"));
        };
        if parent.kind != FsKind::Dir {
            return Err(io::Error::new(io::ErrorKind::NotADirectory, "not a folder"));
        }
        let mut entries: Vec<FsEntry> = nodes
            .iter()
            .filter(|(path, _)| path.parent() == Some(dir))
            .map(|(_, node)| FsEntry {
                name: node.name.clone(),
                kind: node.kind,
                size: node.bytes.len() as u64,
                mtime_ns: node.mtime_ns,
                attributes: node.attributes,
            })
            .collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    fn stat(&self, path: &Path) -> io::Result<FsStat> {
        self.inner.stats.fetch_add(1, Ordering::SeqCst);
        self.unavailable(path)?;
        self.nodes()
            .get(path)
            .map(Node::stat)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "missing"))
    }

    fn open(&self, path: &Path) -> io::Result<Box<dyn Read + Send>> {
        self.unavailable(path)?;
        if let Some(delay) = self.slow_opens().get(path).copied() {
            std::thread::sleep(delay);
        }
        if self.vanishing().remove(path) {
            self.nodes().remove(path);
            return Err(io::Error::new(io::ErrorKind::NotFound, "missing"));
        }
        *self.opens_map().entry(path.to_path_buf()).or_default() += 1;
        self.inner
            .opened_paths
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(path.to_path_buf());
        let mut nodes = self.nodes();
        let Some(node) = nodes.get_mut(path) else {
            return Err(io::Error::new(io::ErrorKind::NotFound, "missing"));
        };
        if node.kind != FsKind::File {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a file"));
        }
        let bytes = node.bytes.clone();
        if let Some(changed) = self.changes().remove(path) {
            node.bytes = changed;
            node.mtime_ns = node.mtime_ns.saturating_add(1);
        }
        let at = self.truncations().get(path).copied().unwrap_or(bytes.len());
        Ok(
            Box::new(Cursor::new(bytes.into_iter().take(at).collect::<Vec<_>>()))
                as Box<dyn Read + Send>,
        )
    }

    fn canonical(&self, path: &Path) -> io::Result<PathBuf> {
        self.unavailable(path)?;
        let nodes = self.nodes();
        let Some(node) = nodes.get(path) else {
            return Err(io::Error::new(io::ErrorKind::NotFound, "missing"));
        };
        Ok(node.target.clone().unwrap_or_else(|| path.to_path_buf()))
    }
}
