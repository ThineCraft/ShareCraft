#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use collab_core::ids::{NodeId, UserId};
use common::{delete, document_in, new_id, put, site, user};
use records::test_support::TestDb;
use records::{
    AccessError, ExternalEntry, NewSource, NodeKind, SourceKind, SourceMode, external_tag,
};
use sqlx::Row;
use time::{Duration, OffsetDateTime};

struct Fixture {
    test: TestDb,
    user: UserId,
    site: NodeId,
    library: NodeId,
}

async fn fixture() -> Fixture {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", true).await;
    let site = site(&test, "Site", Some(ann.id)).await;
    let library = put(
        &test,
        new_id(),
        NodeKind::Library,
        Some(site),
        site,
        "Files",
    )
    .await;
    Fixture {
        test,
        user: ann.id,
        site,
        library,
    }
}

fn share_source(f: &Fixture, library: NodeId) -> NewSource {
    NewSource {
        library,
        kind: SourceKind::Share,
        owner: "ignored".into(),
        repo: "ignored".into(),
        branch: "ignored".into(),
        folder: "ignored".into(),
        location: Some(r"C:\Approved\Files".into()),
        mode: SourceMode::ReadOnly,
        token: None,
        poll_seconds: 300,
        connected_by: f.user,
    }
}

fn github_source(f: &Fixture, library: NodeId) -> NewSource {
    NewSource {
        library,
        kind: SourceKind::Github,
        owner: "thinecraft".into(),
        repo: "docs".into(),
        branch: "main".into(),
        folder: String::new(),
        location: None,
        mode: SourceMode::ReadOnly,
        token: None,
        poll_seconds: 300,
        connected_by: f.user,
    }
}

async fn connect_share(f: &Fixture) {
    f.test
        .db
        .connect_source(&share_source(f, f.library))
        .await
        .unwrap();
}

async fn library(f: &Fixture, name: &str) -> NodeId {
    put(
        &f.test,
        new_id(),
        NodeKind::Library,
        Some(f.site),
        f.site,
        name,
    )
    .await
}

fn at_location(f: &Fixture, library: NodeId, location: &str) -> NewSource {
    let mut source = share_source(f, library);
    source.location = Some(location.into());
    source
}

async fn external_document(f: &Fixture, name: &str) -> NodeId {
    document_in(&f.test, f.user, f.library, f.site, name)
        .await
        .into()
}

fn entry(node: NodeId, path: &str, size: u64, mtime_ns: i64) -> ExternalEntry {
    ExternalEntry {
        node,
        path: path.into(),
        repository_path: path.into(),
        size,
        mtime_ns,
    }
}

#[tokio::test]
async fn connect_a_share_then_read_back_location_and_kind() {
    let f = fixture().await;
    let new = share_source(&f, f.library);
    f.test.db.connect_source(&new).await.unwrap();

    let (config, _) = f.test.db.source_of(f.library).await.unwrap().unwrap();
    assert_eq!(config.kind, SourceKind::Share);
    assert_eq!(config.location.as_deref(), Some(r"C:\Approved\Files"));
    assert_eq!(config.mode, SourceMode::ReadOnly);
    assert_eq!((config.owner.as_str(), config.repo.as_str()), ("", ""));
    assert_eq!((config.branch.as_str(), config.folder.as_str()), ("", ""));
}

#[tokio::test]
async fn a_share_without_a_location_and_a_github_source_with_one_are_refused() {
    let f = fixture().await;
    let mut share = share_source(&f, f.library);
    share.location = None;
    assert_eq!(
        f.test.db.connect_source(&share).await,
        Err(AccessError::InvalidSetting)
    );

    let mut github = github_source(&f, f.library);
    github.location = Some(r"C:\NotGitHub".into());
    assert_eq!(
        f.test.db.connect_source(&github).await,
        Err(AccessError::InvalidSetting)
    );
}

#[tokio::test]
async fn two_libraries_cannot_connect_to_the_same_share_location() {
    let f = fixture().await;
    let second = library(&f, "Second").await;
    f.test
        .db
        .connect_source(&share_source(&f, f.library))
        .await
        .unwrap();

    assert_eq!(
        f.test.db.connect_source(&share_source(&f, second)).await,
        Err(AccessError::ShareLocationInUse)
    );
}

