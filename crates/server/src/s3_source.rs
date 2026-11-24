//! Listing a bucket, and the S3 source: an S3 library's keys become folders and files through
//! the source engine's external mode.
//!
//! A listing is all or nothing. It pages `ListObjectsV2` under a file count, a time and a
//! request budget, and any failure part way (a page that fails after the client's retries, a
//! continuation token that repeats, a malformed or truncated page, a limit passed) answers no
//! listing at all, so the engine never plans from part of a bucket.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::time::{Duration, Instant};

use collab_core::ids::NodeId;
use records::{SourceConfig, SourceStatus, object_tag, reserved_name, windows_safe_name};
use time::OffsetDateTime;

use crate::AppState;
use crate::blobs::Held;
use crate::s3::{S3, S3Error, S3Target};
use crate::s3_xml::ObjectEntry;
use crate::source::{ContentMode, EntryKind, Listing, Look, SourceCaps, StorageSource, TreeEntry};
use crate::source_sync::{Hidden, SyncError};

/// The most keys asked for in one listing page.
pub const PAGE: u16 = 1000;
/// The longest relative path shown, in characters.
pub(crate) const MAX_RELATIVE_CHARS: usize = 400;
/// The longest name shown, in bytes.
const MAX_SEGMENT_BYTES: usize = 255;

/// Limits applied to one complete bucket listing.
#[derive(Clone, Copy, Debug)]
pub struct BucketLimits {
    pub max_files: usize,
    pub max_time: Duration,
    /// List requests this listing may send.
    pub max_requests: u64,
    /// Keys asked for per page (1000).
    pub page: u16,
}

/// A complete listing of the keys below a prefix.
#[derive(Debug)]
pub struct BucketListing {
    pub entries: Vec<TreeEntry>,
    pub hidden: Hidden,
    /// List pages asked for.
    pub requests: u64,
    pub took: Duration,
}

/// Why a complete listing could not be produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BucketError {
    Unreachable,
    Denied,
    BadKeys,
    NoBucket,
    WrongRegion,
    ClockSkew,
    NotApproved,
    /// More objects than allowed; how many were seen when the listing stopped.
    TooManyFiles(usize),
    TooSlow,
    OverBudget,
    Incomplete,
}

fn bucket_error(error: S3Error) -> BucketError {
    match error {
        S3Error::NoBucket | S3Error::NotFound => BucketError::NoBucket,
        S3Error::Denied => BucketError::Denied,
        S3Error::BadKeys => BucketError::BadKeys,
        S3Error::WrongRegion => BucketError::WrongRegion,
        S3Error::ClockSkew => BucketError::ClockSkew,
        S3Error::AddressRefused => BucketError::NotApproved,
        S3Error::Unreachable | S3Error::TimedOut | S3Error::Throttled => BucketError::Unreachable,
        S3Error::Malformed
        | S3Error::Precondition
        | S3Error::Archived
        | S3Error::Unsupported
        | S3Error::Other => BucketError::Incomplete,
    }
}

