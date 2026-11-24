//! Site quotas in the records: reserving bytes against a site's quota, turning a reservation
//! into usage or releasing it, upload sessions holding their declared size while open, and the
//! rebuild of a site's usage from the sizes of every version of its items, live and in the bin,
//! with purged items' versions given back.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use collab_core::ids::{DocId, NodeId, UserId};
use common::{delete, document_in, new_id, put, site, user};
use records::test_support::TestDb;
use records::{
    NewSource, NewUpload, NodeKind, NodeRow, QuotaError, RevisionRecord, SourceKind, SourceMode,
    UploadError, UploadTarget,
};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

/// A revision of `doc` numbered `n` holding `size` bytes.
fn record(doc: DocId, n: u8, size: u64) -> RevisionRecord {
    RevisionRecord {
        doc,
        revision: format!("{:032x}{n:08x}", doc.0.as_u128() & 0xffff_ffff),
        committed_at: OffsetDateTime::from_unix_timestamp(1_800_000_000 + i64::from(n)).unwrap(),
        authors: vec![("u1".into(), "Ann".into())],
        name: "file.pdf".into(),
        fields: json!({}),
        size,
    }
}

/// A site with a library, for tests that need only somewhere to count usage.
async fn site_with_library(test: &TestDb, owner: UserId) -> (NodeId, NodeId) {
    let site = site(test, "Team", Some(owner)).await;
    let library = put(
        test,
        new_id(),
        NodeKind::Library,
        Some(site),
        site,
        "Documents",
    )
    .await;
    (site, library)
}

/// A new upload of `size` bytes by `owner` into `parent` in `site`.
fn upload<'a>(owner: UserId, site: NodeId, parent: NodeId, size: u64) -> NewUpload<'a> {
    NewUpload {
        id: Uuid::new_v4(),
        owner,
        site,
        target: UploadTarget::NewFile { parent },
        name: "file.pdf",
        size,
        sha256: [7; 32],
        base: None,
        on_conflict: None,
        external: false,
    }
}

/// A reservation over the quota is refused and changes nothing; within it, it counts as
/// reserved until it is either turned into usage or released.
#[tokio::test]
async fn reserve_commit_and_release() {
    let test = TestDb::create().await;
    let db = &test.db;
    let root = user(&test, "Root", true).await;
    let (site, _) = site_with_library(&test, root.id).await;

    // No quota: anything may be reserved.
    let big = db.reserve_bytes(site, 1_000_000).await.unwrap();
    db.release_reservation(big).await.unwrap();
    assert_eq!(db.site_usage(site).await.unwrap().reserved, 0);

    let previous = db.set_site_quota(root.id, site, Some(100)).await.unwrap();
    assert_eq!(previous, None);
    let first = db.reserve_bytes(site, 60).await.unwrap();
    assert!(matches!(
        db.reserve_bytes(site, 41).await,
        Err(QuotaError::Exceeded)
    ));
    let usage = db.site_usage(site).await.unwrap();
    assert_eq!(
        (usage.used, usage.reserved, usage.quota),
        (0, 60, Some(100))
    );

    db.commit_reservation(first).await.unwrap();
    let usage = db.site_usage(site).await.unwrap();
    assert_eq!((usage.used, usage.reserved), (60, 0));
    let second = db.reserve_bytes(site, 40).await.unwrap();
    db.release_reservation(second).await.unwrap();
    let usage = db.site_usage(site).await.unwrap();
    assert_eq!((usage.used, usage.reserved), (60, 0));
    assert!(!usage.near_quota());
    let third = db.reserve_bytes(site, 30).await.unwrap();
    db.commit_reservation(third).await.unwrap();
    assert!(db.site_usage(site).await.unwrap().near_quota());

    // Only an administrator sets a quota, and only on a site.
    let ann = user(&test, "Ann", false).await;
    assert!(db.set_site_quota(ann.id, site, None).await.is_err());
    let (_, library) = site_with_library(&test, root.id).await;
    assert!(db.set_site_quota(root.id, library, None).await.is_err());
    assert_eq!(
        db.set_site_quota(root.id, site, None).await.unwrap(),
        Some(100)
    );
    let events: i64 =
        sqlx::query_scalar("select count(*) from events where kind = 'quota.changed'")
            .fetch_one(db.test_pool())
            .await
            .unwrap();
    assert_eq!(events, 2);
}

