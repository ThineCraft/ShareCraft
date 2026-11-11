#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::collections::HashSet;

use collab_core::ids::{NodeId, UserId};
use common::{
    Tree, delete, document_in, gate, grant, new_id, node_events, put, site, tree, user,
    wait_blocked,
};
use records::test_support::TestDb;
use records::{AccessError, Cursor, NodeKind, Role};
use uuid::Uuid;

async fn role(test: &TestDb, user: UserId, node: NodeId) -> Option<Role> {
    test.db.effective_role(user, node).await.unwrap()
}

/// The (user, role) assignments the access page shows for `node`.
async fn shown(test: &TestDb, owner: UserId, node: NodeId) -> Vec<(UserId, Role)> {
    let access = test.db.access(owner, node).await.unwrap();
    access.grants.iter().map(|g| (g.user.id, g.role)).collect()
}

async fn own_rows(test: &TestDb, node: NodeId) -> i64 {
    sqlx::query_scalar("select count(*) from scope_access where node_id = $1")
        .bind(node.0)
        .fetch_one(test.db.test_pool())
        .await
        .unwrap()
}

async fn is_scope(test: &TestDb, node: NodeId) -> bool {
    test.db.node(node).await.unwrap().unwrap().unique_scope
}

fn all(t: &Tree) -> [NodeId; 5] {
    [t.site, t.library, t.a, t.b, t.file]
}

#[tokio::test]
async fn role_inherits_from_site() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    let vic = user(&test, "Vic", false).await;
    let sam = user(&test, "Sam", false).await;
    let t = tree(&test, ann.id).await;
    let result = test
        .db
        .set_node_access(ann.id, t.site, vic.id, Some(Role::Viewer))
        .await;
    assert_eq!(result, Ok((None, Some(Role::Viewer))));
    for node in all(&t) {
        assert_eq!(role(&test, ann.id, node).await, Some(Role::Owner));
        assert_eq!(role(&test, vic.id, node).await, Some(Role::Viewer));
        assert_eq!(role(&test, sam.id, node).await, None);
    }
    assert_eq!(
        test.db.role(vic.id, t.doc).await.unwrap(),
        Some(Role::Viewer)
    );
    assert_eq!(role(&test, ann.id, new_id()).await, None);
    assert_eq!(role(&test, UserId(Uuid::new_v4()), t.file).await, None);
    // Nothing below the site became a scope.
    for node in [t.library, t.a, t.b, t.file] {
        assert!(!is_scope(&test, node).await);
    }
}

#[tokio::test]
async fn break_copies_parent_assignments() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    let vic = user(&test, "Vic", false).await;
    let t = tree(&test, ann.id).await;
    grant(&test, t.site, vic.id, Role::Viewer).await;
    assert_eq!(test.db.break_inheritance(ann.id, t.a).await, Ok(true));
    assert_eq!(test.db.break_inheritance(ann.id, t.a).await, Ok(false));
    assert!(is_scope(&test, t.a).await);
    let access = test.db.access(ann.id, t.a).await.unwrap();
    assert!(!access.inherits);
    assert_eq!(access.source.id, t.a);
    assert_eq!(access.source.kind, NodeKind::Folder);
    assert_eq!(access.source.role, Some(Role::Owner));
    assert_eq!(
        shown(&test, ann.id, t.a).await,
        shown(&test, ann.id, t.site).await
    );
    assert_eq!(
        shown(&test, ann.id, t.a).await,
        vec![(ann.id, Role::Owner), (vic.id, Role::Viewer)]
    );
    let kinds: Vec<String> = node_events(&test, t.a)
        .await
        .into_iter()
        .map(|(kind, _)| kind)
        .collect();
    assert_eq!(kinds, vec!["inheritance.broken"]);
    // Below the new scope, everything inherits from it.
    let below = test.db.access(ann.id, t.file).await.unwrap();
    assert!(below.inherits);
    assert_eq!(below.source.id, t.a);
    // An editor cannot break.
    let ed = user(&test, "Ed", false).await;
    grant(&test, t.site, ed.id, Role::Editor).await;
    assert_eq!(
        test.db.break_inheritance(ed.id, t.library).await,
        Err(AccessError::Forbidden)
    );
}

