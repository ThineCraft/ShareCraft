//! The recycle bin as PostgreSQL reads it: who sees which entries, access as if live, and
//! which entries are due to be purged.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use collab_core::ids::{NodeId, UserId};
use common::{Role, new_id, put, role_of, site, user};
use records::test_support::TestDb;
use records::{AccessError, BIN_DAYS, BinStage, NodeKind, NodeRow, Principal};
use time::{Duration, OffsetDateTime};

/// Marks `node` deleted by `by` at `at`, with the entry `with`, as the projection writes a
/// deletion from Git; `stage` for the entry itself.
async fn bin(
    test: &TestDb,
    node: NodeId,
    with: NodeId,
    by: Option<UserId>,
    at: OffsetDateTime,
    stage: Option<i16>,
) {
    let found = test.db.node(node).await.unwrap().unwrap();
    test.db
        .apply_node(
            &NodeRow {
                id: node,
                kind: found.kind,
                parent: found.parent,
                site: found.site,
                name: found.name,
                deleted_at: Some(at),
                deleted_by: by,
                created_by: found.created_by,
                created_at: found.created_at,
                deleted_with: Some(with),
                bin_stage: stage,
                purged_at: None,
            },
            None,
        )
        .await
        .unwrap();
}

/// The names of the entries of a stage of `site`'s bin that `user` sees.
async fn names(test: &TestDb, user: UserId, site: NodeId, stage: BinStage) -> Vec<String> {
    let (entries, next) = test.db.bin(user, site, stage, None, 100).await.unwrap();
    assert!(next.is_none());
    entries.into_iter().map(|e| e.name).collect()
}

struct Bin {
    site: NodeId,
    library: NodeId,
    /// A folder of the library with permissions of its own: only Bob (owner) there.
    private: NodeId,
    /// A file of the library, deleted by Ann.
    mine: NodeId,
    /// A file of the private folder, deleted by Bob.
    hidden: NodeId,
    admin: UserId,
    ann: UserId,
    bob: UserId,
    vic: UserId,
}

/// A site where Ann edits, Vic views and Bob owns a private folder; Ann has deleted a file of
/// the library and Bob one of his folder.
async fn setup(test: &TestDb) -> Bin {
    let admin = user(test, "Admin", true).await.id;
    let ann = user(test, "Ann", false).await.id;
    let bob = user(test, "Bob", false).await.id;
    let vic = user(test, "Vic", false).await.id;
    let site = site(test, "Site", Some(admin)).await;
    common::grant(test, site, ann, Role::Editor).await;
    common::grant(test, site, vic, Role::Viewer).await;
    let library = put(
        test,
        new_id(),
        NodeKind::Library,
        Some(site),
        site,
        "Documents",
    )
    .await;
    let private = put(
        test,
        new_id(),
        NodeKind::Folder,
        Some(library),
        site,
        "Private",
    )
    .await;
    test.db
        .set_node_access(admin, private, Principal::User(bob), Some("owner"))
        .await
        .unwrap();
    for other in [ann, vic] {
        test.db
            .set_node_access(admin, private, Principal::User(other), None)
            .await
            .unwrap();
    }
    common::without_groups(test, private).await;
    let mine = put(
        test,
        new_id(),
        NodeKind::Folder,
        Some(library),
        site,
        "Mine",
    )
    .await;
    let hidden = put(
        test,
        new_id(),
        NodeKind::Folder,
        Some(private),
        site,
        "Hidden",
    )
    .await;
    let now = OffsetDateTime::now_utc();
    bin(
        test,
        mine,
        mine,
        Some(ann),
        now - Duration::hours(2),
        Some(1),
    )
    .await;
    bin(
        test,
        hidden,
        hidden,
        Some(bob),
        now - Duration::hours(1),
        Some(1),
    )
    .await;
    Bin {
        site,
        library,
        private,
        mine,
        hidden,
        admin,
        ann,
        bob,
        vic,
    }
}

#[tokio::test]
async fn bin_lists_only_what_you_could_restore() {
    let test = TestDb::create().await;
    let b = setup(&test).await;
    let first = BinStage::First;

    // Ann deleted "Mine" and could restore it; "Hidden" was in a folder she cannot see.
    assert_eq!(names(&test, b.ann, b.site, first).await, ["Mine"]);
    // Bob sees only his own folder's entry; he has no access to the library.
    assert_eq!(names(&test, b.bob, b.site, first).await, ["Hidden"]);
    // A viewer could restore nothing.
    assert_eq!(
        names(&test, b.vic, b.site, first).await,
        Vec::<String>::new()
    );
    // An administrator sees both, newest first.
    assert_eq!(
        names(&test, b.admin, b.site, first).await,
        ["Hidden", "Mine"]
    );

    let (entries, _) = test.db.bin(b.ann, b.site, first, None, 10).await.unwrap();
    let entry = &entries[0];
    assert_eq!(entry.id, b.mine);
    assert_eq!(entry.path, ["Site", "Documents"]);
    assert_eq!(entry.deleted_by.as_ref().map(|u| u.id), Some(b.ann));
    assert_eq!(entry.stage, BinStage::First);
    assert_eq!(entry.purge_at, entry.deleted_at + Duration::days(BIN_DAYS));
    assert_eq!(entry.size, 0);

    // The second stage is for the site's managers.
    assert_eq!(
        test.db.bin(b.ann, b.site, BinStage::Second, None, 10).await,
        Err(AccessError::Forbidden)
    );
    assert_eq!(
        names(&test, b.admin, b.site, BinStage::Second).await,
        Vec::<String>::new()
    );
    let now = OffsetDateTime::now_utc();
    bin(
        &test,
        b.mine,
        b.mine,
        Some(b.ann),
        now - Duration::hours(2),
        Some(2),
    )
    .await;
    assert_eq!(
        names(&test, b.ann, b.site, first).await,
        Vec::<String>::new()
    );
    assert_eq!(
        names(&test, b.admin, b.site, BinStage::Second).await,
        ["Mine"]
    );

    // Someone with no access to the site learns nothing of its bin.
    let stranger = user(&test, "Stranger", false).await.id;
    assert_eq!(
        names(&test, stranger, b.site, first).await,
        Vec::<String>::new()
    );
    assert_eq!(
        test.db
            .bin(stranger, b.site, BinStage::Second, None, 10)
            .await,
        Err(AccessError::Forbidden)
    );
    assert_eq!(
        test.db.bin(b.ann, b.library, first, None, 10).await,
        Err(AccessError::NotFound),
        "only a site has a bin"
    );
    let _ = b.private;
}

