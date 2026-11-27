//! Keeping a connected library in step with a branch of a GitHub repository.
//!
//! A sync ([`sync_once`]) reads the branch's head with a conditional request (an unchanged
//! branch costs no rate limit and changes nothing but `last_checked_at`), then its tree. What
//! the library shows of the tree ([`shown_names`], then the library's file policies and the
//! largest file the server takes) is compared with what it holds ([`plan`]). Every byte the
//! plan needs is read (one tarball when more than [`TARBALL_FROM`] files are needed, otherwise
//! single blobs, four at a time), checked against its Git blob id and stored in the content
//! store **before anything in the library changes**: a sync that fails while reading changes
//! nothing. Then the plan is applied file by file through the tree and upload functions every
//! other change uses, as the person who connected the library, each item recorded in
//! `source_items` as it is done, so a sync cut short leaves a state the next one repairs: a
//! node found by name but not recorded is adopted, never duplicated.
//!
//! Removed files go to the site's recycle bin. Sync bytes are counted in the site's usage but
//! never refused for its quota. Uploaded content is held for the malware scanner as any upload
//! is. One sync at a time per library: a PostgreSQL advisory lock.
//!
//! The worker ([`start_worker`], [`tick_once`]) syncs the libraries that are due (their poll
//! time passed and no retry time ahead) every minute, and a library at once when it is woken
//! ([`Sources::wake`]: a webhook delivery, a change of the connection, a read that found a
//! changed file). A file share is listed again at its next check time ([`next_share_check`]):
//! the wait doubles with each listing in a row that changed nothing, at most an hour, and is
//! never less than four times the last listing took. At most [`MAX_SYNCS`]
//! run at once. A rate limit is waited out (`retry_at`); a connection that no longer works
//! (`not_connected`) is not tried again until an owner changes it or presses "Sync now"; any
//! other failure is retried after a back-off of a minute, doubled for each failure in a row, at
//! most an hour.
//!
//! No token, response body or other secret reaches an event, a log line, `last_error` or an
//! error: every message here is a plain sentence of ours.

use std::collections::hash_map::Entry as HashEntry;
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use collab_core::ids::{DocId, NodeId};
use futures_util::{StreamExt, TryStreamExt};
use records::{
    DEFAULT_SOURCE_MAX_BYTES, DEFAULT_SOURCE_MAX_FILES, Editor, ExternalEntry, NewEvent, Node,
    NodeKind, SOURCE_MAX_BYTES, SOURCE_MAX_FILES, SourceConfig, SourceItem, SourceKind, SourceLock,
    SourceState, SourceStatus, SourceSyncTransition, UploadTarget, User, check_name, check_size,
    external_tag_parts, is_format_char, validate_document_name, validate_node_name,
};
use revisions::RevisionId;
use serde_json::json;
use time::OffsetDateTime;
use tokio::sync::{Notify, OwnedRwLockWriteGuard, RwLock, Semaphore};
use tokio::task::{JoinHandle, JoinSet};
use uuid::Uuid;

use crate::AppState;
use crate::api::content::{Created, create_uploaded};
use crate::api::nodes::{after_rename, delete_one, move_one};
use crate::api::uploads::{store_version, uploads_path};
use crate::blobs::Held;
use crate::file_room::SaveReply;
use crate::github::{GithubError, Head, RepoRef, TarLimits, for_each_in_tarball, git_blob_id};
use crate::s3_source::S3Source;
use crate::secrets::{Secret, SecretError};
use crate::share::ShareSource;
use crate::source::{ContentMode, EntryKind, Listing, Look, SourceCaps, StorageSource, TreeEntry};
use crate::tree::{self, TreeOp, unique_node_name};

/// More files than this to read are read from one tarball rather than one by one.
pub const TARBALL_FROM: usize = 20;
/// Blob requests made at once.
const BLOBS_AT_ONCE: usize = 4;
/// Room a tarball's limits leave beyond what the tree lists.
const TAR_SLACK: u64 = 1 << 20;
/// A connection still `syncing` after this long, with no sync holding its lock, was cut short.
pub const STALE_SYNC: time::Duration = time::Duration::minutes(15);
/// The longest GitHub author name a revision note carries, in characters.
const MAX_AUTHOR_CHARS: usize = 100;
/// Attempts at storing a new version when the file session's revision moved meanwhile.
const VERSION_ATTEMPTS: usize = 3;
/// How many complete polls an external item may be absent before it is recycled.
pub const MISSING_GRACE_POLLS: u32 = 1;

/// One item of the repository as the library shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The collision-adjusted path displayed in the library.
    pub path: String,
    /// The original path in the repository, below the configured folder.
    pub repository_path: String,
    pub sha: String,
    pub size: u64,
    pub dir: bool,
}

/// What a sync changes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub add: Vec<Entry>,
    pub change: Vec<(SourceItem, Entry)>,
    pub remove: Vec<SourceItem>,
    pub moves: Vec<(SourceItem, Entry)>,
    pub unchanged: usize,
}

/// What the repository holds that the library does not show.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Hidden {
    pub symlinks: usize,
    pub submodules: usize,
    pub policy: usize,
    pub too_big: usize,
    pub unnameable: usize,
    pub renamed: usize,
    pub system: usize,
    pub offline: usize,
    pub too_long: usize,
    pub unreadable: usize,
    /// Archived objects: shown (so not in [`Hidden::total`]) but not readable until restored.
    pub archived: usize,
}

impl Hidden {
    /// How many files are not shown at all (`renamed` files are shown, so not counted).
    #[must_use]
    pub fn total(&self) -> usize {
        self.symlinks
            .saturating_add(self.submodules)
            .saturating_add(self.policy)
            .saturating_add(self.too_big)
            .saturating_add(self.unnameable)
            .saturating_add(self.system)
            .saturating_add(self.offline)
            .saturating_add(self.too_long)
            .saturating_add(self.unreadable)
    }

    /// Counts grouped for the source details shown by the API and pages.
    #[must_use]
    pub fn detail(&self) -> serde_json::Value {
        serde_json::json!({
            "links": self.symlinks,
            "submodules": self.submodules,
            "types": self.policy,
            "size": self.too_big,
            "names": self.unnameable,
            "system": self.system,
            "offline": self.offline,
            "long": self.too_long,
            "unreadable": self.unreadable,
            "archived": self.archived,
        })
    }
}

/// The difference between what the library holds (`known`) and what it should show (`wanted`):
///
/// - a file at the same path with the same blob is unchanged; with another blob it is a change
///   (a new version of the same document);
/// - a file whose path vanished and a new file with the same blob, exactly one of each and opened
///   by the same editor, is a move (the same node renamed or re-parented);
/// - folders are matched by path alone (a folder is never moved: its files are);
/// - everything else is added or removed.
///
/// `unchanged` counts files. `add` and `remove` come in path order, so a folder comes before
/// what is in it.
#[must_use]
pub fn plan(wanted: &[Entry], known: &[SourceItem]) -> Plan {
    plan_with_repairs(wanted, known, &HashSet::new())
}

fn plan_with_repairs(wanted: &[Entry], known: &[SourceItem], repairs: &HashSet<NodeId>) -> Plan {
    let known_at: HashMap<&str, &SourceItem> = known
        .iter()
        .map(|item| (item.repository_path.as_str(), item))
        .collect();
    let wanted_paths: HashSet<&str> = wanted.iter().map(|e| e.repository_path.as_str()).collect();
    let mut plan = Plan::default();
    let mut new = Vec::new();
    for entry in wanted {
        match known_at.get(entry.repository_path.as_str()) {
            None => new.push(entry),
            Some(item) => match (&item.blob_sha, entry.dir) {
                (None, true) if item.path == entry.path && !repairs.contains(&item.node) => {}
                (Some(sha), false)
                    if *sha == entry.sha
                        && item.path == entry.path
                        && !repairs.contains(&item.node) =>
                {
                    plan.unchanged += 1;
                }
                (Some(sha), false) if *sha == entry.sha => {
                    plan.moves.push(((*item).clone(), entry.clone()));
                }
                (Some(_), false) if item.path == entry.path && !repairs.contains(&item.node) => {
                    plan.change.push(((*item).clone(), entry.clone()));
                }
                (Some(_), false) => {
                    plan.moves.push(((*item).clone(), entry.clone()));
                    plan.change.push(((*item).clone(), entry.clone()));
                }
                // A folder that became a file, or the other way round.
                _ => {
                    plan.remove.push((*item).clone());
                    new.push(entry);
                }
            },
        }
    }
    let vanished: Vec<&SourceItem> = known
        .iter()
        .filter(|item| !wanted_paths.contains(item.repository_path.as_str()))
        .collect();

    let mut new_by_blob: HashMap<&str, usize> = HashMap::new();
    for entry in new.iter().filter(|e| !e.dir) {
        *new_by_blob.entry(entry.sha.as_str()).or_default() += 1;
    }
    let mut gone_by_blob: HashMap<&str, usize> = HashMap::new();
    for item in &vanished {
        if let Some(sha) = &item.blob_sha {
            *gone_by_blob.entry(sha.as_str()).or_default() += 1;
        }
    }
    let single = |counts: &HashMap<&str, usize>, sha: &str| counts.get(sha) == Some(&1);
    let mut moved_blobs = HashSet::new();
    for item in &vanished {
        let Some(sha) = item.blob_sha.as_deref() else {
            continue;
        };
        if !single(&gone_by_blob, sha) || !single(&new_by_blob, sha) {
            continue;
        }
        let Some(entry) = new.iter().find(|e| !e.dir && e.sha == sha) else {
            continue;
        };
        if Editor::for_file_name(last_name(&item.path))
            != Editor::for_file_name(last_name(&entry.path))
        {
            continue;
        }
        plan.moves.push(((*item).clone(), (*entry).clone()));
        moved_blobs.insert(sha.to_owned());
    }
    plan.add = new
        .into_iter()
        .filter(|e| e.dir || !moved_blobs.contains(&e.sha))
        .cloned()
        .collect();
    plan.remove.extend(
        vanished
            .into_iter()
            .filter(|item| {
                item.blob_sha
                    .as_ref()
                    .is_none_or(|sha| !moved_blobs.contains(sha))
            })
            .cloned(),
    );
    plan.add.sort_by(|a, b| a.path.cmp(&b.path));
    plan.remove.sort_by(|a, b| a.path.cmp(&b.path));
    plan
}

/// The last name of a `/`-separated path.
fn last_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// `parent/name`, or `name` at the top.
fn join(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_owned()
    } else {
        format!("{parent}/{name}")
    }
}

/// The unsuffixed collision family of `name`, when it already ends in a numbered suffix. This
/// lets a generated `a (1).md` that meets a literal name continue as `a (2).md`, and lets a
/// new literal `a (2).md` continue the existing `a.md`, `a (2).md` allocation as
/// `a (3).md`, rather than starting a second suffix family.
fn collision_base_name(name: &str, editor: Editor) -> Option<String> {
    let lower = name.to_lowercase();
    let split = match editor.native_extension() {
        Some(ext) if lower.ends_with(ext) => name.len().checked_sub(ext.len())?,
        _ => name.rfind('.').filter(|dot| *dot > 0).unwrap_or(name.len()),
    };
    let (stem, extension) = name.split_at_checked(split)?;
    let open = stem.rfind(" (")?;
    let number = stem.get(open + 2..stem.len().checked_sub(1)?)?;
    if !stem.ends_with(')') || number.parse::<u32>().ok().is_none_or(|n| n < 1) {
        return None;
    }
    Some(format!("{}{extension}", stem.get(..open)?))
}

/// `name` with `number` before the extension, using the same extension split as collision
/// allocation. The ordinary uniqueness allocator advances this number if the generated name
/// is also a literal repository name in the folder.
fn numbered_file_name(name: &str, number: usize) -> String {
    let editor = Editor::for_file_name(name);
    let lower = name.to_lowercase();
    let split = match editor.native_extension() {
        Some(ext) if lower.ends_with(ext) => name.len().saturating_sub(ext.len()),
        _ => name.rfind('.').filter(|dot| *dot > 0).unwrap_or(name.len()),
    };
    let (stem, extension) = name.split_at_checked(split).unwrap_or((name, ""));
    tree::fit(stem, extension, &format!(" ({number})"))
}

fn allocated_file_name(name: &str, names: &HashSet<String>) -> String {
    let editor = Editor::for_file_name(name);
    let candidate = if names.contains(&name.to_lowercase()) {
        collision_base_name(name, editor).unwrap_or_else(|| name.to_owned())
    } else {
        name.to_owned()
    };
    unique_node_name(&candidate, Some(editor), names)
}

/// Whether ShareCraft takes `name` as it is for a folder (`file` false) or a file: no empty,
/// `.` or `..` name, no control, invisible or refused character, no name over the length
/// limit or ending in a dot, nothing trimmed, and for a file a name its editor accepts.
fn nameable(name: &str, file: bool) -> bool {
    if validate_node_name(name) != Ok(name) {
        return false;
    }
    !file || validate_document_name(name, Editor::for_file_name(name)).is_ok()
}

/// What of the tree `entries` the library shows, below `folder` (`/`-separated, outer slashes
/// ignored; empty for the whole repository), with paths relative to it, and what it hides.
///
/// Symbolic links and submodules are hidden and counted, never shown as files. A file with a
/// name ShareCraft refuses anywhere in its path (see [`nameable`]) is hidden and counted as
/// unnameable. Files whose sibling names are equal ignoring case are ordered by original
/// repository path byte order and all shown with numbered suffixes starting at `(1)`; every
/// member is counted as renamed. Folders retain the ordinary first-name-wins allocation and
/// are shown only when they hold a shown file. Policy and size are judged by the caller.
///
/// The result is in path order, each folder before what is in it.
#[must_use]
pub fn shown_names(entries: Vec<TreeEntry>, folder: &str) -> (Vec<Entry>, Hidden) {
    shown_names_with_allocations(entries, folder, &[], false)
}