#[tokio::test]
async fn break_then_change_does_not_touch_parent() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    let vic = user(&test, "Vic", false).await;
    let t = tree(&test, ann.id).await;
    grant(&test, t.site, vic.id, Role::Viewer).await;
    test.db.break_inheritance(ann.id, t.a).await.unwrap();
    let result = test
        .db
        .set_node_access(ann.id, t.a, vic.id, Some(Role::Editor))
        .await;
    assert_eq!(result, Ok((Some(Role::Viewer), Some(Role::Editor))));
    assert_eq!(
        shown(&test, ann.id, t.site).await,
        vec![(ann.id, Role::Owner), (vic.id, Role::Viewer)]
    );
    assert_eq!(role(&test, vic.id, t.library).await, Some(Role::Viewer));
    for node in [t.a, t.b, t.file] {
        assert_eq!(role(&test, vic.id, node).await, Some(Role::Editor));
    }
}

#[tokio::test]
async fn restore_returns_to_parent_roles() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    let vic = user(&test, "Vic", false).await;
    let wes = user(&test, "Wes", false).await;
    let t = tree(&test, ann.id).await;
    grant(&test, t.site, vic.id, Role::Viewer).await;
    test.db.break_inheritance(ann.id, t.a).await.unwrap();
    test.db
        .set_node_access(ann.id, t.a, vic.id, Some(Role::Editor))
        .await
        .unwrap();
    test.db
        .set_node_access(ann.id, t.a, wes.id, Some(Role::Viewer))
        .await
        .unwrap();
    assert_eq!(role(&test, wes.id, t.file).await, Some(Role::Viewer));
    // Someone who owns the folder but not the site cannot restore it.
    test.db
        .set_node_access(ann.id, t.a, wes.id, Some(Role::Owner))
        .await
        .unwrap();
    assert_eq!(
        test.db.restore_inheritance(wes.id, t.a).await,
        Err(AccessError::Forbidden)
    );
    assert_eq!(test.db.restore_inheritance(ann.id, t.a).await, Ok(true));
    assert_eq!(test.db.restore_inheritance(ann.id, t.a).await, Ok(false));
    assert!(!is_scope(&test, t.a).await);
    assert_eq!(own_rows(&test, t.a).await, 0);
    for node in [t.a, t.b, t.file] {
        assert_eq!(role(&test, vic.id, node).await, Some(Role::Viewer));
        assert_eq!(role(&test, wes.id, node).await, None);
    }
    let access = test.db.access(ann.id, t.a).await.unwrap();
    assert!(access.inherits);
    assert_eq!(access.source.id, t.site);
    let kinds: Vec<String> = node_events(&test, t.a)
        .await
        .into_iter()
        .map(|(kind, _)| kind)
        .collect();
    assert_eq!(
        kinds.last().map(String::as_str),
        Some("inheritance.restored")
    );
}

#[tokio::test]
async fn restore_on_site_is_refused() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    let sam = user(&test, "Sam", false).await;
    let t = tree(&test, ann.id).await;
    assert_eq!(
        test.db.restore_inheritance(ann.id, t.site).await,
        Err(AccessError::IsSite)
    );
    assert_eq!(
        test.db.restore_inheritance(sam.id, t.site).await,
        Err(AccessError::NotFound)
    );
    assert_eq!(test.db.break_inheritance(ann.id, t.site).await, Ok(false));
    assert!(is_scope(&test, t.site).await);
    assert_eq!(
        shown(&test, ann.id, t.site).await,
        vec![(ann.id, Role::Owner)]
    );
}

#[tokio::test]
async fn last_owner_kept_per_scope() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    let pat = user(&test, "Pat", false).await;
    let t = tree(&test, ann.id).await;
    assert_eq!(
        test.db.set_node_access(ann.id, t.site, ann.id, None).await,
        Err(AccessError::LastOwner)
    );
    test.db.break_inheritance(ann.id, t.a).await.unwrap();
    assert_eq!(
        test.db
            .set_node_access(ann.id, t.a, ann.id, Some(Role::Editor))
            .await,
        Err(AccessError::LastOwner)
    );
    // A second owner of the site is no owner of the folder's own scope.
    test.db
        .set_node_access(ann.id, t.site, pat.id, Some(Role::Owner))
        .await
        .unwrap();
    assert_eq!(
        test.db.set_node_access(ann.id, t.a, ann.id, None).await,
        Err(AccessError::LastOwner)
    );
    test.db
        .set_node_access(ann.id, t.a, pat.id, Some(Role::Owner))
        .await
        .unwrap();
    assert_eq!(
        test.db
            .set_node_access(ann.id, t.a, ann.id, Some(Role::Editor))
            .await,
        Ok((Some(Role::Owner), Some(Role::Editor)))
    );
    // The site keeps both owners.
    assert_eq!(
        shown(&test, pat.id, t.site).await,
        vec![(ann.id, Role::Owner), (pat.id, Role::Owner)]
    );
    // Removing the only owner of an inheriting node's copy is refused, and nothing is broken.
    assert_eq!(
        test.db.set_node_access(pat.id, t.b, pat.id, None).await,
        Err(AccessError::LastOwner)
    );
    assert!(!is_scope(&test, t.b).await);
}