/// Lists every key below `prefix`, page by page, and turns the keys into entries (see
/// [`entries_of`]). Requests are counted against `library` when one is given, and the files
/// seen so far are published as its listing progress.
///
/// # Errors
/// After every page: more than `max_files` objects (`TooManyFiles`), longer than `max_time`
/// (`TooSlow`), or another page needed with `max_requests` already sent (`OverBudget`). A
/// continuation token that repeats, or a page that cannot be read, is `Incomplete`; a page
/// refused by the store after the client's retries is its mapped error. Any failure answers
/// no listing at all.
pub async fn list_bucket(
    s3: &S3,
    t: &S3Target,
    library: Option<NodeId>,
    prefix: &str,
    limits: &BucketLimits,
) -> Result<BucketListing, BucketError> {
    let started = Instant::now();
    let target = match library {
        Some(library) => t.clone().with_library(library),
        None => t.clone(),
    };
    let progress = library.map(|library| s3.begin_listing(library));
    // Keyed by key: a key a store repeats across pages is kept once, as last seen. Bounded by
    // `max_files` plus one page.
    let mut objects: BTreeMap<String, ObjectEntry> = BTreeMap::new();
    let mut tokens: HashSet<String> = HashSet::new();
    let mut token: Option<String> = None;
    let mut requests: u64 = 0;
    loop {
        if requests >= limits.max_requests {
            return Err(BucketError::OverBudget);
        }
        let remaining = limits.max_time.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err(BucketError::TooSlow);
        }
        let page = tokio::time::timeout(
            remaining,
            s3.list_page(&target, prefix, token.as_deref(), limits.page.max(1)),
        )
        .await
        .map_err(|_| BucketError::TooSlow)?;
        requests = requests.saturating_add(1);
        let page = page.map_err(bucket_error)?;
        for object in page.objects {
            objects.insert(object.key.clone(), object);
        }
        if let Some(progress) = &progress {
            progress.set(objects.len());
        }
        if objects.len() > limits.max_files {
            return Err(BucketError::TooManyFiles(objects.len()));
        }
        if started.elapsed() > limits.max_time {
            return Err(BucketError::TooSlow);
        }
        match page.next {
            None => break,
            Some(next) => {
                if !tokens.insert(next.clone()) {
                    return Err(BucketError::Incomplete);
                }
                token = Some(next);
            }
        }
    }
    drop(progress);
    let objects: Vec<ObjectEntry> = objects.into_values().collect();
    let (entries, hidden) = entries_of(&objects, prefix, u64::MAX, u64::MAX);
    Ok(BucketListing {
        entries,
        hidden,
        requests,
        took: started.elapsed(),
    })
}

/// Whether one `/`-separated segment of a key can be a name in a library.
pub(crate) fn nameable_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment != "."
        && segment != ".."
        && segment.len() <= MAX_SEGMENT_BYTES
        && !segment.chars().any(char::is_control)
        && windows_safe_name(segment)
}

/// Whether a storage class keeps an object offline until it is restored.
fn archived_class(class: Option<&str>) -> bool {
    matches!(class, Some("GLACIER" | "DEEP_ARCHIVE"))
}

/// The entries the keys of `objects` below `prefix` make, and what they hide. Pure.
///
/// The prefix is removed from each key and the rest split on `/`. A key outside the prefix or
/// equal to it, a key with an empty, `.`, `..`, control-character, over-long or Windows-refused
/// segment, and a `/`-ended key with content are counted as unnameable; a path over 400
/// characters is counted as too long; a key with a segment ShareCraft keeps for itself is
/// skipped without a count. A zero-byte `/`-ended key makes its folder. Every folder on the way
/// to what is shown is an entry once, before what is in it; the result is in path order. A
/// key seen twice is one entry, the later one. Archived objects are listed, with an archived
/// tag, and counted in `hidden.archived`. Sizes are left to the engine's own rule, so
/// `cache_max` and `max_file` are not applied here.
#[must_use]
pub fn entries_of(
    objects: &[ObjectEntry],
    prefix: &str,
    cache_max: u64,
    max_file: u64,
) -> (Vec<TreeEntry>, Hidden) {
    // The engine judges size against these when it plans; they are part of the interface so a
    // caller states them, but a size never hides an entry here.
    let _ = (cache_max, max_file);
    let mut latest: BTreeMap<&str, &ObjectEntry> = BTreeMap::new();
    for object in objects {
        latest.insert(object.key.as_str(), object);
    }
    let mut hidden = Hidden::default();
    let mut folders: BTreeSet<String> = BTreeSet::new();
    let mut files: Vec<TreeEntry> = Vec::new();
    for (key, object) in latest {
        let Some(relative) = key.strip_prefix(prefix).filter(|rest| !rest.is_empty()) else {
            hidden.unnameable = hidden.unnameable.saturating_add(1);
            continue;
        };
        let (path, marker) = match relative.strip_suffix('/') {
            Some(_) if object.size != 0 => {
                hidden.unnameable = hidden.unnameable.saturating_add(1);
                continue;
            }
            Some(folder) => (folder, true),
            None => (relative, false),
        };
        if path.split('/').any(reserved_name) {
            continue;
        }
        if !path.split('/').all(nameable_segment) {
            hidden.unnameable = hidden.unnameable.saturating_add(1);
            continue;
        }
        if path.chars().count() > MAX_RELATIVE_CHARS {
            hidden.too_long = hidden.too_long.saturating_add(1);
            continue;
        }
        for (at, _) in path.match_indices('/') {
            if let Some(folder) = path.get(..at) {
                folders.insert(folder.to_owned());
            }
        }
        if marker {
            folders.insert(path.to_owned());
            continue;
        }
        let archived = archived_class(object.class.as_deref());
        if archived {
            hidden.archived = hidden.archived.saturating_add(1);
        }
        files.push(TreeEntry {
            path: path.to_owned(),
            kind: EntryKind::File,
            sha: object_tag(object.size, object.modified_s, &object.etag, archived),
            size: object.size,
        });
    }
    let mut entries: Vec<TreeEntry> = folders
        .into_iter()
        .map(|path| TreeEntry {
            path,
            kind: EntryKind::Dir,
            sha: String::new(),
            size: 0,
        })
        .chain(files)
        .collect();
    entries.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| (left.kind != EntryKind::Dir).cmp(&(right.kind != EntryKind::Dir)))
    });
    (entries, hidden)
}

