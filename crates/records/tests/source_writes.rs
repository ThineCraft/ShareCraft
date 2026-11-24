#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use collab_core::ids::{NodeId, UserId};
use common::{document_in, new_id, put, site, user};
use records::test_support::TestDb;
use records::{
    AccessError, COALESCE, ExternalEntry, NewPendingWrite, NewSource, NewUpload, NodeChange,
    NodeKind, NodeRow, PendingState, S3Place, SealedRow, SourceKind, SourceMode, Tri, Unavailable,
    UploadTarget, Versioning, WriteState, object_tag, s3_location,
};
use revisions::BlobRef;
use serde_json::json;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

struct Fixture {
    test: TestDb,
    user: UserId,
    site: NodeId,
    library: NodeId,
}

fn t0() -> OffsetDateTime {
    OffsetDateTime::UNIX_EPOCH + Duration::days(20_000)
}

/// A site with one library connected to a bucket, read and write.
async fn fixture() -> Fixture {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", true).await;
    test.db
        .set_setting(
            ann.id,
            records::S3_ENDPOINTS,
            json!([{
                "name": "r2",
                "url": "https://r2.example.com",
                "region": "auto",
                "path_style": true,
                "allow": [],
            }]),
        )
        .await
        .unwrap();
    let site = site(&test, "Site", Some(ann.id)).await;
    let library = put(
        &test,
        new_id(),
        NodeKind::Library,
        Some(site),
        site,
        "Bucket",
    )
    .await;
    let f = Fixture {
        test,
        user: ann.id,
        site,
        library,
    };
    connect(&f, library, "write-bucket").await;
    assert!(
        f.test
            .db
            .set_source_mode(library, SourceMode::ReadWrite)
            .await
            .unwrap()
    );
    f
}

async fn connect(f: &Fixture, library: NodeId, bucket: &str) {
    let place = S3Place {
        endpoint: "r2".into(),
        bucket: bucket.into(),
        prefix: String::new(),
        access_key_id: "AKIAEXAMPLE".into(),
        versioning: Versioning::Unknown,
        conditional: Tri::Unknown,
    };
    let token = f
        .test
        .db
        .put_secret(&SealedRow {
            key_id: 1,
            nonce: [7; 12],
            ciphertext: b"sealed key".to_vec(),
        })
        .await
        .unwrap();
    f.test
        .db
        .connect_s3_source(
            &NewSource {
                library,
                kind: SourceKind::S3,
                owner: String::new(),
                repo: String::new(),
                branch: String::new(),
                folder: String::new(),
                location: Some(s3_location("r2", bucket, "")),
                mode: SourceMode::ReadOnly,
                token: Some(token),
                poll_seconds: 300,
                connected_by: f.user,
            },
            &place,
        )
        .await
        .unwrap();
}

async fn scanner_on(f: &Fixture) {
    f.test
        .db
        .set_setting(
            f.user,
            records::SCANNER_URL,
            json!("icap://scanner.example/scan"),
        )
        .await
        .unwrap();
}

/// A document node in the library with no tracking row: a file not yet in the bucket.
async fn new_file(f: &Fixture, name: &str) -> NodeId {
    document_in(&f.test, f.user, f.library, f.site, name)
        .await
        .into()
}

fn tag_of(size: u64, seconds: i64) -> String {
    object_tag(size, seconds, "\"etag\"", false)
}

/// A document node in the library the listing has recorded: size 4, one tag.
async fn tracked_file(f: &Fixture, name: &str) -> (NodeId, String) {
    let node = new_file(f, name).await;
    let tag = tag_of(4, 1_700_000_000);
    f.test
        .db
        .record_external_item(
            f.library,
            &ExternalEntry {
                node,
                path: name.into(),
                repository_path: name.into(),
                size: 4,
                mtime_ns: 1_700_000_000_000_000_000,
                tag: Some(tag.clone()),
            },
        )
        .await
        .unwrap();
    (node, tag)
}

/// Pins `bytes` as the caller of a stage does, then stages them for `node`.
async fn stage(
    f: &Fixture,
    node: NodeId,
    path: &str,
    bytes: &[u8],
    base: Option<&str>,
    flush: bool,
    now: OffsetDateTime,
) -> (BlobRef, String) {
    stage_by(f, f.user, node, path, bytes, base, flush, now).await
}

#[allow(clippy::too_many_arguments)]
async fn stage_by(
    f: &Fixture,
    by: UserId,
    node: NodeId,
    path: &str,
    bytes: &[u8],
    base: Option<&str>,
    flush: bool,
    now: OffsetDateTime,
) -> (BlobRef, String) {
    let blob = BlobRef::of(bytes);
    f.test.db.pin_cached(&blob, now).await.unwrap();
    let revision = f
        .test
        .db
        .stage_write(&NewPendingWrite {
            node,
            library: f.library,
            path,
            blob,
            base_tag: base,
            by,
            flush,
            now,
        })
        .await
        .unwrap();
    (blob, revision)
}