#[tokio::test]
async fn set_access_on_inheriting_node_breaks_first() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    let vic = user(&test, "Vic", false).await;
    let t = tree(&test, ann.id).await;
    grant(&test, t.site, vic.id, Role::Viewer).await;
    // The same role again is no change and breaks nothing.
    assert_eq!(
        test.db
            .set_node_access(ann.id, t.b, vic.id, Some(Role::Viewer))
            .await,
        Ok((Some(Role::Viewer), Some(Role::Viewer)))
    );
    assert!(!is_scope(&test, t.b).await);
    let result = test
        .db
        .set_node_access(ann.id, t.b, vic.id, Some(Role::Editor))
        .await;
    assert_eq!(result, Ok((Some(Role::Viewer), Some(Role::Editor))));
    assert!(is_scope(&test, t.b).await);
    assert_eq!(
        shown(&test, ann.id, t.b).await,
        vec![(ann.id, Role::Owner), (vic.id, Role::Editor)]
    );
    assert_eq!(
        shown(&test, ann.id, t.site).await,
        vec![(ann.id, Role::Owner), (vic.id, Role::Viewer)]
    );
    let kinds: Vec<String> = node_events(&test, t.b)
        .await
        .into_iter()
        .map(|(kind, _)| kind)
        .collect();
    assert_eq!(kinds, vec!["inheritance.broken", "access.changed"]);
    // On a file, the alias by document id does the same.
    let result = test
        .db
        .set_access(ann.id, t.doc, vic.id, Some(Role::Owner))
        .await;
    assert_eq!(result, Ok((Some(Role::Editor), Some(Role::Owner))));
    assert!(is_scope(&test, t.file).await);
    let events = test.db.events_for(t.doc).await.unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(
        kinds,
        vec!["document.created", "inheritance.broken", "access.changed"]
    );
}

#[tokio::test]
async fn nested_breaks_nearest_wins() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    let vic = user(&test, "Vic", false).await;
    let t = tree(&test, ann.id).await;
    grant(&test, t.site, vic.id, Role::Viewer).await;
    test.db
        .set_node_access(ann.id, t.a, vic.id, Some(Role::Editor))
        .await
        .unwrap();
    test.db
        .set_node_access(ann.id, t.file, vic.id, Some(Role::Owner))
        .await
        .unwrap();
    assert_eq!(role(&test, vic.id, t.library).await, Some(Role::Viewer));
    assert_eq!(role(&test, vic.id, t.a).await, Some(Role::Editor));
    assert_eq!(role(&test, vic.id, t.b).await, Some(Role::Editor));
    assert_eq!(role(&test, vic.id, t.file).await, Some(Role::Owner));
    test.db
        .set_node_access(ann.id, t.b, vic.id, None)
        .await
        .unwrap();
    assert_eq!(role(&test, vic.id, t.b).await, None);
    assert_eq!(role(&test, vic.id, t.a).await, Some(Role::Editor));
    assert_eq!(role(&test, vic.id, t.file).await, Some(Role::Owner));
}

