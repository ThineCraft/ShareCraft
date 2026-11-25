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
    AccessError, ExternalEntry, NewPendingWrite, NewSource, NodeChange, NodeKind, NodeRow, S3Place,
    SealedRow, SourceKind, SourceMode, Tri, Versioning, object_tag, s3_location,
};
use revisions::BlobRef;
use serde_json::json;
use time::{Duration, OffsetDateTime};

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

/// A second library of the site, connected to its own bucket.
async fn other_library(f: &Fixture) -> NodeId {
    let other = put(
        &f.test,
        new_id(),
        NodeKind::Library,
        Some(f.site),
        f.site,
        "Other",
    )
    .await;
    connect(f, other, "other-bucket").await;
    other
}

/// A document node in the library with no tracking row.
async fn new_file(f: &Fixture, name: &str) -> NodeId {
    document_in(&f.test, f.user, f.library, f.site, name)
        .await
        .into()
}

/// A tracked file or folder node: `repository_path` is its identity, `path` what is shown.
async fn track(f: &Fixture, repository_path: &str, path: &str) -> NodeId {
    let node = new_file(f, &format!("item {}", new_id().0)).await;
    f.test
        .db
        .record_external_item(
            f.library,
            &ExternalEntry {
                node,
                path: path.into(),
                repository_path: repository_path.into(),
                size: 4,
                mtime_ns: 1_700_000_000_000_000_000,
                tag: Some(object_tag(4, 1_700_000_000, "\"etag\"", false)),
            },
        )
        .await
        .unwrap();
    node
}

/// Every tracked (repository path, shown path) of the library, by repository path.
async fn paths(f: &Fixture) -> Vec<(String, String)> {
    let mut all: Vec<(String, String)> = f
        .test
        .db
        .source_items(f.library)
        .await
        .unwrap()
        .into_iter()
        .map(|i| (i.repository_path, i.path))
        .collect();
    all.sort();
    all
}

fn pair(a: &str, b: &str) -> (String, String) {
    (a.to_owned(), b.to_owned())
}

/// Stages a pending write of `path` for a new node.
async fn stage_path(f: &Fixture, path: &str) {
    let node = new_file(f, &format!("w {}", new_id().0)).await;
    let blob = BlobRef::of(path.as_bytes());
    f.test.db.pin_cached(&blob, t0()).await.unwrap();
    f.test
        .db
        .stage_write(&NewPendingWrite {
            node,
            library: f.library,
            path,
            blob,
            base_tag: None,
            by: f.user,
            flush: false,
            now: t0(),
        })
        .await
        .unwrap();
}

fn purged_row(
    f: &Fixture,
    id: NodeId,
    kind: NodeKind,
    parent: Option<NodeId>,
    name: &str,
) -> NodeRow {
    let now = OffsetDateTime::now_utc();
    NodeRow {
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
    }
}

#[tokio::test]
async fn a_hold_round_trips_and_can_be_deferred_and_forgotten() {
    let f = fixture().await;
    let db = &f.test.db;
    let entry = new_file(&f, "Bin entry").await;
    assert_eq!(db.held(entry).await.unwrap(), None);
    assert!(!db.defer_held(entry, t0()).await.unwrap());
    assert!(!db.forget_held(entry).await.unwrap());

    db.hold_record(
        entry,
        f.library,
        ".sharecraft-bin/2026-11-24/e/docs/",
        7,
        t0(),
    )
    .await
    .unwrap();
    let held = db.held(entry).await.unwrap().unwrap();
    assert_eq!(held.entry, entry);
    assert_eq!(held.library, f.library);
    assert_eq!(held.root, ".sharecraft-bin/2026-11-24/e/docs/");
    assert_eq!(held.objects, 7);
    assert_eq!(held.held_at, t0());
    assert_eq!(held.retry_at, None);

    let later = t0() + Duration::hours(2);
    assert!(db.defer_held(entry, later).await.unwrap());
    assert_eq!(db.held(entry).await.unwrap().unwrap().retry_at, Some(later));

    // Recording it again replaces the record.
    db.hold_record(entry, f.library, "other/", 1, t0() + Duration::days(1))
        .await
        .unwrap();
    let held = db.held(entry).await.unwrap().unwrap();
    assert_eq!((held.root.as_str(), held.objects), ("other/", 1));
    assert_eq!(held.retry_at, None);

    assert!(db.forget_held(entry).await.unwrap());
    assert_eq!(db.held(entry).await.unwrap(), None);
    assert!(!db.forget_held(entry).await.unwrap());
}

