#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use collab_core::ids::{NodeId, UserId};
use common::{Role, changed, grant, new_id, put, role_gate, role_of, tree, user, wait_blocked};
use records::test_support::TestDb;
use records::{
    Access, AccessError, Db, NodeChange, NodeKind, NodeRow, Perms, Principal, RoleDef, RoleRef,
};
use serde_json::json;
use time::OffsetDateTime;

/// The `{view, add, edit}` set of the plan's "Contributor".
fn contributor() -> Perms {
    Perms::ADD.union(Perms::EDIT)
}

/// The `{view, manage_access}` set: someone who manages access and nothing else.
fn gatekeeper() -> Perms {
    Perms::MANAGE_ACCESS
}

async fn access(test: &TestDb, user: UserId, node: NodeId) -> Option<Access> {
    test.db.effective_access(user, node).await.unwrap()
}

async fn is_scope(test: &TestDb, node: NodeId) -> bool {
    test.db.node(node).await.unwrap().unwrap().unique_scope
}

/// The kind and detail of every role event, oldest first.
async fn role_events(test: &TestDb) -> Vec<(String, serde_json::Value)> {
    sqlx::query_as("select kind, detail from events where kind like 'role.%' order by id")
        .fetch_all(test.db.test_pool())
        .await
        .unwrap()
}

/// The row of `node` as it is now, renamed to `name`.
async fn renamed(db: &Db, node: NodeId, name: &str) -> NodeRow {
    let node = db.node(node).await.unwrap().unwrap();
    NodeRow {
        id: node.id,
        kind: node.kind,
        parent: node.parent,
        site: node.site,
        name: name.to_owned(),
        deleted_at: None,
        deleted_by: None,
        created_by: node.created_by,
        created_at: node.created_at,
    }
}

/// A rename of `node` to `name` by `actor`, judged as the server judges one (`edit`).
async fn rename(db: Db, actor: UserId, node: NodeId, name: &str) -> Result<(), AccessError> {
    let row = renamed(&db, node, name).await;
    db.apply_nodes(NodeChange {
        rows: std::slice::from_ref(&row),
        actor: Some(actor),
        needs: &[(node, Perms::EDIT)],
        ..NodeChange::default()
    })
    .await
}

/// A new folder `name` under `parent` by `actor`, judged as the server judges one (`add`).
async fn create_folder(
    db: &Db,
    actor: UserId,
    parent: NodeId,
    site: NodeId,
    name: &str,
) -> Result<NodeId, AccessError> {
    let row = NodeRow {
        id: new_id(),
        kind: NodeKind::Folder,
        parent: Some(parent),
        site,
        name: name.to_owned(),
        deleted_at: None,
        deleted_by: None,
        created_by: Some(actor),
        created_at: OffsetDateTime::now_utc(),
    };
    db.apply_nodes(NodeChange {
        rows: std::slice::from_ref(&row),
        actor: Some(actor),
        needs: &[(parent, Perms::ADD)],
        ..NodeChange::default()
    })
    .await?;
    Ok(row.id)
}

/// A deletion of `node` by `actor`, judged as the server judges one (`delete`).
async fn delete_node(db: &Db, actor: UserId, node: NodeId) -> Result<(), AccessError> {
    let mut row = renamed(db, node, &db.node(node).await.unwrap().unwrap().name).await;
    row.deleted_at = Some(OffsetDateTime::now_utc());
    db.apply_nodes(NodeChange {
        rows: std::slice::from_ref(&row),
        actor: Some(actor),
        needs: &[(node, Perms::DELETE)],
        ..NodeChange::default()
    })
    .await
}