/// As [`shown_names`], preserving displayed folder allocations. File allocations are recomputed
/// from original repository paths so a collision group can grow, shrink and renumber while the
/// source identity remains attached to the same node.
///
/// `bucket` is true for an S3 library, where a folder is a key prefix rather than an object:
/// every folder's repository path ends in `/` (so a file `a` and a folder `a/` are distinct
/// records, whatever is beside them), and a folder listed on its own (a zero-byte marker) is
/// shown even when it holds no shown file. A file whose exact path is also a folder's (only a
/// bucket can hold both) is numbered and the folder keeps the name, unless the file already
/// shows under its own name, which it then keeps.
fn shown_names_with_allocations(
    entries: Vec<TreeEntry>,
    folder: &str,
    allocated: &[SourceItem],
    bucket: bool,
) -> (Vec<Entry>, Hidden) {
    let folder = folder.trim_matches('/');
    let mut hidden = Hidden::default();
    let mut files: Vec<(String, String, u64)> = Vec::new();
    let mut listed_folders: Vec<String> = Vec::new();
    for entry in entries {
        let relative = if folder.is_empty() {
            Some(entry.path.as_str())
        } else {
            entry
                .path
                .strip_prefix(folder)
                .and_then(|rest| rest.strip_prefix('/'))
        };
        let Some(relative) = relative.filter(|r| !r.is_empty()) else {
            continue;
        };
        match entry.kind {
            EntryKind::Symlink => hidden.symlinks += 1,
            EntryKind::Submodule => hidden.submodules += 1,
            EntryKind::Dir => {
                if bucket && relative.split('/').all(|name| nameable(name, false)) {
                    listed_folders.push(relative.to_owned());
                }
            }
            EntryKind::File => {
                let mut parts = relative.split('/').rev();
                let file_ok = parts.next().is_some_and(|name| nameable(name, true));
                if file_ok && parts.all(|name| nameable(name, false)) {
                    files.push((relative.to_owned(), entry.sha, entry.size));
                } else {
                    hidden.unnameable += 1;
                }
            }
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    listed_folders.sort();
    listed_folders.dedup();

    // Every folder shown: those holding a shown file and, for a bucket, those listed alone.
    let mut folder_paths: HashSet<String> = HashSet::new();
    let ancestors = |path: &str, into: &mut HashSet<String>| {
        let mut parent = parent_path(path);
        while !parent.is_empty() && into.insert(parent.to_owned()) {
            parent = parent_path(parent);
        }
    };
    for (path, _, _) in &files {
        ancestors(path, &mut folder_paths);
    }
    for path in &listed_folders {
        folder_paths.insert(path.clone());
        ancestors(path, &mut folder_paths);
    }

    // Files that already show under their own name keep it beside a folder of that name.
    let shown_plain: HashSet<&str> = allocated
        .iter()
        .filter(|item| {
            item.blob_sha.is_some() && last_name(&item.path) == last_name(&item.repository_path)
        })
        .map(|item| item.repository_path.as_str())
        .collect();

    // Collision membership and numbering come only from original repository siblings. Because
    // `files` is in path byte order, insertion order is the required numbering order. Literal
    // names outside a collision group are reserved so generated `(1)`, `(2)` ... names advance
    // through the ordinary case-insensitive uniqueness mechanism instead of displacing them.
    let mut groups: HashMap<(String, String), Vec<String>> = HashMap::new();
    for (path, _, _) in &files {
        groups
            .entry((parent_path(path).to_owned(), last_name(path).to_lowercase()))
            .or_default()
            .push(path.clone());
    }
    let mut collision_numbers = HashMap::new();
    let mut plain_file_names: HashMap<String, HashSet<String>> = HashMap::new();
    for ((parent, _), paths) in groups {
        let beside_folder = paths.len() == 1
            && paths.first().is_some_and(|path| {
                folder_paths.contains(path) && !shown_plain.contains(path.as_str())
            });
        if paths.len() > 1 || beside_folder {
            for (index, path) in paths.into_iter().enumerate() {
                collision_numbers.insert(path, index + 1);
            }
        } else if let Some(path) = paths.first() {
            plain_file_names
                .entry(parent)
                .or_default()
                .insert(last_name(path).to_lowercase());
        }
    }

    let mut wanted = folder_paths.clone();
    wanted.extend(files.iter().map(|(path, _, _)| path.clone()));
    let mut reserved: HashMap<String, HashMap<String, String>> = HashMap::new();
    for item in allocated.iter().filter(|item| item.blob_sha.is_none()) {
        let original = item
            .repository_path
            .strip_suffix('/')
            .unwrap_or(&item.repository_path);
        if !wanted.contains(original) {
            continue;
        }
        reserved
            .entry(parent_path(original).to_owned())
            .or_default()
            .insert(original.to_owned(), last_name(&item.path).to_owned());
    }

    // What to show, in path order: each file, and each folder listed on its own (no leaf).
    let mut items: Vec<(String, Option<(String, u64)>)> = files
        .into_iter()
        .map(|(path, sha, size)| (path, Some((sha, size))))
        .chain(listed_folders.into_iter().map(|path| (path, None)))
        .collect();
    items.sort_by(|a, b| a.0.cmp(&b.0));

    // Shown path of each folder, by its path in the repository; lower-cased names taken in
    // each shown folder.
    let mut folders: HashMap<String, String> = HashMap::new();
    let mut taken: HashMap<String, HashSet<String>> = HashMap::new();
    let mut shown = Vec::new();
    for (path, leaf) in items {
        let mut parts: Vec<&str> = path.split('/').collect();
        let name = if leaf.is_some() {
            match parts.pop() {
                Some(name) => name,
                None => continue,
            }
        } else {
            ""
        };
        let mut original = String::new();
        let mut parent = String::new();
        for part in parts {
            let original_parent = original.clone();
            original = join(&original, part);
            if let Some(known) = folders.get(&original) {
                parent = known.clone();
                continue;
            }
            let names = taken.entry(parent.clone()).or_default();
            if let Some(for_parent) = reserved.get(&original_parent) {
                names.extend(for_parent.values().map(|name| name.to_lowercase()));
            }
            if let Some(literals) = plain_file_names.get(&original_parent) {
                names.extend(literals.iter().cloned());
            }
            let given = reserved
                .get(&original_parent)
                .and_then(|for_parent| for_parent.get(&original))
                .filter(|name| nameable(name, false))
                .cloned()
                .unwrap_or_else(|| unique_node_name(part, None, names));
            if given != part {
                hidden.renamed += 1;
            }
            names.insert(given.to_lowercase());
            let at = join(&parent, &given);
            folders.insert(original.clone(), at.clone());
            shown.push(Entry {
                path: at.clone(),
                repository_path: if bucket {
                    format!("{original}/")
                } else {
                    original.clone()
                },
                sha: String::new(),
                size: 0,
                dir: true,
            });
            parent = at;
        }
        let Some((sha, size)) = leaf else {
            continue;
        };
        let names = taken.entry(parent.clone()).or_default();
        let original_parent = parent_path(&path);
        if let Some(for_parent) = reserved.get(original_parent) {
            names.extend(for_parent.values().map(|name| name.to_lowercase()));
        }
        if let Some(literals) = plain_file_names.get(original_parent) {
            names.extend(literals.iter().cloned());
        }
        let given = collision_numbers.get(&path).map_or_else(
            || name.to_owned(),
            |number| {
                // A collision member may never fall back to the unsuffixed base when a literal
                // numbered name is taken: every member must remain visibly numbered.
                let mut unavailable = names.clone();
                unavailable.insert(name.to_lowercase());
                allocated_file_name(&numbered_file_name(name, *number), &unavailable)
            },
        );
        if given != name {
            hidden.renamed += 1;
        }
        names.insert(given.to_lowercase());
        shown.push(Entry {
            path: join(&parent, &given),
            repository_path: path,
            sha,
            size,
            dir: false,
        });
    }
    shown.sort_by(|a, b| a.path.cmp(&b.path));
    (shown, hidden)
}

/// What a sync that changed the library did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncReport {
    /// The commit now mirrored.
    pub commit: String,
    /// Files added, given new versions, moved to the recycle bin, and moved or renamed.
    pub added: usize,
    pub changed: usize,
    pub removed: usize,
    pub moved: usize,
    pub hidden: Hidden,
    /// The bytes of every file the library now mirrors.
    pub bytes: u64,
}

/// Why a sync did not happen. Each message is a plain sentence of ours.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SyncError {
    /// The repository, branch or token no longer works; someone must change the connection.
    #[error("{0}")]
    NotConnected(String),
    /// GitHub's rate limit is reached until then.
    #[error("GitHub's rate limit is reached; the library is synced again after {0}")]
    RateLimited(OffsetDateTime),
    /// Anything else; worth trying again later.
    #[error("{0}")]
    Failed(String),
}

/// The authenticated context the token of `library` is sealed with.
#[must_use]
pub fn token_context(library: NodeId) -> Vec<u8> {
    format!("source-token:{}", library.0).into_bytes()
}

/// The authenticated context the webhook secret of `library` is sealed with.
#[must_use]
pub fn webhook_context(library: NodeId) -> Vec<u8> {
    format!("source-webhook:{}", library.0).into_bytes()
}

/// Abandons every upload session whose file would go into the connected library `library`
/// (into it or anything below it), whoever started it: a session started while the library
/// was still empty and ordinary must not finish into it once it is connected. Answers how many
/// were abandoned; failures are logged.
pub async fn abandon_uploads_into(state: &AppState, library: NodeId) -> usize {
    let db = state.db();
    let site = match db.node(library).await {
        Ok(Some(node)) => node.site,
        Ok(None) => return 0,
        Err(e) => {
            eprintln!("sharecraft-server: database failure: {e}");
            return 0;
        }
    };
    let sessions = match db.uploads_in_site(site).await {
        Ok(sessions) => sessions,
        Err(e) => {
            eprintln!("sharecraft-server: database failure: {e}");
            return 0;
        }
    };
    let mut abandoned = 0;
    for id in sessions {
        let session = match db.upload_session(id).await {
            Ok(Some(session)) => session,
            // Finished or abandoned meanwhile.
            Ok(None) => continue,
            Err(e) => {
                eprintln!("sharecraft-server: database failure: {e}");
                continue;
            }
        };
        let place = match session.target {
            UploadTarget::NewFile { parent } => parent,
            UploadTarget::NewVersion { doc } => doc.into(),
        };
        match db.source_library_of(place).await {
            Ok(Some(found)) if found == library => {}
            Ok(_) => continue,
            Err(e) => {
                eprintln!("sharecraft-server: database failure: {e}");
                continue;
            }
        }
        match crate::api::uploads::abandon_session(state, id).await {
            Ok(()) => abandoned += 1,
            Err(e) => eprintln!(
                "sharecraft-server: could not abandon an upload into library {}: {}",
                library.0,
                e.code()
            ),
        }
    }
    abandoned
}

/// Puts each connection left `syncing` and last checked before `before` back to `connecting`
/// when its source advisory lock is free, so it is due again; how many. The server's start calls
/// it with the time it starts (no sync of this process runs before), [`sync_once`] with
/// [`STALE_SYNC`] ago. A failure is logged.
pub async fn recover_stale_syncs(state: &AppState, before: OffsetDateTime) -> usize {
    match state.db().reset_stale_syncs(before).await {
        Ok(n) => usize::try_from(n).unwrap_or(usize::MAX),
        Err(e) => {
            eprintln!("sharecraft-server: cannot recover cut-short syncs: {e}");
            0
        }
    }
}

/// The most syncs that run at once.
pub const MAX_SYNCS: usize = 2;
/// How often the worker looks for due libraries when nothing wakes it.
const POLL: std::time::Duration = std::time::Duration::from_secs(60);
/// How soon a library is tried again when its sync found it busy or changed meanwhile.
const AGAIN_SOON: std::time::Duration = std::time::Duration::from_secs(1);
/// The most due libraries taken at a time.
const BATCH: i64 = 32;
/// The wait after a first failed sync, in seconds; it doubles with each failure in a row.
const FIRST_BACK_OFF: i64 = 60;
/// The longest wait after failed syncs, in seconds.
const MAX_BACK_OFF: i64 = 60 * 60;
/// The longest a quiet share waits for its next listing, in seconds, unless its own poll time
/// is longer or its listing is slow (see [`next_share_check`]).
const MAX_SHARE_INTERVAL: u64 = 60 * 60;
/// A share is listed again no sooner than this many times as long as its last listing took, so
/// listing loads the file server at most about a fifth of the time.
const LISTING_SPACING: u32 = 4;

/// When a share listed at `now` is listed again: `poll_seconds × 2^quiet_polls` (the listings in
/// a row that changed nothing), at most an hour or `poll_seconds` if that is longer, and never
/// sooner than four times `listing_took` after `now`. Pure; saturates rather than overflows.
#[must_use]
pub fn next_share_check(
    now: OffsetDateTime,
    poll_seconds: u32,
    quiet_polls: u32,
    listing_took: std::time::Duration,
) -> OffsetDateTime {
    let poll = u64::from(poll_seconds.max(1));
    let cap = MAX_SHARE_INTERVAL.max(poll);
    let doubled = 1_u64
        .checked_shl(quiet_polls)
        .and_then(|factor| poll.checked_mul(factor))
        .map_or(cap, |seconds| seconds.min(cap));
    let interval =
        std::time::Duration::from_secs(doubled).max(listing_took.saturating_mul(LISTING_SPACING));
    let longest = std::time::Duration::from_secs(cap);
    time::Duration::try_from(interval)
        .ok()
        .and_then(|interval| now.checked_add(interval))
        .or_else(|| {
            time::Duration::try_from(longest)
                .ok()
                .and_then(|longest| now.checked_add(longest))
        })
        .unwrap_or(now)
}

/// The sync worker's shared state: the libraries woken (a set, so wakes coalesce), the ones
/// being synced, the failures in a row of each, and the turns.
pub struct Sources {
    /// Woken and not yet looked at.
    pending: Mutex<HashSet<NodeId>>,
    /// Being synced (or waiting for a turn); `true` when woken again meanwhile.
    running: Mutex<HashMap<NodeId, bool>>,
    /// Failed syncs in a row, for the back-off. Kept in memory only.
    failures: Mutex<HashMap<NodeId, u32>>,
    /// Share listings in a row that changed nothing, for the adaptive poll (see
    /// [`next_share_check`]). Kept in memory only, so a restart starts every count over.
    quiet: Mutex<HashMap<NodeId, u32>>,
    /// Libraries being disconnected: no sync of them starts, a listing under way gives up at
    /// its next folder, and a sync that ends meanwhile does not make them due again.
    stopping: Mutex<HashMap<NodeId, usize>>,
    /// Taken by each running sync.
    turns: Arc<Semaphore>,
    /// A test seam held immediately before a worker starts a chosen source.
    start_gate: Arc<RwLock<()>>,
    /// Signals a worker waiting at [`Sources::start_gate`].
    start_held: Notify,
    /// A test seam held as a spawned worker child enters its future.
    child_start_gate: Arc<RwLock<()>>,
    /// Signals that a worker child reached [`Sources::child_start_gate`].
    child_started: Notify,
    /// Told when a library is woken.
    wake: Notify,
}

impl Default for Sources {
    fn default() -> Self {
        Sources {
            pending: Mutex::new(HashSet::new()),
            running: Mutex::new(HashMap::new()),
            failures: Mutex::new(HashMap::new()),
            quiet: Mutex::new(HashMap::new()),
            stopping: Mutex::new(HashMap::new()),
            turns: Arc::new(Semaphore::new(MAX_SYNCS)),
            start_gate: Arc::new(RwLock::new(())),
            start_held: Notify::new(),
            child_start_gate: Arc::new(RwLock::new(())),
            child_started: Notify::new(),
            wake: Notify::new(),
        }
    }
}