/// Many reservations made at once never take a site over its quota together.
#[tokio::test]
async fn concurrent_reservations_respect_quota() {
    let test = TestDb::create().await;
    let db = &test.db;
    let root = user(&test, "Root", true).await;
    let (site, _) = site_with_library(&test, root.id).await;
    db.set_site_quota(root.id, site, Some(100)).await.unwrap();

    let tries: Vec<_> = (0..10)
        .map(|_| {
            let db = db.clone();
            tokio::spawn(async move { db.reserve_bytes(site, 30).await })
        })
        .collect();
    let mut granted = 0;
    for done in tries {
        match done.await.unwrap() {
            Ok(_) => granted += 1,
            Err(QuotaError::Exceeded) => {}
            Err(e) => panic!("{e}"),
        }
    }
    assert_eq!(granted, 3);
    assert_eq!(db.site_usage(site).await.unwrap().reserved, 90);
}

/// An upload session reserves its declared size when it starts, refused over the quota; ending
/// it releases the reservation; claiming it for its finish keeps the bytes reserved for the
/// commit, unless the site has no room for them now.
#[tokio::test]
async fn upload_sessions_reserve_their_size() {
    let test = TestDb::create().await;
    let db = &test.db;
    let root = user(&test, "Root", true).await;
    let (site, library) = site_with_library(&test, root.id).await;
    db.set_site_quota(root.id, site, Some(100)).await.unwrap();

    let a = db
        .start_upload(upload(root.id, site, library, 60))
        .await
        .unwrap();
    assert!(matches!(
        db.start_upload(upload(root.id, site, library, 41)).await,
        Err(UploadError::QuotaExceeded)
    ));
    assert_eq!(db.site_usage(site).await.unwrap().reserved, 60);
    assert!(db.end_upload(a.id).await.unwrap());
    assert_eq!(db.site_usage(site).await.unwrap().reserved, 0);
    assert!(!db.end_upload(a.id).await.unwrap());
    assert_eq!(db.site_usage(site).await.unwrap().reserved, 0);

    // Both start with no quota; the quota then leaves room for one of them only.
    db.set_site_quota(root.id, site, None).await.unwrap();
    let b = db
        .start_upload(upload(root.id, site, library, 60))
        .await
        .unwrap();
    let c = db
        .start_upload(upload(root.id, site, library, 60))
        .await
        .unwrap();
    db.set_site_quota(root.id, site, Some(100)).await.unwrap();
    let claimed = db.claim_upload(b.id).await.unwrap().unwrap();
    assert!(matches!(
        db.claim_upload(c.id).await,
        Err(QuotaError::Exceeded)
    ));
    // The refused session is still open, still reserved.
    assert!(db.upload_session(c.id).await.unwrap().is_some());
    assert!(db.upload_session(b.id).await.unwrap().is_none());
    assert_eq!(db.site_usage(site).await.unwrap().reserved, 120);
    db.commit_reservation(claimed).await.unwrap();
    let usage = db.site_usage(site).await.unwrap();
    assert_eq!((usage.used, usage.reserved), (60, 60));
    assert!(db.end_upload(c.id).await.unwrap());
    assert_eq!(db.site_usage(site).await.unwrap().reserved, 0);
    // A session already gone is claimed as nothing.
    assert!(db.claim_upload(c.id).await.unwrap().is_none());

    // At start, every reservation is dropped but the open sessions'.
    let d = db
        .start_upload(upload(root.id, site, library, 10))
        .await
        .unwrap();
    let _lost = db.reserve_bytes(site, 5).await.unwrap();
    db.reset_reservations().await.unwrap();
    assert_eq!(db.site_usage(site).await.unwrap().reserved, 10);
    db.end_upload(d.id).await.unwrap();
}

