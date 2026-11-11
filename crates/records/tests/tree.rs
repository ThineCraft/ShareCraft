#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use collab_core::ids::NodeId;
use records::test_support::TestDb;
use records::{AccessError, NodeKind, NodeRow, validate_node_name};
use time::OffsetDateTime;
use uuid::Uuid;

fn id() -> NodeId {
    NodeId(Uuid::new_v4())
}

fn row(id: NodeId, kind: NodeKind, parent: Option<NodeId>, site: NodeId, name: &str) -> NodeRow {
    NodeRow {
        id,
        kind,
        parent,
        site,
        name: name.to_string(),
        deleted_at: None,
        deleted_by: None,
        created_by: None,
        created_at: OffsetDateTime::now_utc(),
    }
}

/// A site with a library, returning (site, library).
async fn site_and_library(test: &TestDb) -> (NodeId, NodeId) {
    let site = id();
    let library = id();
    test.db
        .apply_node(&row(site, NodeKind::Site, None, site, "Site"), None)
        .await
        .unwrap();
    test.db
        .apply_node(
            &row(library, NodeKind::Library, Some(site), site, "Documents"),
            None,
        )
        .await
        .unwrap();
    (site, library)
}

#[tokio::test]
async fn upsert_creates_and_updates_a_node() {
    let test = TestDb::create().await;
    let (site, library) = site_and_library(&test).await;
    let folder = id();
    test.db
        .apply_node(
            &row(folder, NodeKind::Folder, Some(library), site, "Plans"),
            None,
        )
        .await
        .unwrap();
    let node = test.db.node(folder).await.unwrap().unwrap();
    assert_eq!(node.kind, NodeKind::Folder);
    assert_eq!(node.parent, Some(library));
    assert_eq!(node.site, site);
    assert_eq!(node.name, "Plans");
    assert!(!node.deleted);

    let other = id();
    test.db
        .apply_node(
            &row(other, NodeKind::Folder, Some(library), site, "Other"),
            None,
        )
        .await
        .unwrap();
    let mut changed = row(folder, NodeKind::Folder, Some(other), site, "Plans 2");
    changed.deleted_at = Some(OffsetDateTime::now_utc());
    test.db.apply_node(&changed, None).await.unwrap();
    let node = test.db.node(folder).await.unwrap().unwrap();
    assert_eq!(node.parent, Some(other));
    assert_eq!(node.name, "Plans 2");
    assert!(node.deleted);
    assert_eq!(test.db.node(id()).await.unwrap(), None);
}

#[tokio::test]
async fn site_rows_start_as_scopes() {
    let test = TestDb::create().await;
    let (site, library) = site_and_library(&test).await;
    assert!(test.db.node(site).await.unwrap().unwrap().unique_scope);
    assert!(!test.db.node(library).await.unwrap().unwrap().unique_scope);
}

#[tokio::test]
async fn sibling_names_unique_ignoring_case() {
    let test = TestDb::create().await;
    let (site, library) = site_and_library(&test).await;
    let first = id();
    test.db
        .apply_node(
            &row(
                first,
                NodeKind::Document,
                Some(library),
                site,
                "report.xlsx",
            ),
            None,
        )
        .await
        .unwrap();
    let second = row(id(), NodeKind::Document, Some(library), site, "Report.xlsx");
    assert_eq!(
        test.db.apply_node(&second, None).await,
        Err(AccessError::NameTaken)
    );
    assert!(
        !test
            .db
            .name_free(library, "REPORT.xlsx", None)
            .await
            .unwrap()
    );
    assert!(
        test.db
            .name_free(library, "REPORT.xlsx", Some(first))
            .await
            .unwrap()
    );

    // Allowed in another folder.
    let folder = id();
    test.db
        .apply_node(
            &row(folder, NodeKind::Folder, Some(library), site, "Archive"),
            None,
        )
        .await
        .unwrap();
    let elsewhere = row(id(), NodeKind::Document, Some(folder), site, "Report.xlsx");
    test.db.apply_node(&elsewhere, None).await.unwrap();

    // Allowed once the first is deleted.
    let mut gone = row(
        first,
        NodeKind::Document,
        Some(library),
        site,
        "report.xlsx",
    );
    gone.deleted_at = Some(OffsetDateTime::now_utc());
    test.db.apply_node(&gone, None).await.unwrap();
    assert!(
        test.db
            .name_free(library, "Report.xlsx", None)
            .await
            .unwrap()
    );
    test.db.apply_node(&second, None).await.unwrap();
}

#[tokio::test]
async fn update_keeps_unique_scope() {
    let test = TestDb::create().await;
    let (site, library) = site_and_library(&test).await;
    test.db
        .apply_node(&row(site, NodeKind::Site, None, site, "Renamed site"), None)
        .await
        .unwrap();
    test.db
        .apply_node(
            &row(library, NodeKind::Library, Some(site), site, "Docs"),
            None,
        )
        .await
        .unwrap();
    assert!(test.db.node(site).await.unwrap().unwrap().unique_scope);
    assert!(!test.db.node(library).await.unwrap().unwrap().unique_scope);
    sqlx::query("update nodes set unique_scope = true where id = $1")
        .bind(library.0)
        .execute(test.db.test_pool())
        .await
        .unwrap();
    test.db
        .apply_node(
            &row(library, NodeKind::Library, Some(site), site, "Docs again"),
            None,
        )
        .await
        .unwrap();
    assert!(test.db.node(library).await.unwrap().unwrap().unique_scope);
}

#[tokio::test]
async fn is_at_or_below_walks_parents() {
    let test = TestDb::create().await;
    let (site, library) = site_and_library(&test).await;
    let a = id();
    let b = id();
    let sibling = id();
    for (node, parent, name) in [(a, library, "A"), (b, a, "B"), (sibling, library, "S")] {
        test.db
            .apply_node(&row(node, NodeKind::Folder, Some(parent), site, name), None)
            .await
            .unwrap();
    }
    assert!(test.db.is_at_or_below(b, b).await.unwrap());
    assert!(test.db.is_at_or_below(b, a).await.unwrap());
    assert!(test.db.is_at_or_below(b, library).await.unwrap());
    assert!(test.db.is_at_or_below(b, site).await.unwrap());
    assert!(!test.db.is_at_or_below(a, b).await.unwrap());
    assert!(!test.db.is_at_or_below(b, sibling).await.unwrap());
    assert!(!test.db.is_at_or_below(id(), site).await.unwrap());

    assert!(test.db.has_live_children(a).await.unwrap());
    assert!(!test.db.has_live_children(b).await.unwrap());
    let mut gone = row(b, NodeKind::Folder, Some(a), site, "B");
    gone.deleted_at = Some(OffsetDateTime::now_utc());
    test.db.apply_node(&gone, None).await.unwrap();
    assert!(!test.db.has_live_children(a).await.unwrap());
}

#[test]
fn validate_node_name_rules() {
    for c in ['"', '*', ':', '<', '>', '?', '/', '\\', '|', '\n', '\u{0}'] {
        let name = format!("a{c}b");
        assert_eq!(
            validate_node_name(&name),
            Err(AccessError::InvalidName),
            "{name:?}"
        );
    }
    let long = "x".repeat(121);
    for name in ["", "   ", "report.", "report. ", long.as_str()] {
        assert_eq!(
            validate_node_name(name),
            Err(AccessError::InvalidName),
            "{name:?}"
        );
    }
    assert_eq!(validate_node_name(&"x".repeat(120)).map(str::len), Ok(120));
    assert_eq!(
        validate_node_name("  Q3 report (final).xlsx "),
        Ok("Q3 report (final).xlsx")
    );
}