/// Locks `m`, also if a holder panicked.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Sources {
    /// Asks the worker to sync `library` at once, without waiting for its poll time (a webhook
    /// delivery, a change of the connection). Wakes before it runs coalesce into one sync; a
    /// wake while it runs syncs it again after. A connection that stopped (`not_connected`) or
    /// waits for a retry time is not synced by a wake. A wake ignores a share's next check
    /// time and starts its count of quiet listings over.
    pub fn wake(&self, library: NodeId) {
        lock(&self.quiet).remove(&library);
        lock(&self.pending).insert(library);
        self.wake.notify_one();
    }

    /// Whether `library` was woken and not yet looked at, forgetting the wake. A test seam: the
    /// servers of tests that run no workers keep every wake here.
    #[doc(hidden)]
    pub fn take_wake_for_tests(&self, library: NodeId) -> bool {
        lock(&self.pending).remove(&library)
    }

    /// Forgets the failures in a row of `library`: someone changed the connection.
    pub(crate) fn forget_failures(&self, library: NodeId) {
        lock(&self.failures).remove(&library);
    }

    /// Counts a finished share listing of `library` and answers its quiet listings in a row: a
    /// listing that `changed` something, or one woken again while it ran (the wake asked for a
    /// fresh start), starts the count over; one that changed nothing adds one.
    fn quiet_after(&self, library: NodeId, changed: bool) -> u32 {
        let woken = lock(&self.running).get(&library).copied().unwrap_or(false);
        let mut quiet = lock(&self.quiet);
        if changed || woken {
            quiet.remove(&library);
            return 0;
        }
        let count = quiet.entry(library).or_insert(0);
        // From 32 on the interval is at its cap whatever the count.
        *count = count.saturating_add(1).min(32);
        *count
    }

    /// Starts the count of quiet listings of `library` over: its listing failed.
    fn forget_quiet(&self, library: NodeId) {
        lock(&self.quiet).remove(&library);
    }

    /// Stops the syncs of `library` until the answer is dropped (a disconnect): none starts, a
    /// listing under way gives up at its next folder, and one that ends meanwhile does not make
    /// the library due again; a listing that gives up records nothing. Stops of one library
    /// are counted, so the syncs stay stopped until every holder has dropped its answer.
    pub fn stop(&self, library: NodeId) -> Stopping<'_> {
        let mut stopping = lock(&self.stopping);
        let holders = stopping.entry(library).or_insert(0);
        *holders = holders.saturating_add(1);
        Stopping {
            sources: self,
            library,
        }
    }

    /// Whether `library` is being disconnected (see [`Sources::stop`]).
    pub(crate) fn is_stopping(&self, library: NodeId) -> bool {
        lock(&self.stopping).contains_key(&library)
    }

    /// Holds worker source starts until the returned guard is dropped.
    ///
    /// This is a test seam for changes recorded after a worker takes its global turn but before
    /// it atomically starts the source.
    #[doc(hidden)]
    pub async fn hold_starts(&self) -> OwnedRwLockWriteGuard<()> {
        Arc::clone(&self.start_gate).write_owned().await
    }

    /// Waits until a worker reaches the held pre-transition start gate.
    #[doc(hidden)]
    pub async fn wait_start_held(&self) {
        self.start_held.notified().await;
    }

    /// Holds spawned worker children before they enter their sync future.
    #[doc(hidden)]
    pub async fn hold_child_starts(&self) -> OwnedRwLockWriteGuard<()> {
        Arc::clone(&self.child_start_gate).write_owned().await
    }

    /// Waits until a spawned worker child is held by [`Sources::hold_child_starts`].
    #[doc(hidden)]
    pub async fn wait_child_start(&self) {
        self.child_started.notified().await;
    }

    fn take_pending(&self) -> Vec<NodeId> {
        std::mem::take(&mut *lock(&self.pending))
            .into_iter()
            .collect()
    }

    fn has_pending(&self) -> bool {
        !lock(&self.pending).is_empty()
    }

    /// Takes `library` to sync; `false` if it is being synced already, which then, if `woken`,
    /// syncs it again after.
    fn start(&self, library: NodeId, woken: bool) -> bool {
        let mut running = lock(&self.running);
        match running.get_mut(&library) {
            Some(again) => {
                *again |= woken;
                false
            }
            None => {
                running.insert(library, false);
                true
            }
        }
    }

    /// Gives `library` up after its sync; whether it was woken meanwhile.
    fn done(&self, library: NodeId) -> bool {
        lock(&self.running).remove(&library).unwrap_or(false)
    }

    /// The wait after a failed sync of `library`: 60 seconds, doubled for each failure in a
    /// row before it, at most an hour. Counts this failure.
    fn back_off(&self, library: NodeId) -> time::Duration {
        let mut failures = lock(&self.failures);
        let before = failures.entry(library).or_insert(0);
        let seconds = (0..*before).fold(FIRST_BACK_OFF, |s, _| (s * 2).min(MAX_BACK_OFF));
        *before = before.saturating_add(1).min(32);
        time::Duration::seconds(seconds)
    }
}

/// The syncs of a library stopped by [`Sources::stop`], until this is dropped.
pub struct Stopping<'a> {
    sources: &'a Sources,
    library: NodeId,
}

impl Drop for Stopping<'_> {
    fn drop(&mut self) {
        let mut stopping = lock(&self.sources.stopping);
        if let HashEntry::Occupied(mut holders) = stopping.entry(self.library) {
            *holders.get_mut() = holders.get().saturating_sub(1);
            if *holders.get() == 0 {
                holders.remove();
            }
        }
    }
}

/// Syncs every library that is woken (see [`Sources::wake`]) or due (see
/// [`records::Db::due_sources`]) as of `now`, at most [`MAX_SYNCS`] at once, and waits for
/// them. Answers how many syncs it started. A library already being synced is left to that
/// sync. It ends by keeping the file-share cache within its size ([`crate::external::evict`]),
/// so a lowered size takes effect by the next tick.
pub async fn tick_once(state: &AppState, now: OffsetDateTime) -> usize {
    let mut running = JoinSet::new();
    let count = start_due(state, now, &mut running).await;
    while let Some(sync) = running.join_next().await {
        if let Err(e) = sync {
            eprintln!("sharecraft-server: a sync did not finish: {e}");
        }
    }
    crate::external::evict(state).await;
    count
}

/// Starts the syncs of the libraries woken or due as of `now` in `running`; how many.
async fn start_due(state: &AppState, now: OffsetDateTime, running: &mut JoinSet<()>) -> usize {
    recover_stale_syncs(state, now.checked_sub(STALE_SYNC).unwrap_or(now)).await;
    let sources = state.sources();
    let db = state.db();
    let woken = sources.take_pending();
    let mut chosen: Vec<(NodeId, bool)> = Vec::new();
    match db.wakeable_sources(now, &woken).await {
        Ok(ids) => chosen.extend(ids.into_iter().map(|id| (id, true))),
        Err(e) => {
            eprintln!("sharecraft-server: database failure: {e}");
            // Kept, to be looked at again soon.
            lock(&sources.pending).extend(woken);
        }
    }
    match db.due_sources(now, BATCH).await {
        Ok(ids) => {
            for id in ids {
                if !chosen.iter().any(|(c, _)| *c == id) {
                    chosen.push((id, false));
                }
            }
        }
        Err(e) => eprintln!("sharecraft-server: database failure: {e}"),
    }
    let mut started = 0;
    for (library, _) in chosen.into_iter().filter(|(library, woken)| {
        !sources.is_stopping(*library) && sources.start(*library, *woken)
    }) {
        started += 1;
        let mut guard = RunningGuard::new(state.clone(), library);
        running.spawn({
            let state = state.clone();
            async move {
                state.sources().child_started.notify_one();
                let child_start = state.sources().child_start_gate.read().await;
                drop(child_start);
                guard.finish(sync_one(&state, library, now).await);
            }
        });
    }
    started
}

/// Removes a child from the in-memory running set whether it returns or is aborted with its
/// worker. A wake received while it ran is restored; a busy or superseded start is retried soon.
struct RunningGuard {
    state: AppState,
    library: NodeId,
    retry_soon: bool,
}

impl RunningGuard {
    fn new(state: AppState, library: NodeId) -> Self {
        Self {
            state,
            library,
            retry_soon: false,
        }
    }

    fn finish(&mut self, retry_soon: bool) {
        self.retry_soon = retry_soon;
    }
}

impl Drop for RunningGuard {
    fn drop(&mut self) {
        let sources = self.state.sources();
        if sources.done(self.library) {
            sources.wake(self.library);
        } else if self.retry_soon {
            let state = self.state.clone();
            let library = self.library;
            tokio::spawn(async move {
                tokio::time::sleep(AGAIN_SOON).await;
                state.sources().wake(library);
            });
        } else if sources.has_pending() {
            sources.wake.notify_one();
        }
    }
}

/// A worker start after its per-library advisory lock has been considered.
enum WorkerStart {
    Started(Box<StartedSync>),
    BusyOrSuperseded,
    Ineligible,
}

struct StartedSync {
    source_lock: SourceLock,
    config: SourceConfig,
    status: SourceStatus,
    generation: i64,
}

async fn start_worker_sync(state: &AppState, library: NodeId, now: OffsetDateTime) -> WorkerStart {
    let db = state.db();
    let source_lock = match db.try_lock_source(library).await {
        Ok(Some(source_lock)) => source_lock,
        Ok(None) => return WorkerStart::BusyOrSuperseded,
        Err(e) => {
            eprintln!("sharecraft-server: database failure: {e}");
            return WorkerStart::Ineligible;
        }
    };
    // Being disconnected: the turn is given up at once for the disconnect to take.
    if state.sources().is_stopping(library) {
        return WorkerStart::Ineligible;
    }
    let generation = match db.source_generation(library).await {
        Ok(Some(generation)) => generation,
        Ok(None) => return WorkerStart::Ineligible,
        Err(e) => {
            eprintln!("sharecraft-server: database failure: {e}");
            return WorkerStart::Ineligible;
        }
    };
    let (config, status) = match db.source_of(library).await {
        Ok(Some(source)) => source,
        Ok(None) => return WorkerStart::Ineligible,
        Err(e) => {
            eprintln!("sharecraft-server: database failure: {e}");
            return WorkerStart::Ineligible;
        }
    };
    state.sources().start_held.notify_one();
    let start_guard = state.sources().start_gate.read().await;
    let transition = db
        .mark_source_syncing_if_eligible(library, now, generation)
        .await;
    drop(start_guard);
    match transition {
        Ok(SourceSyncTransition::Started) => WorkerStart::Started(Box::new(StartedSync {
            source_lock,
            config,
            status,
            generation,
        })),
        Ok(SourceSyncTransition::Superseded) => WorkerStart::BusyOrSuperseded,
        Ok(SourceSyncTransition::Ineligible) => WorkerStart::Ineligible,
        Err(e) => {
            eprintln!("sharecraft-server: database failure: {e}");
            WorkerStart::Ineligible
        }
    }
}

/// Syncs `library` as of `now`, taking a turn first, and keeps the back-off: after a failure
/// worth retrying (`failed`) no sync is tried before 60 seconds, doubled for each failure in a
/// row, at most an hour; a success, or a stop for someone to act on (`not_connected`), starts
/// the count over. Whether it is to be tried again soon: it was busy (`Ok(None)` while
/// another sync of it ran), or changed meanwhile (due again at once).
async fn sync_one(state: &AppState, library: NodeId, now: OffsetDateTime) -> bool {
    let waiting = std::time::Instant::now();
    let Ok(_turn) = Arc::clone(&state.sources().turns).acquire_owned().await else {
        return false;
    };
    let outcome = match start_worker_sync(state, library, now).await {
        WorkerStart::Started(started) => {
            let queued = waiting.elapsed();
            let StartedSync {
                source_lock,
                config,
                status,
                generation,
            } = *started;
            let outcome = match config.kind {
                SourceKind::Github => {
                    run(&GithubSource, state, &config, &status, now, generation).await
                }
                SourceKind::Share => {
                    run(&ShareSource, state, &config, &status, now, generation).await
                }
                SourceKind::S3 => run(&S3Source, state, &config, &status, now, generation).await,
            };
            let finished = finish(state, &config, status, now, queued, outcome, generation).await;
            drop(source_lock);
            finished
        }
        WorkerStart::BusyOrSuperseded => return true,
        WorkerStart::Ineligible => return false,
    };
    let current = match state.db().source_of(library).await {
        Ok(Some((_, status))) => status.state,
        // Disconnected meanwhile.
        Ok(None) => return false,
        Err(e) => {
            eprintln!("sharecraft-server: database failure: {e}");
            return false;
        }
    };
    let sources = state.sources();
    match (outcome, current) {
        (Err(SyncError::Failed(_)), SourceState::Failed) => {
            let until = now.checked_add(sources.back_off(library)).unwrap_or(now);
            if let Err(e) = state.db().back_off_source(library, until).await {
                eprintln!("sharecraft-server: database failure: {e}");
            }
            false
        }
        (_, SourceState::Ok | SourceState::NotConnected) => {
            sources.forget_failures(library);
            false
        }
        (_, SourceState::Connecting | SourceState::Syncing) => true,
        _ => false,
    }
}