/// The rebuild counts every version of every item in the site, live and in the bin, and
/// nothing of purged items; purging gives the space back, and moving a file to another site
/// takes its versions' sizes with it.
#[tokio::test]
async fn usage_rebuild_counts_live_and_binned_versions() {
    let test = TestDb::create().await;
    let db = &test.db;
    let root = user(&test, "Root", true).await;
    let (site, library) = site_with_library(&test, root.id).await;
    let live = document_in(&test, root.id, library, site, "live.txt").await;
    let binned = document_in(&test, root.id, library, site, "binned.txt").await;
    let purged = document_in(&test, root.id, library, site, "purged.txt").await;
    let mut parent = None;
    for (n, size) in [(1u8, 10u64), (2, 20)] {
        let r = record(live, n, size);
        assert!(db.append_revision(parent.as_deref(), &r).await.unwrap());
        parent = Some(r.revision);
    }
    assert!(
        db.append_revision(None, &record(binned, 1, 300))
            .await
            .unwrap()
    );
    assert!(
        db.append_revision(None, &record(purged, 1, 4000))
            .await
            .unwrap()
    );

    db.flag_usage_rebuild().await.unwrap();
    assert!(db.usage_rebuild_flagged().await.unwrap());
    db.rebuild_usage(true).await.unwrap();
    assert!(!db.usage_rebuild_flagged().await.unwrap());
    assert_eq!(db.site_usage(site).await.unwrap().used, 4330);

    // In the bin it still counts; purged, it is given back.
    let node = |id: DocId, deleted: bool, purged: bool| NodeRow {
        id: id.into(),
        kind: NodeKind::Document,
        parent: Some(library),
        site,
        name: format!("{}.txt", id.0),
        deleted_at: deleted.then(OffsetDateTime::now_utc),
        deleted_by: None,
        created_by: None,
        created_at: OffsetDateTime::now_utc(),
        deleted_with: deleted.then_some(id.into()),
        bin_stage: (deleted && !purged).then_some(1),
        purged_at: purged.then(OffsetDateTime::now_utc),
    };
    db.apply_node(&node(binned, true, false), None)
        .await
        .unwrap();
    db.apply_node(&node(purged, true, false), None)
        .await
        .unwrap();
    assert_eq!(db.site_usage(site).await.unwrap().used, 4330);
    db.apply_node(&node(purged, true, true), None)
        .await
        .unwrap();
    assert_eq!(db.site_usage(site).await.unwrap().used, 330);
    // Applied again, as a reconcile from Git would, it gives nothing back twice.
    db.apply_node(&node(purged, true, true), None)
        .await
        .unwrap();
    assert_eq!(db.site_usage(site).await.unwrap().used, 330);

    // Moved to another site, a file's versions count there.
    let (other, other_library) = site_with_library(&test, root.id).await;
    let mut moved = node(live, false, false);
    moved.site = other;
    moved.parent = Some(other_library);
    db.apply_node(&moved, None).await.unwrap();
    assert_eq!(db.site_usage(site).await.unwrap().used, 300);
    assert_eq!(db.site_usage(other).await.unwrap().used, 30);

    db.rebuild_usage(true).await.unwrap();
    assert_eq!(db.site_usage(site).await.unwrap().used, 300);
    assert_eq!(db.site_usage(other).await.unwrap().used, 30);

    // A saved version adds its bytes to its file's site, whatever the quota.
    db.set_site_quota(root.id, other, Some(1)).await.unwrap();
    db.add_usage(live, 5).await.unwrap();
    assert_eq!(db.site_usage(other).await.unwrap().used, 35);
}