#[tokio::test]
async fn administrator_owns_everything() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    let root = user(&test, "Root", true).await;
    let t = tree(&test, ann.id).await;
    for node in all(&t) {
        assert_eq!(role(&test, root.id, node).await, Some(Role::Owner));
    }
    let sites = test.db.sites_for(root.id).await.unwrap();
    assert_eq!(sites.len(), 1);
    assert_eq!((sites[0].0.id, sites[0].1), (t.site, Some(Role::Owner)));
    let (children, next) = test
        .db
        .children(root.id, t.library, None, 100)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(next, None);
    assert_eq!(children.len(), 1);
    assert_eq!(
        (children[0].node.id, children[0].role),
        (t.a, Some(Role::Owner))
    );
    // The administrator manages access without holding an assignment.
    assert_eq!(test.db.break_inheritance(root.id, t.b).await, Ok(true));
    assert_eq!(
        shown(&test, root.id, t.b).await,
        vec![(ann.id, Role::Owner)]
    );
    assert_eq!(test.db.restore_inheritance(root.id, t.b).await, Ok(true));
    // Deleted is deleted, even for an administrator.
    delete(&test, t.b).await;
    assert_eq!(role(&test, root.id, t.file).await, None);
}

#[tokio::test]
async fn disabled_account_has_nothing() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    let vic = user(&test, "Vic", false).await;
    let root = user(&test, "Root", true).await;
    let zed = user(&test, "Zed", true).await;
    let t = tree(&test, ann.id).await;
    grant(&test, t.site, vic.id, Role::Viewer).await;
    test.db.set_disabled(root.id, vic.id, true).await.unwrap();
    test.db.set_disabled(root.id, zed.id, true).await.unwrap();
    for who in [vic.id, zed.id] {
        for node in all(&t) {
            assert_eq!(role(&test, who, node).await, None);
        }
        assert!(test.db.sites_for(who).await.unwrap().is_empty());
        assert_eq!(test.db.children(who, t.site, None, 10).await.unwrap(), None);
        assert_eq!(test.db.path(who, t.file).await.unwrap(), None);
    }
    test.db.set_disabled(root.id, ann.id, true).await.unwrap();
    assert_eq!(
        test.db.access(ann.id, t.a).await.err(),
        Some(AccessError::NotFound)
    );
    assert_eq!(
        test.db.break_inheritance(ann.id, t.a).await,
        Err(AccessError::NotFound)
    );
    test.db.set_disabled(root.id, vic.id, false).await.unwrap();
    assert_eq!(role(&test, vic.id, t.file).await, Some(Role::Viewer));
}

#[tokio::test]
async fn deleted_ancestor_hides_everything_below() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    let wes = user(&test, "Wes", false).await;
    let t = tree(&test, ann.id).await;
    test.db
        .set_node_access(ann.id, t.file, wes.id, Some(Role::Editor))
        .await
        .unwrap();
    assert!(test.db.path(wes.id, t.library).await.unwrap().is_some());
    delete(&test, t.a).await;
    for node in [t.a, t.b, t.file] {
        assert_eq!(role(&test, ann.id, node).await, None);
        assert_eq!(role(&test, wes.id, node).await, None);
        assert_eq!(test.db.path(ann.id, node).await.unwrap(), None);
    }
    assert_eq!(role(&test, ann.id, t.library).await, Some(Role::Owner));
    assert_eq!(test.db.children(ann.id, t.a, None, 10).await.unwrap(), None);
    let (listed, _) = test
        .db
        .children(ann.id, t.library, None, 10)
        .await
        .unwrap()
        .unwrap();
    assert!(listed.is_empty());
    assert_eq!(
        test.db.access(ann.id, t.file).await.err(),
        Some(AccessError::NotFound)
    );
    // A grant inside the deleted folder gives no limited access to its parents.
    assert!(test.db.sites_for(wes.id).await.unwrap().is_empty());
    assert_eq!(test.db.path(wes.id, t.library).await.unwrap(), None);
    assert_eq!(
        test.db.children(wes.id, t.site, None, 10).await.unwrap(),
        None
    );
    assert_eq!(
        test.db.children(wes.id, t.library, None, 10).await.unwrap(),
        None
    );
}

