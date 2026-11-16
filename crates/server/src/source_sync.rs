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
//! No token, response body or other secret reaches an event, a log line, `last_error` or an
//! error: every message here is a plain sentence of ours.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::sync::Arc;

use collab_core::ids::{DocId, NodeId};
use futures_util::{StreamExt, TryStreamExt};
use records::{
    DEFAULT_SOURCE_MAX_BYTES, DEFAULT_SOURCE_MAX_FILES, Editor, NewEvent, NodeKind,
    SOURCE_MAX_BYTES, SOURCE_MAX_FILES, SourceConfig, SourceItem, SourceState, SourceStatus, User,
    check_name, check_size, is_format_char, validate_document_name, validate_node_name,
};
use revisions::RevisionId;
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::AppState;
use crate::api::content::{Created, create_uploaded};
use crate::api::nodes::{after_rename, delete_one, move_one};
use crate::api::uploads::{store_version, uploads_path};
use crate::blobs::Held;
use crate::file_room::SaveReply;
use crate::github::{
    EntryKind, GithubError, Head, RepoRef, TarLimits, TreeEntry, for_each_in_tarball, git_blob_id,
};
use crate::secrets::{Sealed, Secret};
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

/// One item of the repository as the library shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
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
    let known_at: HashMap<&str, &SourceItem> = known
        .iter()
        .map(|item| (item.path.as_str(), item))
        .collect();
    let wanted_paths: HashSet<&str> = wanted.iter().map(|e| e.path.as_str()).collect();
    let mut plan = Plan::default();
    let mut new = Vec::new();
    for entry in wanted {
        match known_at.get(entry.path.as_str()) {
            None => new.push(entry),
            Some(item) => match (&item.blob_sha, entry.dir) {
                (None, true) => {}
                (Some(sha), false) if *sha == entry.sha => plan.unchanged += 1,
                (Some(_), false) => plan.change.push(((*item).clone(), entry.clone())),
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
        .filter(|item| !wanted_paths.contains(item.path.as_str()))
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
/// unnameable. Siblings whose names are equal ignoring case keep the first in byte order; the
/// others are shown as `name (2).ext`, `name (3).ext` … and counted as renamed. Folders are
/// shown only when they hold a shown file. Policy and size are judged by the caller.
///
/// The result is in path order, each folder before what is in it.
#[must_use]
pub fn shown_names(entries: Vec<TreeEntry>, folder: &str) -> (Vec<Entry>, Hidden) {
    let folder = folder.trim_matches('/');
    let mut hidden = Hidden::default();
    let mut files: Vec<(String, String, u64)> = Vec::new();
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
            EntryKind::Dir => {}
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

    // Shown path of each folder, by its path in the repository; lower-cased names taken in
    // each shown folder.
    let mut folders: HashMap<String, String> = HashMap::new();
    let mut taken: HashMap<String, HashSet<String>> = HashMap::new();
    let mut shown = Vec::new();
    for (path, sha, size) in files {
        let mut parts: Vec<&str> = path.split('/').collect();
        let Some(name) = parts.pop() else {
            continue;
        };
        let mut original = String::new();
        let mut parent = String::new();
        for part in parts {
            original = join(&original, part);
            if let Some(known) = folders.get(&original) {
                parent = known.clone();
                continue;
            }
            let names = taken.entry(parent.clone()).or_default();
            let given = unique_node_name(part, None, names);
            if given != part {
                hidden.renamed += 1;
            }
            names.insert(given.to_lowercase());
            let at = join(&parent, &given);
            folders.insert(original.clone(), at.clone());
            shown.push(Entry {
                path: at.clone(),
                sha: String::new(),
                size: 0,
                dir: true,
            });
            parent = at;
        }
        let names = taken.entry(parent.clone()).or_default();
        let given = unique_node_name(name, Some(Editor::for_file_name(name)), names);
        if given != name {
            hidden.renamed += 1;
        }
        names.insert(given.to_lowercase());
        shown.push(Entry {
            path: join(&parent, &given),
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

/// Puts every connection left `syncing` and last checked before `before` back to
/// `connecting`, so it is due again; how many. The server's start calls it with the time it
/// starts (no sync runs before), [`sync_once`] with [`STALE_SYNC`] ago. A failure is logged.
pub async fn recover_stale_syncs(state: &AppState, before: OffsetDateTime) -> usize {
    match state.db().reset_stale_syncs(before).await {
        Ok(n) => usize::try_from(n).unwrap_or(usize::MAX),
        Err(e) => {
            eprintln!("sharecraft-server: cannot recover cut-short syncs: {e}");
            0
        }
    }
}

/// Syncs `library` with its branch as of `now`: `Ok(None)` when the branch is unchanged (or
/// another sync of it is running), otherwise what changed. The connection's status says how it
/// went; `source.synced` (counts and the short commit id) or `source.failed` (the reason) is
/// recorded. A connection left `syncing` by a sync cut short over [`STALE_SYNC`] ago is made
/// due again first.
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
    let Some((config, status)) = db.source_of(library).await.map_err(db_failed)? else {
        return Err(SyncError::NotConnected(
            "This library is not connected to a repository.".to_owned(),
        ));
    };
    // Holding the lock, nothing else is syncing it: a `syncing` state here was cut short.
    let syncing = SourceStatus {
        state: SourceState::Syncing,
        last_checked_at: Some(now),
        ..status.clone()
    };
    db.set_source_status(library, &syncing)
        .await
        .map_err(db_failed)?;
    let outcome = run(state, &config, &status).await;
    finish(state, &config, status, now, outcome).await
}

/// What a sync that did not fail found.
enum Outcome {
    /// Nothing to do; the ETag to keep.
    Unchanged { etag: Option<String> },
    /// The library now mirrors `report.commit`.
    Synced {
        report: SyncReport,
        etag: Option<String>,
        files: usize,
    },
}

/// Records how the sync of `config` went, as of `now`, in its status and the event log.
async fn finish(
    state: &AppState,
    config: &SourceConfig,
    before: SourceStatus,
    now: OffsetDateTime,
    outcome: Result<Outcome, Failure>,
) -> Result<Option<SyncReport>, SyncError> {
    let library = config.library;
    let mut status = SourceStatus {
        last_checked_at: Some(now),
        ..before
    };
    let answer = match outcome {
        Ok(Outcome::Unchanged { etag }) => {
            status.state = SourceState::Ok;
            status.etag = etag;
            status.last_error = None;
            status.retry_at = None;
            Ok(None)
        }
        Ok(Outcome::Synced {
            report,
            etag,
            files,
        }) => {
            status.state = SourceState::Ok;
            status.etag = etag;
            status.last_commit = Some(report.commit.clone());
            status.last_synced_at = Some(now);
            status.last_error = None;
            status.retry_at = None;
            status.files = i64::try_from(files).unwrap_or(i64::MAX);
            status.bytes = i64::try_from(report.bytes).unwrap_or(i64::MAX);
            // Files not shown at all. Files shown under a suffixed name (`renamed`) are shown,
            // so they are not counted here; the `source.synced` event carries their number.
            status.hidden = i64::try_from(report.hidden.total()).unwrap_or(i64::MAX);
            state.events().emit(NewEvent {
                kind: "source.synced",
                actor: None,
                subject: None,
                document: None,
                node: Some(library),
                detail: json!({
                    "commit": short(&report.commit),
                    "added": report.added,
                    "changed": report.changed,
                    "removed": report.removed,
                    "moved": report.moved,
                    "hidden": report.hidden.total(),
                    "renamed": report.hidden.renamed,
                    "bytes": report.bytes,
                }),
            });
            Ok(Some(report))
        }
        Err(Failure { error: e, applying }) => {
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
            state.events().emit(NewEvent {
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
    if let Err(e) = state.db().set_source_status(library, &status).await {
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

/// The `etag` recorded after a sync failed while applying: the library may be half way between
/// two commits, so the next sync reads the branch in full (no conditional request) and applies
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

/// The sync itself: reads, checks, then applies.
async fn run(
    state: &AppState,
    config: &SourceConfig,
    status: &SourceStatus,
) -> Result<Outcome, Failure> {
    let db = state.db();
    let library = config.library;
    let repo = RepoRef::parse(&format!("{}/{}", config.owner, config.repo)).map_err(|_| {
        SyncError::NotConnected(
            "The repository's name is not one GitHub uses; connect the library again.".to_owned(),
        )
    })?;
    let token = token_of(state, config).await?;
    let token = token.as_ref();
    let github = state.github();
    let dirty = status.etag.as_deref() == Some(DIRTY_ETAG);
    let sent_etag = if dirty { None } else { status.etag.as_deref() };
    let head = github
        .head(&repo, &config.branch, token, sent_etag)
        .await
        .map_err(github_error)?;
    let (commit, tree_sha, etag, author) = match head {
        Head::NotModified => {
            return Ok(Outcome::Unchanged {
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
    if !dirty && status.last_commit.as_deref() == Some(commit.as_str()) {
        return Ok(Outcome::Unchanged {
            etag: etag.or_else(|| status.etag.clone()),
        });
    }
    let user = connector(state, config, &commit, author.as_deref()).await?;
    let tree = github
        .tree(&repo, &tree_sha, token)
        .await
        .map_err(github_error)?;
    let whole = Whole::of(&tree).ok_or_else(too_big_to_add)?;

    // The library's file policies and the largest file the server takes, judged before names
    // are given, so a hidden file never pushes a shown sibling to a suffix.
    let policies = tree::policies_at(db, library).await.map_err(|e| {
        eprintln!(
            "sharecraft-server: cannot read the policies of library {}: {e}",
            library.0
        );
        failed("The library's file policies could not be read, so nothing was changed.")
    })?;
    let largest = state.config().max_file_bytes;
    let mut refused = Hidden::default();
    let folder = config.folder.trim_matches('/');
    let tree: Vec<TreeEntry> = tree
        .into_iter()
        .filter(|entry| {
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
    let (shown, mut hidden) = shown_names(tree, folder);
    hidden.policy = refused.policy;
    hidden.too_big = refused.too_big;

    let files = shown.iter().filter(|e| !e.dir).count();
    let bytes =
        total_size(shown.iter().filter(|e| !e.dir).map(|e| e.size)).ok_or_else(too_big_to_add)?;
    let (max_files, max_bytes) = caps(state).await?;
    if files > max_files {
        return Err(failed(&format!(
            "The repository has {files} files to show, more than the {max_files} this server \
             mirrors for one library, so nothing was changed."
        ))
        .into());
    }
    if bytes > max_bytes {
        return Err(failed(&format!(
            "The repository's files hold {bytes} bytes, more than the {max_bytes} this server \
             mirrors for one library, so nothing was changed."
        ))
        .into());
    }

    // Every recorded item whose node is gone (deleted outside the sync) is planned as if it
    // were not there, so it is made again. Nothing is written yet: the rows of the lost items
    // are dropped only when applying starts.
    let recorded = db.source_items(library).await.map_err(db_failed)?;
    let live: HashSet<NodeId> = db
        .live_source_nodes(library)
        .await
        .map_err(db_failed)?
        .into_iter()
        .collect();
    let (known, lost): (Vec<SourceItem>, Vec<SourceItem>) = recorded
        .into_iter()
        .partition(|item| live.contains(&item.node));
    let lost: Vec<NodeId> = lost.into_iter().map(|item| item.node).collect();
    let plan = plan(&shown, &known);

    let mut needed: HashMap<String, u64> = HashMap::new();
    for entry in plan
        .add
        .iter()
        .chain(plan.change.iter().map(|(_, e)| e))
        .filter(|e| !e.dir)
    {
        needed.insert(entry.sha.clone(), entry.size);
    }
    let fetched = fetch(
        state,
        &Reading {
            repo: &repo,
            token,
            commit: &commit,
            folder: config.folder.trim_matches('/'),
            whole,
            max_bytes,
        },
        &needed,
    )
    .await?;

    let counts = apply(state, library, &user, &known, &lost, plan, &fetched)
        .await
        .map_err(|error| Failure {
            error,
            applying: true,
        })?;
    drop(fetched);
    Ok(Outcome::Synced {
        report: SyncReport {
            commit,
            added: counts.added,
            changed: counts.changed,
            removed: counts.removed,
            moved: counts.moved,
            hidden,
            bytes,
        },
        etag,
        files,
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
async fn token_of(state: &AppState, config: &SourceConfig) -> Result<Option<Secret>, SyncError> {
    let Some(id) = config.token else {
        return Ok(None);
    };
    let gone = || {
        SyncError::NotConnected(
            "The stored token could not be opened; set the token again.".to_owned(),
        )
    };
    let row = state
        .db()
        .get_secret(id)
        .await
        .map_err(db_failed)?
        .ok_or_else(gone)?;
    let sealed = Sealed {
        key_id: row.key_id,
        nonce: row.nonce,
        ciphertext: row.ciphertext,
    };
    let plain = state
        .secrets()
        .unseal(&sealed, &token_context(config.library))
        .map_err(|_| gone())?;
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
async fn caps(state: &AppState) -> Result<(usize, u64), SyncError> {
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
    Ok((
        usize::try_from(files).unwrap_or(usize::MAX),
        u64::try_from(bytes).unwrap_or(u64::MAX),
    ))
}

/// The whole tree's size: its entries, and its files' bytes and the largest.
#[derive(Clone, Copy, Debug)]
struct Whole {
    entries: usize,
    bytes: u64,
    largest: u64,
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
    max_bytes: u64,
}

/// Why reading the tarball did not work.
enum TarFailure {
    /// It is larger than the limits allow: the files are read one by one instead.
    TooLarge,
    Failed(SyncError),
}

/// Reads every blob of `needed` (blob id to size), checks each against its id and stores it,
/// held: from one tarball when more than [`TARBALL_FROM`] are needed and the whole tree is
/// within the byte cap, otherwise (or when the tarball is larger than allowed) one by one.
async fn fetch(
    state: &AppState,
    reading: &Reading<'_>,
    needed: &HashMap<String, u64>,
) -> Result<HashMap<String, Held>, SyncError> {
    if needed.is_empty() {
        return Ok(HashMap::new());
    }
    if needed.len() > TARBALL_FROM && reading.whole.bytes <= reading.max_bytes {
        match by_tarball(state, reading, needed).await {
            Ok(fetched) => return Ok(fetched),
            Err(TarFailure::Failed(e)) => return Err(e),
            Err(TarFailure::TooLarge) => {
                println!("sharecraft-server: a tarball was over its limits; reading files singly");
            }
        }
    }
    by_blobs(state, reading, needed).await
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
async fn apply(
    state: &AppState,
    library: NodeId,
    user: &User,
    known: &[SourceItem],
    lost: &[NodeId],
    plan: Plan,
    fetched: &HashMap<String, Held>,
) -> Result<Counts, SyncError> {
    if !lost.is_empty() {
        state
            .db()
            .remove_source_items(library, lost)
            .await
            .map_err(db_failed)?;
    }
    let mut work = Applying {
        state,
        library,
        user,
        folders: known
            .iter()
            .filter(|item| item.blob_sha.is_none())
            .map(|item| (item.path.clone(), item.node))
            .collect(),
        tracked: known.iter().map(|item| item.node).collect(),
        fetched,
    };
    let mut counts = Counts::default();
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
    }
    for entry in plan.add.iter().filter(|e| e.dir) {
        work.add_folder(entry, &mut gone_folders).await?;
    }
    let mut waiting = Vec::new();
    for (item, entry) in &plan.moves {
        if let Some(later) = work.move_file(item, entry).await? {
            waiting.push(later);
        }
        counts.moved += 1;
    }
    let mut gone_folders: Vec<SourceItem> = gone_folders.into_values().collect();
    gone_folders.sort_by(|a, b| b.path.cmp(&a.path));
    for item in gone_folders {
        work.to_bin(&item).await?;
    }
    for (node, entry) in waiting {
        work.final_name(node, entry).await?;
    }
    for (item, entry) in &plan.change {
        work.new_version(item.node, entry).await?;
        counts.changed += 1;
    }
    for entry in plan.add.iter().filter(|e| !e.dir) {
        work.add_file(entry).await?;
        counts.added += 1;
    }
    Ok(counts)
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

impl Applying<'_> {
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
        db.remove_source_items(self.library, &[node])
            .await
            .map_err(db_failed)?;
        let item = SourceItem {
            node,
            path: entry.path.clone(),
            blob_sha: (!entry.dir).then(|| entry.sha.clone()),
            size: i64::try_from(entry.size).unwrap_or(i64::MAX),
        };
        db.upsert_source_items(self.library, std::slice::from_ref(&item))
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
                return self.new_version(node.id, entry).await;
            }
            Some(_) => return Err(taken(&entry.path)),
            None => {}
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
            sha: sha.to_owned(),
            size: 1,
            dir: false,
        }
    }

    fn known(path: &str, sha: &str) -> SourceItem {
        SourceItem {
            node: NodeId(Uuid::new_v4()),
            path: path.to_owned(),
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
            blob_sha: None,
            size: 0,
        };
        let wanted = [Entry {
            path: "docs".to_owned(),
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
    fn case_collisions_keep_the_first_and_suffix_the_rest() {
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
                "A.md",
                "Docs",
                "Docs/x.md",
                "a (2).md",
                "docs (2)",
                "docs (2)/y.md"
            ]
        );
        let lower = shown.iter().find(|e| e.path == "a (2).md").unwrap();
        assert_eq!(lower.sha, "s2");
        assert_eq!(hidden.renamed, 2);
        assert_eq!(hidden.unnameable, 0);
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