#[tokio::test]
async fn builtin_roles_exist_and_cannot_change() {
    let test = TestDb::create().await;
    let root = user(&test, "Root", true).await;
    let roles = test.db.roles().await.unwrap();
    let summary: Vec<(&str, &str, Vec<&str>, bool)> = roles
        .iter()
        .map(|r| (r.id.as_str(), r.name.as_str(), r.perms.names(), r.builtin))
        .collect();
    assert_eq!(
        summary,
        vec![
            ("viewer", "Viewer", vec!["view"], true),
            (
                "editor",
                "Editor",
                vec!["view", "add", "edit", "delete"],
                true
            ),
            (
                "owner",
                "Owner",
                vec![
                    "view",
                    "add",
                    "edit",
                    "delete",
                    "manage_access",
                    "manage_site"
                ],
                true
            ),
        ]
    );
    for role in &roles {
        assert_eq!(Some(role.clone()), RoleDef::builtin(&role.id));
        assert_eq!(
            test.db
                .change_role(root.id, &role.id, Some("Renamed"), None)
                .await,
            Err(AccessError::BuiltinRole)
        );
        assert_eq!(
            test.db
                .change_role(root.id, &role.id, None, Some(Perms::ALL))
                .await,
            Err(AccessError::BuiltinRole)
        );
        assert_eq!(
            test.db.delete_role(root.id, &role.id).await,
            Err(AccessError::BuiltinRole)
        );
    }
    assert_eq!(test.db.roles().await.unwrap(), roles);
    assert!(role_events(&test).await.is_empty());
    // An administrator's access is every permission, shown as Owner.
    assert_eq!(Access::administrator().perms, Perms::ALL);
    assert_eq!(
        Access::administrator().roles,
        vec![RoleRef {
            id: "owner".into(),
            name: "Owner".into()
        }]
    );
}

#[tokio::test]
async fn perms_always_include_view() {
    let test = TestDb::create().await;
    let root = user(&test, "Root", true).await;
    for perm in [
        Perms::ADD,
        Perms::EDIT,
        Perms::DELETE,
        Perms::MANAGE_ACCESS,
        Perms::MANAGE_SITE,
        Perms::ALL,
    ] {
        assert!(perm.contains(Perms::VIEW), "{perm:?}");
    }
    assert_eq!(
        Perms::from_names(["edit"]).unwrap(),
        Perms::EDIT,
        "view is added"
    );
    assert_eq!(Perms::from_bits(1 << 2), Some(Perms::EDIT));
    assert_eq!(Perms::from_bits(0), Some(Perms::NONE));
    assert_eq!(Perms::from_names(Vec::<String>::new()), Some(Perms::NONE));
    assert_eq!(Perms::from_names(["edit", "fly"]), None);
    assert_eq!(Perms::from_bits(1 << 6), None);
    assert!(Perms::ALL.contains(contributor()) && !contributor().contains(Perms::DELETE));
    assert_eq!(contributor().union(Perms::DELETE).names().len(), 4);
    // Serialised as the sorted list of names; read back the same, with view added.
    assert_eq!(
        serde_json::to_value(Perms::MANAGE_SITE.union(Perms::ADD)).unwrap(),
        json!(["view", "add", "manage_site"])
    );
    let read: Perms = serde_json::from_value(json!(["delete", "edit"])).unwrap();
    assert_eq!(read.names(), vec!["view", "edit", "delete"]);
    assert!(serde_json::from_value::<Perms>(json!(["admin"])).is_err());
    // A role always has view, however it is asked for.
    let empty = test
        .db
        .create_role(root.id, "Nothing", Perms::NONE)
        .await
        .unwrap();
    assert_eq!(empty.perms, Perms::VIEW);
    let changed = test
        .db
        .change_role(root.id, &empty.id, None, Some(Perms::NONE))
        .await
        .unwrap();
    assert_eq!(changed.perms, Perms::VIEW);
    // The database refuses a role without view too.
    let stored = sqlx::query("insert into roles (id, name, perms) values ('bad', 'Bad', 4)")
        .execute(test.db.test_pool())
        .await;
    assert!(stored.is_err());
}

