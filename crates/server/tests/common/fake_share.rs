//! An in-memory file share used by the file-share integration tests.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use records::SharePath;
use server::share::{FsBudget, FsEntry, FsKind, FsStat, ShareFs};

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
    slow_read: Mutex<HashMap<PathBuf, Duration>>,
    slow_stat: Mutex<HashMap<PathBuf, Duration>>,
    slow_list: Mutex<Duration>,
    pause_list_after_read: (Mutex<bool>, Condvar),
    hang_canonical: (Mutex<HashSet<PathBuf>>, Condvar),
    canonical_hanging: AtomicUsize,
    canonical_hanging_peak: AtomicUsize,
    vanish_on_open: Mutex<HashSet<PathBuf>>,
    change_during_read: Mutex<HashMap<PathBuf, VecDeque<Vec<u8>>>>,
    change_after_stat: Mutex<HashMap<PathBuf, VecDeque<Vec<u8>>>>,
    truncate_read: Mutex<HashMap<PathBuf, usize>>,
    read_error: Mutex<HashMap<PathBuf, io::ErrorKind>>,
    lists: AtomicUsize,
    active_lists: AtomicUsize,
    maximum_active_lists: AtomicUsize,
    stats: AtomicUsize,
    opens: Mutex<HashMap<PathBuf, usize>>,
    open_started: AtomicUsize,
    active_opens: AtomicUsize,
    maximum_active_opens: AtomicUsize,
    read_started: AtomicUsize,
    stat_delayed: AtomicUsize,
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
                slow_read: Mutex::new(HashMap::new()),
                slow_stat: Mutex::new(HashMap::new()),
                slow_list: Mutex::new(Duration::ZERO),
                pause_list_after_read: (Mutex::new(false), Condvar::new()),
                hang_canonical: (Mutex::new(HashSet::new()), Condvar::new()),
                canonical_hanging: AtomicUsize::new(0),
                canonical_hanging_peak: AtomicUsize::new(0),
                vanish_on_open: Mutex::new(HashSet::new()),
                change_during_read: Mutex::new(HashMap::new()),
                change_after_stat: Mutex::new(HashMap::new()),
                truncate_read: Mutex::new(HashMap::new()),
                read_error: Mutex::new(HashMap::new()),
                lists: AtomicUsize::new(0),
                active_lists: AtomicUsize::new(0),
                maximum_active_lists: AtomicUsize::new(0),
                stats: AtomicUsize::new(0),
                opens: Mutex::new(HashMap::new()),
                open_started: AtomicUsize::new(0),
                active_opens: AtomicUsize::new(0),
                maximum_active_opens: AtomicUsize::new(0),
                read_started: AtomicUsize::new(0),
                stat_delayed: AtomicUsize::new(0),
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

    pub fn slow_read(&self, path: &str, duration: Duration) {
        self.inner
            .slow_read
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(self.path(path), duration);
    }

    pub fn read_started(&self) -> usize {
        self.inner.read_started.load(Ordering::SeqCst)
    }

    pub fn maximum_active_opens(&self) -> usize {
        self.inner.maximum_active_opens.load(Ordering::SeqCst)
    }

    pub fn slow_stat(&self, path: &str, duration: Duration) {
        self.inner
            .slow_stat
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(self.path(path), duration);
    }

    pub fn stat_delayed(&self) -> usize {
        self.inner.stat_delayed.load(Ordering::SeqCst)
    }

    pub fn slow_list(&self, duration: Duration) {
        *self.slow_listing() = duration;
    }

    pub fn pause_list_after_read(&self) {
        *self
            .inner
            .pause_list_after_read
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = true;
    }

    pub fn resume_list_after_read(&self) {
        *self
            .inner
            .pause_list_after_read
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = false;
        self.inner.pause_list_after_read.1.notify_all();
    }

    /// Makes every canonical resolution of `path` block until [`FakeShare::release_canonical`],
    /// like a `CreateFile` on an unreachable server.
    pub fn hang_canonical(&self, path: &str) {
        let path = self.path(path);
        self.inner
            .hang_canonical
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(path);
    }

    pub fn release_canonical(&self) {
        self.inner
            .hang_canonical
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
        self.inner.hang_canonical.1.notify_all();
    }

    /// How many canonical resolutions are blocked by [`FakeShare::hang_canonical`] now.
    pub fn canonical_hanging(&self) -> usize {
        self.inner.canonical_hanging.load(Ordering::SeqCst)
    }

    /// The most canonical resolutions ever blocked by [`FakeShare::hang_canonical`] at once.
    pub fn canonical_hanging_peak(&self) -> usize {
        self.inner.canonical_hanging_peak.load(Ordering::SeqCst)
    }

    pub fn vanish_on_open(&self, path: &str) {
        self.vanishing().insert(self.path(path));
    }

    pub fn change_during_read(&self, path: &str, new_bytes: impl AsRef<[u8]>) {
        self.changes()
            .entry(self.path(path))
            .or_default()
            .push_back(new_bytes.as_ref().to_vec());
    }

    pub fn change_after_stat(&self, path: &str, new_bytes: impl AsRef<[u8]>) {
        self.inner
            .change_after_stat
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(self.path(path))
            .or_default()
            .push_back(new_bytes.as_ref().to_vec());
    }

    pub fn truncate_read(&self, path: &str, at: usize) {
        self.truncations().insert(self.path(path), at);
    }

    pub fn fail_during_read(&self, path: &str, kind: io::ErrorKind) {
        self.read_errors().insert(self.path(path), kind);
    }

    pub fn lists(&self) -> usize {
        self.inner.lists.load(Ordering::SeqCst)
    }

    pub fn stats(&self) -> usize {
        self.inner.stats.load(Ordering::SeqCst)
    }

    pub fn maximum_active_lists(&self) -> usize {
        self.inner.maximum_active_lists.load(Ordering::SeqCst)
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

    pub fn open_started(&self) -> usize {
        self.inner.open_started.load(Ordering::SeqCst)
    }

    fn path(&self, path: &str) -> PathBuf {
        let candidate = Path::new(path);
        if candidate.is_absolute() || candidate.starts_with(&self.inner.root) {
            candidate.to_path_buf()
        } else if cfg!(windows) {
            self.inner.root.join(path.replace('/', "\\"))
        } else {
            self.inner.root.join(path.replace('\\', "/"))
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

    fn changes(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, VecDeque<Vec<u8>>>> {
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

    fn read_errors(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, io::ErrorKind>> {
        self.inner
            .read_error
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

    fn resolve(&self, path: &Path) -> io::Result<PathBuf> {
        let nodes = self.nodes();
        let mut current = path.to_path_buf();
        let mut seen = HashSet::new();
        loop {
            if !seen.insert(current.clone()) {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "link cycle"));
            }

            let components: Vec<_> = current
                .components()
                .map(|component| component.as_os_str().to_owned())
                .collect();
            let mut prefix = PathBuf::new();
            let mut replacement = None;
            for (position, component) in components.iter().enumerate() {
                prefix.push(component);
                let Some(target) = nodes.get(&prefix).and_then(|node| node.target.as_ref()) else {
                    continue;
                };
                let mut resolved = target.clone();
                for suffix in components.iter().skip(position.saturating_add(1)) {
                    resolved.push(suffix);
                }
                replacement = Some(resolved);
                break;
            }

            if let Some(resolved) = replacement {
                current = resolved;
                continue;
            }
            return nodes
                .contains_key(&current)
                .then_some(current)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "missing"));
        }
    }

    fn contained(&self, approved_root: &Path, path: &Path) -> io::Result<PathBuf> {
        let root = self.resolve(approved_root)?;
        let candidate = self.resolve(path)?;
        let root = root
            .to_str()
            .and_then(|text| SharePath::parse(text).ok())
            .ok_or_else(path_refused)?;
        let parsed = candidate
            .to_str()
            .and_then(|text| SharePath::parse(text).ok())
            .ok_or_else(path_refused)?;
        if parsed.is_within(&root) {
            Ok(candidate)
        } else {
            Err(path_refused())
        }
    }
}

struct ActiveList<'a>(&'a AtomicUsize);

impl Drop for ActiveList<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn path_refused() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, "path refused")
}