#[tokio::test]
async fn bin_pages_continue_after_their_cursor() {
    let test = TestDb::create().await;
    let b = setup(&test).await;
    let (page, next) = test
        .db
        .bin(b.admin, b.site, BinStage::First, None, 1)
        .await
        .unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].id, b.hidden);
    let next = next.expect("a second page");
    let (page, next) = test
        .db
        .bin(b.admin, b.site, BinStage::First, Some(&next), 1)
        .await
        .unwrap();
    assert_eq!(page[0].id, b.mine);
    assert!(next.is_none());
}

#[tokio::test]
async fn access_as_if_live_ignores_the_deletion() {
    let test = TestDb::create().await;
    let b = setup(&test).await;
    // Live: the same as the one access decision.
    assert_eq!(
        test.db.access_as_if_live(b.ann, b.library).await.unwrap(),
        test.db.effective_access(b.ann, b.library).await.unwrap()
    );
    // Deleted: none for real, as it was for whoever had it.
    assert_eq!(test.db.effective_access(b.ann, b.mine).await.unwrap(), None);
    assert_eq!(
        role_of(test.db.access_as_if_live(b.ann, b.mine).await.unwrap()),
        Some(Role::Editor)
    );
    assert_eq!(
        role_of(test.db.access_as_if_live(b.bob, b.hidden).await.unwrap()),
        Some(Role::Owner)
    );
    assert_eq!(
        test.db.access_as_if_live(b.ann, b.hidden).await.unwrap(),
        None
    );

    // A file deleted with a folder above it, and the folder deleted itself.
    let inner = put(
        &test,
        new_id(),
        NodeKind::Folder,
        Some(b.private),
        b.site,
        "Inner",
    )
    .await;
    let now = OffsetDateTime::now_utc();
    bin(&test, b.private, b.private, Some(b.bob), now, Some(1)).await;
    bin(&test, inner, b.private, Some(b.bob), now, None).await;
    assert_eq!(
        role_of(test.db.access_as_if_live(b.bob, inner).await.unwrap()),
        Some(Role::Owner)
    );

    // A purged node has none; below a purged folder, a node is judged at its library's top.
    sqlx::query("update nodes set purged_at = now(), bin_stage = null where id = $1")
        .bind(b.private.0)
        .execute(test.db.test_pool())
        .await
        .unwrap();
    assert_eq!(
        test.db.access_as_if_live(b.bob, b.private).await.unwrap(),
        None
    );
    assert_eq!(
        test.db.access_as_if_live(b.bob, b.hidden).await.unwrap(),
        None,
        "Bob had access only in the purged folder"
    );
    assert_eq!(
        role_of(test.db.access_as_if_live(b.ann, b.hidden).await.unwrap()),
        Some(Role::Editor)
    );
}

#[tokio::test]
async fn due_for_purge_after_93_days() {
    let test = TestDb::create().await;
    let b = setup(&test).await;
    let now = OffsetDateTime::now_utc();
    assert!(test.db.due_for_purge(now).await.unwrap().is_empty());
    assert_eq!(
        test.db
            .due_for_purge(now + Duration::days(BIN_DAYS) - Duration::minutes(90))
            .await
            .unwrap(),
        [b.mine]
    );
    assert_eq!(
        test.db
            .due_for_purge(now + Duration::days(BIN_DAYS))
            .await
            .unwrap(),
        [b.mine, b.hidden]
    );
    assert_eq!(
        test.db.bin_state(b.mine).await.unwrap().map(|s| s.stage),
        Some(BinStage::First)
    );
    assert_eq!(test.db.bin_state(b.library).await.unwrap(), None);
}

#[tokio::test]
async fn deleted_before_the_bin_is_its_own_entry() {
    let test = TestDb::create().await;
    let b = setup(&test).await;
    let old = put(
        &test,
        new_id(),
        NodeKind::Folder,
        Some(b.library),
        b.site,
        "Old",
    )
    .await;
    // As plan 1 wrote a deletion: no bin state.
    common::delete(&test, old).await;
    let row = test
        .db
        .node_rows()
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.id == old)
        .unwrap();
    assert_eq!((row.deleted_with, row.bin_stage), (Some(old), Some(1)));
    assert!(
        names(&test, b.admin, b.site, BinStage::First)
            .await
            .contains(&"Old".to_owned())
    );
}