/// A library connected to a bucket as a storage source.
pub(crate) struct S3Source;

impl StorageSource for S3Source {
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
        let db = state.db();
        let library = config.library;
        let settings = db.share_settings().await.map_err(database_failed)?;
        db.s3_place(library)
            .await
            .map_err(database_failed)?
            .ok_or_else(not_a_bucket)?;
        let budget = db.budget(library).await.map_err(database_failed)?;
        let used = db
            .usage(library, OffsetDateTime::now_utc().date())
            .await
            .map_err(database_failed)?;
        let unflushed = state.s3().unflushed_usage(library);
        let max_requests = budget
            .list_requests
            .saturating_sub(used.list.saturating_add(unflushed.list));
        let target = state
            .s3()
            .target(state, library)
            .await
            .map_err(target_failed)?;
        let max_time = state
            .s3()
            .list_time(Duration::from_secs(settings.list_seconds));
        let limits = BucketLimits {
            max_files: settings.max_files,
            max_time,
            max_requests,
            page: PAGE,
        };
        let listed = list_bucket(state.s3(), &target, Some(library), target.prefix(), &limits)
            .await
            .map_err(|error| listing_failed(error, &limits))?;
        Ok(Look::Listing(Box::new(Listing {
            entries: listed.entries,
            protected: HashSet::new(),
            bookmark: String::new(),
            etag: None,
            credit: None,
            hidden: listed.hidden,
            took: listed.took,
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

fn database_failed(error: impl std::fmt::Display) -> SyncError {
    eprintln!("sharecraft-server: database failure: {error}");
    SyncError::Failed("The database could not be read, so nothing was changed.".to_owned())
}

fn not_a_bucket() -> SyncError {
    SyncError::NotConnected("This library is not connected to a bucket.".to_owned())
}

fn not_connected(message: &str) -> SyncError {
    SyncError::NotConnected(message.to_owned())
}

fn failed(message: String) -> SyncError {
    SyncError::Failed(message)
}

const NOT_APPROVED: &str = "This storage endpoint is no longer approved for ShareCraft. An administrator can approve it again.";
const BAD_KEYS: &str =
    "The storage refused the access key. A library owner can replace it under Source.";

/// The error for a target that could not be built.
fn target_failed(error: S3Error) -> SyncError {
    match error {
        S3Error::AddressRefused => not_connected(NOT_APPROVED),
        S3Error::BadKeys => not_connected(BAD_KEYS),
        _ => failed("The database could not be read, so nothing was changed.".to_owned()),
    }
}

/// The plain sentence for a listing that failed. A wrong key, bucket, region or approval is
/// not connected: retrying it would only hammer the store.
fn listing_failed(error: BucketError, limits: &BucketLimits) -> SyncError {
    match error {
        BucketError::NotApproved => not_connected(NOT_APPROVED),
        BucketError::BadKeys => not_connected(BAD_KEYS),
        BucketError::NoBucket => not_connected("The bucket was not found."),
        BucketError::Denied => not_connected("The access key may not list this bucket."),
        BucketError::WrongRegion => {
            not_connected("The bucket is in another region than the endpoint.")
        }
        BucketError::Unreachable => {
            failed("The storage could not be reached, so nothing was changed.".to_owned())
        }
        BucketError::ClockSkew => failed(
            "The server's clock differs too much from the storage's, so nothing was changed."
                .to_owned(),
        ),
        BucketError::TooManyFiles(_) => failed(format!(
            "The bucket holds more than {} files, the most this server shows for one library, so nothing was changed.",
            limits.max_files
        )),
        BucketError::TooSlow => failed(format!(
            "Listing the bucket took longer than {} seconds, so nothing was changed.",
            limits.max_time.as_secs()
        )),
        BucketError::OverBudget => failed(
            "Listing was stopped to stay inside this library's request budget, so nothing was changed."
                .to_owned(),
        ),
        BucketError::Incomplete => {
            failed("The storage's listing was incomplete, so nothing was changed.".to_owned())
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    fn object(key: &str, size: u64) -> ObjectEntry {
        ObjectEntry {
            key: key.to_owned(),
            size,
            etag: "\"e\"".to_owned(),
            modified_s: 1_700_000_000,
            class: None,
        }
    }

    fn listed(objects: &[ObjectEntry], prefix: &str) -> (Vec<(String, EntryKind)>, Hidden) {
        let (entries, hidden) = entries_of(objects, prefix, u64::MAX, u64::MAX);
        (
            entries
                .into_iter()
                .map(|entry| (entry.path, entry.kind))
                .collect(),
            hidden,
        )
    }

    fn dir(path: &str) -> (String, EntryKind) {
        (path.to_owned(), EntryKind::Dir)
    }

    fn file(path: &str) -> (String, EntryKind) {
        (path.to_owned(), EntryKind::File)
    }

    #[test]
    fn keys_become_folders_and_files_in_path_order() {
        let (entries, hidden) = listed(
            &[
                object("docs/2024/report.pdf", 5),
                object("docs/notes.txt", 3),
                object("readme.md", 1),
            ],
            "",
        );
        assert_eq!(
            entries,
            [
                dir("docs"),
                dir("docs/2024"),
                file("docs/2024/report.pdf"),
                file("docs/notes.txt"),
                file("readme.md"),
            ]
        );
        assert_eq!(hidden, Hidden::default());
        let (full, _) = entries_of(&[object("docs/notes.txt", 3)], "", 0, 0);
        assert_eq!(full[1].size, 3);
        assert_eq!(full[1].sha, object_tag(3, 1_700_000_000, "\"e\"", false));
        assert_eq!(full[0].sha, "");
    }

    #[test]
    fn a_zero_byte_marker_is_a_folder_and_a_marker_with_content_is_hidden() {
        let (entries, hidden) = listed(
            &[
                object("photos/", 0),
                object("photos/cat.jpg", 9),
                object("odd/", 4),
            ],
            "",
        );
        assert_eq!(entries, [dir("photos"), file("photos/cat.jpg")]);
        assert_eq!(hidden.unnameable, 1);
    }

    #[test]
    fn an_empty_marker_folder_is_listed() {
        let (entries, hidden) = listed(&[object("empty/inner/", 0)], "");
        assert_eq!(entries, [dir("empty"), dir("empty/inner")]);
        assert_eq!(hidden, Hidden::default());
    }

    #[test]
    fn bad_segments_hide_one_object_and_are_counted() {
        let long_segment = "x".repeat(256);
        let long_path = format!("{}/{}", "a".repeat(200), "b".repeat(200));
        assert_eq!(long_path.chars().count(), 401);
        let unnameable = [
            "a//b".to_owned(),
            "../x".to_owned(),
            "a/./b".to_owned(),
            "a/\u{7}b".to_owned(),
            "CON.txt".to_owned(),
            "a./b".to_owned(),
            format!("dir/{long_segment}"),
            "/leading".to_owned(),
        ];
        for key in &unnameable {
            let (entries, hidden) = listed(&[object(key, 1), object("fine.txt", 1)], "");
            assert_eq!(entries, [file("fine.txt")], "{key:?}");
            assert_eq!(hidden.unnameable, 1, "{key:?}");
        }
        let (entries, hidden) = listed(&[object(&long_path, 1), object("fine.txt", 1)], "");
        assert_eq!(entries, [file("fine.txt")]);
        assert_eq!(hidden.too_long, 1);
        assert_eq!(hidden.unnameable, 0);

        let all: Vec<ObjectEntry> = unnameable
            .iter()
            .map(|key| object(key, 1))
            .chain([object("a/kept.txt", 1)])
            .collect();
        let (entries, hidden) = listed(&all, "");
        assert_eq!(entries, [dir("a"), file("a/kept.txt")]);
        assert_eq!(hidden.unnameable, unnameable.len());
    }

    #[test]
    fn keys_outside_or_equal_to_the_prefix_are_dropped() {
        let (entries, hidden) = listed(
            &[
                object("lib/", 0),
                object("lib/in.txt", 1),
                object("other/out.txt", 1),
                object("library.txt", 1),
            ],
            "lib/",
        );
        assert_eq!(entries, [file("in.txt")]);
        assert_eq!(hidden.unnameable, 3);
    }

    #[test]
    fn sharecrafts_own_keys_are_skipped_and_not_counted() {
        let (entries, hidden) = listed(
            &[
                object(".sharecraft-bin/old.txt", 1),
                object("a/.sharecraft-tmp-123", 1),
                object("a/.SHARECRAFT-BIN/", 0),
                object("a/b.txt", 1),
            ],
            "",
        );
        assert_eq!(entries, [dir("a"), file("a/b.txt")]);
        assert_eq!(hidden, Hidden::default());
    }

    #[test]
    fn archived_objects_are_listed_with_an_archived_tag_and_counted() {
        let mut cold = object("cold.bin", 4);
        cold.class = Some("GLACIER".to_owned());
        let mut deep = object("deep.bin", 4);
        deep.class = Some("DEEP_ARCHIVE".to_owned());
        let mut ia = object("warm.bin", 4);
        ia.class = Some("STANDARD_IA".to_owned());
        let (entries, hidden) = entries_of(&[cold, deep, ia], "", u64::MAX, u64::MAX);
        assert_eq!(entries.len(), 3);
        assert!(entries[0].sha.ends_with("!a"));
        assert!(entries[1].sha.ends_with("!a"));
        assert!(!entries[2].sha.ends_with("!a"));
        assert_eq!(hidden.archived, 2);
        assert_eq!(hidden.total(), 0, "archived files are shown, not hidden");
    }

    #[test]
    fn a_repeated_key_is_one_entry() {
        let first = object("same.txt", 1);
        let mut later = object("same.txt", 2);
        later.etag = "\"later\"".to_owned();
        let (entries, _) = entries_of(&[first, later], "", u64::MAX, u64::MAX);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].size, 2);
        assert_eq!(
            entries[0].sha,
            object_tag(2, 1_700_000_000, "\"later\"", false)
        );
    }

    #[test]
    fn unsorted_input_gives_sorted_output() {
        let (entries, _) = listed(
            &[
                object("z.txt", 1),
                object("b/y.txt", 1),
                object("a b.txt", 1),
                object("a/x.txt", 1),
                object("a", 1),
            ],
            "",
        );
        assert_eq!(
            entries,
            [
                dir("a"),
                file("a"),
                file("a b.txt"),
                file("a/x.txt"),
                dir("b"),
                file("b/y.txt"),
                file("z.txt"),
            ]
        );
    }

    #[test]
    fn the_tag_changes_with_the_etag_alone() {
        let one = object("f.txt", 5);
        let mut two = object("f.txt", 5);
        two.etag = "\"other\"".to_owned();
        let (first, _) = entries_of(&[one], "", u64::MAX, u64::MAX);
        let (second, _) = entries_of(&[two], "", u64::MAX, u64::MAX);
        assert_ne!(first[0].sha, second[0].sha);
        assert_eq!(first[0].size, second[0].size);
    }
}