#[tokio::test]
async fn reconnecting_the_same_share_library_is_still_source_exists() {
    let f = fixture().await;
    let source = share_source(&f, f.library);
    f.test.db.connect_source(&source).await.unwrap();

    assert_eq!(
        f.test.db.connect_source(&source).await,
        Err(AccessError::SourceExists)
    );
}

#[tokio::test]
async fn windows_case_and_separator_variants_are_the_same_share_location() {
    let f = fixture().await;
    let second = library(&f, "Second").await;
    f.test
        .db
        .connect_source(&at_location(&f, f.library, r"C:\Approved\Files"))
        .await
        .unwrap();
    assert_eq!(
        f.test
            .db
            .connect_source(&at_location(&f, second, "c:/approved/files"))
            .await,
        Err(AccessError::ShareLocationInUse)
    );

    f.test.db.disconnect_source(f.library).await.unwrap();
    f.test
        .db
        .connect_source(&at_location(&f, f.library, r"\\SRV\Share\a"))
        .await
        .unwrap();
    assert_eq!(
        f.test
            .db
            .connect_source(&at_location(&f, second, r"\\srv\share\A"))
            .await,
        Err(AccessError::ShareLocationInUse)
    );
}

#[tokio::test]
async fn a_child_share_location_cannot_overlap_its_parent() {
    let f = fixture().await;
    let second = library(&f, "Second").await;
    f.test
        .db
        .connect_source(&at_location(&f, f.library, r"C:\Approved\Files"))
        .await
        .unwrap();

    assert_eq!(
        f.test
            .db
            .connect_source(&at_location(&f, second, r"C:\Approved\Files\Child"))
            .await,
        Err(AccessError::ShareLocationInUse)
    );
}

#[tokio::test]
async fn a_parent_share_location_cannot_overlap_its_child() {
    let f = fixture().await;
    let second = library(&f, "Second").await;
    f.test
        .db
        .connect_source(&at_location(&f, f.library, r"C:\Approved\Files\Child"))
        .await
        .unwrap();

    assert_eq!(
        f.test
            .db
            .connect_source(&at_location(&f, second, r"C:\Approved\Files"))
            .await,
        Err(AccessError::ShareLocationInUse)
    );
}

#[tokio::test]
async fn sibling_locations_and_different_unc_shares_are_allowed() {
    let f = fixture().await;
    let sibling = library(&f, "Sibling").await;
    let first_unc = library(&f, "First UNC").await;
    let second_unc = library(&f, "Second UNC").await;

    f.test
        .db
        .connect_source(&at_location(&f, f.library, r"C:\Approved\First"))
        .await
        .unwrap();
    f.test
        .db
        .connect_source(&at_location(&f, sibling, r"C:\Approved\Second"))
        .await
        .unwrap();
    f.test
        .db
        .connect_source(&at_location(&f, first_unc, r"\\srv\one\folder"))
        .await
        .unwrap();
    f.test
        .db
        .connect_source(&at_location(&f, second_unc, r"\\srv\two\folder"))
        .await
        .unwrap();
}

#[tokio::test]
async fn disconnecting_a_share_frees_its_location() {
    let f = fixture().await;
    let second = library(&f, "Second").await;
    f.test
        .db
        .connect_source(&share_source(&f, f.library))
        .await
        .unwrap();
    f.test.db.disconnect_source(f.library).await.unwrap();

    f.test
        .db
        .connect_source(&share_source(&f, second))
        .await
        .unwrap();
}

#[tokio::test]
async fn concurrent_connections_to_the_same_location_have_one_winner() {
    let f = fixture().await;
    let second = library(&f, "Second").await;
    let first_source = share_source(&f, f.library);
    let second_source = share_source(&f, second);

    let (first, second) = tokio::join!(
        f.test.db.connect_source(&first_source),
        f.test.db.connect_source(&second_source)
    );

    assert!(matches!(
        (&first, &second),
        (Ok(()), Err(AccessError::ShareLocationInUse))
            | (Err(AccessError::ShareLocationInUse), Ok(()))
    ));
}

