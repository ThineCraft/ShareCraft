//! The source-independent seam used by connected-library synchronisation.

use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;

use records::{SourceConfig, SourceStatus};

use crate::AppState;
use crate::blobs::Held;
use crate::source_sync::{Hidden, SyncError};

/// What a tree entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    /// A symbolic link (mode 120000): its blob is the link target, not content.
    Symlink,
    /// A submodule: its id is a commit in another repository.
    Submodule,
}

/// One entry of a recursive tree listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    /// Its path from the repository root, `/`-separated.
    pub path: String,
    pub kind: EntryKind,
    /// The Git id of its blob, tree or (for a submodule) commit.
    pub sha: String,
    /// Its size in bytes for a file or symbolic link; 0 otherwise.
    pub size: u64,
}

/// Whether source content is copied into the content store during a sync or read on demand.
#[derive(Clone, Copy)]
pub(crate) enum ContentMode {
    Mirrored,
    #[allow(dead_code)]
    External,
}

/// What asking a source for its current contents found.
pub(crate) enum Look<S> {
    Unchanged { etag: Option<String> },
    Listing(Listing<S>),
}

/// A complete source listing and the source-specific state needed to fetch its content.
pub(crate) struct Listing<S> {
    pub entries: Vec<TreeEntry>,
    pub bookmark: String,
    pub etag: Option<String>,
    pub credit: Option<String>,
    pub hidden: Hidden,
    #[allow(dead_code)]
    pub took: Duration,
    pub seen: S,
}

/// One snapshot of a source's per-library limits for a sync.
#[derive(Clone, Copy)]
pub(crate) struct SourceCaps {
    pub max_files: usize,
    pub max_bytes: Option<u64>,
}

impl SourceCaps {
    pub fn exceeded_by(self, files: usize, bytes: u64) -> bool {
        files > self.max_files || self.max_bytes.is_some_and(|maximum| bytes > maximum)
    }
}

/// A connected-library storage source.
pub(crate) trait StorageSource: Send + Sync {
    type Seen: Send + Sync;

    fn content(&self) -> ContentMode;

    fn look(
        &self,
        state: &AppState,
        config: &SourceConfig,
        status: &SourceStatus,
        dirty: bool,
    ) -> impl Future<Output = Result<Look<Self::Seen>, SyncError>> + Send;

    fn list(
        &self,
        state: &AppState,
        listing: &mut Listing<Self::Seen>,
    ) -> impl Future<Output = Result<(), SyncError>> + Send;

    fn caps(&self, state: &AppState) -> impl Future<Output = Result<SourceCaps, SyncError>> + Send;

    fn fetch(
        &self,
        state: &AppState,
        config: &SourceConfig,
        listing: &Listing<Self::Seen>,
        caps: SourceCaps,
        needed: &HashMap<String, u64>,
    ) -> impl Future<Output = Result<HashMap<String, Held>, SyncError>> + Send;
}