fn is_revision(text: &str) -> bool {
    text.len() == 40
        && text
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

async fn generation(f: &Fixture) -> i64 {
    f.test
        .db
        .source_generation(f.library)
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn staging_mints_a_revision_and_records_the_base_and_the_path() {
    let f = fixture().await;
    let node = new_file(&f, "A.txt").await;
    let (blob, revision) = stage(
        &f,
        node,
        "docs/A.txt",
        b"first",
        Some("tag-one"),
        false,
        t0(),
    )
    .await;
    assert!(is_revision(&revision), "{revision}");

    let row = f.test.db.pending_write(node).await.unwrap().unwrap();
    assert_eq!(row.node, node);
    assert_eq!(row.library, f.library);
    assert_eq!(row.path, "docs/A.txt");
    assert_eq!(row.blob, blob);
    assert_eq!(row.revision, revision);
    assert_eq!(row.base_tag.as_deref(), Some("tag-one"));
    assert_eq!(row.by, f.user);
    assert_eq!(row.state, PendingState::Scanning);
    assert_eq!((row.error, row.attempts, row.next_try_at), (None, 0, None));
    assert!(!row.flush);
    assert_eq!((row.first_staged_at, row.staged_at), (t0(), t0()));

    let new = new_file(&f, "B.txt").await;
    stage(&f, new, "B.txt", b"fresh", None, false, t0()).await;
    assert_eq!(
        f.test
            .db
            .pending_write(new)
            .await
            .unwrap()
            .unwrap()
            .base_tag,
        None,
        "a new file has no base: it must not exist"
    );
    assert_eq!(f.test.db.pending_write(new_id()).await.unwrap(), None);
    let all = f.test.db.pending_writes_of(f.library).await.unwrap();
    assert_eq!(all.len(), 2);
}

#[tokio::test]
async fn a_second_stage_keeps_the_first_base_and_time_and_replaces_the_content() {
    let f = fixture().await;
    let other = user(&f.test, "Bob", false).await.id;
    let node = new_file(&f, "A.txt").await;
    let (_, first) = stage(&f, node, "A.txt", b"one", Some("base-a"), false, t0()).await;
    // A failed attempt is forgotten by a newer stage.
    let one = BlobRef::of(b"one");
    assert!(
        f.test
            .db
            .fail_write(node, &one.sha256, "no luck", t0() + Duration::hours(1))
            .await
            .unwrap()
    );

    let later = t0() + Duration::seconds(10);
    let (blob, second) = stage_by(
        &f,
        other,
        node,
        "A.txt",
        b"two two",
        Some("base-b"),
        true,
        later,
    )
    .await;
    assert_ne!(first, second);
    let row = f.test.db.pending_write(node).await.unwrap().unwrap();
    assert_eq!(row.base_tag.as_deref(), Some("base-a"));
    assert_eq!(row.first_staged_at, t0());
    assert_eq!(row.staged_at, later);
    assert_eq!(row.blob, blob);
    assert_eq!(row.blob.size, 7);
    assert_eq!(row.by, other);
    assert_eq!(row.revision, second);
    assert!(row.flush);
    assert_eq!(row.state, PendingState::Scanning);
    assert_eq!((row.error, row.attempts, row.next_try_at), (None, 0, None));

    // Flush, once asked for, stays.
    stage(
        &f,
        node,
        "A.txt",
        b"three",
        Some("base-c"),
        false,
        later + Duration::seconds(1),
    )
    .await;
    let row = f.test.db.pending_write(node).await.unwrap().unwrap();
    assert!(row.flush);
    assert_eq!(row.base_tag.as_deref(), Some("base-a"));
    assert_eq!(
        f.test.db.pending_writes_of(f.library).await.unwrap().len(),
        1
    );
}

#[tokio::test]
async fn staging_a_tracked_file_moves_its_revision_and_a_new_file_keeps_it_in_the_row() {
    let f = fixture().await;
    let (node, tag) = tracked_file(&f, "T.txt").await;
    let before = f.test.db.external_item(node).await.unwrap().unwrap();
    let (_, revision) = stage(&f, node, "T.txt", b"edited", Some(&tag), false, t0()).await;
    let after = f.test.db.external_item(node).await.unwrap().unwrap();
    assert_eq!(after.revision, revision);
    assert_ne!(after.revision, before.revision);
    assert_eq!(
        after.tag, tag,
        "the tag is the bucket's until it is written"
    );

    let (_, again) = stage(&f, node, "T.txt", b"edited again", Some(&tag), false, t0()).await;
    assert_ne!(again, revision);
    assert_eq!(
        f.test
            .db
            .external_item(node)
            .await
            .unwrap()
            .unwrap()
            .revision,
        again
    );

    let new = new_file(&f, "N.txt").await;
    let (_, kept) = stage(&f, new, "N.txt", b"new", None, false, t0()).await;
    assert_eq!(f.test.db.source_item_of(new).await.unwrap(), None);
    assert_eq!(f.test.db.external_item(new).await.unwrap(), None);
    assert_eq!(
        f.test
            .db
            .pending_write(new)
            .await
            .unwrap()
            .unwrap()
            .revision,
        kept
    );
}

#[tokio::test]
async fn staged_content_is_referenced_and_cannot_be_released_evicted_or_unpinned() {
    let f = fixture().await;
    let db = &f.test.db;
    let node = new_file(&f, "A.txt").await;
    let (blob, _) = stage(&f, node, "A.txt", b"staged", None, false, t0()).await;
    let loose = BlobRef::of(b"only cached");
    db.pin_cached(&loose, t0()).await.unwrap();

    // Not unpinned, not offered to the eviction, not released, not unreferenced.
    assert!(!db.unpin_cached(&blob.sha256).await.unwrap());
    assert_eq!(db.cached_bytes().await.unwrap(), blob.size + loose.size);
    assert_eq!(db.coldest_cached(10).await.unwrap(), vec![loose]);
    // Dropping the cache row altogether (as a bug elsewhere might) changes nothing: the
    // pending row alone keeps the content.
    sqlx::query("delete from source_cache where sha256 = $1")
        .bind(blob.hex())
        .execute(db.test_pool())
        .await
        .unwrap();
    assert!(!db.release_blob_if_unreferenced(&blob.sha256).await.unwrap());
    let far = OffsetDateTime::now_utc() + Duration::days(1);
    let unreferenced = db.unreferenced_blobs(far).await.unwrap();
    assert!(!unreferenced.contains(&blob.sha256));

    // The scanner sees it: queued when unscanned, due when pending.
    scanner_on(&f).await;
    // The staged content (named by its pending row alone) and the cached one.
    assert_eq!(db.queue_unscanned().await.unwrap(), 2);
    let due = db.due_scans(far, 10).await.unwrap();
    assert!(due.iter().any(|d| d.blob == blob), "{due:?}");

    // Once the row is gone, nothing names it any more.
    assert!(db.drop_write(node, &blob.sha256).await.unwrap());
    assert!(db.release_blob_if_unreferenced(&blob.sha256).await.unwrap());
}

#[tokio::test]
async fn due_writes_wait_for_the_throttle_and_the_retry_time_and_flush_skips_the_throttle() {
    let f = fixture().await;
    let db = &f.test.db;
    let a = new_file(&f, "A.txt").await;
    let b = new_file(&f, "B.txt").await;
    let (blob_a, _) = stage(&f, a, "A.txt", b"aaa", None, false, t0()).await;
    let five = t0() + Duration::seconds(5);
    stage(&f, b, "B.txt", b"bbb", None, true, five).await;

    // The flushed one is due at once; the other waits for the throttle.
    let due = db.due_writes(five, 10).await.unwrap();
    assert_eq!(due.iter().map(|w| w.node).collect::<Vec<_>>(), vec![b]);
    let before = t0() + COALESCE - Duration::seconds(1);
    assert!(
        !db.due_writes(before, 10)
            .await
            .unwrap()
            .iter()
            .any(|w| w.node == a)
    );
    // At exactly the throttle both are due, oldest first.
    let due = db.due_writes(t0() + COALESCE, 10).await.unwrap();
    assert_eq!(due.iter().map(|w| w.node).collect::<Vec<_>>(), vec![a, b]);
    assert_eq!(db.due_writes(t0() + COALESCE, 1).await.unwrap().len(), 1);

    // A retry time holds a row back, flushed or not.
    let retry_at = t0() + Duration::minutes(5);
    assert!(
        db.fail_write(a, &blob_a.sha256, "offline", retry_at)
            .await
            .unwrap()
    );
    let soon = t0() + Duration::minutes(2);
    assert_eq!(
        db.due_writes(soon, 10)
            .await
            .unwrap()
            .iter()
            .map(|w| w.node)
            .collect::<Vec<_>>(),
        vec![b]
    );
    assert_eq!(db.due_writes(retry_at, 10).await.unwrap().len(), 2);

    // Retry makes it due at once, flush skips the throttle only.
    assert!(db.retry_write(a, soon).await.unwrap());
    let row = db.pending_write(a).await.unwrap().unwrap();
    assert_eq!(row.state, PendingState::Scanning);
    assert_eq!(row.error.as_deref(), Some("offline"), "kept until written");
    assert_eq!(db.due_writes(soon, 10).await.unwrap().len(), 2);

    let c = new_file(&f, "C.txt").await;
    stage(&f, c, "C.txt", b"ccc", None, false, soon).await;
    assert!(
        !db.due_writes(soon, 10)
            .await
            .unwrap()
            .iter()
            .any(|w| w.node == c)
    );
    assert!(db.flush_write(c, soon).await.unwrap());
    assert!(
        db.due_writes(soon, 10)
            .await
            .unwrap()
            .iter()
            .any(|w| w.node == c)
    );
}

#[tokio::test]
async fn finish_write_sets_the_tag_keeps_the_revision_and_removes_the_row() {
    let f = fixture().await;
    let db = &f.test.db;
    let (node, old_tag) = tracked_file(&f, "T.txt").await;
    assert!(
        db.set_external_unavailable(node, &old_tag, Some(Unavailable::Denied))
            .await
            .unwrap()
    );
    let (blob, revision) = stage(
        &f,
        node,
        "T.txt",
        b"edited text",
        Some(&old_tag),
        false,
        t0(),
    )
    .await;

    let new_tag = tag_of(blob.size, 1_800_000_000);
    let modified = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
    assert!(
        db.finish_write(node, &blob.sha256, &new_tag, blob.size, modified)
            .await
            .unwrap()
    );

    assert_eq!(db.pending_write(node).await.unwrap(), None);
    let item = db.source_item_of(node).await.unwrap().unwrap();
    assert_eq!(item.blob_sha.as_deref(), Some(new_tag.as_str()));
    assert_eq!(item.size, i64::try_from(blob.size).unwrap());
    let external = db.external_item(node).await.unwrap().unwrap();
    assert_eq!(external.tag, new_tag);
    assert_eq!(external.size, blob.size);
    assert_eq!(external.mtime_ns, 1_800_000_000_000_000_000);
    assert_eq!(external.revision, revision, "the staged revision stays");
    assert_eq!(external.sha256, Some(blob.sha256));
    assert_eq!(external.unavailable, None);
    assert_eq!(external.version, None);

    // Nothing left to finish.
    assert!(
        !db.finish_write(node, &blob.sha256, &new_tag, blob.size, modified)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn finish_write_for_superseded_content_keeps_the_row_and_moves_its_base() {
    let f = fixture().await;
    let db = &f.test.db;
    let (node, old_tag) = tracked_file(&f, "T.txt").await;
    let (first, _) = stage(&f, node, "T.txt", b"first", Some(&old_tag), false, t0()).await;
    // A newer save arrives while the first is being written.
    let (second, newer) = stage(
        &f,
        node,
        "T.txt",
        b"second, longer",
        Some(&old_tag),
        false,
        t0() + Duration::seconds(3),
    )
    .await;

    let written = tag_of(first.size, 1_800_000_000);
    let modified = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
    assert!(
        !db.finish_write(node, &first.sha256, &written, first.size, modified)
            .await
            .unwrap()
    );
    let row = db.pending_write(node).await.unwrap().unwrap();
    assert_eq!(row.blob, second);
    assert_eq!(row.revision, newer);
    assert_eq!(
        row.base_tag.as_deref(),
        Some(written.as_str()),
        "the next write is based on what the bucket holds now"
    );
    let external = db.external_item(node).await.unwrap().unwrap();
    assert_eq!(external.tag, written, "the listing sees no phantom change");
    assert_eq!(external.revision, newer);
    assert_ne!(external.sha256, Some(first.sha256));

    // A new file superseded the same way is tracked, so a listing finds its node.
    let new = new_file(&f, "N.txt").await;
    let (one, _) = stage(&f, new, "dir/N.txt", b"one", None, false, t0()).await;
    stage(&f, new, "dir/N.txt", b"one and more", None, false, t0()).await;
    let tag = tag_of(one.size, 1_800_000_001);
    assert!(
        !db.finish_write(new, &one.sha256, &tag, one.size, modified)
            .await
            .unwrap()
    );
    let row = db.pending_write(new).await.unwrap().unwrap();
    assert_eq!(row.base_tag.as_deref(), Some(tag.as_str()));
    let item = db.source_item_of(new).await.unwrap().unwrap();
    assert_eq!(item.repository_path, "dir/N.txt");
    assert_eq!(item.blob_sha.as_deref(), Some(tag.as_str()));
}

#[tokio::test]
async fn finishing_a_new_file_creates_its_tracking_row() {
    let f = fixture().await;
    let db = &f.test.db;
    let node = new_file(&f, "N.txt").await;
    let (blob, revision) = stage(&f, node, "dir/N.txt", b"brand new", None, false, t0()).await;
    assert_eq!(db.source_item_of(node).await.unwrap(), None);

    let tag = tag_of(blob.size, 1_800_000_000);
    let modified = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
    assert!(
        db.finish_write(node, &blob.sha256, &tag, blob.size, modified)
            .await
            .unwrap()
    );
    assert_eq!(db.pending_write(node).await.unwrap(), None);
    let item = db.source_item_of(node).await.unwrap().unwrap();
    assert_eq!(item.node, node);
    assert_eq!(item.path, "dir/N.txt");
    assert_eq!(item.repository_path, "dir/N.txt");
    assert_eq!(item.blob_sha.as_deref(), Some(tag.as_str()));
    assert_eq!(item.size, i64::try_from(blob.size).unwrap());
    let external = db.external_item(node).await.unwrap().unwrap();
    assert_eq!(external.revision, revision);
    assert_eq!(external.sha256, Some(blob.sha256));
    assert_eq!(external.tag, tag);
    assert_eq!(external.library, f.library);
}

#[tokio::test]
async fn fail_retry_flush_and_drop_are_refused_for_other_content() {
    let f = fixture().await;
    let db = &f.test.db;
    let node = new_file(&f, "A.txt").await;
    let (blob, _) = stage(&f, node, "A.txt", b"mine", None, false, t0()).await;
    let other = BlobRef::of(b"someone else's");
    let retry_at = t0() + Duration::minutes(1);

    assert!(
        !db.fail_write(node, &other.sha256, "no", retry_at)
            .await
            .unwrap()
    );
    assert!(!db.drop_write(node, &other.sha256).await.unwrap());
    let row = db.pending_write(node).await.unwrap().unwrap();
    assert_eq!(row.state, PendingState::Scanning);
    assert_eq!((row.error.clone(), row.attempts), (None, 0));
    assert_eq!(row.blob, blob);

    // For the right content they work; the error is cut to 300 characters.
    let long = "é".repeat(400);
    assert!(
        db.fail_write(node, &blob.sha256, &long, retry_at)
            .await
            .unwrap()
    );
    let row = db.pending_write(node).await.unwrap().unwrap();
    assert_eq!(row.state, PendingState::Failed);
    assert_eq!(row.error.as_deref().map(|e| e.chars().count()), Some(300));
    assert_eq!((row.attempts, row.next_try_at), (1, Some(retry_at)));
    assert!(
        db.fail_write(node, &blob.sha256, "again", retry_at)
            .await
            .unwrap()
    );
    assert_eq!(db.pending_write(node).await.unwrap().unwrap().attempts, 2);
    assert_eq!(db.pending_counts(f.library).await.unwrap(), (0, 1));

    // No row, no change.
    let gone = new_id();
    assert!(!db.retry_write(gone, t0()).await.unwrap());
    assert!(!db.flush_write(gone, t0()).await.unwrap());
    assert!(
        !db.fail_write(gone, &blob.sha256, "x", retry_at)
            .await
            .unwrap()
    );
    assert!(!db.drop_write(gone, &blob.sha256).await.unwrap());

    assert!(db.retry_write(node, t0()).await.unwrap());
    assert_eq!(db.pending_counts(f.library).await.unwrap(), (1, 0));
    assert!(db.flush_write(node, t0()).await.unwrap());
    assert!(db.pending_write(node).await.unwrap().unwrap().flush);
}

#[tokio::test]
async fn dropping_a_tracked_write_withdraws_its_revision() {
    let f = fixture().await;
    let db = &f.test.db;
    let (node, tag) = tracked_file(&f, "T.txt").await;
    let original = db.external_item(node).await.unwrap().unwrap();
    let (blob, staged) = stage(&f, node, "T.txt", b"edit", Some(&tag), false, t0()).await;

    assert!(db.drop_write(node, &blob.sha256).await.unwrap());
    assert_eq!(db.pending_write(node).await.unwrap(), None);
    let after = db.external_item(node).await.unwrap().unwrap();
    assert!(is_revision(&after.revision));
    assert_ne!(after.revision, staged, "the staged revision is withdrawn");
    assert_ne!(after.revision, original.revision, "a fresh one is minted");
    assert_eq!(after.tag, original.tag);
    assert_eq!(after.sha256, original.sha256);

    // A new file's drop leaves no tracking row behind.
    let new = new_file(&f, "N.txt").await;
    let (blob, _) = stage(&f, new, "N.txt", b"new", None, false, t0()).await;
    assert!(db.drop_write(new, &blob.sha256).await.unwrap());
    assert_eq!(db.source_item_of(new).await.unwrap(), None);
}

#[tokio::test]
async fn mode_round_trips_and_github_cannot_be_read_write() {
    let f = fixture().await;
    let db = &f.test.db;
    let (config, _) = db.source_of(f.library).await.unwrap().unwrap();
    assert_eq!(config.mode, SourceMode::ReadWrite);

    // No change, no generation.
    let before = generation(&f).await;
    assert!(
        !db.set_source_mode(f.library, SourceMode::ReadWrite)
            .await
            .unwrap()
    );
    assert_eq!(generation(&f).await, before);

    assert!(
        db.set_source_mode(f.library, SourceMode::ReadOnly)
            .await
            .unwrap()
    );
    assert_eq!(generation(&f).await, before + 1);
    let (config, _) = db.source_of(f.library).await.unwrap().unwrap();
    assert_eq!(config.mode, SourceMode::ReadOnly);
    assert!(
        db.set_source_mode(f.library, SourceMode::ReadWrite)
            .await
            .unwrap()
    );
    assert_eq!(generation(&f).await, before + 2);

    // A library with no connection has no mode to change.
    assert!(
        !db.set_source_mode(new_id(), SourceMode::ReadWrite)
            .await
            .unwrap()
    );

    // A GitHub library is read-only for good.
    let github = put(
        &f.test,
        new_id(),
        NodeKind::Library,
        Some(f.site),
        f.site,
        "Repo",
    )
    .await;
    db.connect_source(&NewSource {
        library: github,
        kind: SourceKind::Github,
        owner: "owner".into(),
        repo: "repo".into(),
        branch: "main".into(),
        folder: String::new(),
        location: None,
        mode: SourceMode::ReadOnly,
        token: None,
        poll_seconds: 300,
        connected_by: f.user,
    })
    .await
    .unwrap();
    assert_eq!(
        db.set_source_mode(github, SourceMode::ReadWrite).await,
        Err(AccessError::InvalidSetting)
    );
    let (config, _) = db.source_of(github).await.unwrap().unwrap();
    assert_eq!(config.mode, SourceMode::ReadOnly);
    assert!(
        !db.set_source_mode(github, SourceMode::ReadOnly)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn read_only_is_refused_while_writes_are_pending() {
    let f = fixture().await;
    let db = &f.test.db;
    let node = new_file(&f, "A.txt").await;
    let (blob, _) = stage(&f, node, "A.txt", b"waiting", None, false, t0()).await;
    let before = generation(&f).await;

    assert_eq!(
        db.set_source_mode(f.library, SourceMode::ReadOnly).await,
        Err(AccessError::WritesPending)
    );
    let (config, _) = db.source_of(f.library).await.unwrap().unwrap();
    assert_eq!(config.mode, SourceMode::ReadWrite);
    assert_eq!(generation(&f).await, before);
    // Staying read and write is not a change, so nothing is refused.
    assert!(
        !db.set_source_mode(f.library, SourceMode::ReadWrite)
            .await
            .unwrap()
    );

    assert!(db.drop_write(node, &blob.sha256).await.unwrap());
    assert!(
        db.set_source_mode(f.library, SourceMode::ReadOnly)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn write_state_round_trips_and_is_cut_to_300_characters() {
    let f = fixture().await;
    let db = &f.test.db;
    assert_eq!(
        db.write_state(f.library).await.unwrap(),
        Some((WriteState::Ok, None))
    );
    assert_eq!(db.write_state(new_id()).await.unwrap(), None);

    let long = "é".repeat(400);
    assert!(
        db.set_write_state(f.library, WriteState::Blocked, Some(&long))
            .await
            .unwrap()
    );
    let (state, error) = db.write_state(f.library).await.unwrap().unwrap();
    assert_eq!(state, WriteState::Blocked);
    assert_eq!(error.map(|e| e.chars().count()), Some(300));
    // The same again changes nothing.
    assert!(
        !db.set_write_state(f.library, WriteState::Blocked, Some(&long))
            .await
            .unwrap()
    );

    assert!(
        db.set_write_state(f.library, WriteState::Ok, None)
            .await
            .unwrap()
    );
    assert_eq!(
        db.write_state(f.library).await.unwrap(),
        Some((WriteState::Ok, None))
    );
    assert!(
        !db.set_write_state(new_id(), WriteState::Blocked, Some("x"))
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn disconnect_is_refused_while_writes_are_pending() {
    let f = fixture().await;
    let db = &f.test.db;
    let node = new_file(&f, "A.txt").await;
    let (blob, _) = stage(&f, node, "A.txt", b"waiting", None, false, t0()).await;

    assert_eq!(
        db.disconnect_source(f.library).await,
        Err(AccessError::WritesPending)
    );
    assert_eq!(
        db.disconnect_source_as(f.user, f.library).await,
        Err(AccessError::WritesPending)
    );
    assert!(db.source_of(f.library).await.unwrap().is_some());
    assert!(db.pending_write(node).await.unwrap().is_some());

    assert!(db.drop_write(node, &blob.sha256).await.unwrap());
    assert_eq!(db.disconnect_source(f.library).await.unwrap().len(), 1);
    assert!(db.source_of(f.library).await.unwrap().is_none());
}

#[tokio::test]
async fn purging_a_node_or_a_site_removes_its_pending_rows() {
    let f = fixture().await;
    let db = &f.test.db;
    let doomed = new_file(&f, "Doomed.txt").await;
    let kept = new_file(&f, "Kept.txt").await;
    stage(&f, doomed, "Doomed.txt", b"doomed", None, false, t0()).await;
    stage(&f, kept, "Kept.txt", b"kept", None, false, t0()).await;

    let now = OffsetDateTime::now_utc();
    let purged = |id: NodeId, kind: NodeKind, parent: Option<NodeId>, name: &str| NodeRow {
        id,
        kind,
        parent,
        site: f.site,
        name: name.into(),
        deleted_at: Some(now),
        deleted_by: None,
        created_by: None,
        created_at: now,
        deleted_with: (kind != NodeKind::Site).then_some(f.site),
        bin_stage: None,
        purged_at: Some(now),
    };
    db.apply_nodes(NodeChange {
        rows: &[purged(
            doomed,
            NodeKind::Document,
            Some(f.library),
            "Doomed.txt",
        )],
        ..NodeChange::default()
    })
    .await
    .unwrap();
    assert_eq!(db.pending_write(doomed).await.unwrap(), None);
    assert!(db.pending_write(kept).await.unwrap().is_some());

    db.apply_nodes(NodeChange {
        rows: &[
            purged(f.site, NodeKind::Site, None, "Site"),
            purged(f.library, NodeKind::Library, Some(f.site), "Bucket"),
            purged(kept, NodeKind::Document, Some(f.library), "Kept.txt"),
        ],
        purge_site: Some(f.site),
        ..NodeChange::default()
    })
    .await
    .unwrap();
    let left: i64 = sqlx::query_scalar("select count(*) from source_writes")
        .fetch_one(db.test_pool())
        .await
        .unwrap();
    assert_eq!(left, 0);
    assert!(db.source_of(f.library).await.unwrap().is_none());
}

fn external_upload(f: &Fixture, size: u64, external: bool) -> NewUpload<'static> {
    NewUpload {
        id: Uuid::new_v4(),
        owner: f.user,
        site: f.site,
        target: UploadTarget::NewFile { parent: f.library },
        name: "big.bin",
        size,
        sha256: [9; 32],
        base: None,
        on_conflict: None,
        external,
    }
}

#[tokio::test]
async fn an_external_upload_session_reserves_no_quota() {
    let f = fixture().await;
    let db = &f.test.db;
    db.set_site_quota(f.user, f.site, Some(100)).await.unwrap();

    // Far over the quota, and still started; nothing reserved.
    let big = db
        .start_upload(external_upload(&f, 5_000, true))
        .await
        .unwrap();
    assert!(big.external);
    assert_eq!(big.size, 5_000);
    assert!(db.upload_session(big.id).await.unwrap().unwrap().external);
    assert_eq!(db.site_usage(f.site).await.unwrap().reserved, 0);

    // An ordinary session beside it still reserves, and is still refused over the quota.
    let ordinary = db
        .start_upload(external_upload(&f, 60, false))
        .await
        .unwrap();
    assert!(!ordinary.external);
    assert_eq!(db.site_usage(f.site).await.unwrap().reserved, 60);
    assert!(matches!(
        db.start_upload(external_upload(&f, 41, false)).await,
        Err(records::UploadError::QuotaExceeded)
    ));

    // Ending the external one releases nothing; at a start only the ordinary one is set aside.
    assert!(db.end_upload(big.id).await.unwrap());
    assert_eq!(db.site_usage(f.site).await.unwrap().reserved, 60);
    let again = db
        .start_upload(external_upload(&f, 7_000, true))
        .await
        .unwrap();
    db.reset_reservations().await.unwrap();
    assert_eq!(db.site_usage(f.site).await.unwrap().reserved, 60);

    // Claiming it answers a reservation of nothing, and leaves the ordinary one's alone.
    let claimed = db.claim_upload(again.id).await.unwrap().unwrap();
    assert_eq!((claimed.site(), claimed.bytes()), (f.site, 0));
    assert!(db.upload_session(again.id).await.unwrap().is_none());
    assert_eq!(db.site_usage(f.site).await.unwrap().reserved, 60);
    db.commit_reservation(claimed).await.unwrap();
    let usage = db.site_usage(f.site).await.unwrap();
    assert_eq!((usage.used, usage.reserved), (0, 60));
    assert!(db.end_upload(ordinary.id).await.unwrap());
    assert_eq!(db.site_usage(f.site).await.unwrap().reserved, 0);
}

#[tokio::test]
async fn migrating_a_phase_1_database_keeps_its_rows() {
    let test = TestDb::create_at(Some(24)).await;
    let ann = user(&test, "MigrationAnn", true).await;
    let site = site(&test, "Migration Site", Some(ann.id)).await;
    let library = put(
        &test,
        new_id(),
        NodeKind::Library,
        Some(site),
        site,
        "Bucket",
    )
    .await;
    let doc: NodeId = document_in(&test, ann.id, library, site, "A.txt")
        .await
        .into();
    let pool = test.db.test_pool();
    sqlx::query(
        "insert into library_sources \
         (library_id, kind, owner, repo, branch, folder, location, mode, poll_seconds, \
          connected_by, generation) \
         values ($1, 's3', '', '', '', '', 's3://r2/old-bucket/', 'read_only', 300, $2, 5)",
    )
    .bind(library.0)
    .bind(ann.id.0)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "insert into source_s3 (library_id, endpoint, bucket, access_key_id) \
         values ($1, 'r2', 'old-bucket', 'AKIA')",
    )
    .bind(library.0)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "insert into source_items \
         (library_id, path, repository_path, node_id, blob_sha, size, revision, modified_at) \
         values ($1, 'A.txt', 'A.txt', $2, $3, 4, $4, now())",
    )
    .bind(library.0)
    .bind(doc.0)
    .bind(tag_of(4, 1_700_000_000))
    .bind("b".repeat(40))
    .execute(pool)
    .await
    .unwrap();
    let session = Uuid::new_v4();
    sqlx::query(
        "insert into upload_sessions (id, owner, site_id, parent_id, name, size, sha256) \
         values ($1, $2, $3, $4, 'f.bin', 10, $5)",
    )
    .bind(session)
    .bind(ann.id.0)
    .bind(site.0)
    .bind(library.0)
    .bind(vec![1_u8; 32])
    .execute(pool)
    .await
    .unwrap();

    test.db.migrate().await.unwrap();

    let (config, _) = test.db.source_of(library).await.unwrap().unwrap();
    assert_eq!(config.kind, SourceKind::S3);
    assert_eq!(config.mode, SourceMode::ReadOnly);
    assert_eq!(test.db.source_generation(library).await.unwrap(), Some(5));
    assert_eq!(
        test.db.write_state(library).await.unwrap(),
        Some((WriteState::Ok, None))
    );
    let item = test.db.external_item(doc).await.unwrap().unwrap();
    assert_eq!(item.revision, "b".repeat(40));
    assert_eq!(test.db.pending_writes_of(library).await.unwrap(), vec![]);
    let upload = test.db.upload_session(session).await.unwrap().unwrap();
    assert!(!upload.external, "an old session reserves as it did");

    // The new rules work on the migrated database.
    assert!(
        test.db
            .set_source_mode(library, SourceMode::ReadWrite)
            .await
            .unwrap()
    );
    let blob = BlobRef::of(b"edit");
    test.db.pin_cached(&blob, t0()).await.unwrap();
    test.db
        .stage_write(&NewPendingWrite {
            node: doc,
            library,
            path: "A.txt",
            blob,
            base_tag: Some(&tag_of(4, 1_700_000_000)),
            by: ann.id,
            flush: false,
            now: t0(),
        })
        .await
        .unwrap();
    assert_eq!(test.db.pending_counts(library).await.unwrap(), (1, 0));
}

#[tokio::test]
async fn staging_is_refused_for_a_read_only_library_and_after_a_switch_to_read_only() {
    let f = fixture().await;
    let db = &f.test.db;
    let node = new_file(&f, "A.txt").await;
    let blob = BlobRef::of(b"content");
    db.pin_cached(&blob, t0()).await.unwrap();
    let new = NewPendingWrite {
        node,
        library: f.library,
        path: "A.txt",
        blob,
        base_tag: None,
        by: f.user,
        flush: false,
        now: t0(),
    };

    assert!(
        db.set_source_mode(f.library, SourceMode::ReadOnly)
            .await
            .unwrap()
    );
    assert_eq!(
        db.stage_write(&new).await,
        Err(AccessError::LibraryReadOnly)
    );
    assert_eq!(db.pending_write(node).await.unwrap(), None);

    // Read and write again: accepted. No connection at all: refused.
    assert!(
        db.set_source_mode(f.library, SourceMode::ReadWrite)
            .await
            .unwrap()
    );
    db.stage_write(&new).await.unwrap();
    let elsewhere = NewPendingWrite {
        library: new_id(),
        node: new_file(&f, "B.txt").await,
        ..new
    };
    assert_eq!(
        db.stage_write(&elsewhere).await,
        Err(AccessError::LibraryReadOnly)
    );
}

#[tokio::test]
async fn a_superseded_finish_forgets_the_old_content_so_a_dropped_newer_stage_serves_the_bucket() {
    let f = fixture().await;
    let db = &f.test.db;
    let (node, old_tag) = tracked_file(&f, "T.txt").await;
    let old = BlobRef::of(b"read earlier");
    db.pin_cached(&old, t0()).await.unwrap();
    db.set_external_content(node, &old_tag, &old.sha256, t0())
        .await
        .unwrap();
    assert_eq!(
        db.external_item(node).await.unwrap().unwrap().sha256,
        Some(old.sha256)
    );

    let (a, _) = stage(&f, node, "T.txt", b"stage A", Some(&old_tag), false, t0()).await;
    let (b, _) = stage(&f, node, "T.txt", b"stage B!", Some(&old_tag), false, t0()).await;
    let written = tag_of(a.size, 1_800_000_000);
    let modified = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
    assert!(
        !db.finish_write(node, &a.sha256, &written, a.size, modified)
            .await
            .unwrap()
    );
    assert!(db.drop_write(node, &b.sha256).await.unwrap());

    let item = db.external_item(node).await.unwrap().unwrap();
    assert_eq!(item.tag, written);
    assert_eq!(item.sha256, None, "the next read fetches from the bucket");
}