#[tokio::test]
async fn record_external_item_round_trips_and_writes_the_tag() {
    let f = fixture().await;
    connect_share(&f).await;
    let node = external_document(&f, "Notes.txt").await;
    let e = entry(node, "Notes.txt", 17, 1_700_000_000_123_456_789);

    f.test.db.record_external_item(f.library, &e).await.unwrap();

    let item = f.test.db.external_item(node).await.unwrap().unwrap();
    assert_eq!(item.library, f.library);
    assert_eq!(item.node, node);
    assert_eq!(item.location, r"C:\Approved\Files");
    assert_eq!(item.repository_path, "Notes.txt");
    assert_eq!((item.size, item.mtime_ns), (e.size, e.mtime_ns));
    assert_eq!(item.tag, external_tag(e.size, e.mtime_ns));
    assert_eq!(item.sha256, None);
    assert_eq!(item.unavailable, None);
    assert_eq!(item.revision.len(), 40);
    assert!(
        item.revision
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    );
}

#[tokio::test]
async fn a_changed_tag_clears_the_hash_and_makes_a_new_revision_and_an_unchanged_one_keeps_both() {
    let f = fixture().await;
    connect_share(&f).await;
    let node = external_document(&f, "Notes.txt").await;
    let first = entry(node, "Notes.txt", 17, 1_700_000_000_000_000_000);
    f.test
        .db
        .record_external_item(f.library, &first)
        .await
        .unwrap();
    let first_tag = external_tag(first.size, first.mtime_ns);
    assert!(
        f.test
            .db
            .set_external_content(node, &first_tag, &[7; 32], OffsetDateTime::UNIX_EPOCH)
            .await
            .unwrap()
    );
    let with_content = f.test.db.external_item(node).await.unwrap().unwrap();

    f.test
        .db
        .record_external_item(f.library, &first)
        .await
        .unwrap();
    let unchanged = f.test.db.external_item(node).await.unwrap().unwrap();
    assert_eq!(unchanged.revision, with_content.revision);
    assert_eq!(unchanged.sha256, Some([7; 32]));

    let changed = entry(node, "Notes.txt", 18, first.mtime_ns + 1);
    f.test
        .db
        .record_external_item(f.library, &changed)
        .await
        .unwrap();
    let changed = f.test.db.external_item(node).await.unwrap().unwrap();
    assert_ne!(changed.revision, unchanged.revision);
    assert_eq!(changed.sha256, None);
    assert_eq!(changed.unavailable, None);
}

#[tokio::test]
async fn a_github_source_cannot_be_read_write() {
    let f = fixture().await;
    let mut source = github_source(&f, f.library);
    source.mode = SourceMode::ReadWrite;
    assert_eq!(
        f.test.db.connect_source(&source).await,
        Err(AccessError::InvalidSetting)
    );
}

#[tokio::test]
async fn set_external_content_is_refused_when_the_tag_moved() {
    let f = fixture().await;
    connect_share(&f).await;
    let node = external_document(&f, "Notes.txt").await;
    let e = entry(node, "Notes.txt", 17, 1000);
    f.test.db.record_external_item(f.library, &e).await.unwrap();

    assert!(
        !f.test
            .db
            .set_external_content(node, "x99-99", &[9; 32], OffsetDateTime::UNIX_EPOCH)
            .await
            .unwrap()
    );
    let item = f.test.db.external_item(node).await.unwrap().unwrap();
    assert_eq!(item.tag, external_tag(e.size, e.mtime_ns));
    assert_eq!(item.sha256, None);
}

#[tokio::test]
async fn set_external_stat_moves_the_tag_once() {
    let f = fixture().await;
    connect_share(&f).await;
    let node = external_document(&f, "Notes.txt").await;
    let e = entry(node, "Notes.txt", 17, 1000);
    f.test.db.record_external_item(f.library, &e).await.unwrap();
    let old_tag = external_tag(e.size, e.mtime_ns);
    let before = f.test.db.external_item(node).await.unwrap().unwrap();

    assert!(
        f.test
            .db
            .set_external_stat(node, &old_tag, 23, 2000)
            .await
            .unwrap()
    );
    assert!(
        !f.test
            .db
            .set_external_stat(node, &old_tag, 24, 3000)
            .await
            .unwrap()
    );
    let after = f.test.db.external_item(node).await.unwrap().unwrap();
    assert_eq!(after.tag, external_tag(23, 2000));
    assert_eq!((after.size, after.mtime_ns), (23, 2000));
    assert_ne!(after.revision, before.revision);
    assert_eq!(after.sha256, None);
}