#[tokio::test]
async fn custom_role_crud_by_administrator_only() {
    let test = TestDb::create().await;
    let root = user(&test, "Root", true).await;
    let ann = user(&test, "Ann", false).await;
    let zed = user(&test, "Zed", true).await;
    test.db.set_disabled(root.id, zed.id, true).await.unwrap();
    // Not an administrator, or no longer an enabled one.
    for who in [ann.id, zed.id] {
        assert_eq!(
            test.db.create_role(who, "Contributor", contributor()).await,
            Err(AccessError::Forbidden)
        );
    }
    let long = "n".repeat(61);
    for name in ["", "   ", "a\u{0}b", long.as_str()] {
        assert_eq!(
            test.db.create_role(root.id, name, contributor()).await,
            Err(AccessError::InvalidName),
            "{name:?}"
        );
    }
    let made = test
        .db
        .create_role(root.id, "  Contributor ", contributor())
        .await
        .unwrap();
    assert_eq!(made.name, "Contributor");
    assert_eq!(made.perms, contributor());
    assert!(!made.builtin);
    assert!(!["viewer", "editor", "owner"].contains(&made.id.as_str()));
    let roles = test.db.roles().await.unwrap();
    assert_eq!(roles.len(), 4);
    assert_eq!(roles[3], made);

    for who in [ann.id, zed.id] {
        assert_eq!(
            test.db
                .change_role(who, &made.id, Some("Writer"), None)
                .await,
            Err(AccessError::Forbidden)
        );
        assert_eq!(
            test.db.delete_role(who, &made.id).await,
            Err(AccessError::Forbidden)
        );
    }
    assert_eq!(
        test.db
            .change_role(root.id, "no-such-role", Some("Writer"), None)
            .await,
        Err(AccessError::NoSuchRole)
    );
    let writer = test
        .db
        .change_role(
            root.id,
            &made.id,
            Some("Writer"),
            Some(contributor().union(Perms::DELETE)),
        )
        .await
        .unwrap();
    assert_eq!(writer.id, made.id);
    assert_eq!(writer.name, "Writer");
    assert_eq!(writer.perms.names(), vec!["view", "add", "edit", "delete"]);
    // Only what is given changes.
    let renamed = test
        .db
        .change_role(root.id, &made.id, Some("Author"), None)
        .await
        .unwrap();
    assert_eq!(renamed.perms, writer.perms);
    assert_eq!(test.db.roles().await.unwrap()[3], renamed);

    assert_eq!(test.db.delete_role(root.id, &made.id).await, Ok(()));
    assert_eq!(test.db.roles().await.unwrap().len(), 3);
    assert_eq!(
        test.db.delete_role(root.id, &made.id).await,
        Err(AccessError::NoSuchRole)
    );

    let id = made.id.as_str();
    assert_eq!(
        role_events(&test).await,
        vec![
            (
                "role.created".to_string(),
                json!({ "id": id, "name": "Contributor", "perms": ["view", "add", "edit"] })
            ),
            (
                "role.changed".to_string(),
                json!({
                    "id": id,
                    "before": { "name": "Contributor", "perms": ["view", "add", "edit"] },
                    "after": { "name": "Writer", "perms": ["view", "add", "edit", "delete"] },
                })
            ),
            (
                "role.changed".to_string(),
                json!({
                    "id": id,
                    "before": { "name": "Writer", "perms": ["view", "add", "edit", "delete"] },
                    "after": { "name": "Author", "perms": ["view", "add", "edit", "delete"] },
                })
            ),
            (
                "role.deleted".to_string(),
                json!({ "id": id, "name": "Author", "perms": ["view", "add", "edit", "delete"] })
            ),
        ]
    );
}

