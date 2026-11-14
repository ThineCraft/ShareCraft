#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use collab_core::ids::DocId;
use records::test_support::TestDb;
use records::{BLOB_REFS_DIRTY, BLOB_REFS_UPGRADE};
use revisions::BlobRef;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

fn doc() -> DocId {
    DocId(Uuid::new_v4())
}

const REV_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const REV_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

/// A pending claim keeps the content from being released; settled with a revision it is a
/// completed row that still does; settled without one it is gone, and only then may the content
/// be erased.
#[tokio::test]
async fn claims_hold_content_until_settled() {
    let test = TestDb::create().await;
    let db = &test.db;
    let blob = BlobRef::of(b"bytes");
    let (d1, d2) = (doc(), doc());

    let first = db.claim_blob(blob, d1).await.unwrap();
    assert!(!db.release_blob_if_unreferenced(&blob.sha256).await.unwrap());
    db.settle_claim(first, Some(REV_A)).await.unwrap();
    assert_eq!(
        db.blob_refs_of(&blob.sha256).await.unwrap(),
        vec![(d1, REV_A.to_owned())]
    );
    assert!(!db.release_blob_if_unreferenced(&blob.sha256).await.unwrap());

    // The same revision recorded again from Git adds nothing; settling twice does nothing.
    db.record_blob_ref(d1, blob, REV_A).await.unwrap();
    db.settle_claim(first, None).await.unwrap();
    assert_eq!(db.blob_refs_of(&blob.sha256).await.unwrap().len(), 1);

    // Another document's failed save leaves nothing behind.
    let second = db.claim_blob(blob, d2).await.unwrap();
    db.settle_claim(second, None).await.unwrap();
    assert_eq!(db.blob_refs_of(&blob.sha256).await.unwrap().len(), 1);

    // Once nothing names it, the row goes and the caller may erase.
    sqlx::query("delete from blob_refs")
        .execute(db.test_pool())
        .await
        .unwrap();
    assert!(db.blob_created_at(&blob.sha256).await.unwrap().is_some());
    assert!(db.release_blob_if_unreferenced(&blob.sha256).await.unwrap());
    assert!(db.blob_created_at(&blob.sha256).await.unwrap().is_none());
    // Content with no row is named by nothing either.
    assert!(db.release_blob_if_unreferenced(&blob.sha256).await.unwrap());
}

/// An erasure waits for a claim being written, then counts it.
#[tokio::test]
async fn release_waits_for_a_claim_in_progress() {
    let test = TestDb::create().await;
    let db = &test.db;
    let blob = BlobRef::of(b"shared");
    db.record_blob_ref(doc(), blob, REV_A).await.unwrap();
    sqlx::query("delete from blob_refs")
        .execute(db.test_pool())
        .await
        .unwrap();

    // A transaction holding the row as a claim does, before its pending row is written.
    let mut holder = db.test_pool().begin().await.unwrap();
    sqlx::query("select sha256 from blobs where sha256 = $1 for share")
        .bind(blob.hex())
        .fetch_one(&mut *holder)
        .await
        .unwrap();
    let releasing = {
        let db = db.clone();
        tokio::spawn(async move { db.release_blob_if_unreferenced(&blob.sha256).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(!releasing.is_finished(), "the release waits for the lock");
    sqlx::query(
        "insert into blob_refs (sha256, doc_id, revision, pending) values ($1, $2, null, true)",
    )
    .bind(blob.hex())
    .bind(Uuid::new_v4())
    .execute(&mut *holder)
    .await
    .unwrap();
    holder.commit().await.unwrap();
    assert!(!releasing.await.unwrap().unwrap(), "the pending row counts");
}

#[tokio::test]
async fn unreferenced_and_stale_rows_are_found_by_age() {
    let test = TestDb::create().await;
    let db = &test.db;
    let (kept, loose, young) = (
        BlobRef::of(b"kept"),
        BlobRef::of(b"loose"),
        BlobRef::of(b"y"),
    );
    let d = doc();
    db.record_blob_ref(d, kept, REV_A).await.unwrap();
    let claim = db.claim_blob(loose, d).await.unwrap();
    db.settle_claim(claim, None).await.unwrap();
    db.claim_blob(young, d).await.unwrap();
    sqlx::query("update blobs set created_at = now() - interval '2 days' where sha256 <> $1")
        .bind(young.hex())
        .execute(db.test_pool())
        .await
        .unwrap();
    let day_ago = OffsetDateTime::now_utc() - Duration::days(1);
    assert_eq!(
        db.unreferenced_blobs(day_ago).await.unwrap(),
        vec![loose.sha256]
    );

    assert!(db.stale_claims(day_ago).await.unwrap().is_empty());
    sqlx::query("update blob_refs set created_at = now() - interval '2 hours' where pending")
        .execute(db.test_pool())
        .await
        .unwrap();
    let hour_ago = OffsetDateTime::now_utc() - Duration::hours(1);
    let stale = db.stale_claims(hour_ago).await.unwrap();
    assert_eq!(stale.len(), 1);
    assert_eq!((stale[0].doc, stale[0].blob), (d, young));
}

#[tokio::test]
async fn rebuild_replaces_completed_rows_and_clears_flags_only_when_complete() {
    let test = TestDb::create().await;
    let db = &test.db;
    // The upgrade marker is set by the migration.
    assert!(db.blob_refs_flagged().await.unwrap());
    let (a, b) = (BlobRef::of(b"a"), BlobRef::of(b"b"));
    let d = doc();
    db.record_blob_ref(d, a, REV_A).await.unwrap();
    let pending = db.claim_blob(b, d).await.unwrap();

    // A rebuild that could not read everything only adds.
    db.rebuild_blob_refs(&[(d, b, REV_B.to_owned())], false)
        .await
        .unwrap();
    assert_eq!(db.blob_refs_of(&a.sha256).await.unwrap().len(), 1);
    assert!(db.blob_refs_flagged().await.unwrap());

    // A complete one replaces the completed rows, keeps pending ones and clears the flags.
    db.set_blob_refs_flag(BLOB_REFS_DIRTY).await.unwrap();
    db.rebuild_blob_refs(&[(d, b, REV_B.to_owned())], true)
        .await
        .unwrap();
    assert!(db.blob_refs_of(&a.sha256).await.unwrap().is_empty());
    assert_eq!(
        db.blob_refs_of(&b.sha256).await.unwrap(),
        vec![(d, REV_B.to_owned())]
    );
    assert!(!db.blob_refs_flagged().await.unwrap());
    assert!(!db.release_blob_if_unreferenced(&b.sha256).await.unwrap());
    db.settle_claim(pending, None).await.unwrap();

    db.set_blob_refs_flag(BLOB_REFS_UPGRADE).await.unwrap();
    assert!(db.blob_refs_flagged().await.unwrap());
}