#[tokio::test]
async fn removed_above_while_breaking_below() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    let xan = user(&test, "Xan", false).await;
    let t = tree(&test, ann.id).await;
    test.db.break_inheritance(ann.id, t.a).await.unwrap();
    for round in 0..5 {
        test.db
            .set_node_access(ann.id, t.a, xan.id, Some(Role::Owner))
            .await
            .unwrap();
        assert_eq!(role(&test, xan.id, t.file).await, Some(Role::Owner));
        // Hold folder A's row while both calls start: the removal queues first, then the
        // break below A, which was allowed when it started. They then run strictly in turn.
        let held = gate(&test, t.a).await;
        let first = test.db.clone();
        let removal =
            tokio::spawn(async move { first.set_node_access(ann.id, t.a, xan.id, None).await });
        wait_blocked(&test, held.pid, 1).await;
        let second = test.db.clone();
        let breaking = tokio::spawn(async move { second.break_inheritance(xan.id, t.file).await });
        wait_blocked(&test, held.pid, 2).await;
        held.open().await;
        let removed = removal.await.unwrap();
        let broken = breaking.await.unwrap();
        assert_eq!(removed, Ok((Some(Role::Owner), None)), "round {round}");
        assert_eq!(broken, Err(AccessError::NotFound), "round {round}");
        let on_file: i64 = sqlx::query_scalar(
            "select count(*) from scope_access where node_id = $1 and user_id = $2",
        )
        .bind(t.file.0)
        .bind(xan.id.0)
        .fetch_one(test.db.test_pool())
        .await
        .unwrap();
        assert_eq!(on_file, 0, "round {round}: no grant on the file");
        assert!(!is_scope(&test, t.file).await, "round {round}");
        assert_eq!(role(&test, xan.id, t.file).await, None, "round {round}");
    }
}

#[tokio::test]
async fn limited_access_shows_only_the_path() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    let wes = user(&test, "Wes", false).await;
    let t = tree(&test, ann.id).await;
    // Siblings at every level that Wes must never see.
    let a2 = put(
        &test,
        new_id(),
        NodeKind::Folder,
        Some(t.library),
        t.site,
        "A2",
    )
    .await;
    let b2 = put(&test, new_id(), NodeKind::Folder, Some(t.a), t.site, "B2").await;
    let other = document_in(&test, ann.id, t.b, t.site, "Other").await;
    let library2 = put(
        &test,
        new_id(),
        NodeKind::Library,
        Some(t.site),
        t.site,
        "Lib2",
    )
    .await;
    let site2 = site(&test, "Elsewhere", Some(ann.id)).await;
    test.db
        .set_node_access(ann.id, t.file, wes.id, Some(Role::Viewer))
        .await
        .unwrap();

    assert_eq!(
        test.db.role(wes.id, t.doc).await.unwrap(),
        Some(Role::Viewer)
    );
    let sites = test.db.sites_for(wes.id).await.unwrap();
    assert_eq!(sites.len(), 1);
    assert_eq!((sites[0].0.id, sites[0].1), (t.site, None));

    let ids = |listed: &[records::Listed]| -> Vec<(NodeId, Option<Role>)> {
        listed.iter().map(|l| (l.node.id, l.role)).collect()
    };
    let (listed, next) = test
        .db
        .children(wes.id, t.site, None, 100)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((ids(&listed), next), (vec![(t.library, None)], None));
    let (listed, _) = test
        .db
        .children(wes.id, t.library, None, 100)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ids(&listed), vec![(t.a, None)]);
    let (listed, _) = test
        .db
        .children(wes.id, t.a, None, 100)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ids(&listed), vec![(t.b, None)]);
    let (listed, _) = test
        .db
        .children(wes.id, t.b, None, 100)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ids(&listed), vec![(t.file, Some(Role::Viewer))]);
    assert_eq!(listed[0].editor, Some(records::Editor::Text));

    let path = test.db.path(wes.id, t.file).await.unwrap().unwrap();
    let crumbs: Vec<(NodeId, NodeKind, &str, Option<Role>)> = path
        .iter()
        .map(|c| (c.id, c.kind, c.name.as_str(), c.role))
        .collect();
    assert_eq!(
        crumbs,
        vec![
            (t.site, NodeKind::Site, "Site", None),
            (t.library, NodeKind::Library, "Documents", None),
            (t.a, NodeKind::Folder, "A", None),
            (t.b, NodeKind::Folder, "B", None),
            (t.file, NodeKind::Document, "Notes", Some(Role::Viewer)),
        ]
    );
    for hidden in [a2, b2, other.into(), library2, site2] {
        assert_eq!(test.db.path(wes.id, hidden).await.unwrap(), None);
        assert_eq!(
            test.db.children(wes.id, hidden, None, 100).await.unwrap(),
            None
        );
        assert_eq!(role(&test, wes.id, hidden).await, None);
    }
    for limited in [t.site, t.library, t.a, t.b] {
        assert_eq!(role(&test, wes.id, limited).await, None);
        assert_eq!(
            test.db.access(wes.id, limited).await.err(),
            Some(AccessError::NotFound)
        );
    }
    // The owner sees everything, with roles.
    let (listed, _) = test
        .db
        .children(ann.id, t.b, None, 100)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        ids(&listed),
        vec![
            (t.file, Some(Role::Owner)),
            (other.into(), Some(Role::Owner))
        ]
    );
}