#[tokio::test]
async fn role_names_unique_ignoring_case() {
    let test = TestDb::create().await;
    let root = user(&test, "Root", true).await;
    let made = test
        .db
        .create_role(root.id, "Contributor", contributor())
        .await
        .unwrap();
    for name in ["contributor", " CONTRIBUTOR ", "viewer", "OWNER"] {
        assert_eq!(
            test.db.create_role(root.id, name, Perms::VIEW).await,
            Err(AccessError::NameTaken),
            "{name:?}"
        );
    }
    let other = test
        .db
        .create_role(root.id, "Reviewer", Perms::VIEW)
        .await
        .unwrap();
    for name in ["CONTRIBUTOR", "Editor"] {
        assert_eq!(
            test.db
                .change_role(root.id, &other.id, Some(name), None)
                .await,
            Err(AccessError::NameTaken),
            "{name:?}"
        );
    }
    // A role may change the case of its own name.
    let recased = test
        .db
        .change_role(root.id, &made.id, Some("CONTRIBUTOR"), None)
        .await
        .unwrap();
    assert_eq!(recased.name, "CONTRIBUTOR");
    // A deleted role's name is free again.
    test.db.delete_role(root.id, &other.id).await.unwrap();
    assert!(
        test.db
            .create_role(root.id, "reviewer", Perms::VIEW)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn role_in_use_cannot_be_deleted() {
    let test = TestDb::create().await;
    let root = user(&test, "Root", true).await;
    let ann = user(&test, "Ann", false).await;
    let cy = user(&test, "Cy", false).await;
    let t = tree(&test, ann.id).await;
    let role = test
        .db
        .create_role(root.id, "Contributor", contributor())
        .await
        .unwrap();
    test.db
        .set_node_access(ann.id, t.b, Principal::User(cy.id), Some(&role.id))
        .await
        .unwrap();
    assert_eq!(
        test.db.delete_role(root.id, &role.id).await,
        Err(AccessError::RoleInUse)
    );
    // Still in use by the copy below a break, after the original is gone.
    test.db.break_inheritance(ann.id, t.file).await.unwrap();
    test.db
        .set_node_access(ann.id, t.b, Principal::User(cy.id), None)
        .await
        .unwrap();
    assert_eq!(
        test.db.delete_role(root.id, &role.id).await,
        Err(AccessError::RoleInUse)
    );
    test.db
        .set_node_access(ann.id, t.file, Principal::User(cy.id), None)
        .await
        .unwrap();
    assert_eq!(test.db.delete_role(root.id, &role.id).await, Ok(()));
    assert_eq!(
        test.db
            .set_node_access(ann.id, t.b, Principal::User(cy.id), Some(&role.id))
            .await,
        Err(AccessError::NoSuchRole)
    );
}

#[tokio::test]
async fn custom_role_grants_exactly_its_permissions() {
    let test = TestDb::create().await;
    let root = user(&test, "Root", true).await;
    let ann = user(&test, "Ann", false).await;
    let cy = user(&test, "Cy", false).await;
    let t = tree(&test, ann.id).await;
    let role = test
        .db
        .create_role(root.id, "Contributor", contributor())
        .await
        .unwrap();
    test.db
        .set_node_access(ann.id, t.a, Principal::User(cy.id), Some(&role.id))
        .await
        .unwrap();
    let expected = Access {
        perms: contributor(),
        roles: vec![role.role_ref()],
    };
    for node in [t.a, t.b, t.file] {
        assert_eq!(access(&test, cy.id, node).await, Some(expected.clone()));
    }
    assert_eq!(access(&test, cy.id, t.library).await, None);
    let held = access(&test, cy.id, t.file).await.unwrap();
    assert!(held.can_view() && held.can_add() && held.can_edit());
    assert!(!held.can_delete() && !held.can_manage_access() && !held.can_manage_site());
    // They can create and save, but not delete or manage access.
    let folder = create_folder(&test.db, cy.id, t.b, t.site, "New")
        .await
        .unwrap();
    assert_eq!(
        rename(test.db.clone(), cy.id, t.file, "Saved").await,
        Ok(())
    );
    assert_eq!(
        delete_node(&test.db, cy.id, folder).await,
        Err(AccessError::Forbidden)
    );
    assert_eq!(
        test.db.access(cy.id, t.b).await.err(),
        Some(AccessError::Forbidden)
    );
    assert_eq!(
        test.db.break_inheritance(cy.id, t.b).await,
        Err(AccessError::Forbidden)
    );
    // An editor, with the built-in role, may delete it.
    let ed = user(&test, "Ed", false).await;
    // Folder A became a scope when Cy was given the role there.
    grant(&test, t.a, ed.id, Role::Editor).await;
    assert!(
        access(&test, ed.id, folder).await.unwrap().can_delete(),
        "Editor has delete"
    );
    assert_eq!(delete_node(&test.db, ed.id, folder).await, Ok(()));
}

#[tokio::test]
async fn last_manager_kept_per_scope() {
    let test = TestDb::create().await;
    let root = user(&test, "Root", true).await;
    let ann = user(&test, "Ann", false).await;
    let gus = user(&test, "Gus", false).await;
    let cy = user(&test, "Cy", false).await;
    let t = tree(&test, ann.id).await;
    let keeper = test
        .db
        .create_role(root.id, "Gatekeeper", gatekeeper())
        .await
        .unwrap();
    let writer = test
        .db
        .create_role(root.id, "Contributor", contributor())
        .await
        .unwrap();
    test.db
        .set_node_access(ann.id, t.site, Principal::User(gus.id), Some(&keeper.id))
        .await
        .unwrap();
    test.db
        .set_node_access(ann.id, t.site, Principal::User(cy.id), Some(&writer.id))
        .await
        .unwrap();
    // A custom role with manage_access counts as a manager, so the owner may leave.
    assert_eq!(
        changed(
            test.db
                .set_node_access(ann.id, t.site, Principal::User(ann.id), None)
                .await
        ),
        Ok((Some(Role::Owner), None))
    );
    assert_eq!(access(&test, ann.id, t.site).await, None);
    // Gus is now the only manager; a role without manage_access does not count.
    assert_eq!(
        test.db
            .set_node_access(gus.id, t.site, Principal::User(gus.id), None)
            .await,
        Err(AccessError::LastManager)
    );
    assert_eq!(
        test.db
            .set_node_access(gus.id, t.site, Principal::User(gus.id), Some("viewer"))
            .await,
        Err(AccessError::LastManager)
    );
    // On a folder's own scope, the rule holds separately, and nothing is broken by a refusal.
    assert_eq!(
        test.db
            .set_node_access(gus.id, t.b, Principal::User(gus.id), None)
            .await,
        Err(AccessError::LastManager)
    );
    assert!(!is_scope(&test, t.b).await);
    // With a second manager, either may step down.
    test.db
        .set_node_access(root.id, t.site, Principal::User(ann.id), Some("owner"))
        .await
        .unwrap();
    assert_eq!(
        test.db
            .set_node_access(gus.id, t.site, Principal::User(gus.id), Some("viewer"))
            .await,
        Ok((
            Some(keeper.role_ref()),
            Some(RoleDef::builtin("viewer").unwrap().role_ref())
        ))
    );
    assert_eq!(
        role_of(access(&test, gus.id, t.b).await),
        Some(Role::Viewer)
    );
}

#[tokio::test]
async fn cannot_assign_beyond_own_access() {
    let test = TestDb::create().await;
    let root = user(&test, "Root", true).await;
    let ann = user(&test, "Ann", false).await;
    let gus = user(&test, "Gus", false).await;
    let cy = user(&test, "Cy", false).await;
    let t = tree(&test, ann.id).await;
    let keeper = test
        .db
        .create_role(root.id, "Gatekeeper", gatekeeper())
        .await
        .unwrap();
    let writer = test
        .db
        .create_role(root.id, "Contributor", contributor())
        .await
        .unwrap();
    test.db
        .set_node_access(ann.id, t.a, Principal::User(gus.id), Some(&keeper.id))
        .await
        .unwrap();
    // Gus manages access to folder A but holds only view there.
    for role in ["editor", "owner", writer.id.as_str()] {
        assert_eq!(
            test.db
                .set_node_access(gus.id, t.b, Principal::User(cy.id), Some(role))
                .await,
            Err(AccessError::BeyondYourAccess),
            "{role}"
        );
    }
    // Nothing was broken by the refusals.
    assert!(!is_scope(&test, t.b).await);
    assert_eq!(access(&test, cy.id, t.b).await, None);
    // What Gus holds himself he may give.
    for role in ["viewer", keeper.id.as_str()] {
        test.db
            .set_node_access(gus.id, t.a, Principal::User(cy.id), Some(role))
            .await
            .unwrap();
    }
    assert_eq!(
        access(&test, cy.id, t.b).await.map(|a| a.perms),
        Some(gatekeeper())
    );
    // Not even to himself does he give more.
    assert_eq!(
        test.db
            .set_node_access(gus.id, t.a, Principal::User(gus.id), Some("owner"))
            .await,
        Err(AccessError::BeyondYourAccess)
    );
    // The owner of the site may give anything they hold; an administrator anything at all.
    test.db
        .set_node_access(ann.id, t.a, Principal::User(cy.id), Some(&writer.id))
        .await
        .unwrap();
    test.db
        .set_node_access(root.id, t.a, Principal::User(gus.id), Some("owner"))
        .await
        .unwrap();
    assert_eq!(role_of(access(&test, gus.id, t.b).await), Some(Role::Owner));
    // A role that does not exist is refused as such.
    assert_eq!(
        test.db
            .set_node_access(ann.id, t.a, Principal::User(cy.id), Some("nobody"))
            .await,
        Err(AccessError::NoSuchRole)
    );
}

#[tokio::test]
async fn role_change_and_decision_take_turns() {
    let test = TestDb::create().await;
    let root = user(&test, "Root", true).await;
    let ann = user(&test, "Ann", false).await;
    let xan = user(&test, "Xan", false).await;
    let t = tree(&test, ann.id).await;
    let role = test
        .db
        .create_role(root.id, "Contributor", contributor())
        .await
        .unwrap();
    // Xan's only access to the file comes from the custom role on folder A.
    test.db
        .set_node_access(ann.id, t.a, Principal::User(xan.id), Some(&role.id))
        .await
        .unwrap();
    assert!(access(&test, xan.id, t.file).await.unwrap().can_edit());

    // The role change queues first: the rename, judged by the role, waits for it, then sees
    // the role without edit and is refused.
    let held = role_gate(&test, &role.id).await;
    let first = test.db.clone();
    let id = role.id.clone();
    let change = tokio::spawn(async move {
        first
            .change_role(root.id, &id, None, Some(Perms::VIEW))
            .await
    });
    wait_blocked(&test, held.pid, 1).await;
    let renaming = tokio::spawn(rename(test.db.clone(), xan.id, t.file, "Late"));
    wait_blocked(&test, held.pid, 2).await;
    held.open().await;
    assert_eq!(change.await.unwrap().map(|r| r.perms), Ok(Perms::VIEW));
    assert_eq!(renaming.await.unwrap(), Err(AccessError::Forbidden));
    assert_eq!(test.db.node(t.file).await.unwrap().unwrap().name, "Notes");

    // The rename queues first: it is judged by the role as it was, and the change waits.
    test.db
        .change_role(root.id, &role.id, None, Some(contributor()))
        .await
        .unwrap();
    let held = role_gate(&test, &role.id).await;
    let renaming = tokio::spawn(rename(test.db.clone(), xan.id, t.file, "Early"));
    wait_blocked(&test, held.pid, 1).await;
    let second = test.db.clone();
    let id = role.id.clone();
    let change = tokio::spawn(async move {
        second
            .change_role(root.id, &id, None, Some(Perms::VIEW))
            .await
    });
    wait_blocked(&test, held.pid, 2).await;
    held.open().await;
    assert_eq!(renaming.await.unwrap(), Ok(()));
    assert_eq!(change.await.unwrap().map(|r| r.perms), Ok(Perms::VIEW));
    assert_eq!(test.db.node(t.file).await.unwrap().unwrap().name, "Early");
    // The change reaches Xan at once, everywhere the role is held.
    for node in [t.a, t.b, t.file] {
        assert_eq!(
            access(&test, xan.id, node).await.map(|a| a.perms),
            Some(Perms::VIEW)
        );
    }
}

#[tokio::test]
async fn listings_follow_assignments() {
    let test = TestDb::create().await;
    let root = user(&test, "Root", true).await;
    let ann = user(&test, "Ann", false).await;
    let vic = user(&test, "Vic", false).await;
    let wes = user(&test, "Wes", false).await;
    let t = tree(&test, ann.id).await;
    let other = put(
        &test,
        new_id(),
        NodeKind::Folder,
        Some(t.a),
        t.site,
        "Other",
    )
    .await;
    let role = test
        .db
        .create_role(root.id, "Contributor", contributor())
        .await
        .unwrap();
    test.db
        .set_node_access(ann.id, t.site, Principal::User(vic.id), Some("viewer"))
        .await
        .unwrap();
    test.db
        .set_node_access(ann.id, t.b, Principal::User(wes.id), Some(&role.id))
        .await
        .unwrap();
    let as_writer = Some(Access {
        perms: contributor(),
        roles: vec![role.role_ref()],
    });

    // Sites: full access, limited access, or every site for an administrator.
    let sites = |who: UserId| {
        let db = test.db.clone();
        async move {
            db.sites_for(who)
                .await
                .unwrap()
                .into_iter()
                .map(|(node, access)| (node.id, access))
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(
        sites(vic.id).await,
        vec![(
            t.site,
            Some(Access::from_role(&RoleDef::builtin("viewer").unwrap()))
        )]
    );
    assert_eq!(sites(wes.id).await, vec![(t.site, None)]);
    assert_eq!(
        sites(root.id).await,
        vec![(t.site, Some(Access::administrator()))]
    );

    // Every listing agrees with the one access decision, node by node.
    for who in [ann.id, vic.id, wes.id, root.id] {
        for node in [t.site, t.library, t.a, t.b, t.file, other] {
            let decided = access(&test, who, node).await;
            if let Some(path) = test.db.path(who, node).await.unwrap() {
                assert_eq!(path.last().unwrap().role, decided, "path of {node:?}");
                for crumb in &path {
                    assert_eq!(crumb.role, access(&test, who, crumb.id).await);
                }
            } else {
                assert_eq!(decided, None);
            }
            if let Some((listed, _)) = test.db.children(who, node, None, 100).await.unwrap() {
                for child in listed {
                    assert_eq!(child.role, access(&test, who, child.node.id).await);
                }
            }
        }
    }
    // Wes sees the way down to folder B, and inside it his role.
    let (listed, _) = test
        .db
        .children(wes.id, t.a, None, 100)
        .await
        .unwrap()
        .unwrap();
    let shown: Vec<_> = listed.iter().map(|l| (l.node.id, l.role.clone())).collect();
    assert_eq!(shown, vec![(t.b, as_writer.clone())]);
    let (listed, _) = test
        .db
        .children(wes.id, t.b, None, 100)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].role, as_writer);
    let path = test.db.path(wes.id, t.file).await.unwrap().unwrap();
    let roles: Vec<_> = path.iter().map(|c| c.role.clone()).collect();
    assert_eq!(
        roles,
        vec![None, None, None, as_writer.clone(), as_writer.clone()]
    );
    assert_eq!(test.db.path(wes.id, other).await.unwrap(), None);
    // The access page shows the custom role with its permissions.
    let shown = test.db.access(ann.id, t.b).await.unwrap();
    let grants: Vec<_> = shown
        .grants
        .iter()
        .map(|g| (g.user.id, g.role.id.clone(), g.perms))
        .collect();
    assert_eq!(
        grants,
        vec![
            (ann.id, "owner".to_string(), Perms::ALL),
            (vic.id, "viewer".to_string(), Perms::VIEW),
            (wes.id, role.id.clone(), contributor()),
        ]
    );
}

#[tokio::test]
async fn upgrade_copies_scope_access() {
    // A database as plan 1 left it: migrated up to its last migration, with grants.
    let test = TestDb::create_at(Some(3)).await;
    let ann = user(&test, "Ann", false).await;
    let vic = user(&test, "Vic", false).await;
    let wes = user(&test, "Wes", false).await;
    let site = new_id();
    put(&test, site, NodeKind::Site, None, site, "Site").await;
    let library = put(
        &test,
        new_id(),
        NodeKind::Library,
        Some(site),
        site,
        "Documents",
    )
    .await;
    let folder = put(&test, new_id(), NodeKind::Folder, Some(library), site, "A").await;
    sqlx::query("update nodes set unique_scope = true where id = $1")
        .bind(folder.0)
        .execute(test.db.test_pool())
        .await
        .unwrap();
    let rows = [
        (site, ann.id, "owner", None),
        (site, vic.id, "viewer", Some(ann.id)),
        (folder, ann.id, "owner", Some(ann.id)),
        (folder, wes.id, "editor", Some(ann.id)),
    ];
    for (node, who, role, by) in rows {
        sqlx::query(
            "insert into scope_access (node_id, user_id, role, granted_by, granted_at) \
             values ($1, $2, $3, $4, '2026-01-02T03:04:05Z')",
        )
        .bind(node.0)
        .bind(who.0)
        .bind(role)
        .bind(by.map(|u| u.0))
        .execute(test.db.test_pool())
        .await
        .unwrap();
    }

    test.db.migrate().await.unwrap();

    let copied: Vec<(
        uuid::Uuid,
        String,
        uuid::Uuid,
        String,
        Option<uuid::Uuid>,
        String,
    )> = sqlx::query_as(
        "select node_id, principal_kind, principal_id, role_id, granted_by, \
                    to_char(granted_at at time zone 'UTC', 'YYYY-MM-DD HH24:MI:SS') \
             from assignments order by node_id, principal_id",
    )
    .fetch_all(test.db.test_pool())
    .await
    .unwrap();
    let mut expected: Vec<_> = rows
        .iter()
        .map(|(node, who, role, by)| {
            (
                node.0,
                "user".to_string(),
                who.0,
                (*role).to_string(),
                by.map(|u| u.0),
                "2026-01-02 03:04:05".to_string(),
            )
        })
        .collect();
    expected.sort_by_key(|row| (row.0, row.2));
    assert_eq!(copied, expected);
    // The old table is kept, renamed and unread.
    let kept: i64 = sqlx::query_scalar("select count(*) from scope_access_plan1")
        .fetch_one(test.db.test_pool())
        .await
        .unwrap();
    assert_eq!(kept, 4);
    let old: Option<String> = sqlx::query_scalar("select to_regclass('scope_access')::text")
        .fetch_one(test.db.test_pool())
        .await
        .unwrap();
    assert_eq!(old, None);
    // Everyone keeps the access they had.
    let file = put(&test, new_id(), NodeKind::Document, Some(folder), site, "f").await;
    assert_eq!(
        role_of(access(&test, ann.id, file).await),
        Some(Role::Owner)
    );
    assert_eq!(
        role_of(access(&test, vic.id, library).await),
        Some(Role::Viewer)
    );
    assert_eq!(role_of(access(&test, vic.id, file).await), None);
    assert_eq!(
        role_of(access(&test, wes.id, file).await),
        Some(Role::Editor)
    );
    assert_eq!(role_of(access(&test, wes.id, library).await), None);
}

#[tokio::test]
async fn role_change_keeps_a_manager_per_scope() {
    let test = TestDb::create().await;
    let root = user(&test, "Root", true).await;
    let ann = user(&test, "Ann", false).await;
    let gus = user(&test, "Gus", false).await;
    let cy = user(&test, "Cy", false).await;
    let t = tree(&test, ann.id).await;
    let keeper = test
        .db
        .create_role(root.id, "Gatekeeper", gatekeeper())
        .await
        .unwrap();
    // On the site the role is one of two managers (Ann is owner there).
    test.db
        .set_node_access(ann.id, t.site, Principal::User(cy.id), Some(&keeper.id))
        .await
        .unwrap();
    // On folder A it becomes the only manager once Ann leaves its own scope.
    test.db
        .set_node_access(ann.id, t.a, Principal::User(gus.id), Some(&keeper.id))
        .await
        .unwrap();
    test.db
        .set_node_access(ann.id, t.a, Principal::User(ann.id), None)
        .await
        .unwrap();
    assert!(is_scope(&test, t.a).await);

    // Losing manage_access would leave folder A with no manager.
    let dropped = Perms::VIEW;
    assert_eq!(
        test.db
            .change_role(root.id, &keeper.id, None, Some(dropped))
            .await,
        Err(AccessError::LastManager)
    );
    assert_eq!(
        test.db.roles().await.unwrap().last().map(|r| r.perms),
        Some(gatekeeper()),
        "nothing changed"
    );
    // A rename that keeps the permissions is no loss.
    assert!(
        test.db
            .change_role(root.id, &keeper.id, Some("Keeper"), None)
            .await
            .is_ok()
    );

    // With a second manager on folder A, the change goes through.
    test.db
        .set_node_access(root.id, t.a, Principal::User(ann.id), Some("owner"))
        .await
        .unwrap();
    let changed = test
        .db
        .change_role(root.id, &keeper.id, None, Some(dropped))
        .await
        .unwrap();
    assert_eq!(changed.perms, Perms::VIEW);
    assert_eq!(
        access(&test, gus.id, t.b).await.map(|a| a.perms),
        Some(Perms::VIEW)
    );
    assert!(
        access(&test, ann.id, t.b)
            .await
            .unwrap()
            .can_manage_access()
    );
}