/// The sync worker, running until dropped.
pub struct Worker {
    task: JoinHandle<()>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Starts the sync worker: it starts the syncs of woken and due libraries whenever a library
/// is woken and every minute (every second while a woken library waits), without waiting for
/// them to finish. At most once a minute it also keeps the file-share cache within its size
/// ([`crate::external::evict`]), so a lowered size takes effect within a minute.
pub fn start_worker(state: AppState) -> Worker {
    let task = tokio::spawn(async move {
        let mut running = JoinSet::new();
        let mut evicted: Option<tokio::time::Instant> = None;
        let mut swept: Option<tokio::time::Instant> = None;
        // The sweep runs on its own task (owned here, so dropping the worker stops it): a
        // provider that hangs must not hold up the syncs. A tick is skipped while one runs.
        let mut sweeping = JoinSet::new();
        loop {
            while sweeping.try_join_next().is_some() {}
            while let Some(finished) = running.try_join_next() {
                if let Err(e) = finished {
                    eprintln!("sharecraft-server: a sync did not finish: {e}");
                }
            }
            // The JoinSet owns every started sync, so dropping this worker stops them too.
            start_due(&state, OffsetDateTime::now_utc(), &mut running).await;
            if evicted.is_none_or(|at| at.elapsed() >= POLL.saturating_sub(AGAIN_SOON)) {
                crate::external::evict(&state).await;
                evicted = Some(tokio::time::Instant::now());
            }
            // Secrets forgotten while their values live at the secret provider are deleted
            // there, and a delete that failed is tried again.
            if sweeping.is_empty()
                && swept.is_none_or(|at| at.elapsed() >= POLL.saturating_sub(AGAIN_SOON))
            {
                let state = state.clone();
                sweeping.spawn(async move {
                    state.secret_store().sweep_forgotten(state.db()).await;
                });
                swept = Some(tokio::time::Instant::now());
            }
            let wait = if state.sources().has_pending() {
                AGAIN_SOON
            } else {
                POLL
            };
            tokio::select! {
                () = state.sources().wake.notified() => {}
                () = tokio::time::sleep(wait) => {}
            }
        }
    });
    Worker { task }
}

/// Syncs `library` with its branch as of `now`. `Ok(None)` means no new report: either the
/// branch was unchanged or this sync was busy/superseded. Callers other than the worker must
/// check the source state to tell which. Otherwise the answer says what changed. The
/// connection's status says how it went; `source.synced` (counts and the short commit id) or
/// `source.failed` (the reason) is recorded. A connection left `syncing` by a sync cut short
/// over [`STALE_SYNC`] ago is made due again first.
///
/// # Errors
/// `NotConnected` when the library is not connected, or the repository, branch, token or the
/// person who connected it no longer works; `RateLimited` until GitHub allows requests again;
/// `Failed` for anything else. None of them changes anything in the library unless the
/// failure came while applying, where what was applied stays recorded.
pub async fn sync_once(
    state: &AppState,
    library: NodeId,
    now: OffsetDateTime,
) -> Result<Option<SyncReport>, SyncError> {
    recover_stale_syncs(state, now.checked_sub(STALE_SYNC).unwrap_or(now)).await;
    let db = state.db();
    let Some(_turn) = db.try_lock_source(library).await.map_err(db_failed)? else {
        return Ok(None);
    };
    let not_connected =
        || SyncError::NotConnected("This library is not connected to a repository.".to_owned());
    // Read before the configuration: a change between the two reads moves the generation, so
    // this sync then records nothing (see `finish`).
    let generation = db
        .source_generation(library)
        .await
        .map_err(db_failed)?
        .ok_or_else(not_connected)?;
    let Some((config, status)) = db.source_of(library).await.map_err(db_failed)? else {
        return Err(not_connected());
    };
    // Holding the lock, nothing else is syncing it: a `syncing` state here was cut short. If
    // the connection changed since it was read, the sync woken by that change does the work.
    if !db
        .mark_source_syncing(library, now, generation)
        .await
        .map_err(db_failed)?
    {
        return Ok(None);
    }
    let outcome = match config.kind {
        SourceKind::Github => run(&GithubSource, state, &config, &status, now, generation).await,
        SourceKind::Share => run(&ShareSource, state, &config, &status, now, generation).await,
        SourceKind::S3 => run(&S3Source, state, &config, &status, now, generation).await,
    };
    finish(
        state,
        &config,
        status,
        now,
        std::time::Duration::ZERO,
        outcome,
        generation,
    )
    .await
}

/// What a sync that did not fail found.
enum Outcome {
    /// Nothing to do; the ETag to keep.
    Unchanged {
        etag: Option<String>,
        listing_detail: Option<serde_json::Value>,
        /// How long the listing took (zero when nothing was listed).
        took: std::time::Duration,
    },
    /// The library now mirrors `report.commit`.
    Synced {
        report: SyncReport,
        etag: Option<String>,
        files: usize,
        content: ContentMode,
        took: std::time::Duration,
    },
}

/// Records how the sync of `config` went, as of `now`, in its status and the event log. The
/// status is written only if the connection is still at `generation`: a change made meanwhile
/// (another branch or folder, a token, "Sync now") must not be undone by this sync's bookmark.
/// Then the connection is only made due again at once, marked half applied when it is.
/// `queued` is how long the sync waited after `now` before it started listing.
async fn finish(
    state: &AppState,
    config: &SourceConfig,
    before: SourceStatus,
    now: OffsetDateTime,
    queued: std::time::Duration,
    outcome: Result<Outcome, Failure>,
    generation: i64,
) -> Result<Option<SyncReport>, SyncError> {
    let library = config.library;
    // Being disconnected (see [`Sources::stop`]): a sync that gave up, or failed meanwhile,
    // records no failure, event or back-off; the disconnect removes the connection, or makes
    // it due again itself if it fails.
    if state.sources().is_stopping(library)
        && let Err(Failure { error, .. }) = outcome
    {
        state.sources().forget_quiet(library);
        return Err(error);
    }
    let mut status = SourceStatus {
        last_checked_at: Some(now),
        ..before
    };
    // Recorded only with the status: a sync whose connection changed meanwhile records neither.
    let event: Option<NewEvent>;
    let listing_detail: Option<serde_json::Value>;
    // A share is listed again after an interval that grows while its listings change nothing
    // (see [`next_share_check`]); other sources keep their poll time.
    let share = matches!(config.kind, SourceKind::Share);
    let sources = state.sources();
    let next_check = |changed: bool, took: std::time::Duration| {
        share.then(|| {
            let quiet = sources.quiet_after(library, changed);
            // Counted from when the listing finished, not from `now`: a sync that waited for a
            // turn must not find its next check already past. Whole seconds, so a quick listing
            // keeps the times a tick was given.
            let listed = time::Duration::try_from(std::time::Duration::from_secs(
                queued.saturating_add(took).as_secs(),
            ))
            .ok()
            .and_then(|elapsed| now.checked_add(elapsed))
            .unwrap_or(now);
            next_share_check(listed, config.poll_seconds, quiet, took)
        })
    };
    let answer = match outcome {
        Ok(Outcome::Unchanged {
            etag,
            listing_detail: detail,
            took,
        }) => {
            status.next_check_at = next_check(false, took);
            status.state = SourceState::Ok;
            status.etag = etag;
            status.last_error = None;
            status.retry_at = None;
            listing_detail = detail;
            event = None;
            Ok(None)
        }
        Ok(Outcome::Synced {
            report,
            etag,
            files,
            content,
            took,
        }) => {
            status.next_check_at = next_check(true, took);
            status.state = SourceState::Ok;
            status.etag = etag;
            status.last_commit = (!report.commit.is_empty()).then(|| report.commit.clone());
            status.last_synced_at = Some(now);
            status.last_error = None;
            status.retry_at = None;
            status.files = i64::try_from(files).unwrap_or(i64::MAX);
            status.bytes = i64::try_from(report.bytes).unwrap_or(i64::MAX);
            // Files not shown at all. Files shown under a suffixed name (`renamed`) are shown,
            // so they are not counted here; the `source.synced` event carries their number.
            status.hidden = i64::try_from(report.hidden.total()).unwrap_or(i64::MAX);
            listing_detail =
                matches!(content, ContentMode::External).then(|| report.hidden.detail());
            let detail = if matches!(content, ContentMode::External) {
                json!({
                    "added": report.added,
                    "changed": report.changed,
                    "removed": report.removed,
                    "moved": report.moved,
                    "hidden": report.hidden.total(),
                    "renamed": report.hidden.renamed,
                    "files": files,
                    "seconds": took.as_secs_f64(),
                })
            } else {
                json!({
                    "commit": short(&report.commit),
                    "added": report.added,
                    "changed": report.changed,
                    "removed": report.removed,
                    "moved": report.moved,
                    "hidden": report.hidden.total(),
                    "renamed": report.hidden.renamed,
                    "bytes": report.bytes,
                })
            };
            event = Some(NewEvent {
                kind: "source.synced",
                actor: None,
                subject: None,
                document: None,
                node: Some(library),
                detail,
            });
            Ok(Some(report))
        }
        Err(Failure { error: e, applying }) => {
            listing_detail = None;
            // Retried after the back-off (`retry_at`), not after a quiet share's long interval.
            status.next_check_at = None;
            if share {
                sources.forget_quiet(library);
            }
            if applying {
                // Half applied: never answered as unchanged, even at the last commit.
                status.etag = Some(DIRTY_ETAG.to_owned());
            }
            status.state = match e {
                SyncError::NotConnected(_) => SourceState::NotConnected,
                SyncError::RateLimited(_) => SourceState::RateLimited,
                SyncError::Failed(_) => SourceState::Failed,
            };
            status.retry_at = match e {
                SyncError::RateLimited(at) => Some(at),
                _ => None,
            };
            let reason = e.to_string();
            status.last_error = Some(reason.clone());
            event = Some(NewEvent {
                kind: "source.failed",
                actor: None,
                subject: None,
                document: None,
                node: Some(library),
                detail: json!({ "reason": reason }),
            });
            Err(e)
        }
    };
    let db = state.db();
    let recorded = match db.set_source_status_if(library, &status, generation).await {
        Ok(true) => {
            if let Some(detail) = listing_detail
                && let Err(e) = db
                    .record_listing(library, generation, status.next_check_at, &detail)
                    .await
            {
                eprintln!(
                    "sharecraft-server: cannot record the listing of library {}: {e}",
                    library.0
                );
            }
            if let Some(event) = event {
                state.events().emit(event);
            }
            Ok(())
        }
        // Changed meanwhile: due again at once, still marked if the library is half applied.
        // Being disconnected: not made due again; the disconnect removes the connection, or
        // makes it due itself if it fails.
        Ok(false) if sources.is_stopping(library) => {
            sources.forget_quiet(library);
            Ok(())
        }
        Ok(false) => {
            sources.forget_quiet(library);
            let dirty = (status.etag.as_deref() == Some(DIRTY_ETAG)).then_some(DIRTY_ETAG);
            if let (SourceState::RateLimited, Some(until), Some(error)) =
                (status.state, status.retry_at, status.last_error.as_deref())
            {
                db.rate_limit_source(library, until, error, dirty).await
            } else {
                db.requeue_source(library, dirty).await
            }
        }
        Err(e) => Err(e),
    };
    if let Err(e) = recorded {
        eprintln!(
            "sharecraft-server: cannot record the sync of library {}: {e}",
            library.0
        );
    }
    answer
}

/// The first seven characters of a commit id.
fn short(commit: &str) -> &str {
    commit.get(..7).unwrap_or(commit)
}

/// The `etag` recorded as a sync starts applying (and kept when it fails or is cut short while
/// applying): the library may be half way between two commits, so the next sync reads the branch in full (no conditional request) and applies
/// its plan even when the branch is back at `last_commit`. It is never sent to GitHub.
const DIRTY_ETAG: &str = "sharecraft:applied-in-part";

/// A failed sync, and whether it failed while applying (see [`DIRTY_ETAG`]).
struct Failure {
    error: SyncError,
    applying: bool,
}

impl From<SyncError> for Failure {
    fn from(error: SyncError) -> Self {
        Failure {
            error,
            applying: false,
        }
    }
}

/// The GitHub details needed after a repository has been listed.
pub(crate) struct GithubSeen {
    repo: RepoRef,
    token: Option<Secret>,
    commit: String,
    tree: String,
    whole: Option<Whole>,
}

/// GitHub as a connected-library storage source.
pub(crate) struct GithubSource;

impl StorageSource for GithubSource {
    type Seen = GithubSeen;

    fn content(&self) -> ContentMode {
        ContentMode::Mirrored
    }

    async fn look(
        &self,
        state: &AppState,
        config: &SourceConfig,
        status: &SourceStatus,
        dirty: bool,
    ) -> Result<Look<Self::Seen>, SyncError> {
        let started = std::time::Instant::now();
        let repo = RepoRef::parse(&format!("{}/{}", config.owner, config.repo)).map_err(|_| {
            SyncError::NotConnected(
                "The repository's name is not one GitHub uses; connect the library again."
                    .to_owned(),
            )
        })?;
        let token = token_of(state, config).await?;
        let github = state.github();
        let sent_etag = if dirty { None } else { status.etag.as_deref() };
        let head = github
            .head(&repo, &config.branch, token.as_ref(), sent_etag)
            .await
            .map_err(github_error)?;
        let (commit, tree_sha, etag, author) = match head {
            Head::NotModified => {
                return Ok(Look::Unchanged {
                    etag: status.etag.clone(),
                });
            }
            Head::Moved {
                commit,
                tree,
                etag,
                author,
            } => (commit, tree, etag, author),
        };
        Ok(Look::Listing(Box::new(Listing {
            entries: Vec::new(),
            protected: HashSet::new(),
            bookmark: commit.clone(),
            etag,
            credit: author,
            hidden: Hidden::default(),
            took: started.elapsed(),
            seen: GithubSeen {
                repo,
                token,
                commit,
                tree: tree_sha,
                whole: None,
            },
        })))
    }

    async fn list(
        &self,
        state: &AppState,
        listing: &mut Listing<Self::Seen>,
    ) -> Result<(), SyncError> {
        let entries = state
            .github()
            .tree(
                &listing.seen.repo,
                &listing.seen.tree,
                listing.seen.token.as_ref(),
            )
            .await
            .map_err(github_error)?;
        listing.seen.whole = Some(Whole::of(&entries).ok_or_else(too_big_to_add)?);
        listing.entries = entries;
        Ok(())
    }

    async fn caps(&self, state: &AppState) -> Result<SourceCaps, SyncError> {
        let db = state.db();
        let read = |value: Option<serde_json::Value>, default: i64| {
            value
                .and_then(|v| v.as_i64())
                .filter(|n| *n >= 1)
                .unwrap_or(default)
        };
        let files = read(
            db.setting(SOURCE_MAX_FILES).await.map_err(db_failed)?,
            DEFAULT_SOURCE_MAX_FILES,
        );
        let bytes = read(
            db.setting(SOURCE_MAX_BYTES).await.map_err(db_failed)?,
            DEFAULT_SOURCE_MAX_BYTES,
        );
        Ok(SourceCaps {
            max_files: usize::try_from(files).unwrap_or(usize::MAX),
            max_bytes: Some(u64::try_from(bytes).unwrap_or(u64::MAX)),
        })
    }