#[tokio::test]
async fn external_item_is_none_for_a_github_item_a_folder_and_a_binned_file() {
    let f = fixture().await;
    connect_share(&f).await;
    let folder = put(
        &f.test,
        new_id(),
        NodeKind::Folder,
        Some(f.library),
        f.site,
        "Folder",
    )
    .await;
    f.test
        .db
        .record_external_item(f.library, &entry(folder, "Folder", 0, 1000))
        .await
        .unwrap();
    assert_eq!(f.test.db.external_item(folder).await.unwrap(), None);

    let binned = external_document(&f, "Binned.txt").await;
    f.test
        .db
        .record_external_item(f.library, &entry(binned, "Binned.txt", 1, 1000))
        .await
        .unwrap();
    delete(&f.test, binned).await;
    assert_eq!(f.test.db.external_item(binned).await.unwrap(), None);

    let github_library = put(
        &f.test,
        new_id(),
        NodeKind::Library,
        Some(f.site),
        f.site,
        "GitHub",
    )
    .await;
    f.test
        .db
        .connect_source(&github_source(&f, github_library))
        .await
        .unwrap();
    let github_file: NodeId = document_in(&f.test, f.user, github_library, f.site, "Git.md")
        .await
        .into();
    sqlx::query(
        "insert into source_items \
         (library_id, path, repository_path, node_id, blob_sha, size, revision, modified_at) \
         values ($1, 'Git.md', 'Git.md', $2, 'x1-1000', 1, $3, to_timestamp(0))",
    )
    .bind(github_library.0)
    .bind(github_file.0)
    .bind("0123456789abcdef0123456789abcdef01234567")
    .execute(f.test.db.test_pool())
    .await
    .unwrap();
    assert_eq!(f.test.db.external_item(github_file).await.unwrap(), None);
}

#[tokio::test]
async fn note_missing_waits_for_the_grace_then_answers_the_node() {
    let f = fixture().await;
    connect_share(&f).await;
    let node = external_document(&f, "Notes.txt").await;
    f.test
        .db
        .record_external_item(f.library, &entry(node, "Notes.txt", 1, 1000))
        .await
        .unwrap();
    let now = OffsetDateTime::UNIX_EPOCH + Duration::days(10);

    assert_eq!(
        f.test
            .db
            .note_missing(f.library, &[node], now, Duration::hours(1))
            .await
            .unwrap(),
        []
    );
    assert_eq!(
        f.test
            .db
            .note_missing(
                f.library,
                &[node],
                now + Duration::hours(1),
                Duration::hours(1),
            )
            .await
            .unwrap(),
        [node]
    );
}

#[tokio::test]
async fn clear_missing_except_resets_the_others() {
    let f = fixture().await;
    connect_share(&f).await;
    let keep = external_document(&f, "Keep.txt").await;
    let clear = external_document(&f, "Clear.txt").await;
    for (node, name) in [(keep, "Keep.txt"), (clear, "Clear.txt")] {
        f.test
            .db
            .record_external_item(f.library, &entry(node, name, 1, 1000))
            .await
            .unwrap();
    }
    let now = OffsetDateTime::UNIX_EPOCH + Duration::days(10);
    f.test
        .db
        .note_missing(f.library, &[keep, clear], now, Duration::ZERO)
        .await
        .unwrap();

    f.test
        .db
        .clear_missing_except(f.library, &[keep])
        .await
        .unwrap();
    let rows: Vec<(uuid::Uuid, Option<OffsetDateTime>)> = sqlx::query_as(
        "select node_id, missing_since from source_items where library_id = $1 order by path",
    )
    .bind(f.library.0)
    .fetch_all(f.test.db.test_pool())
    .await
    .unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().any(|(id, at)| *id == keep.0 && at.is_some()));
    assert!(rows.iter().any(|(id, at)| *id == clear.0 && at.is_none()));
}