#[tokio::test]
async fn held_entries_of_lists_only_that_library() {
    let f = fixture().await;
    let db = &f.test.db;
    let other = other_library(&f).await;
    let a = new_file(&f, "A").await;
    let b = new_file(&f, "B").await;
    let c = new_file(&f, "C").await;
    db.hold_record(a, f.library, "a/", 1, t0() + Duration::minutes(2))
        .await
        .unwrap();
    db.hold_record(b, f.library, "b/", 1, t0()).await.unwrap();
    db.hold_record(c, other, "c/", 1, t0()).await.unwrap();
    assert_eq!(db.held_entries_of(f.library).await.unwrap(), vec![b, a]);
    assert_eq!(db.held_entries_of(other).await.unwrap(), vec![c]);
    assert!(db.held_entries_of(new_id()).await.unwrap().is_empty());
}

#[tokio::test]
async fn disconnect_drops_held_rows_and_leaves_nothing_else() {
    let f = fixture().await;
    let db = &f.test.db;
    let other = other_library(&f).await;
    let mine = new_file(&f, "Mine").await;
    let theirs = new_file(&f, "Theirs").await;
    db.hold_record(mine, f.library, "m/", 3, t0())
        .await
        .unwrap();
    db.hold_record(theirs, other, "t/", 3, t0()).await.unwrap();

    // Held rows do not block a switch to read-only, nor a disconnect.
    assert!(
        db.set_source_mode(f.library, SourceMode::ReadOnly)
            .await
            .unwrap()
    );
    assert_eq!(db.disconnect_source(f.library).await.unwrap().len(), 1);
    assert_eq!(db.held(mine).await.unwrap(), None);
    assert!(db.held(theirs).await.unwrap().is_some());
    // The entry's node is untouched.
    let left: i64 = sqlx::query_scalar("select count(*) from nodes where id = $1")
        .bind(mine.0)
        .fetch_one(db.test_pool())
        .await
        .unwrap();
    assert_eq!(left, 1);
}