    async fn fetch(
        &self,
        state: &AppState,
        config: &SourceConfig,
        listing: &Listing<Self::Seen>,
        caps: SourceCaps,
        needed: &HashMap<String, u64>,
    ) -> Result<HashMap<String, Held>, SyncError> {
        let whole = listing.seen.whole.ok_or_else(too_big_to_add)?;
        let reading = Reading {
            repo: &listing.seen.repo,
            token: listing.seen.token.as_ref(),
            commit: &listing.seen.commit,
            folder: config.folder.trim_matches('/'),
            whole,
        };
        if needed.is_empty() {
            return Ok(HashMap::new());
        }
        if caps.tarball_worth_trying(needed.len(), whole) {
            match by_tarball(state, &reading, needed).await {
                Ok(fetched) => return Ok(fetched),
                Err(TarFailure::Failed(e)) => return Err(e),
                Err(TarFailure::TooLarge) => {
                    println!(
                        "sharecraft-server: a tarball was over its limits; reading files singly"
                    );
                }
            }
        }
        by_blobs(state, &reading, needed).await
    }
}

/// The source-independent sync itself: reads, checks, then applies.
async fn run<S: StorageSource>(
    source: &S,
    state: &AppState,
    config: &SourceConfig,
    status: &SourceStatus,
    now: OffsetDateTime,
    generation: i64,
) -> Result<Outcome, Failure> {
    let db = state.db();
    let library = config.library;
    let dirty = status.etag.as_deref() == Some(DIRTY_ETAG);
    let mut listing = match source.look(state, config, status, dirty).await? {
        Look::Unchanged { etag } => {
            return Ok(Outcome::Unchanged {
                etag,
                listing_detail: None,
                took: std::time::Duration::ZERO,
            });
        }
        Look::Listing(listing) => *listing,
    };
    if !dirty
        && !listing.bookmark.is_empty()
        && status.last_commit.as_deref() == Some(listing.bookmark.as_str())
    {
        return Ok(Outcome::Unchanged {
            etag: listing.etag.or_else(|| status.etag.clone()),
            listing_detail: None,
            took: listing.took,
        });
    }
    let user = connector(state, config, &listing.bookmark, listing.credit.as_deref()).await?;
    source.list(state, &mut listing).await?;
    // The rows of items deleted into a bucket's holding folder stay with their bin entries
    // until they are restored or purged; the listing never sees the holding folder.
    let recorded: Vec<SourceItem> = db
        .source_items(library)
        .await
        .map_err(db_failed)?
        .into_iter()
        .filter(|item| !crate::s3_write::is_held_path(&item.repository_path))
        .collect();
    // Files with a change waiting to be written are left alone: a tracked one keeps its
    // recorded entry and nothing is planned for it; a new one's path is not taken from the
    // listing (its write may have landed before its finish was recorded).
    let pending = db.pending_writes_of(library).await.map_err(db_failed)?;
    if !pending.is_empty() {
        let tracked: HashSet<NodeId> = recorded.iter().map(|item| item.node).collect();
        let mut new_paths = HashSet::new();
        for row in pending {
            if tracked.contains(&row.node) {
                listing.protected.insert(row.node);
            } else {
                new_paths.insert(row.path);
            }
        }
        let folder = config.folder.trim_matches('/');
        listing.entries.retain(|entry| {
            below(&entry.path, folder).is_none_or(|path| !new_paths.contains(path))
        });
    }
    let protected_paths: HashSet<String> = recorded
        .iter()
        .filter(|item| listing.protected.contains(&item.node))
        .map(|item| item.repository_path.clone())
        .collect();

    // The library's file policies and the largest file the server takes, judged before names
    // are given, so a hidden file never pushes a shown sibling to a suffix.
    let policies = tree::policies_at(db, library).await.map_err(|e| {
        eprintln!(
            "sharecraft-server: cannot read the policies of library {}: {e}",
            library.0
        );
        failed("The library's file policies could not be read, so nothing was changed.")
    })?;
    let mut largest = state.config().max_file_bytes;
    if matches!(source.content(), ContentMode::External) {
        let settings = db.share_settings().await.map_err(db_failed)?;
        largest = largest.min(settings.cache_max_bytes / 2);
    }
    let mut refused = std::mem::take(&mut listing.hidden);
    let folder = config.folder.trim_matches('/');
    let tree: Vec<TreeEntry> = std::mem::take(&mut listing.entries)
        .into_iter()
        .filter(|entry| {
            if protected_paths.contains(entry.path.as_str()) {
                return false;
            }
            if entry.kind != EntryKind::File || below(&entry.path, folder).is_none() {
                return true;
            }
            let name = last_name(&entry.path);
            if let Some((settings, org)) = &policies {
                if check_name(name, settings, org).is_err() {
                    refused.policy += 1;
                    return false;
                }
                if check_size(entry.size, settings).is_err() {
                    refused.too_big += 1;
                    return false;
                }
            }
            if entry.size > largest {
                refused.too_big += 1;
                return false;
            }
            true
        })
        .collect();
    let bucket = matches!(config.kind, SourceKind::S3);
    let (mut shown, mut hidden) = shown_names_with_allocations(tree, folder, &recorded, bucket);
    shown.extend(
        recorded
            .iter()
            .filter(|item| listing.protected.contains(&item.node))
            .map(|item| Entry {
                path: item.path.clone(),
                repository_path: item.repository_path.clone(),
                sha: item.blob_sha.clone().unwrap_or_default(),
                size: u64::try_from(item.size).unwrap_or(0),
                dir: item.blob_sha.is_none(),
            }),
    );
    shown.sort_by(|left, right| left.path.cmp(&right.path));
    hidden.symlinks = hidden.symlinks.saturating_add(refused.symlinks);
    hidden.submodules = hidden.submodules.saturating_add(refused.submodules);
    hidden.policy = hidden.policy.saturating_add(refused.policy);
    hidden.too_big = hidden.too_big.saturating_add(refused.too_big);
    hidden.unnameable = hidden.unnameable.saturating_add(refused.unnameable);
    hidden.renamed = hidden.renamed.saturating_add(refused.renamed);
    hidden.system = hidden.system.saturating_add(refused.system);
    hidden.offline = hidden.offline.saturating_add(refused.offline);
    hidden.too_long = hidden.too_long.saturating_add(refused.too_long);
    hidden.unreadable = hidden.unreadable.saturating_add(refused.unreadable);
    hidden.archived = hidden.archived.saturating_add(refused.archived);

    let files = shown.iter().filter(|e| !e.dir).count();
    let bytes =
        total_size(shown.iter().filter(|e| !e.dir).map(|e| e.size)).ok_or_else(too_big_to_add)?;
    let caps = source.caps(state).await?;
    if caps.exceeded_by(files, bytes) {
        if files > caps.max_files {
            return Err(failed(&format!(
                "The repository has {files} files to show, more than the {} this server \
                 mirrors for one library, so nothing was changed.",
                caps.max_files
            ))
            .into());
        }
        if let Some(max_bytes) = caps.max_bytes {
            return Err(failed(&format!(
                "The repository's files hold {bytes} bytes, more than the {max_bytes} this server \
                 mirrors for one library, so nothing was changed."
            ))
            .into());
        }
    }

    // A tracked node is usable only when it is live at the displayed parent and name recorded
    // for it. A live file elsewhere is kept as a repair candidate so the planner moves the same
    // node back; a missing/deleted node (and a misplaced folder) is planned as lost.
    let locations: HashMap<NodeId, _> = db
        .source_item_locations(library)
        .await
        .map_err(db_failed)?
        .into_iter()
        .map(|location| (location.node, location))
        .collect();
    let mut ordered = recorded;
    ordered.sort_by_key(|item| item.path.matches('/').count());
    let mut known = Vec::new();
    let mut lost = Vec::new();
    let mut repairs = HashSet::new();
    let mut valid_folders: HashMap<String, NodeId> = HashMap::new();
    for item in ordered {
        if listing.protected.contains(&item.node) {
            if item.blob_sha.is_none() {
                valid_folders.insert(item.path.clone(), item.node);
            }
            known.push(item);
            continue;
        }
        let expected_parent = if parent_path(&item.path).is_empty() {
            Some(library)
        } else {
            valid_folders.get(parent_path(&item.path)).copied()
        };
        let location = locations.get(&item.node);
        let live = location.is_some_and(|at| !at.deleted && at.name.is_some());
        let correct = live
            && expected_parent.is_some_and(|parent| {
                location.is_some_and(|at| {
                    at.parent == Some(parent) && at.name.as_deref() == Some(last_name(&item.path))
                })
            });
        if correct {
            if item.blob_sha.is_none() {
                valid_folders.insert(item.path.clone(), item.node);
            }
            known.push(item);
        } else if live && item.blob_sha.is_some() {
            repairs.insert(item.node);
            known.push(item);
        } else {
            lost.push(item.node);
        }
    }
    let plannable_shown: Vec<Entry> = shown
        .iter()
        .filter(|entry| !protected_paths.contains(&entry.repository_path))
        .cloned()
        .collect();
    let plannable_known: Vec<SourceItem> = known
        .iter()
        .filter(|item| !listing.protected.contains(&item.node))
        .cloned()
        .collect();
    let mut plan = plan_with_repairs(&plannable_shown, &plannable_known, &repairs);
    if matches!(source.content(), ContentMode::External) {
        let zero_moves: Vec<(SourceItem, Entry)> = plan
            .moves
            .iter()
            .filter(|(_, entry)| entry.size == 0)
            .cloned()
            .collect();
        plan.moves.retain(|(_, entry)| entry.size != 0);
        for (item, entry) in zero_moves {
            plan.remove.push(item);
            plan.add.push(entry);
        }
        let missing: Vec<NodeId> = plan.remove.iter().map(|item| item.node).collect();
        db.clear_missing_except(library, &missing)
            .await
            .map_err(db_failed)?;
    }

    if matches!(source.content(), ContentMode::External)
        && lost.is_empty()
        && plan.add.is_empty()
        && plan.change.is_empty()
        && plan.remove.is_empty()
        && plan.moves.is_empty()
        && status.files == i64::try_from(files).unwrap_or(i64::MAX)
        && status.bytes == i64::try_from(bytes).unwrap_or(i64::MAX)
        && status.hidden == i64::try_from(hidden.total()).unwrap_or(i64::MAX)
    {
        return Ok(Outcome::Unchanged {
            etag: None,
            listing_detail: Some(hidden.detail()),
            took: listing.took,
        });
    }

    let mut needed: HashMap<String, u64> = HashMap::new();
    for entry in plan
        .add
        .iter()
        .chain(plan.change.iter().map(|(_, e)| e))
        .filter(|e| !e.dir)
    {
        needed.insert(entry.sha.clone(), entry.size);
    }
    let fetched = source.fetch(state, config, &listing, caps, &needed).await?;

    // Marked half applied before anything changes, so a sync cut short by the process ending
    // never answers unchanged later, even when the branch is back at the last commit. A sync
    // that finishes replaces the marker with the branch's entity tag.
    if matches!(source.content(), ContentMode::Mirrored) {
        db.mark_source_etag(library, DIRTY_ETAG)
            .await
            .map_err(db_failed)?;
    }
    let counts = apply(
        state,
        library,
        &user,
        &known,
        &lost,
        plan,
        &fetched,
        source.content(),
        config.kind,
        now,
        config.poll_seconds,
        generation,
    )
    .await
    .map_err(|error| Failure {
        error,
        applying: true,
    })?;
    drop(fetched);
    Ok(Outcome::Synced {
        report: SyncReport {
            commit: listing.bookmark,
            added: counts.added,
            changed: counts.changed,
            removed: counts.removed,
            moved: counts.moved,
            hidden,
            bytes,
        },
        etag: listing.etag,
        files,
        content: source.content(),
        took: listing.took,
    })
}

/// `Failed` with `message`.
fn failed(message: &str) -> SyncError {
    SyncError::Failed(message.to_owned())
}

/// The error for sizes that overflow when added up.
fn too_big_to_add() -> SyncError {
    failed("The repository lists sizes too large to add up, so nothing was changed.")
}

/// The sum of `sizes`; `None` if it overflows.
fn total_size(mut sizes: impl Iterator<Item = u64>) -> Option<u64> {
    sizes.try_fold(0u64, u64::checked_add)
}

/// `path` relative to `folder` (no outer slashes; empty for the whole repository), if it is
/// below it.
fn below<'a>(path: &'a str, folder: &str) -> Option<&'a str> {
    let relative = if folder.is_empty() {
        Some(path)
    } else {
        path.strip_prefix(folder)
            .and_then(|rest| rest.strip_prefix('/'))
    };
    relative.filter(|r| !r.is_empty())
}

/// A GitHub author name fit to be shown: control and invisible format characters dropped,
/// trimmed, at most [`MAX_AUTHOR_CHARS`]; `None` if nothing is left.
fn clean_author(author: &str) -> Option<String> {
    let kept: String = author
        .chars()
        .filter(|c| !c.is_control() && !is_format_char(*c))
        .collect();
    let cut: String = kept.trim().chars().take(MAX_AUTHOR_CHARS).collect();
    let cut = cut.trim_end().to_owned();
    (!cut.is_empty()).then_some(cut)
}

/// The error for a database failure, which is logged.
fn db_failed(e: impl std::fmt::Display) -> SyncError {
    eprintln!("sharecraft-server: database failure while syncing: {e}");
    failed("The database could not be reached, so the sync stopped.")
}

/// The plain error for a GitHub failure. Never the response or the token.
fn github_error(e: GithubError) -> SyncError {
    match e {
        GithubError::NotFound => SyncError::NotConnected(
            "The repository or branch was not found on GitHub, or the token cannot see it."
                .to_owned(),
        ),
        GithubError::Unauthorized => SyncError::NotConnected(
            "GitHub refused the token: it is wrong, expired or revoked, or may not read this \
             repository."
                .to_owned(),
        ),
        GithubError::RateLimited { retry_at } => SyncError::RateLimited(retry_at),
        GithubError::Truncated => failed(
            "The repository has too many files for GitHub to list at once, so nothing was \
             changed.",
        ),
        GithubError::TooLarge => {
            failed("A file from GitHub was larger than its listing said, so nothing was changed.")
        }
        GithubError::Network(_) => failed("GitHub could not be reached, so nothing was changed."),
        GithubError::Other(_) => {
            failed("GitHub's answer could not be used, so nothing was changed.")
        }
    }
}

/// The connection's token, opened, if it has one.
pub(crate) async fn token_of(
    state: &AppState,
    config: &SourceConfig,
) -> Result<Option<Secret>, SyncError> {
    let Some(id) = config.token else {
        return Ok(None);
    };
    let gone = || {
        SyncError::NotConnected(
            "The stored token could not be opened; set the token again.".to_owned(),
        )
    };
    let plain = match state
        .secret_store()
        .get(state.db(), id, &token_context(config.library))
        .await
    {
        Ok(Some(plain)) => plain,
        Ok(None) => return Err(gone()),
        // Worth trying again later: the sync is not marked disconnected.
        Err(SecretError::Unavailable(sentence)) => return Err(failed(&sentence)),
        Err(_) => return Err(gone()),
    };
    String::from_utf8(plain)
        .map(|text| Some(Secret::new(text)))
        .map_err(|_| gone())
}

/// The person who connected the library, as the sync is credited: their account, named
/// `<name> via GitHub <commit>`, with ` by <author>` when the commit names one, so each
/// revision says where it came from.
async fn connector(
    state: &AppState,
    config: &SourceConfig,
    commit: &str,
    author: Option<&str>,
) -> Result<User, SyncError> {
    let user = state
        .db()
        .user(config.connected_by)
        .await
        .map_err(db_failed)?
        .filter(|user| !user.disabled)
        .ok_or_else(|| {
            SyncError::NotConnected(
                "The person who connected this library no longer has an account; connect it \
                 again."
                    .to_owned(),
            )
        })?;
    let by: String = author
        .and_then(clean_author)
        .map(|a| format!(" by {a}"))
        .unwrap_or_default();
    Ok(User {
        name: format!("{} via GitHub {}{by}", user.name, short(commit)),
        ..user
    })
}

/// The caps on what one library mirrors: files and bytes.
pub(crate) async fn caps(state: &AppState) -> Result<(usize, u64), SyncError> {
    let caps = GithubSource.caps(state).await?;
    Ok((caps.max_files, caps.max_bytes.unwrap_or(u64::MAX)))
}

/// The whole tree's size: its entries, and its files' bytes and the largest.
#[derive(Clone, Copy, Debug)]
struct Whole {
    entries: usize,
    bytes: u64,
    largest: u64,
}

impl SourceCaps {
    fn tarball_worth_trying(self, needed: usize, whole: Whole) -> bool {
        needed > TARBALL_FROM
            && self
                .max_bytes
                .is_some_and(|max_bytes| whole.bytes <= max_bytes)
    }
}