#[tokio::test]
async fn next_check_at_delays_due_and_a_requeue_clears_it() {
    let f = fixture().await;
    connect_share(&f).await;
    let now = OffsetDateTime::UNIX_EPOCH + Duration::days(10);
    assert!(
        f.test
            .db
            .record_listing(
                f.library,
                0,
                Some(now + Duration::hours(1)),
                &serde_json::json!({"hidden": 2}),
            )
            .await
            .unwrap()
    );

    assert!(f.test.db.due_sources(now, 10).await.unwrap().is_empty());
    f.test.db.requeue_source(f.library, None).await.unwrap();
    assert_eq!(f.test.db.due_sources(now, 10).await.unwrap(), [f.library]);
    let next: Option<OffsetDateTime> =
        sqlx::query_scalar("select next_check_at from library_sources where library_id = $1")
            .bind(f.library.0)
            .fetch_one(f.test.db.test_pool())
            .await
            .unwrap();
    assert_eq!(next, None);
}

#[tokio::test]
async fn record_listing_is_refused_at_a_stale_generation() {
    let f = fixture().await;
    connect_share(&f).await;
    f.test.db.bump_source_generation(f.library).await.unwrap();
    let next = OffsetDateTime::UNIX_EPOCH + Duration::days(10);

    assert!(
        !f.test
            .db
            .record_listing(f.library, 0, Some(next), &serde_json::json!({"hidden": 2}))
            .await
            .unwrap()
    );
    let row = sqlx::query(
        "select next_check_at, hidden_detail from library_sources where library_id = $1",
    )
    .bind(f.library.0)
    .fetch_one(f.test.db.test_pool())
    .await
    .unwrap();
    assert_eq!(row.get::<Option<OffsetDateTime>, _>("next_check_at"), None);
    assert_eq!(
        row.get::<serde_json::Value, _>("hidden_detail"),
        serde_json::json!({})
    );
}

#[tokio::test]
async fn disconnect_removes_external_rows() {
    let f = fixture().await;
    connect_share(&f).await;
    let node = external_document(&f, "Notes.txt").await;
    f.test
        .db
        .record_external_item(f.library, &entry(node, "Notes.txt", 1, 1000))
        .await
        .unwrap();

    f.test.db.disconnect_source(f.library).await.unwrap();
    assert_eq!(f.test.db.external_item(node).await.unwrap(), None);
    let count: i64 = sqlx::query_scalar("select count(*) from source_items where node_id = $1")
        .bind(node.0)
        .fetch_one(f.test.db.test_pool())
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn migrating_a_database_with_github_rows_keeps_them() {
    let test = TestDb::create_at(Some(19)).await;
    let ann = user(&test, "MigrationAnn", true).await;
    let site = site(&test, "Migration Site", Some(ann.id)).await;
    let library = put(
        &test,
        new_id(),
        NodeKind::Library,
        Some(site),
        site,
        "Migration Library",
    )
    .await;
    let document: NodeId = document_in(&test, ann.id, library, site, "A.md")
        .await
        .into();
    sqlx::query(
        "insert into library_sources \
         (library_id, kind, owner, repo, branch, folder, poll_seconds, connected_by) \
         values ($1, 'github', 'owner', 'repo', 'main', '', 300, $2)",
    )
    .bind(library.0)
    .bind(ann.id.0)
    .execute(test.db.test_pool())
    .await
    .unwrap();
    sqlx::query(
        "insert into source_items \
         (library_id, path, repository_path, node_id, blob_sha, size) \
         values ($1, 'A.md', 'docs/A.md', $2, 'blob-one', 7)",
    )
    .bind(library.0)
    .bind(document.0)
    .execute(test.db.test_pool())
    .await
    .unwrap();

    test.db.migrate().await.unwrap();

    let (config, _) = test.db.source_of(library).await.unwrap().unwrap();
    assert_eq!(config.kind, SourceKind::Github);
    assert_eq!(config.location, None);
    assert_eq!(config.mode, SourceMode::ReadOnly);
    let items = test.db.source_items(library).await.unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].repository_path, "docs/A.md");
    assert_eq!(items[0].blob_sha.as_deref(), Some("blob-one"));
}