/// A test path written in Windows form, given in this system's own form: unchanged on
/// Windows, and elsewhere without its drive letter and with `/` separators, so
/// `C:\approved\held` is `/approved/held` on Linux. Tests about UNC or drive paths
/// themselves name them directly instead.
#[allow(dead_code)]
pub fn native(windows: &str) -> String {
    if cfg!(windows) {
        return windows.to_owned();
    }
    let mut chars = windows.chars();
    let rest = match (chars.next(), chars.next()) {
        (Some(letter), Some(':')) if letter.is_ascii_alphabetic() => chars.as_str(),
        _ => windows,
    };
    rest.replace('\\', "/")
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
    fn list(
        &self,
        approved_root: &Path,
        dir: &Path,
        budget: &FsBudget,
    ) -> io::Result<Vec<FsEntry>> {
        self.inner.lists.fetch_add(1, Ordering::SeqCst);
        let active = self.inner.active_lists.fetch_add(1, Ordering::SeqCst) + 1;
        self.inner
            .maximum_active_lists
            .fetch_max(active, Ordering::SeqCst);
        let _active = ActiveList(&self.inner.active_lists);
        self.unavailable(dir)?;
        let delay = *self.slow_listing();
        if !delay.is_zero() {
            std::thread::sleep(delay);
        }
        let canonical = self.canonical(approved_root, dir)?;
        if self
            .nodes()
            .get(dir)
            .is_some_and(|node| node.kind == FsKind::Link)
        {
            return Err(io::Error::new(io::ErrorKind::NotADirectory, "not a folder"));
        }
        let dir = self.canonical(approved_root, &canonical)?;
        let nodes = self.nodes();
        let Some(parent) = nodes.get(&dir) else {
            return Err(io::Error::new(io::ErrorKind::NotFound, "missing"));
        };
        if parent.kind != FsKind::Dir {
            return Err(io::Error::new(io::ErrorKind::NotADirectory, "not a folder"));
        }
        let mut children: Vec<_> = nodes
            .iter()
            .filter(|(path, _)| path.parent() == Some(dir.as_path()))
            .collect();
        children.sort_by(|left, right| left.1.name.cmp(&right.1.name));
        let mut entries = Vec::new();
        for (_, node) in children {
            budget.claim()?;
            entries.push(FsEntry {
                name: node.name.clone(),
                kind: node.kind,
                size: node.bytes.len() as u64,
                mtime_ns: node.mtime_ns,
                attributes: node.attributes,
            });
        }
        drop(nodes);
        let (paused, wake) = &self.inner.pause_list_after_read;
        let mut paused = paused.lock().unwrap_or_else(|error| error.into_inner());
        while *paused {
            paused = wake.wait(paused).unwrap_or_else(|error| error.into_inner());
        }
        Ok(entries)
    }

    fn stat(&self, approved_root: &Path, path: &Path) -> io::Result<FsStat> {
        self.inner.stats.fetch_add(1, Ordering::SeqCst);
        self.unavailable(path)?;
        let canonical = self.canonical(approved_root, path)?;
        let mut nodes = self.nodes();
        let stat = nodes
            .get(path)
            .or_else(|| nodes.get(&canonical))
            .map(Node::stat)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "missing"))?;
        let changed = {
            let mut changes = self
                .inner
                .change_after_stat
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let changed = changes.get_mut(path).and_then(VecDeque::pop_front);
            if changes.get(path).is_some_and(VecDeque::is_empty) {
                changes.remove(path);
            }
            changed
        };
        if let Some(changed) = changed
            && let Some(node) = nodes.get_mut(&canonical)
        {
            node.bytes = changed;
            node.mtime_ns = node.mtime_ns.saturating_add(1);
        }
        drop(nodes);
        let delay = self
            .inner
            .slow_stat
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(path)
            .copied();
        if let Some(delay) = delay {
            self.inner.stat_delayed.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(delay);
        }
        Ok(stat)
    }

    fn open(&self, approved_root: &Path, path: &Path) -> io::Result<Box<dyn Read + Send>> {
        self.unavailable(path)?;
        self.inner.open_started.fetch_add(1, Ordering::SeqCst);
        let active = self.inner.active_opens.fetch_add(1, Ordering::SeqCst) + 1;
        self.inner
            .maximum_active_opens
            .fetch_max(active, Ordering::SeqCst);
        let _active = ActiveList(&self.inner.active_opens);
        let delay = self.slow_opens().get(path).copied();
        if let Some(delay) = delay {
            std::thread::sleep(delay);
        }
        let canonical = self.canonical(approved_root, path)?;
        if self
            .nodes()
            .get(path)
            .is_some_and(|node| node.kind == FsKind::Link)
        {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a file"));
        }
        let canonical = self.canonical(approved_root, &canonical)?;
        if self.vanishing().remove(path) {
            self.nodes().remove(&canonical);
            return Err(io::Error::new(io::ErrorKind::NotFound, "missing"));
        }
        *self.opens_map().entry(path.to_path_buf()).or_default() += 1;
        self.inner
            .opened_paths
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(path.to_path_buf());
        let mut nodes = self.nodes();
        let Some(node) = nodes.get_mut(&canonical) else {
            return Err(io::Error::new(io::ErrorKind::NotFound, "missing"));
        };
        if node.kind != FsKind::File {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a file"));
        }
        let bytes = node.bytes.clone();
        let changed = {
            let mut changes = self.changes();
            let changed = changes.get_mut(path).and_then(VecDeque::pop_front);
            if changes.get(path).is_some_and(VecDeque::is_empty) {
                changes.remove(path);
            }
            changed
        };
        if let Some(changed) = changed {
            node.bytes = changed;
            node.mtime_ns = node.mtime_ns.saturating_add(1);
        }
        let at = self.truncations().get(path).copied().unwrap_or(bytes.len());
        let bytes = bytes.into_iter().take(at).collect::<Vec<_>>();
        if let Some(kind) = self.read_errors().remove(path) {
            return Ok(Box::new(FailingReader {
                bytes: Cursor::new(bytes),
                kind,
                read_once: false,
            }));
        }
        if let Some(delay) = self
            .inner
            .slow_read
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(path)
            .copied()
        {
            return Ok(Box::new(SlowReader {
                bytes: Cursor::new(bytes),
                delay,
                started: Arc::clone(&self.inner),
                delayed: false,
            }));
        }
        Ok(Box::new(Cursor::new(bytes)) as Box<dyn Read + Send>)
    }

    fn canonical(&self, approved_root: &Path, path: &Path) -> io::Result<PathBuf> {
        {
            let (hanging, wake) = &self.inner.hang_canonical;
            let mut hanging = hanging.lock().unwrap_or_else(|error| error.into_inner());
            if hanging.contains(path) {
                let now = self.inner.canonical_hanging.fetch_add(1, Ordering::SeqCst) + 1;
                self.inner
                    .canonical_hanging_peak
                    .fetch_max(now, Ordering::SeqCst);
                while hanging.contains(path) {
                    hanging = wake
                        .wait(hanging)
                        .unwrap_or_else(|error| error.into_inner());
                }
                self.inner.canonical_hanging.fetch_sub(1, Ordering::SeqCst);
            }
        }
        self.unavailable(path)?;
        self.contained(approved_root, path)
    }
}

struct SlowReader {
    bytes: Cursor<Vec<u8>>,
    delay: Duration,
    started: Arc<Inner>,
    delayed: bool,
}

impl Read for SlowReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if !self.delayed {
            self.delayed = true;
            self.started.read_started.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(self.delay);
        }
        self.bytes.read(buf)
    }
}

struct FailingReader {
    bytes: Cursor<Vec<u8>>,
    kind: io::ErrorKind,
    read_once: bool,
}

impl Read for FailingReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.read_once {
            return Err(io::Error::new(self.kind, "injected read failure"));
        }
        self.read_once = true;
        self.bytes.read(buf)
    }
}