impl Whole {
    /// `None` when the sizes overflow when added up.
    fn of(tree: &[TreeEntry]) -> Option<Whole> {
        Some(Whole {
            entries: tree.len(),
            bytes: total_size(tree.iter().map(|e| e.size))?,
            largest: tree.iter().map(|e| e.size).max().unwrap_or(0),
        })
    }
}

/// What reading the files needs.
struct Reading<'a> {
    repo: &'a RepoRef,
    token: Option<&'a Secret>,
    commit: &'a str,
    /// The mirrored folder, without outer slashes.
    folder: &'a str,
    whole: Whole,
}

/// Why reading the tarball did not work.
enum TarFailure {
    /// It is larger than the limits allow: the files are read one by one instead.
    TooLarge,
    Failed(SyncError),
}

/// The message for bytes that do not match their id.
const MISMATCH: &str =
    "A file read from GitHub did not match the repository's record of it, so nothing was changed.";

/// Reads `needed` one blob request each, [`BLOBS_AT_ONCE`] at a time.
async fn by_blobs(
    state: &AppState,
    reading: &Reading<'_>,
    needed: &HashMap<String, u64>,
) -> Result<HashMap<String, Held>, SyncError> {
    // Each request owns what it uses, so the whole sync stays `Send` for any caller.
    let reads = needed.iter().map(|(sha, size)| {
        let github = state.github().clone();
        let blobs = Arc::clone(state.blobs());
        let repo = reading.repo.clone();
        let token = reading.token.cloned();
        let (sha, size) = (sha.clone(), *size);
        async move {
            let bytes = github
                .blob(&repo, &sha, token.as_ref(), size)
                .await
                .map_err(github_error)?;
            if git_blob_id(&bytes) != sha {
                return Err(failed(MISMATCH));
            }
            let held = blobs.write_bytes(bytes).await.map_err(store_failed)?;
            Ok((sha, held))
        }
    });
    let reads: Vec<_> = reads.collect();
    futures_util::stream::iter(reads)
        .buffer_unordered(BLOBS_AT_ONCE)
        .try_collect()
        .await
}

/// The error for content that could not be stored, which is logged.
fn store_failed(e: String) -> SyncError {
    eprintln!("sharecraft-server: cannot store a file read from GitHub: {e}");
    failed("A file read from GitHub could not be stored, so nothing was changed.")
}

/// Reads `needed` from the commit's tarball, downloaded to a temporary file in the uploads
/// folder and read in one pass: each file below the folder whose size is needed is read, and
/// kept when its blob id is.
async fn by_tarball(
    state: &AppState,
    reading: &Reading<'_>,
    needed: &HashMap<String, u64>,
) -> Result<HashMap<String, Held>, TarFailure> {
    let unreadable = || {
        TarFailure::Failed(failed(
            "The download from GitHub could not be read, so nothing was changed.",
        ))
    };
    let dir = uploads_path(&state.config().data_dir);
    tokio::fs::create_dir_all(&dir).await.map_err(|e| {
        eprintln!("sharecraft-server: cannot make the uploads folder: {e}");
        unreadable()
    })?;
    let dest = dir.join(format!("github-{}.tar.gz", Uuid::new_v4().simple()));
    let whole = reading.whole;
    let entries = u64::try_from(whole.entries).unwrap_or(u64::MAX);
    // The download is compressed: never more than its contents and headers.
    let limit = whole
        .bytes
        .saturating_add(entries.saturating_mul(4096))
        .saturating_add(TAR_SLACK);
    match state
        .github()
        .tarball(reading.repo, reading.commit, reading.token, &dest, limit)
        .await
    {
        Ok(()) => {}
        Err(GithubError::TooLarge) => return Err(TarFailure::TooLarge),
        Err(e) => return Err(TarFailure::Failed(github_error(e))),
    }
    // The listing bounds what the tarball holds, with room for what it adds besides the listed
    // files: GitHub's pax header, long-name records and the like, a few KiB each.
    let limits = TarLimits {
        max_entry_bytes: whole.largest.max(TAR_SLACK),
        max_total_bytes: whole
            .bytes
            .saturating_add(entries.saturating_mul(4096))
            .saturating_add(TAR_SLACK),
        // The listing's entries, the top folder and the header GitHub adds.
        max_entries: whole.entries.saturating_add(16),
    };
    let handle = tokio::runtime::Handle::current();
    let blobs = Arc::clone(state.blobs());
    let wanted = needed.clone();
    let prefix = if reading.folder.is_empty() {
        String::new()
    } else {
        format!("{}/", reading.folder)
    };
    let path = dest.clone();
    let read = tokio::task::spawn_blocking(move || {
        let sizes: HashSet<u64> = wanted.values().copied().collect();
        let mut got: HashMap<String, Held> = HashMap::new();
        let result = for_each_in_tarball(&path, &limits, &mut |name, reader, size| {
            if !name.starts_with(&prefix) || !sizes.contains(&size) {
                return Ok(());
            }
            let mut bytes = Vec::new();
            reader.take(size).read_to_end(&mut bytes)?;
            let id = git_blob_id(&bytes);
            if wanted.contains_key(&id) && !got.contains_key(&id) {
                let held = handle
                    .block_on(blobs.write_bytes(bytes))
                    .map_err(std::io::Error::other)?;
                got.insert(id, held);
            }
            Ok(())
        });
        let _ = std::fs::remove_file(&path);
        result.map(|()| got)
    })
    .await;
    let got = match read {
        Ok(Ok(got)) => got,
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::FileTooLarge => {
            return Err(TarFailure::TooLarge);
        }
        Ok(Err(e)) => {
            eprintln!(
                "sharecraft-server: cannot read a tarball from GitHub: {}",
                e.kind()
            );
            return Err(unreadable());
        }
        Err(e) => {
            eprintln!("sharecraft-server: reading a tarball did not finish: {e}");
            let _ = tokio::fs::remove_file(&dest).await;
            return Err(unreadable());
        }
    };
    if got.len() < needed.len() {
        return Err(TarFailure::Failed(failed(
            "The download from GitHub did not hold every file it should, so nothing was changed.",
        )));
    }
    Ok(got)
}

/// What applying a plan did.
#[derive(Default)]
struct Counts {
    added: usize,
    changed: usize,
    removed: usize,
    moved: usize,
}

/// The `/`-separated parent path of `path`; empty at the top.
fn parent_path(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

/// The plan's changes being made in one library.
struct Applying<'a> {
    state: &'a AppState,
    library: NodeId,
    user: &'a User,
    mode: ContentMode,
    kind: SourceKind,
    /// The node of each folder path, as known or made.
    folders: HashMap<String, NodeId>,
    /// Every node recorded as an item when the sync started.
    tracked: HashSet<NodeId>,
    fetched: &'a HashMap<String, Held>,
}

/// Applies `plan` in `library` for `user`, with the bytes `fetched` (blob id to stored
/// content). First the rows of the `lost` items (nodes gone outside the sync) are dropped;
/// then files removed (freeing their names), folders made, shallowest first, files moved (out
/// of the folders that go; under a temporary name where the final one is still taken), the
/// folders gone binned, deepest first, the moved files given their final names, and last the
/// files changed and added. Each item is recorded as it is done.
#[allow(clippy::too_many_arguments)]
async fn apply(
    state: &AppState,
    library: NodeId,
    user: &User,
    known: &[SourceItem],
    lost: &[NodeId],
    mut plan: Plan,
    fetched: &HashMap<String, Held>,
    mode: ContentMode,
    kind: SourceKind,
    now: OffsetDateTime,
    poll_seconds: u32,
    generation: i64,
) -> Result<Counts, SyncError> {
    if !lost.is_empty() {
        state
            .db()
            .remove_source_items(library, lost)
            .await
            .map_err(db_failed)?;
    }
    if matches!(mode, ContentMode::External) && !plan.remove.is_empty() {
        let nodes: Vec<NodeId> = plan.remove.iter().map(|item| item.node).collect();
        let seconds = i64::from(poll_seconds.saturating_mul(MISSING_GRACE_POLLS));
        let expired = state
            .db()
            .note_missing(library, &nodes, now, time::Duration::seconds(seconds))
            .await
            .map_err(db_failed)?;
        let expired: HashSet<NodeId> = expired.into_iter().collect();
        plan.remove.retain(|item| expired.contains(&item.node));
    }
    let mut work = Applying {
        state,
        library,
        user,
        mode,
        kind,
        folders: known
            .iter()
            .filter(|item| item.blob_sha.is_none())
            .map(|item| (item.path.clone(), item.node))
            .collect(),
        tracked: known.iter().map(|item| item.node).collect(),
        fetched,
    };
    let mut counts = Counts::default();
    let mut applied_operations = 0_usize;
    let (gone_folders, gone_files): (Vec<SourceItem>, Vec<SourceItem>) = plan
        .remove
        .into_iter()
        .partition(|item| item.blob_sha.is_none());
    // Folders whose node a new folder (the same name in another case) takes over.
    let mut gone_folders: HashMap<NodeId, SourceItem> = gone_folders
        .into_iter()
        .map(|item| (item.node, item))
        .collect();

    for item in gone_files {
        work.to_bin(&item).await?;
        counts.removed += 1;
        applied_operations = applied_operations.saturating_add(1);
        check_generation(state, library, generation, applied_operations).await?;
    }
    if matches!(mode, ContentMode::External) {
        // A file replacing a folder needs that folder's name freed before the combined batch is
        // created. Other gone folders stay until after moves, preserving the existing ordering.
        let file_add_paths: HashSet<&str> = plan
            .add
            .iter()
            .filter(|entry| !entry.dir)
            .map(|entry| entry.path.as_str())
            .collect();
        let replaced_folders: Vec<NodeId> = gone_folders
            .iter()
            .filter(|(_, item)| file_add_paths.contains(item.path.as_str()))
            .map(|(node, _)| *node)
            .collect();
        for node in replaced_folders {
            let Some(item) = gone_folders.remove(&node) else {
                continue;
            };
            work.to_bin(&item).await?;
            work.folders.remove(&item.path);
            applied_operations = applied_operations.saturating_add(1);
            check_generation(state, library, generation, applied_operations).await?;
        }
        let adds: Vec<&Entry> = plan
            .add
            .iter()
            .filter(|entry| entry.dir)
            .chain(plan.add.iter().filter(|entry| !entry.dir))
            .collect();
        let added = work
            .add_external_batches(
                &adds,
                &mut gone_folders,
                state.config().normalized_source_create_batch_size(),
                generation,
            )
            .await?;
        counts.added = counts.added.saturating_add(added);
        applied_operations = applied_operations.saturating_add(adds.len());
    } else {
        for entry in plan.add.iter().filter(|e| e.dir) {
            work.add_folder(entry, &mut gone_folders).await?;
            applied_operations = applied_operations.saturating_add(1);
            check_generation(state, library, generation, applied_operations).await?;
        }
    }
    let mut waiting = Vec::new();
    for (item, entry) in &plan.moves {
        if let Some(later) = work.move_file(item, entry).await? {
            waiting.push(later);
        }
        counts.moved += 1;
        applied_operations = applied_operations.saturating_add(1);
        check_generation(state, library, generation, applied_operations).await?;
    }
    let mut gone_folders: Vec<SourceItem> = gone_folders.into_values().collect();
    gone_folders.sort_by(|a, b| b.path.cmp(&a.path));
    for item in gone_folders {
        work.to_bin(&item).await?;
        applied_operations = applied_operations.saturating_add(1);
        check_generation(state, library, generation, applied_operations).await?;
    }
    for (node, entry) in waiting {
        work.final_name(node, entry).await?;
        applied_operations = applied_operations.saturating_add(1);
        check_generation(state, library, generation, applied_operations).await?;
    }
    for (item, entry) in &plan.change {
        work.new_version(item.node, entry).await?;
        counts.changed += 1;
        applied_operations = applied_operations.saturating_add(1);
        check_generation(state, library, generation, applied_operations).await?;
    }
    if !matches!(mode, ContentMode::External) {
        for entry in plan.add.iter().filter(|e| !e.dir) {
            work.add_file(entry).await?;
            counts.added += 1;
            applied_operations = applied_operations.saturating_add(1);
            check_generation(state, library, generation, applied_operations).await?;
        }
    }
    Ok(counts)
}

async fn check_generation(
    state: &AppState,
    library: NodeId,
    generation: i64,
    applied_operations: usize,
) -> Result<(), SyncError> {
    if !applied_operations.is_multiple_of(100) {
        return Ok(());
    }
    let current = state
        .db()
        .source_generation(library)
        .await
        .map_err(db_failed)?;
    if current == Some(generation) {
        Ok(())
    } else {
        Err(failed(
            "The source changed while its files were being listed, so the sync stopped.",
        ))
    }
}

/// `name` with a suffix no other name has, before the extension its editor needs: the
/// temporary name of a file moved to where its final name is still taken.
fn temporary_name(name: &str) -> String {
    let editor = Editor::for_file_name(name);
    let lower = name.to_lowercase();
    let split = match editor.native_extension() {
        Some(ext) if lower.ends_with(ext) => name.len().saturating_sub(ext.len()),
        _ => name.rfind('.').filter(|dot| *dot > 0).unwrap_or(name.len()),
    };
    let (stem, extension) = name.split_at_checked(split).unwrap_or((name, ""));
    let tag = Uuid::new_v4().simple().to_string();
    let suffix = format!(" ~sync {}", tag.get(..8).unwrap_or(&tag));
    tree::fit(stem, extension, &suffix)
}

fn external_mtime(entry: &Entry, kind: SourceKind) -> Result<i64, SyncError> {
    external_tag_parts(&entry.sha, entry.size)
        .map(|parts| parts.mtime_ns)
        .ok_or_else(|| {
            if kind == SourceKind::S3 {
                failed("An object in the bucket had an invalid change tag.")
            } else {
                failed("A file on the share had an invalid change tag.")
            }
        })
}

