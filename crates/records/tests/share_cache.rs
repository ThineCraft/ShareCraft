#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use collab_core::ids::{DocId, NodeId, UserId};
use common::{document_in, new_id, put, site, user};
use records::test_support::TestDb;
use records::{
    ExternalEntry, NewSource, NodeKind, ScanResult, ScanState, SourceKind, SourceMode, external_tag,
};
use revisions::BlobRef;
use serde_json::json;
use time::{Duration, OffsetDateTime};

const REVISION: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

async fn scanner_on(test: &TestDb, admin: UserId) {
    test.db
        .set_setting(
            admin,
            records::SCANNER_URL,
            json!("icap://scanner.example/scan"),
        )
        .await
        .unwrap();
}

async fn scan_state(test: &TestDb, blob: &BlobRef) -> (ScanState, bool) {
    let state = test.db.scan_state(&blob.sha256).await.unwrap().unwrap();
    (state.state, state.hold)
}

struct ShareFixture {
    test: TestDb,
    owner: UserId,
    site: NodeId,
    library: NodeId,
}

async fn share_fixture() -> ShareFixture {
    let test = TestDb::create().await;
    let owner = user(&test, "Owner", true).await.id;
    let site = site(&test, "Site", Some(owner)).await;
    let library = put(
        &test,
        new_id(),
        NodeKind::Library,
        Some(site),
        site,
        "Files",
    )
    .await;
    test.db
        .connect_source(&NewSource {
            library,
            kind: SourceKind::Share,
            owner: String::new(),
            repo: String::new(),
            branch: String::new(),
            folder: String::new(),
            location: Some(r"C:\Approved\Files".into()),
            mode: SourceMode::ReadOnly,
            token: None,
            poll_seconds: 300,
            connected_by: owner,
        })
        .await
        .unwrap();
    ShareFixture {
        test,
        owner,
        site,
        library,
    }
}

async fn external_document(
    fixture: &ShareFixture,
    name: &str,
    blob: &BlobRef,
    modified_at: OffsetDateTime,
) -> DocId {
    let doc = document_in(
        &fixture.test,
        fixture.owner,
        fixture.library,
        fixture.site,
        name,
    )
    .await;
    let mtime_ns = i64::try_from(modified_at.unix_timestamp_nanos()).unwrap();
    let entry = ExternalEntry {
        node: doc.into(),
        path: name.into(),
        repository_path: name.into(),
        size: blob.size,
        mtime_ns,
    };
    fixture
        .test
        .db
        .record_external_item(fixture.library, &entry)
        .await
        .unwrap();
    fixture
        .test
        .db
        .set_external_content(
            doc.into(),
            &external_tag(entry.size, entry.mtime_ns),
            &blob.sha256,
            modified_at,
        )
        .await
        .unwrap();
    doc
}

#[tokio::test]
async fn a_pin_creates_the_blob_row_pending_when_a_scanner_is_set_and_unscanned_otherwise() {
    let test = TestDb::create().await;
    let admin = user(&test, "Admin", true).await.id;
    let before = BlobRef::of(b"before scanner");
    let after = BlobRef::of(b"after scanner");

    test.db
        .pin_cached(&before, OffsetDateTime::UNIX_EPOCH)
        .await
        .unwrap();
    scanner_on(&test, admin).await;
    test.db
        .pin_cached(&after, OffsetDateTime::UNIX_EPOCH)
        .await
        .unwrap();

    assert_eq!(
        scan_state(&test, &before).await,
        (ScanState::Unscanned, true)
    );
    assert_eq!(scan_state(&test, &after).await, (ScanState::Pending, true));
}

#[tokio::test]
async fn pinning_content_already_scanned_clean_keeps_the_verdict() {
    let test = TestDb::create().await;
    let admin = user(&test, "Admin", true).await.id;
    let blob = BlobRef::of(b"clean");
    scanner_on(&test, admin).await;
    test.db
        .pin_cached(&blob, OffsetDateTime::UNIX_EPOCH)
        .await
        .unwrap();
    assert!(
        test.db
            .record_scan(&blob.sha256, &ScanResult::Clean, OffsetDateTime::UNIX_EPOCH)
            .await
            .unwrap()
    );

    test.db
        .pin_cached(&blob, OffsetDateTime::UNIX_EPOCH + Duration::hours(1))
        .await
        .unwrap();

    assert_eq!(scan_state(&test, &blob).await, (ScanState::Clean, true));
}