/// Every version below a connected library is outside site usage, including through a rebuild
/// and purge; disconnecting makes the surviving files ordinary and counts all their versions.
#[tokio::test]
async fn connected_library_versions_are_exempt_until_disconnect() {
    let test = TestDb::create().await;
    let db = &test.db;
    let root = user(&test, "Root", true).await;
    let (site, ordinary_library) = site_with_library(&test, root.id).await;
    let mirror = put(
        &test,
        new_id(),
        NodeKind::Library,
        Some(site),
        site,
        "Mirror",
    )
    .await;
    let already_binned = document_in(&test, root.id, mirror, site, "already-binned.txt").await;
    assert!(
        db.append_revision(None, &record(already_binned, 1, 25))
            .await
            .unwrap()
    );
    db.add_usage(already_binned, 25).await.unwrap();
    delete(&test, already_binned.into()).await;
    assert_eq!(db.site_usage(site).await.unwrap().used, 25);
    db.connect_source(&NewSource {
        library: mirror,
        kind: SourceKind::Github,
        owner: "octo".into(),
        repo: "docs".into(),
        branch: "main".into(),
        folder: String::new(),
        location: None,
        mode: SourceMode::ReadOnly,
        token: None,
        poll_seconds: 300,
        connected_by: root.id,
    })
    .await
    .unwrap();
    assert_eq!(db.site_usage(site).await.unwrap().used, 0);

    let ordinary = document_in(&test, root.id, ordinary_library, site, "ordinary.txt").await;
    let mirrored = document_in(&test, root.id, mirror, site, "mirrored.txt").await;
    let purged = document_in(&test, root.id, mirror, site, "purged.txt").await;
    for (doc, number, size) in [
        (ordinary, 1, 7),
        (mirrored, 1, 100),
        (mirrored, 2, 50),
        (purged, 1, 1_000),
    ] {
        let parent = (number > 1).then(|| record(doc, number - 1, 100).revision);
        assert!(
            db.append_revision(parent.as_deref(), &record(doc, number, size))
                .await
                .unwrap()
        );
        db.add_usage(doc, size).await.unwrap();
    }
    assert_eq!(db.site_usage(site).await.unwrap().used, 7);

    db.flag_usage_rebuild().await.unwrap();
    db.rebuild_usage(true).await.unwrap();
    assert_eq!(db.site_usage(site).await.unwrap().used, 7);

    let old = db.node(purged.into()).await.unwrap().unwrap();
    db.apply_node(
        &NodeRow {
            id: purged.into(),
            kind: old.kind,
            parent: old.parent,
            site: old.site,
            name: old.name,
            deleted_at: Some(OffsetDateTime::now_utc()),
            deleted_by: Some(root.id),
            created_by: old.created_by,
            created_at: old.created_at,
            deleted_with: Some(purged.into()),
            bin_stage: None,
            purged_at: Some(OffsetDateTime::now_utc()),
        },
        None,
    )
    .await
    .unwrap();
    assert_eq!(db.site_usage(site).await.unwrap().used, 7);

    db.set_site_quota(root.id, site, Some(1)).await.unwrap();
    db.disconnect_source(mirror).await.unwrap();
    assert_eq!(db.site_usage(site).await.unwrap().used, 182);
    assert!(matches!(
        db.reserve_bytes(site, 1).await,
        Err(QuotaError::Exceeded)
    ));
}

/// Versions recorded before sizes were kept are listed for the rebuild to read from Git.
#[tokio::test]
async fn sizes_missing_are_filled() {
    let test = TestDb::create().await;
    let db = &test.db;
    let root = user(&test, "Root", true).await;
    let (site, library) = site_with_library(&test, root.id).await;
    let doc = document_in(&test, root.id, library, site, "old.txt").await;
    let r = record(doc, 1, 0);
    db.append_revision(None, &r).await.unwrap();
    sqlx::query("update revisions set size = null")
        .execute(db.test_pool())
        .await
        .unwrap();
    assert_eq!(db.revision_sizes_missing().await.unwrap(), vec![doc]);
    db.set_revision_sizes(doc, &[(r.revision.clone(), 77)])
        .await
        .unwrap();
    assert!(db.revision_sizes_missing().await.unwrap().is_empty());
    db.rebuild_usage(true).await.unwrap();
    assert_eq!(db.site_usage(site).await.unwrap().used, 77);
}

/// Purging a file with a version whose size is not known yet cannot give back what it does
/// not know: it asks for a rebuild of the usage.
#[tokio::test]
async fn purge_with_unknown_sizes_flags_a_rebuild() {
    let test = TestDb::create().await;
    let db = &test.db;
    let root = user(&test, "Root", true).await;
    let (site, library) = site_with_library(&test, root.id).await;
    let doc = document_in(&test, root.id, library, site, "old.txt").await;
    db.append_revision(None, &record(doc, 1, 0)).await.unwrap();
    db.rebuild_usage(true).await.unwrap();
    sqlx::query("update revisions set size = null")
        .execute(db.test_pool())
        .await
        .unwrap();
    assert!(!db.usage_rebuild_flagged().await.unwrap());

    let now = OffsetDateTime::now_utc();
    db.apply_node(
        &NodeRow {
            id: doc.into(),
            kind: NodeKind::Document,
            parent: Some(library),
            site,
            name: "old.txt".into(),
            deleted_at: Some(now),
            deleted_by: None,
            created_by: None,
            created_at: now,
            deleted_with: Some(doc.into()),
            bin_stage: None,
            purged_at: Some(now),
        },
        None,
    )
    .await
    .unwrap();
    assert!(db.usage_rebuild_flagged().await.unwrap());
}