impl Applying<'_> {
    /// Creates new share nodes in count-bounded commits. Existing untracked nodes keep the
    /// adopt-by-path replacement used before batching.
    async fn add_external_batches(
        &mut self,
        entries: &[&Entry],
        gone: &mut HashMap<NodeId, SourceItem>,
        batch_size: usize,
        generation: i64,
    ) -> Result<usize, SyncError> {
        let mut batch = Vec::with_capacity(batch_size);
        let mut added_files = 0_usize;
        let mut adopted_since_check = 0_usize;
        let mut children: HashMap<NodeId, HashMap<String, Option<Node>>> = HashMap::new();
        let mut git_ids = tree::external_tree_ids(self.state).await.map_err(|e| {
            eprintln!("sharecraft-server: sync could not read the content tree: {e:?}");
            failed("The content tree could not be read, so the sync stopped.")
        })?;
        for entry in entries {
            let parent = self.folder(parent_path(&entry.path))?;
            let name = last_name(&entry.path);
            if let HashEntry::Vacant(slot) = children.entry(parent) {
                let known = self
                    .state
                    .db()
                    .live_children(parent)
                    .await
                    .map_err(db_failed)?
                    .into_iter()
                    .map(|node| (node.name.to_lowercase(), Some(node)))
                    .collect();
                slot.insert(known);
            }
            let existing = children
                .get(&parent)
                .and_then(|known| known.get(&name.to_lowercase()))
                .cloned()
                .flatten();
            if entry.dir {
                match existing {
                    Some(node)
                        if node.kind == NodeKind::Folder && !self.tracked.contains(&node.id) =>
                    {
                        self.record(node.id, entry).await?;
                        self.folders.insert(entry.path.clone(), node.id);
                        adopted_since_check = adopted_since_check.saturating_add(1);
                        if adopted_since_check == batch_size {
                            self.ensure_external_generation(generation).await?;
                            adopted_since_check = 0;
                        }
                        continue;
                    }
                    Some(node)
                        if node.kind == NodeKind::Folder && gone.remove(&node.id).is_some() =>
                    {
                        if node.name != name {
                            self.change(
                                TreeOp::Rename {
                                    id: node.id,
                                    name: name.to_owned(),
                                },
                                &entry.path,
                            )
                            .await?;
                        }
                        self.record(node.id, entry).await?;
                        self.folders.insert(entry.path.clone(), node.id);
                        adopted_since_check = adopted_since_check.saturating_add(1);
                        if adopted_since_check == batch_size {
                            self.ensure_external_generation(generation).await?;
                            adopted_since_check = 0;
                        }
                        continue;
                    }
                    Some(_) => return Err(taken(&entry.path)),
                    None => {}
                }
            } else {
                match existing {
                    Some(node)
                        if node.kind == NodeKind::Document && !self.tracked.contains(&node.id) =>
                    {
                        self.record(node.id, entry).await?;
                        added_files = added_files.saturating_add(1);
                        adopted_since_check = adopted_since_check.saturating_add(1);
                        if adopted_since_check == batch_size {
                            self.ensure_external_generation(generation).await?;
                            adopted_since_check = 0;
                        }
                        continue;
                    }
                    Some(_) => return Err(taken(&entry.path)),
                    None => {}
                }
            }
            let id = NodeId(Uuid::new_v4());
            if let Some(known) = children.get_mut(&parent) {
                known.insert(name.to_lowercase(), None);
            }
            if entry.dir {
                self.folders.insert(entry.path.clone(), id);
            } else {
                added_files = added_files.saturating_add(1);
            }
            let mtime_ns = if entry.dir {
                0
            } else {
                external_mtime(entry, self.kind)?
            };
            batch.push(tree::ExternalNodeCreate {
                id,
                parent,
                name: name.to_owned(),
                path: entry.path.clone(),
                repository_path: entry.repository_path.clone(),
                size: entry.size,
                mtime_ns,
                tag: (!entry.dir).then(|| entry.sha.clone()),
                editor: (!entry.dir).then(|| Editor::for_file_name(name)),
            });
            if batch.len() == batch_size {
                self.commit_external_batch(&batch, generation, &git_ids)
                    .await?;
                git_ids.extend(batch.iter().map(|create| create.id.0));
                batch.clear();
            }
        }
        if !batch.is_empty() {
            self.commit_external_batch(&batch, generation, &git_ids)
                .await?;
        }
        if adopted_since_check > 0 {
            self.ensure_external_generation(generation).await?;
        }
        Ok(added_files)
    }

    async fn commit_external_batch(
        &self,
        batch: &[tree::ExternalNodeCreate],
        generation: i64,
        git_ids: &HashSet<Uuid>,
    ) -> Result<(), SyncError> {
        tree::create_external_nodes(self.state, self.user, self.library, batch, git_ids)
            .await
            .map_err(|e| {
                if self.kind == SourceKind::S3 {
                    eprintln!("sharecraft-server: sync could not add a batch of S3 items: {e:?}");
                    failed("A batch of S3 items could not be added, so the sync stopped.")
                } else {
                    eprintln!("sharecraft-server: sync could not add a file-share batch: {e:?}");
                    failed("A batch of file-share items could not be added, so the sync stopped.")
                }
            })?;
        self.ensure_external_generation(generation).await
    }

    async fn ensure_external_generation(&self, generation: i64) -> Result<(), SyncError> {
        let current = self
            .state
            .db()
            .source_generation(self.library)
            .await
            .map_err(db_failed)?;
        if current == Some(generation) {
            Ok(())
        } else {
            Err(failed(
                "The source changed while its files were being listed, so the sync stopped.",
            ))
        }
    }

    /// The node of the folder at `path` (the library at the top).
    fn folder(&self, path: &str) -> Result<NodeId, SyncError> {
        if path.is_empty() {
            return Ok(self.library);
        }
        self.folders.get(path).copied().ok_or_else(|| {
            failed(&format!(
                "The folder {path} was not made, so the sync stopped."
            ))
        })
    }

    /// Records `node` as holding `entry`, replacing any item it held before.
    async fn record(&self, node: NodeId, entry: &Entry) -> Result<(), SyncError> {
        let db = self.state.db();
        if matches!(self.mode, ContentMode::External) && !entry.dir {
            let mtime_ns = external_mtime(entry, self.kind)?;
            return db
                .record_external_item(
                    self.library,
                    &ExternalEntry {
                        node,
                        path: entry.path.clone(),
                        repository_path: entry.repository_path.clone(),
                        size: entry.size,
                        mtime_ns,
                        tag: Some(entry.sha.clone()),
                    },
                )
                .await
                .map_err(db_failed);
        }
        let item = SourceItem {
            node,
            path: entry.path.clone(),
            repository_path: entry.repository_path.clone(),
            blob_sha: (!entry.dir).then(|| entry.sha.clone()),
            size: i64::try_from(entry.size).unwrap_or(i64::MAX),
        };
        db.replace_source_item(self.library, &item)
            .await
            .map_err(db_failed)
    }

    /// A fresh hold on the content of blob `sha`, read earlier.
    async fn held(&self, sha: &str, path: &str) -> Result<Held, SyncError> {
        let missing = || {
            failed(&format!(
                "The content of {path} was not read, so the sync stopped."
            ))
        };
        let blob = self.fetched.get(sha).ok_or_else(missing)?.blob();
        self.state
            .blobs()
            .hold_stored(blob)
            .await
            .ok_or_else(missing)
    }

    /// Moves the node of `item` to the recycle bin and forgets the item. A node gone already
    /// is only forgotten.
    async fn to_bin(&self, item: &SourceItem) -> Result<(), SyncError> {
        let live = self
            .state
            .db()
            .node(item.node)
            .await
            .map_err(db_failed)?
            .is_some_and(|n| !n.deleted);
        if live {
            delete_one(self.state, self.user, item.node)
                .await
                .map_err(|e| {
                    eprintln!(
                        "sharecraft-server: sync could not delete {}: {e:?}",
                        item.path
                    );
                    failed(&format!(
                        "{} could not be moved to the recycle bin, so the sync stopped.",
                        item.path
                    ))
                })?;
        }
        self.state
            .db()
            .remove_source_items(self.library, &[item.node])
            .await
            .map_err(db_failed)
    }

    /// Makes the folder `entry`, or adopts one of its name there: one the sync made but did not
    /// record, or a recorded one about to go whose name differs only in case.
    async fn add_folder(
        &mut self,
        entry: &Entry,
        gone: &mut HashMap<NodeId, SourceItem>,
    ) -> Result<(), SyncError> {
        let parent = self.folder(parent_path(&entry.path))?;
        let name = last_name(&entry.path);
        let db = self.state.db();
        let existing = db.live_child_named(parent, name).await.map_err(db_failed)?;
        let node = match existing {
            Some(node) if node.kind == NodeKind::Folder && !self.tracked.contains(&node.id) => {
                node.id
            }
            Some(node) if node.kind == NodeKind::Folder && gone.remove(&node.id).is_some() => {
                if node.name != name {
                    self.change(
                        TreeOp::Rename {
                            id: node.id,
                            name: name.to_owned(),
                        },
                        &entry.path,
                    )
                    .await?;
                }
                node.id
            }
            Some(_) => return Err(taken(&entry.path)),
            None => {
                let id = NodeId(Uuid::new_v4());
                self.change(
                    TreeOp::Create {
                        parent: Some(parent),
                        kind: NodeKind::Folder,
                        name: name.to_owned(),
                        id,
                    },
                    &entry.path,
                )
                .await?;
                id
            }
        };
        self.record(node, entry).await?;
        self.folders.insert(entry.path.clone(), node);
        Ok(())
    }

    /// Applies one tree change for the sync's person.
    async fn change(&self, op: TreeOp, path: &str) -> Result<(), SyncError> {
        tree::apply(self.state, self.user, op)
            .await
            .map(drop)
            .map_err(|e| {
                eprintln!("sharecraft-server: sync could not change {path}: {e}");
                failed(&format!(
                    "{path} could not be written, so the sync stopped."
                ))
            })
    }

    /// Renames the file `node` to `name`, telling its open session.
    async fn rename(&self, node: NodeId, name: &str, path: &str) -> Result<(), SyncError> {
        self.change(
            TreeOp::Rename {
                id: node,
                name: name.to_owned(),
            },
            path,
        )
        .await?;
        after_rename(self.state, DocId(node.0)).await;
        Ok(())
    }

    /// Moves the node of `item` to where `entry` is, as a move by hand is made: through
    /// [`move_one`], which rechecks the open sessions below it, so anyone who cannot see the
    /// new place loses the file. A file that is also renamed first takes a temporary name, so
    /// its old name cannot collide where it goes. When its final name is still taken there (by
    /// a folder about to go), the node and entry are answered, for [`Applying::final_name`]
    /// once the folders are gone; otherwise it is renamed and recorded now.
    async fn move_file(
        &self,
        item: &SourceItem,
        entry: &Entry,
    ) -> Result<Option<(NodeId, Entry)>, SyncError> {
        let parent = self.folder(parent_path(&entry.path))?;
        let name = last_name(&entry.path);
        let db = self.state.db();
        let node = db
            .node(item.node)
            .await
            .map_err(db_failed)?
            .ok_or_else(|| failed(&format!("{} is gone, so the sync stopped.", item.path)))?;
        let free = db
            .name_free(parent, name, Some(node.id))
            .await
            .map_err(db_failed)?;
        let moving = node.parent != Some(parent);
        // Where the final name is free and only the folder changes, the node moves as it is.
        if !free || (moving && node.name != name) {
            self.rename(node.id, &temporary_name(name), &entry.path)
                .await?;
        }
        if moving {
            move_one(self.state, self.user, node.id, parent)
                .await
                .map_err(|e| {
                    eprintln!(
                        "sharecraft-server: sync could not move {}: {e:?}",
                        entry.path
                    );
                    failed(&format!(
                        "{} could not be moved, so the sync stopped.",
                        entry.path
                    ))
                })?;
        }
        if !free {
            return Ok(Some((node.id, entry.clone())));
        }
        self.final_name(node.id, entry.clone()).await?;
        Ok(None)
    }

    /// Gives the moved file `node` the name of `entry`, if it has not, and records it.
    async fn final_name(&self, node: NodeId, entry: Entry) -> Result<(), SyncError> {
        let name = last_name(&entry.path);
        let current = self
            .state
            .db()
            .node(node)
            .await
            .map_err(db_failed)?
            .ok_or_else(|| failed(&format!("{} is gone, so the sync stopped.", entry.path)))?;
        if current.name != name {
            self.rename(node, name, &entry.path).await?;
        }
        self.record(node, &entry).await
    }

    /// Stores the content of `entry` as a new version of the file `node`, through its file
    /// session as an uploaded version is, and records it.
    async fn new_version(&self, node: NodeId, entry: &Entry) -> Result<(), SyncError> {
        if matches!(self.mode, ContentMode::External) {
            return self.record(node, entry).await;
        }
        let db = self.state.db();
        let doc = DocId(node.0);
        let document = db
            .document(doc)
            .await
            .map_err(db_failed)?
            .ok_or_else(|| failed(&format!("{} is gone, so the sync stopped.", entry.path)))?;
        let mut base = document
            .revision
            .as_deref()
            .and_then(|text| text.parse::<RevisionId>().ok());
        let mut reply = SaveReply::Failed;
        for _ in 0..VERSION_ATTEMPTS {
            let held = self.held(&entry.sha, &entry.path).await?;
            reply = store_version(
                self.state,
                self.user,
                doc,
                &document.name,
                base.clone(),
                held,
            )
            .await;
            match &reply {
                SaveReply::Conflict { current } => base = current.clone(),
                _ => break,
            }
        }
        match reply {
            SaveReply::Saved(_) | SaveReply::Unchanged(_) => self.record(node, entry).await,
            SaveReply::BeingEdited => Err(failed(&format!(
                "{} is open for editing, so its new version waits for the next sync.",
                entry.path
            ))),
            _ => Err(failed(&format!(
                "The new version of {} could not be stored, so the sync stopped.",
                entry.path
            ))),
        }
    }

    /// Makes the file `entry` with its content as its first version, or, for a file of its
    /// name there that the sync made but did not record, stores it as a new version.
    async fn add_file(&self, entry: &Entry) -> Result<(), SyncError> {
        let parent = self.folder(parent_path(&entry.path))?;
        let name = last_name(&entry.path);
        let existing = self
            .state
            .db()
            .live_child_named(parent, name)
            .await
            .map_err(db_failed)?;
        match existing {
            Some(node) if node.kind == NodeKind::Document && !self.tracked.contains(&node.id) => {
                if matches!(self.mode, ContentMode::External) {
                    return self.record(node.id, entry).await;
                }
                return self.new_version(node.id, entry).await;
            }
            Some(_) => return Err(taken(&entry.path)),
            None => {}
        }
        if matches!(self.mode, ContentMode::External) {
            let id = DocId(Uuid::new_v4());
            tree::create_document(
                self.state,
                self.user,
                parent,
                id,
                name,
                Editor::for_file_name(name),
                Some(entry.size),
            )
            .await
            .map_err(|e| {
                eprintln!(
                    "sharecraft-server: sync could not add {}: {e:?}",
                    entry.path
                );
                failed(&format!(
                    "{} could not be added, so the sync stopped.",
                    entry.path
                ))
            })?;
            return self.record(NodeId(id.0), entry).await;
        }
        let held = self.held(&entry.sha, &entry.path).await?;
        let created = create_uploaded(
            self.state,
            self.user,
            parent,
            name,
            Editor::for_file_name(name),
            held,
            false,
        )
        .await;
        match created {
            Ok(Created::Done { doc, .. }) => self.record(NodeId(doc.0), entry).await,
            Ok(Created::Failed(_)) => Err(failed(&format!(
                "{} could not be stored, so the sync stopped.",
                entry.path
            ))),
            Err(e) => {
                eprintln!(
                    "sharecraft-server: sync could not add {}: {e:?}",
                    entry.path
                );
                Err(failed(&format!(
                    "{} could not be added, so the sync stopped.",
                    entry.path
                )))
            }
        }
    }
}