#[tokio::test]
async fn multipart_rows_round_trip_and_stale_ones_are_oldest_first() {
    let f = fixture().await;
    let db = &f.test.db;
    let node = new_file(&f, "big.bin").await;
    db.note_multipart(
        f.library,
        node,
        "big.bin",
        "up-new",
        t0() + Duration::hours(5),
    )
    .await
    .unwrap();
    db.note_multipart(f.library, node, "big.bin", "up-old", t0())
        .await
        .unwrap();
    db.note_multipart(
        f.library,
        node,
        "big.bin",
        "up-mid",
        t0() + Duration::hours(1),
    )
    .await
    .unwrap();

    let stale = db
        .stale_multiparts(t0() + Duration::hours(2), 10)
        .await
        .unwrap();
    assert_eq!(
        stale
            .iter()
            .map(|m| m.upload_id.as_str())
            .collect::<Vec<_>>(),
        ["up-old", "up-mid"]
    );
    let first = stale.first().unwrap();
    assert_eq!(first.library, f.library);
    assert_eq!(first.node, node);
    assert_eq!(first.key, "big.bin");
    assert_eq!(first.started_at, t0());
    // Strictly before.
    assert!(db.stale_multiparts(t0(), 10).await.unwrap().is_empty());
    // The limit.
    assert_eq!(
        db.stale_multiparts(t0() + Duration::days(1), 2)
            .await
            .unwrap()
            .len(),
        2
    );

    assert!(db.forget_multipart(f.library, "up-old").await.unwrap());
    assert!(!db.forget_multipart(f.library, "up-old").await.unwrap());
    assert!(!db.forget_multipart(new_id(), "up-mid").await.unwrap());
    assert_eq!(
        db.stale_multiparts(t0() + Duration::days(1), 10)
            .await
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn read_only_and_disconnect_are_refused_while_a_multipart_is_open() {
    let f = fixture().await;
    let db = &f.test.db;
    let node = new_file(&f, "big.bin").await;
    db.note_multipart(f.library, node, "big.bin", "up-1", t0())
        .await
        .unwrap();

    assert_eq!(
        db.set_source_mode(f.library, SourceMode::ReadOnly).await,
        Err(AccessError::WritesPending)
    );
    assert_eq!(
        db.disconnect_source(f.library).await,
        Err(AccessError::WritesPending)
    );
    assert_eq!(
        db.disconnect_source_as(f.user, f.library).await,
        Err(AccessError::WritesPending)
    );
    assert!(db.source_of(f.library).await.unwrap().is_some());

    assert!(db.forget_multipart(f.library, "up-1").await.unwrap());
    assert!(
        db.set_source_mode(f.library, SourceMode::ReadOnly)
            .await
            .unwrap()
    );
    assert_eq!(db.disconnect_source(f.library).await.unwrap().len(), 1);
}

#[tokio::test]
async fn moving_a_file_rewrites_both_paths() {
    let f = fixture().await;
    let db = &f.test.db;
    let node = track(&f, "a/x.txt", "a/x.txt").await;
    track(&f, "a/y.txt", "a/y.txt").await;
    assert_eq!(
        db.move_source_items(f.library, "a/x.txt", "b/z.txt")
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        paths(&f).await,
        [pair("a/y.txt", "a/y.txt"), pair("b/z.txt", "b/z.txt")]
    );
    let item = db.source_item_of(node).await.unwrap().unwrap();
    assert_eq!(item.repository_path, "b/z.txt");
    // Nothing there: nothing changes.
    assert_eq!(
        db.move_source_items(f.library, "nope", "x").await.unwrap(),
        0
    );
    // A file is not a prefix: a sibling that merely starts with its name stays.
    track(&f, "a/y.txt2", "a/y.txt2").await;
    assert_eq!(
        db.move_source_items(f.library, "a/y.txt", "c.txt")
            .await
            .unwrap(),
        1
    );
    assert!(paths(&f).await.contains(&pair("a/y.txt2", "a/y.txt2")));
}

#[tokio::test]
async fn moving_a_folder_rewrites_every_descendant_and_nothing_else() {
    let f = fixture().await;
    let db = &f.test.db;
    for p in [
        "folder/",
        "folder/a.txt",
        "folder/sub/",
        "folder/sub/b.txt",
        "folder2/c.txt",
        "folder_/d.txt",
        "fold%/e.txt",
        "other/folder/f.txt",
    ] {
        track(&f, p, p).await;
    }
    assert_eq!(
        db.move_source_items(f.library, "folder/", "moved/folder/")
            .await
            .unwrap(),
        4
    );
    let got: Vec<String> = paths(&f).await.into_iter().map(|(r, _)| r).collect();
    assert_eq!(
        got,
        [
            "fold%/e.txt",
            "folder2/c.txt",
            "folder_/d.txt",
            "moved/folder/",
            "moved/folder/a.txt",
            "moved/folder/sub/",
            "moved/folder/sub/b.txt",
            "other/folder/f.txt",
        ]
    );
    // Wildcard characters are literal.
    assert_eq!(
        db.move_source_items(f.library, "fold%/", "w/")
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        db.move_source_items(f.library, "folder_/", "u/")
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        db.move_source_items(f.library, "fold_r2/", "v/")
            .await
            .unwrap(),
        0
    );
    // A folder moved into itself shuffles inside its own subtree without tripping.
    assert_eq!(
        db.move_source_items(f.library, "moved/folder/", "moved/folder/sub/")
            .await
            .unwrap(),
        4
    );
}

#[tokio::test]
async fn moving_onto_a_taken_path_changes_nothing() {
    let f = fixture().await;
    let db = &f.test.db;
    track(&f, "a/", "a/").await;
    track(&f, "a/x.txt", "a/x.txt").await;
    track(&f, "a/y.txt", "a/y.txt").await;
    track(&f, "b/y.txt", "b/y.txt").await;
    let before = paths(&f).await;
    // A file of the folder would land on an existing file.
    assert!(db.move_source_items(f.library, "a/", "b/").await.is_err());
    assert_eq!(paths(&f).await, before);
    // A file onto a file.
    assert!(
        db.move_source_items(f.library, "a/y.txt", "b/y.txt")
            .await
            .is_err()
    );
    assert_eq!(paths(&f).await, before);

    // The shown path alone may be what is taken.
    track(&f, "c/real.txt", "d/shown.txt").await;
    let before = paths(&f).await;
    assert!(
        db.move_source_items(f.library, "a/x.txt", "d/shown.txt")
            .await
            .is_err()
    );
    assert_eq!(paths(&f).await, before);
}

#[tokio::test]
async fn a_case_collision_suffix_survives_a_move() {
    let f = fixture().await;
    let db = &f.test.db;
    track(&f, "dir/", "dir/").await;
    track(&f, "dir/Name.txt", "dir/Name.txt").await;
    track(&f, "dir/name.txt", "dir/name.txt (2)").await;
    // A shown path with nothing in common with the repository path stays as it is.
    track(&f, "dir/odd.txt", "elsewhere/odd.txt").await;
    assert_eq!(
        db.move_source_items(f.library, "dir/", "new/dir/")
            .await
            .unwrap(),
        4
    );
    assert_eq!(
        paths(&f).await,
        [
            pair("new/dir/", "new/dir/"),
            pair("new/dir/Name.txt", "new/dir/Name.txt"),
            pair("new/dir/name.txt", "new/dir/name.txt (2)"),
            pair("new/dir/odd.txt", "elsewhere/odd.txt"),
        ]
    );
    // A single file keeps its suffix too.
    assert_eq!(
        db.move_source_items(f.library, "new/dir/name.txt", "new/dir/other.txt")
            .await
            .unwrap(),
        1
    );
    assert!(
        paths(&f)
            .await
            .contains(&pair("new/dir/other.txt", "new/dir/other.txt (2)"))
    );
}

#[tokio::test]
async fn writes_below_counts_the_file_and_everything_under_a_folder() {
    let f = fixture().await;
    let db = &f.test.db;
    for p in [
        "docs/a.txt",
        "docs/sub/b.txt",
        "docs2/c.txt",
        "docs_/d.txt",
        "top.txt",
    ] {
        stage_path(&f, p).await;
    }
    assert_eq!(db.writes_below(f.library, "docs/").await.unwrap(), 2);
    assert_eq!(db.writes_below(f.library, "docs/a.txt").await.unwrap(), 1);
    assert_eq!(db.writes_below(f.library, "docs").await.unwrap(), 0);
    assert_eq!(db.writes_below(f.library, "doc_/").await.unwrap(), 0);
    assert_eq!(db.writes_below(f.library, "top.txt").await.unwrap(), 1);
    assert_eq!(db.writes_below(f.library, "top.tx").await.unwrap(), 0);
    assert_eq!(db.writes_below(new_id(), "docs/").await.unwrap(), 0);
}

#[tokio::test]
async fn purging_a_node_removes_its_held_and_multipart_rows() {
    let f = fixture().await;
    let db = &f.test.db;
    let doomed = new_file(&f, "Doomed").await;
    let kept = new_file(&f, "Kept").await;
    for (node, tag) in [(doomed, "d"), (kept, "k")] {
        db.hold_record(node, f.library, &format!("{tag}/"), 1, t0())
            .await
            .unwrap();
        db.note_multipart(f.library, node, tag, &format!("up-{tag}"), t0())
            .await
            .unwrap();
    }
    db.apply_nodes(NodeChange {
        rows: &[purged_row(
            &f,
            doomed,
            NodeKind::Document,
            Some(f.library),
            "Doomed",
        )],
        ..NodeChange::default()
    })
    .await
    .unwrap();
    assert_eq!(db.held(doomed).await.unwrap(), None);
    assert!(db.held(kept).await.unwrap().is_some());
    let open = db
        .stale_multiparts(t0() + Duration::days(1), 10)
        .await
        .unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open.first().unwrap().node, kept);

    // A purged site takes the rest.
    db.apply_nodes(NodeChange {
        rows: &[
            purged_row(&f, f.site, NodeKind::Site, None, "Site"),
            purged_row(&f, f.library, NodeKind::Library, Some(f.site), "Bucket"),
            purged_row(&f, kept, NodeKind::Document, Some(f.library), "Kept"),
        ],
        purge_site: Some(f.site),
        ..NodeChange::default()
    })
    .await
    .unwrap();
    let left: i64 = sqlx::query_scalar(
        "select (select count(*) from source_held) + (select count(*) from source_multipart)",
    )
    .fetch_one(db.test_pool())
    .await
    .unwrap();
    assert_eq!(left, 0);
}