#[tokio::test]
async fn pinned_content_is_not_unreferenced_and_is_not_released() {
    let test = TestDb::create().await;
    let blob = BlobRef::of(b"pinned");
    test.db
        .pin_cached(&blob, OffsetDateTime::UNIX_EPOCH)
        .await
        .unwrap();

    assert!(
        test.db
            .unreferenced_blobs(OffsetDateTime::now_utc() + Duration::days(1))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        !test
            .db
            .release_blob_if_unreferenced(&blob.sha256)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn unpinned_content_with_no_other_reference_is_released() {
    let test = TestDb::create().await;
    let blob = BlobRef::of(b"unpinned");
    test.db
        .pin_cached(&blob, OffsetDateTime::UNIX_EPOCH)
        .await
        .unwrap();

    assert!(test.db.unpin_cached(&blob.sha256).await.unwrap());
    assert!(
        test.db
            .release_blob_if_unreferenced(&blob.sha256)
            .await
            .unwrap()
    );
    assert_eq!(test.db.blob_created_at(&blob.sha256).await.unwrap(), None);
}

#[tokio::test]
async fn content_both_pinned_and_named_by_a_version_survives_an_unpin() {
    let test = TestDb::create().await;
    let blob = BlobRef::of(b"two references");
    let doc = DocId(uuid::Uuid::new_v4());
    test.db.record_blob_ref(doc, blob, REVISION).await.unwrap();
    test.db
        .pin_cached(&blob, OffsetDateTime::UNIX_EPOCH)
        .await
        .unwrap();

    assert!(test.db.unpin_cached(&blob.sha256).await.unwrap());
    assert!(
        !test
            .db
            .release_blob_if_unreferenced(&blob.sha256)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn coldest_cached_orders_by_last_use_and_touch_moves_an_entry_back() {
    let test = TestDb::create().await;
    let zero = OffsetDateTime::UNIX_EPOCH;
    let first = BlobRef::of(b"first");
    let second = BlobRef::of(b"second");
    let third = BlobRef::of(b"third");
    for (blob, hours) in [(&first, 1), (&second, 2), (&third, 3)] {
        test.db
            .pin_cached(blob, zero + Duration::hours(hours))
            .await
            .unwrap();
    }

    assert_eq!(
        test.db.coldest_cached(2).await.unwrap(),
        vec![first, second]
    );
    assert!(
        test.db
            .touch_cached(&first.sha256, zero + Duration::hours(4))
            .await
            .unwrap()
    );
    assert!(!test.db.touch_cached(&[0; 32], zero).await.unwrap());
    assert_eq!(
        test.db.coldest_cached(3).await.unwrap(),
        vec![second, third, first]
    );
}

#[tokio::test]
async fn cached_bytes_sums_sizes() {
    let test = TestDb::create().await;
    let first = BlobRef::of(b"1234");
    let second = BlobRef::of(b"1234567");
    test.db
        .pin_cached(&first, OffsetDateTime::UNIX_EPOCH)
        .await
        .unwrap();
    test.db
        .pin_cached(&second, OffsetDateTime::UNIX_EPOCH)
        .await
        .unwrap();

    assert_eq!(test.db.cached_bytes().await.unwrap(), 11);
}

#[tokio::test]
async fn pinned_content_is_due_for_the_scanner_with_the_items_name() {
    let fixture = share_fixture().await;
    scanner_on(&fixture.test, fixture.owner).await;
    let blob = BlobRef::of(b"external bytes");
    external_document(&fixture, "Notes.txt", &blob, OffsetDateTime::UNIX_EPOCH).await;
    fixture
        .test
        .db
        .pin_cached(&blob, OffsetDateTime::UNIX_EPOCH)
        .await
        .unwrap();

    let due = fixture
        .test
        .db
        .due_scans(OffsetDateTime::now_utc(), 10)
        .await
        .unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].blob, blob);
    assert_eq!(due[0].name.as_deref(), Some("Notes.txt"));
}

#[tokio::test]
async fn a_held_external_file_is_marked_in_scan_marks_and_named_for_quarantine() {
    let fixture = share_fixture().await;
    scanner_on(&fixture.test, fixture.owner).await;
    let blob = BlobRef::of(b"held external bytes");
    let doc = external_document(&fixture, "Held.txt", &blob, OffsetDateTime::UNIX_EPOCH).await;
    fixture
        .test
        .db
        .pin_cached(&blob, OffsetDateTime::UNIX_EPOCH)
        .await
        .unwrap();

    let mark = fixture.test.db.scan_mark(doc).await.unwrap().unwrap();
    assert_eq!(mark.state, ScanState::Pending);
    let quarantine = fixture.test.db.quarantine_list().await.unwrap();
    assert_eq!(quarantine.len(), 1);
    assert_eq!(quarantine[0].documents.len(), 1);
    assert_eq!(quarantine[0].documents[0].id, doc);
    assert_eq!(quarantine[0].documents[0].name, "Held.txt");
}

#[tokio::test]
async fn a_share_file_lists_with_the_shares_modified_time() {
    let fixture = share_fixture().await;
    let modified = OffsetDateTime::UNIX_EPOCH + Duration::days(20);
    let blob = BlobRef::of(b"listed");
    let doc = external_document(&fixture, "Listed.txt", &blob, modified).await;

    let (children, _) = fixture
        .test
        .db
        .children(fixture.owner, fixture.library, None, 10)
        .await
        .unwrap()
        .unwrap();
    let listed = children
        .iter()
        .find(|item| item.node.id == doc.into())
        .unwrap();
    assert_eq!(listed.revised_at, Some(modified));
}

#[tokio::test]
async fn an_ordinary_never_saved_file_still_lists_without_a_time() {
    let test = TestDb::create().await;
    let owner = user(&test, "Owner", false).await.id;
    let site = site(&test, "Site", Some(owner)).await;
    let library = put(
        &test,
        new_id(),
        NodeKind::Library,
        Some(site),
        site,
        "Files",
    )
    .await;
    let doc = document_in(&test, owner, library, site, "Draft.txt").await;

    let (children, _) = test
        .db
        .children(owner, library, None, 10)
        .await
        .unwrap()
        .unwrap();
    let listed = children
        .iter()
        .find(|item| item.node.id == doc.into())
        .unwrap();
    assert_eq!(listed.revised_at, None);
}