#[tokio::test]
async fn children_page_in_order() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    let t = tree(&test, ann.id).await;
    let parent = put(
        &test,
        new_id(),
        NodeKind::Folder,
        Some(t.library),
        t.site,
        "Many",
    )
    .await;
    let mut expected = Vec::new();
    for i in 0..250u32 {
        // Upper and lower case alternate, so only an order that ignores case keeps the numbers
        // in sequence.
        let name = format!("{}{i:03}", if i % 2 == 0 { "Item" } else { "item" });
        let (kind, rank) = if i % 3 == 0 {
            (NodeKind::Folder, 0)
        } else {
            (NodeKind::Document, 1)
        };
        let id = put(&test, new_id(), kind, Some(parent), t.site, &name).await;
        expected.push((rank, name.to_lowercase(), id));
    }
    expected.sort();
    let expected: Vec<NodeId> = expected.into_iter().map(|(_, _, id)| id).collect();

    let mut seen = Vec::new();
    let mut sizes = Vec::new();
    let mut after: Option<Cursor> = None;
    loop {
        let (page, next) = test
            .db
            .children(ann.id, parent, after.as_ref(), 100)
            .await
            .unwrap()
            .unwrap();
        sizes.push(page.len());
        seen.extend(page.iter().map(|l| l.node.id));
        match next {
            Some(cursor) => after = Some(cursor),
            None => break,
        }
    }
    assert_eq!(sizes, vec![100, 100, 50]);
    assert_eq!(seen.iter().collect::<HashSet<_>>().len(), 250, "no repeats");
    assert_eq!(seen, expected);
}

#[tokio::test]
async fn break_cost_does_not_grow_with_descendants() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    let vic = user(&test, "Vic", false).await;
    let wes = user(&test, "Wes", false).await;
    let t = tree(&test, ann.id).await;
    grant(&test, t.site, vic.id, Role::Viewer).await;
    grant(&test, t.site, wes.id, Role::Editor).await;
    let empty = put(
        &test,
        new_id(),
        NodeKind::Library,
        Some(t.site),
        t.site,
        "Empty",
    )
    .await;
    let full = put(
        &test,
        new_id(),
        NodeKind::Library,
        Some(t.site),
        t.site,
        "Full",
    )
    .await;
    sqlx::query(
        "insert into nodes (id, kind, parent_id, site_id, name, created_at) \
         select gen_random_uuid(), 'document', $1, $2, 'file' || g, now() \
         from generate_series(1, 2000) g",
    )
    .bind(full.0)
    .bind(t.site.0)
    .execute(test.db.test_pool())
    .await
    .unwrap();
    // Count every row written to every table, by table.
    for statement in [
        "create table writes (tbl text not null)",
        "create function count_write() returns trigger language plpgsql as \
         $$ begin insert into writes values (TG_TABLE_NAME); return null; end $$",
    ] {
        sqlx::query(statement)
            .execute(test.db.test_pool())
            .await
            .unwrap();
    }
    for table in [
        "nodes",
        "scope_access",
        "events",
        "documents",
        "document_access",
    ] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "create trigger count_{table} after insert or update or delete on {table} \
             for each row execute function count_write()"
        )))
        .execute(test.db.test_pool())
        .await
        .unwrap();
    }
    let mut written = Vec::new();
    for library in [empty, full] {
        sqlx::query("delete from writes")
            .execute(test.db.test_pool())
            .await
            .unwrap();
        assert_eq!(test.db.break_inheritance(ann.id, library).await, Ok(true));
        let rows: Vec<(String, i64)> =
            sqlx::query_as("select tbl, count(*) from writes group by tbl order by tbl")
                .fetch_all(test.db.test_pool())
                .await
                .unwrap();
        written.push(rows);
    }
    let copied = 3;
    assert_eq!(
        written[0],
        vec![
            ("events".to_string(), 1),
            ("nodes".to_string(), 1),
            ("scope_access".to_string(), copied),
        ]
    );
    assert_eq!(written[1], written[0], "the same rows for 2,000 files");
    for library in [empty, full] {
        assert_eq!(own_rows(&test, library).await, copied);
    }
}