/// The error for a path whose name something else in the library holds.
fn taken(path: &str) -> SyncError {
    failed(&format!(
        "{path} cannot be shown: something else in the library has its name."
    ))
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use collab_core::ids::NodeId;
    use uuid::Uuid;

    use super::*;
    use crate::github::EntryKind;

    fn file(path: &str, sha: &str) -> Entry {
        Entry {
            path: path.to_owned(),
            repository_path: path.to_owned(),
            sha: sha.to_owned(),
            size: 1,
            dir: false,
        }
    }

    fn known(path: &str, sha: &str) -> SourceItem {
        SourceItem {
            node: NodeId(Uuid::new_v4()),
            path: path.to_owned(),
            repository_path: path.to_owned(),
            blob_sha: Some(sha.to_owned()),
            size: 1,
        }
    }

    fn tree(path: &str, kind: EntryKind, sha: &str) -> TreeEntry {
        TreeEntry {
            path: path.to_owned(),
            kind,
            sha: sha.to_owned(),
            size: 3,
        }
    }

    fn paths(entries: &[Entry]) -> Vec<&str> {
        entries.iter().map(|e| e.path.as_str()).collect()
    }

    fn file_paths(entries: &[Entry]) -> Vec<&str> {
        entries
            .iter()
            .filter(|e| !e.dir)
            .map(|e| e.path.as_str())
            .collect()
    }

    #[test]
    fn sizes_that_overflow_are_refused_not_wrapped() {
        assert_eq!(total_size([1, 2, 3].into_iter()), Some(6));
        assert_eq!(total_size([u64::MAX, 1].into_iter()), None);
        let huge = |path: &str| TreeEntry {
            path: path.to_owned(),
            kind: EntryKind::File,
            sha: "s".to_owned(),
            size: u64::MAX / 2 + 1,
        };
        assert!(Whole::of(&[huge("a"), huge("b")]).is_none());
        assert!(Whole::of(&[huge("a")]).is_some());
    }

    #[test]
    fn one_cap_snapshot_drives_enforcement_and_tarball_selection() {
        let whole = Whole {
            entries: 25,
            bytes: 80,
            largest: 4,
        };
        let snapshot = SourceCaps {
            max_files: 30,
            max_bytes: Some(100),
        };
        assert!(!snapshot.exceeded_by(25, 80));
        assert!(snapshot.tarball_worth_trying(25, whole));

        let changed_later = SourceCaps {
            max_files: 30,
            max_bytes: Some(50),
        };
        assert!(changed_later.exceeded_by(25, 80));
        assert!(!changed_later.tarball_worth_trying(25, whole));
    }

    #[test]
    fn author_names_lose_control_and_format_characters() {
        assert_eq!(
            clean_author("Octo\u{202E}Cat\u{200B}\n").as_deref(),
            Some("OctoCat")
        );
        assert_eq!(clean_author(" \u{2066}\t "), None);
        let long = "n".repeat(300);
        assert_eq!(clean_author(&long).map(|a| a.chars().count()), Some(100));
    }

    #[test]
    fn temporary_names_keep_the_editor_extension() {
        for name in ["report.docx", "model.solidcraft.json", "notes.md", "README"] {
            let temporary = temporary_name(name);
            assert_ne!(temporary, name);
            assert_eq!(
                Editor::for_file_name(&temporary),
                Editor::for_file_name(name),
                "{temporary}"
            );
            assert!(nameable(&temporary, true), "{temporary}");
        }
        assert!(temporary_name("model.solidcraft.json").ends_with(".solidcraft.json"));
    }

    #[test]
    fn unchanged_changed_added_removed() {
        let same = known("same.md", "a1");
        let edited = known("edited.md", "b1");
        let gone = known("gone.md", "c1");
        let wanted = [
            file("same.md", "a1"),
            file("edited.md", "b2"),
            file("new.md", "d1"),
        ];
        let plan = plan(&wanted, &[same, edited.clone(), gone.clone()]);
        assert_eq!(plan.unchanged, 1);
        assert_eq!(plan.change, vec![(edited, file("edited.md", "b2"))]);
        assert_eq!(plan.add, vec![file("new.md", "d1")]);
        assert_eq!(plan.remove, vec![gone]);
        assert!(plan.moves.is_empty());
    }

    #[test]
    fn a_rename_with_the_same_blob_is_a_move() {
        let old = known("docs/old.md", "a1");
        let wanted = [file("guide/new.md", "a1")];
        let plan = plan(&wanted, std::slice::from_ref(&old));
        assert_eq!(plan.moves, vec![(old, file("guide/new.md", "a1"))]);
        assert!(plan.add.is_empty());
        assert!(plan.remove.is_empty());
    }

    #[test]
    fn two_files_with_one_blob_are_not_mistaken_for_a_move() {
        // One vanished, two new with its blob: which one moved cannot be told.
        let old = known("old.md", "a1");
        let wanted = [file("one.md", "a1"), file("two.md", "a1")];
        let plan = plan(&wanted, std::slice::from_ref(&old));
        assert!(plan.moves.is_empty());
        assert_eq!(paths(&plan.add), ["one.md", "two.md"]);
        assert_eq!(plan.remove, vec![old]);

        // Two vanished with one blob, one new: the same.
        let (a, b) = (known("a.md", "x1"), known("b.md", "x1"));
        let plan = super::plan(&[file("c.md", "x1")], &[a.clone(), b.clone()]);
        assert!(plan.moves.is_empty());
        assert_eq!(paths(&plan.add), ["c.md"]);
        assert_eq!(plan.remove, vec![a, b]);
    }

    #[test]
    fn a_move_to_another_kind_of_file_is_a_remove_and_an_add() {
        // The same bytes under an extension another editor opens: not a rename.
        let old = known("report.md", "a1");
        let plan = plan(&[file("report.docx", "a1")], std::slice::from_ref(&old));
        assert!(plan.moves.is_empty());
        assert_eq!(plan.remove, vec![old]);
        assert_eq!(paths(&plan.add), ["report.docx"]);
    }

    #[test]
    fn folders_are_matched_by_path() {
        let folder = SourceItem {
            node: NodeId(Uuid::new_v4()),
            path: "docs".to_owned(),
            repository_path: "docs".to_owned(),
            blob_sha: None,
            size: 0,
        };
        let wanted = [Entry {
            path: "docs".to_owned(),
            repository_path: "docs".to_owned(),
            sha: String::new(),
            size: 0,
            dir: true,
        }];
        let plan = plan(&wanted, std::slice::from_ref(&folder));
        assert!(plan.add.is_empty() && plan.remove.is_empty() && plan.change.is_empty());
        let gone = super::plan(&[], std::slice::from_ref(&folder));
        assert_eq!(gone.remove, vec![folder]);
    }

    #[test]
    fn case_collisions_number_every_file_and_leave_other_siblings_unchanged() {
        let (shown, hidden) = shown_names(
            vec![
                tree("a.md", EntryKind::File, "s2"),
                tree("A.md", EntryKind::File, "s1"),
                tree("Docs/x.md", EntryKind::File, "s3"),
                tree("docs/y.md", EntryKind::File, "s4"),
            ],
            "",
        );
        assert_eq!(
            paths(&shown),
            [
                "A (1).md",
                "Docs",
                "Docs/x.md",
                "a (2).md",
                "docs (2)",
                "docs (2)/y.md",
            ]
        );
        let lower = shown.iter().find(|e| e.path == "a (2).md").unwrap();
        assert_eq!(lower.sha, "s2");
        assert_eq!(hidden.renamed, 3);
        assert_eq!(hidden.unnameable, 0);
    }

    #[test]
    fn three_way_case_collision_is_numbered_by_original_path_byte_order() {
        let (shown, hidden) = shown_names(
            vec![
                tree("readme.md", EntryKind::File, "s3"),
                tree("README.md", EntryKind::File, "s1"),
                tree("Readme.md", EntryKind::File, "s2"),
                tree("notes.md", EntryKind::File, "s4"),
            ],
            "",
        );
        assert_eq!(
            paths(&shown),
            [
                "README (1).md",
                "Readme (2).md",
                "notes.md",
                "readme (3).md",
            ]
        );
        assert_eq!(hidden.renamed, 3);
    }

    #[test]
    fn case_collision_in_a_subfolder_numbers_only_that_folders_group() {
        let (shown, hidden) = shown_names(
            vec![
                tree("docs/Readme.md", EntryKind::File, "s1"),
                tree("docs/readme.md", EntryKind::File, "s2"),
                tree("Readme.md", EntryKind::File, "s3"),
            ],
            "",
        );
        assert_eq!(
            paths(&shown),
            [
                "Readme.md",
                "docs",
                "docs/Readme (1).md",
                "docs/readme (2).md"
            ]
        );
        assert_eq!(hidden.renamed, 2);
    }

    #[test]
    fn in_a_bucket_a_file_beside_a_folder_of_its_exact_name_is_numbered_and_the_folder_keeps_it() {
        let (shown, hidden) = shown_names_with_allocations(
            vec![
                tree("a", EntryKind::File, "s1"),
                tree("a/b.md", EntryKind::File, "s2"),
            ],
            "",
            &[],
            true,
        );
        assert_eq!(paths(&shown), ["a", "a (1)", "a/b.md"]);
        let folder = shown.iter().find(|e| e.path == "a").unwrap();
        assert!(folder.dir);
        assert_eq!(folder.repository_path, "a/");
        let file = shown.iter().find(|e| e.path == "a (1)").unwrap();
        assert!(!file.dir);
        assert_eq!(file.repository_path, "a");
        assert_eq!(hidden.renamed, 1);
    }

    #[test]
    fn a_file_beside_a_folder_differing_in_case_keeps_the_old_naming() {
        for bucket in [false, true] {
            let (shown, hidden) = shown_names_with_allocations(
                vec![
                    tree("README", EntryKind::File, "s1"),
                    tree("readme/guide.md", EntryKind::File, "s2"),
                ],
                "",
                &[],
                bucket,
            );
            assert_eq!(
                paths(&shown),
                ["README", "readme (2)", "readme (2)/guide.md"],
                "bucket {bucket}"
            );
            assert_eq!(hidden.renamed, 1);
            let folder = shown.iter().find(|e| e.dir).unwrap();
            let expected = if bucket { "readme/" } else { "readme" };
            assert_eq!(folder.repository_path, expected);
        }
    }

    #[test]
    fn a_bucket_shows_a_folder_listed_alone_and_other_sources_do_not() {
        let entries = || {
            vec![
                tree("empty", EntryKind::Dir, ""),
                tree("empty/inner", EntryKind::Dir, ""),
                tree("bad./x", EntryKind::Dir, ""),
                tree("kept.md", EntryKind::File, "s1"),
            ]
        };
        let (shown, _) = shown_names_with_allocations(entries(), "", &[], true);
        assert_eq!(paths(&shown), ["empty", "empty/inner", "kept.md"]);
        assert!(shown.iter().filter(|e| e.path != "kept.md").all(|e| e.dir));
        let (shown, _) = shown_names(entries(), "");
        assert_eq!(paths(&shown), ["kept.md"]);
    }

    #[test]
    fn a_file_already_shown_under_its_name_keeps_it_when_a_folder_of_that_name_appears() {
        let recorded = [SourceItem {
            node: NodeId(Uuid::new_v4()),
            path: "a".to_owned(),
            repository_path: "a".to_owned(),
            blob_sha: Some("s1".to_owned()),
            size: 1,
        }];
        let (shown, _) = shown_names_with_allocations(
            vec![
                tree("a", EntryKind::File, "s1"),
                tree("a/b.md", EntryKind::File, "s2"),
            ],
            "",
            &recorded,
            true,
        );
        assert_eq!(paths(&shown), ["a", "a (2)", "a (2)/b.md"]);
        let file = shown.iter().find(|e| e.path == "a").unwrap();
        assert!(!file.dir);
    }

    #[test]
    fn literal_numbered_name_stays_plain_and_collision_group_remains_unique() {
        let (shown, hidden) = shown_names(
            vec![
                tree("Readme.md", EntryKind::File, "s1"),
                tree("readme.md", EntryKind::File, "s2"),
                tree("Readme (1).md", EntryKind::File, "s3"),
            ],
            "",
        );
        assert_eq!(
            paths(&shown),
            ["Readme (1).md", "Readme (2).md", "readme (3).md"]
        );
        assert_eq!(hidden.renamed, 2);
    }

    #[test]
    fn a_file_named_a_dot_dot_is_unnameable() {
        let (shown, hidden) = shown_names(
            vec![
                tree("..", EntryKind::File, "s1"),
                tree("docs/../x.md", EntryKind::File, "s2"),
                tree("./y.md", EntryKind::File, "s3"),
                tree("ends-in-a-dot.", EntryKind::File, "s4"),
                tree("ok.md", EntryKind::File, "s5"),
            ],
            "",
        );
        assert_eq!(paths(&shown), ["ok.md"]);
        assert_eq!(hidden.unnameable, 4);
    }

    #[test]
    fn symlinks_and_submodules_are_hidden_and_counted() {
        let (shown, hidden) = shown_names(
            vec![
                tree("docs", EntryKind::Dir, "t1"),
                tree("docs/link.md", EntryKind::Symlink, "s1"),
                tree("vendor", EntryKind::Submodule, "c1"),
                tree("docs/real.md", EntryKind::File, "s2"),
            ],
            "",
        );
        assert_eq!(paths(&shown), ["docs", "docs/real.md"]);
        assert_eq!(hidden.symlinks, 1);
        assert_eq!(hidden.submodules, 1);
    }

    #[test]
    fn folder_prefix_is_stripped_and_outside_files_are_ignored() {
        let (shown, hidden) = shown_names(
            vec![
                tree("README.md", EntryKind::File, "s1"),
                tree("docs", EntryKind::Dir, "t1"),
                tree("docs/guide.md", EntryKind::File, "s2"),
                tree("docs/api/index.md", EntryKind::File, "s3"),
                tree("docsite/other.md", EntryKind::File, "s4"),
                tree("docs/only-a-link", EntryKind::Symlink, "s5"),
                tree("elsewhere/link", EntryKind::Symlink, "s6"),
            ],
            "/docs/",
        );
        assert_eq!(paths(&shown), ["api", "api/index.md", "guide.md"]);
        assert!(shown.iter().find(|e| e.path == "api").unwrap().dir);
        assert_eq!(file_paths(&shown), ["api/index.md", "guide.md"]);
        // What is outside the folder is not counted either.
        assert_eq!(hidden.symlinks, 1);
    }

    #[test]
    fn empty_names_and_names_over_255_are_unnameable() {
        let long = "x".repeat(300);
        let (shown, hidden) = shown_names(
            vec![
                tree("docs//a.md", EntryKind::File, "s1"),
                tree(&format!("{long}.md"), EntryKind::File, "s2"),
                tree(&format!("{long}/a.md"), EntryKind::File, "s3"),
                tree("tab\there.md", EntryKind::File, "s4"),
                tree("fine.md", EntryKind::File, "s5"),
            ],
            "",
        );
        assert_eq!(paths(&shown), ["fine.md"]);
        assert_eq!(hidden.unnameable, 4);
    }
}