#[tokio::test]
async fn cycle_in_the_data_ends_every_walk() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    let t = tree(&test, ann.id).await;
    // Folder A made a child of its own child B: nothing there leads up to a site any more.
    sqlx::query("update nodes set parent_id = $2 where id = $1")
        .bind(t.a.0)
        .bind(t.b.0)
        .execute(test.db.test_pool())
        .await
        .unwrap();
    assert_eq!(role(&test, ann.id, t.file).await, None);
    assert_eq!(test.db.path(ann.id, t.b).await.unwrap(), None);
    assert!(test.db.is_at_or_below(t.file, t.a).await.unwrap());
    assert!(!test.db.is_at_or_below(t.file, t.site).await.unwrap());
    assert!(test.db.scope_users(t.file).await.unwrap().is_empty());
    assert_eq!(test.db.sites_for(ann.id).await.unwrap().len(), 1);
}

/// A rename of `t.file` to `name` by `actor`, as the server mirrors one after its commit.
async fn rename_file(
    db: records::Db,
    actor: UserId,
    t_file: NodeId,
    name: &str,
) -> Result<(), AccessError> {
    let node = db.node(t_file).await.unwrap().unwrap();
    let row = records::NodeRow {
        id: node.id,
        kind: node.kind,
        parent: node.parent,
        site: node.site,
        name: name.to_owned(),
        deleted_at: None,
        deleted_by: None,
        created_by: node.created_by,
        created_at: node.created_at,
    };
    db.apply_nodes(records::NodeChange {
        rows: std::slice::from_ref(&row),
        actor: Some(actor),
        needs: &[(t_file, Role::Editor)],
        ..records::NodeChange::default()
    })
    .await
}

#[tokio::test]
async fn revoke_above_and_tree_change_take_turns() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    let xan = user(&test, "Xan", false).await;
    let t = tree(&test, ann.id).await;
    // Xan's only role on the file comes from folder A, two levels above it.
    test.db
        .set_node_access(ann.id, t.a, xan.id, Some(Role::Editor))
        .await
        .unwrap();
    assert_eq!(role(&test, xan.id, t.file).await, Some(Role::Editor));

    // The revoke queues first: the rename waits for it, then sees it and is refused.
    let held = gate(&test, t.a).await;
    let first = test.db.clone();
    let removal =
        tokio::spawn(async move { first.set_node_access(ann.id, t.a, xan.id, None).await });
    wait_blocked(&test, held.pid, 1).await;
    let renaming = tokio::spawn(rename_file(test.db.clone(), xan.id, t.file, "Late"));
    wait_blocked(&test, held.pid, 2).await;
    held.open().await;
    assert_eq!(removal.await.unwrap(), Ok((Some(Role::Editor), None)));
    assert_eq!(renaming.await.unwrap(), Err(AccessError::NotFound));
    assert_eq!(test.db.node(t.file).await.unwrap().unwrap().name, "Notes");

    // The rename queues first: it is judged before the revoke, which waits for it.
    test.db
        .set_node_access(ann.id, t.a, xan.id, Some(Role::Editor))
        .await
        .unwrap();
    let held = gate(&test, t.file).await;
    let renaming = tokio::spawn(rename_file(test.db.clone(), xan.id, t.file, "Early"));
    wait_blocked(&test, held.pid, 1).await;
    let second = test.db.clone();
    let removal =
        tokio::spawn(async move { second.set_node_access(ann.id, t.a, xan.id, None).await });
    // The revoke waits behind the rename, which holds folder A while it waits for the file.
    wait_blocked(&test, held.pid, 2).await;
    held.open().await;
    assert_eq!(renaming.await.unwrap(), Ok(()));
    assert_eq!(removal.await.unwrap(), Ok((Some(Role::Editor), None)));
    assert_eq!(test.db.node(t.file).await.unwrap().unwrap().name, "Early");
    assert_eq!(role(&test, xan.id, t.file).await, None);
}
