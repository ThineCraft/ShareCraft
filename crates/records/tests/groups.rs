#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::time::Duration;

use collab_core::ids::{GroupId, NodeId, UserId};
use common::{
    Role, gate, grant, group_gate, new_id, node_events, put, role_of, tree, user, wait_blocked,
};
use records::test_support::TestDb;
use records::{
    Access, AccessError, Db, Group, NodeChange, NodeKind, NodeRow, Perms, Principal, SiteGroup,
};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

/// The groups of `site`: Owners, Members, Visitors.
struct SiteGroups {
    owners: GroupId,
    members: GroupId,
    visitors: GroupId,
}

async fn site_groups(test: &TestDb, site: NodeId) -> SiteGroups {
    let rows: Vec<(Uuid, String)> =
        sqlx::query_as("select id, purpose from groups where site_id = $1")
            .bind(site.0)
            .fetch_all(test.db.test_pool())
            .await
            .unwrap();
    assert_eq!(rows.len(), 3, "{rows:?}");
    let find = |purpose: &str| {
        GroupId(
            rows.iter()
                .find(|(_, p)| p == purpose)
                .map(|(id, _)| *id)
                .unwrap(),
        )
    };
    SiteGroups {
        owners: find("owners"),
        members: find("members"),
        visitors: find("visitors"),
    }
}

fn row(id: NodeId, kind: NodeKind, parent: Option<NodeId>, site: NodeId, name: &str) -> NodeRow {
    NodeRow {
        id,
        kind,
        parent,
        site,
        name: name.to_owned(),
        deleted_at: None,
        deleted_by: None,
        created_by: None,
        created_at: OffsetDateTime::now_utc(),
    }
}

/// A site named `name` with its Documents library, created by `creator` as the server creates
/// one: its groups made in the same transaction, the creator a member of Owners.
async fn new_site(test: &TestDb, creator: UserId, name: &str) -> (NodeId, NodeId) {
    let site = new_id();
    let library = new_id();
    let rows = [
        row(site, NodeKind::Site, None, site, name),
        row(library, NodeKind::Library, Some(site), site, "Documents"),
    ];
    test.db
        .apply_nodes(NodeChange {
            rows: &rows,
            actor: Some(creator),
            site_groups: Some(creator),
            ..NodeChange::default()
        })
        .await
        .unwrap();
    (site, library)
}

async fn access(test: &TestDb, user: UserId, node: NodeId) -> Option<Access> {
    test.db.effective_access(user, node).await.unwrap()
}

async fn role(test: &TestDb, user: UserId, node: NodeId) -> Option<Role> {
    role_of(access(test, user, node).await)
}

fn names(groups: &[Group]) -> Vec<String> {
    groups.iter().map(|g| g.name.clone()).collect()
}

/// The person assignments on `node`.
async fn user_rows(test: &TestDb, node: NodeId) -> i64 {
    sqlx::query_scalar(
        "select count(*) from assignments where node_id = $1 and principal_kind = 'user'",
    )
    .bind(node.0)
    .fetch_one(test.db.test_pool())
    .await
    .unwrap()
}

#[tokio::test]
async fn new_site_has_three_groups_and_creator_is_owner() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    let admin = user(&test, "Root", true).await;
    let (site, library) = new_site(&test, ann.id, "Marketing").await;
    let groups = site_groups(&test, site).await;

    // The site's access comes from its groups alone: no person has an assignment of their own.
    assert_eq!(user_rows(&test, site).await, 0);
    let given: Vec<(Uuid, String)> = sqlx::query_as(
        "select principal_id, role_id from assignments where node_id = $1 order by role_id",
    )
    .bind(site.0)
    .fetch_all(test.db.test_pool())
    .await
    .unwrap();
    assert_eq!(
        given,
        vec![
            (groups.members.0, "editor".to_owned()),
            (groups.owners.0, "owner".to_owned()),
            (groups.visitors.0, "viewer".to_owned()),
        ]
    );
    assert_eq!(role(&test, ann.id, site).await, Some(Role::Owner));
    assert_eq!(role(&test, ann.id, library).await, Some(Role::Owner));
    assert_eq!(
        test.db.groups_of(ann.id).await.unwrap(),
        vec![groups.owners]
    );

    let visible = test.db.groups_visible_to(admin.id).await.unwrap();
    assert_eq!(
        names(&visible),
        [
            "Marketing Members",
            "Marketing Owners",
            "Marketing Visitors"
        ]
    );
    assert!(visible.iter().all(|g| g.site == Some(site)));
    let (owners, members) = test
        .db
        .group(admin.id, groups.owners)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(owners.purpose, Some(SiteGroup::Owners));
    assert_eq!(owners.member_count, 1);
    assert_eq!(
        members.iter().map(|u| u.id).collect::<Vec<_>>(),
        vec![ann.id]
    );

    let events = node_events(&test, site).await;
    let created = events.iter().filter(|(k, _)| k == "group.created").count();
    assert_eq!(created, 3);
    assert!(events.iter().any(|(k, d)| k == "group.member_added"
        && d["group"] == json!(groups.owners.0)
        && d["purpose"] == json!("owners")));

    // A site written by reconcile (no creator) also gets its groups, with nobody in them; and
    // writing it again makes no more.
    let other = common::site(&test, "Other", None).await;
    let other_groups = site_groups(&test, other).await;
    for group in [
        other_groups.owners,
        other_groups.members,
        other_groups.visitors,
    ] {
        let (shown, members) = test.db.group(admin.id, group).await.unwrap().unwrap();
        assert_eq!(shown.member_count, 0);
        assert!(members.is_empty());
    }
    put(&test, other, NodeKind::Site, None, other, "Other again").await;
    site_groups(&test, other).await;
}

#[tokio::test]
async fn union_over_groups() {
    let test = TestDb::create().await;
    let admin = user(&test, "Root", true).await;
    let bob = user(&test, "Bob", false).await;
    let t = tree(&test, admin.id).await;
    let groups = site_groups(&test, t.site).await;
    let cleaners = test.db.create_group(admin.id, "Cleaners").await.unwrap();
    let deleter = test
        .db
        .create_role(admin.id, "Deleter", Perms::DELETE)
        .await
        .unwrap();
    test.db
        .set_node_access(
            admin.id,
            t.site,
            Principal::Group(cleaners.id),
            Some(&deleter.id),
        )
        .await
        .unwrap();
    assert_eq!(access(&test, bob.id, t.file).await, None);

    assert!(
        test.db
            .add_member(admin.id, groups.visitors, bob.id)
            .await
            .unwrap()
    );
    assert!(
        test.db
            .add_member(admin.id, cleaners.id, bob.id)
            .await
            .unwrap()
    );
    let both = access(&test, bob.id, t.file).await.unwrap();
    assert_eq!(both.perms, Perms::DELETE);
    assert!(both.can_view() && both.can_delete() && !both.can_edit());
    let roles: Vec<&str> = both.roles.iter().map(|r| r.id.as_str()).collect();
    assert!(roles.contains(&"viewer") && roles.contains(&deleter.id.as_str()));
    assert_eq!(roles.len(), 2);

    // The listing, the breadcrumb and the sites see the same union.
    let (listed, _) = test
        .db
        .children(bob.id, t.b, None, 50)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(listed[0].role.as_ref(), Some(&both));
    let path = test.db.path(bob.id, t.file).await.unwrap().unwrap();
    assert_eq!(path.last().unwrap().role.as_ref(), Some(&both));
    let sites = test.db.sites_for(bob.id).await.unwrap();
    assert_eq!(sites[0].1.as_ref(), Some(&both));

    // Out of one group, Bob keeps the other's permissions.
    assert!(
        test.db
            .remove_member(admin.id, cleaners.id, bob.id)
            .await
            .unwrap()
    );
    assert_eq!(role(&test, bob.id, t.file).await, Some(Role::Viewer));
    assert!(
        test.db
            .add_member(admin.id, cleaners.id, bob.id)
            .await
            .unwrap()
    );
    assert!(
        test.db
            .remove_member(admin.id, groups.visitors, bob.id)
            .await
            .unwrap()
    );
    let left = access(&test, bob.id, t.file).await.unwrap();
    assert_eq!(left.perms, Perms::DELETE);
    assert_eq!(left.roles, vec![deleter.role_ref()]);
}

#[tokio::test]
async fn member_added_gains_and_removed_loses() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    let bob = user(&test, "Bob", false).await;
    let (site, library) = new_site(&test, ann.id, "Team").await;
    let groups = site_groups(&test, site).await;
    let folder = put(&test, new_id(), NodeKind::Folder, Some(library), site, "F").await;
    assert_eq!(role(&test, bob.id, folder).await, None);
    assert!(test.db.sites_for(bob.id).await.unwrap().is_empty());

    assert!(
        test.db
            .add_member(ann.id, groups.members, bob.id)
            .await
            .unwrap()
    );
    assert!(
        !test
            .db
            .add_member(ann.id, groups.members, bob.id)
            .await
            .unwrap(),
        "already a member"
    );
    assert_eq!(role(&test, bob.id, folder).await, Some(Role::Editor));
    assert_eq!(test.db.sites_for(bob.id).await.unwrap().len(), 1);
    assert!(
        test.db
            .children(bob.id, library, None, 50)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        test.db.groups_of(bob.id).await.unwrap(),
        vec![groups.members]
    );

    assert!(
        test.db
            .remove_member(ann.id, groups.members, bob.id)
            .await
            .unwrap()
    );
    assert!(
        !test
            .db
            .remove_member(ann.id, groups.members, bob.id)
            .await
            .unwrap(),
        "no longer a member"
    );
    assert_eq!(role(&test, bob.id, folder).await, None);
    assert!(test.db.sites_for(bob.id).await.unwrap().is_empty());
    assert_eq!(test.db.path(bob.id, folder).await.unwrap(), None);
    assert!(test.db.groups_of(bob.id).await.unwrap().is_empty());

    let events = node_events(&test, site).await;
    let kinds: Vec<&str> = events
        .iter()
        .filter(|(_, d)| d["group"] == json!(groups.members.0))
        .map(|(k, _)| k.as_str())
        .collect();
    assert_eq!(kinds, ["group.member_added", "group.member_removed"]);

    assert_eq!(
        test.db
            .add_member(ann.id, groups.members, UserId(Uuid::new_v4()))
            .await,
        Err(AccessError::NoSuchUser)
    );
    assert_eq!(
        test.db
            .add_member(ann.id, GroupId(Uuid::new_v4()), bob.id)
            .await,
        Err(AccessError::NoSuchGroup)
    );
}

#[tokio::test]
async fn site_owner_manages_site_groups_only() {
    let test = TestDb::create().await;
    let admin = user(&test, "Root", true).await;
    let ann = user(&test, "Ann", false).await;
    let bob = user(&test, "Bob", false).await;
    let carl = user(&test, "Carl", false).await;
    let (a, _) = new_site(&test, ann.id, "A").await;
    let (b, _) = new_site(&test, admin.id, "B").await;
    let (c, _) = new_site(&test, admin.id, "C").await;
    let a_groups = site_groups(&test, a).await;
    let b_groups = site_groups(&test, b).await;
    let c_groups = site_groups(&test, c).await;
    let org = test.db.create_group(admin.id, "Everyone").await.unwrap();
    test.db
        .add_member(admin.id, c_groups.visitors, ann.id)
        .await
        .unwrap();

    assert_eq!(
        test.db.add_member(ann.id, a_groups.visitors, bob.id).await,
        Ok(true)
    );
    // Site B is out of her sight; on site C she only views; organisation groups are for
    // administrators.
    assert_eq!(
        test.db.add_member(ann.id, b_groups.visitors, carl.id).await,
        Err(AccessError::NotFound)
    );
    assert_eq!(
        test.db.add_member(ann.id, c_groups.visitors, carl.id).await,
        Err(AccessError::Forbidden)
    );
    assert_eq!(
        test.db
            .remove_member(ann.id, c_groups.visitors, ann.id)
            .await,
        Err(AccessError::Forbidden)
    );
    assert_eq!(
        test.db.add_member(ann.id, org.id, carl.id).await,
        Err(AccessError::Forbidden)
    );
    // Editing on the site is not managing it.
    test.db
        .add_member(ann.id, a_groups.members, carl.id)
        .await
        .unwrap();
    assert_eq!(
        test.db.add_member(carl.id, a_groups.visitors, bob.id).await,
        Err(AccessError::Forbidden)
    );
    assert_eq!(
        test.db
            .remove_member(carl.id, a_groups.visitors, bob.id)
            .await,
        Err(AccessError::Forbidden)
    );
    // The site's owner removes; the administrator manages every group.
    assert_eq!(
        test.db
            .remove_member(ann.id, a_groups.visitors, bob.id)
            .await,
        Ok(true)
    );
    assert_eq!(
        test.db.add_member(admin.id, org.id, carl.id).await,
        Ok(true)
    );
    assert_eq!(
        test.db.add_member(admin.id, b_groups.owners, carl.id).await,
        Ok(true)
    );
}

#[tokio::test]
async fn organisation_groups_by_administrator_only() {
    let test = TestDb::create().await;
    let admin = user(&test, "Root", true).await;
    let ann = user(&test, "Ann", false).await;
    let (site, _) = new_site(&test, ann.id, "Site").await;

    assert_eq!(
        test.db.create_group(ann.id, "Mine").await,
        Err(AccessError::Forbidden)
    );
    let made = test.db.create_group(admin.id, "  Finance  ").await.unwrap();
    assert_eq!(made.name, "Finance");
    assert_eq!(
        (made.site, made.purpose, made.member_count),
        (None, None, 0)
    );
    assert_eq!(
        test.db.create_group(admin.id, "FINANCE").await,
        Err(AccessError::NameTaken)
    );
    assert_eq!(
        test.db.create_group(admin.id, "   ").await,
        Err(AccessError::InvalidName)
    );
    assert_eq!(
        test.db.create_group(admin.id, &"x".repeat(61)).await,
        Err(AccessError::InvalidName)
    );
    assert_eq!(
        test.db.delete_group(ann.id, made.id).await,
        Err(AccessError::Forbidden)
    );
    assert_eq!(
        test.db
            .delete_group(admin.id, site_groups(&test, site).await.owners)
            .await,
        Err(AccessError::SiteGroup)
    );
    test.db.add_member(admin.id, made.id, ann.id).await.unwrap();
    test.db.delete_group(admin.id, made.id).await.unwrap();
    assert_eq!(test.db.group(admin.id, made.id).await.unwrap(), None);
    assert_eq!(
        test.db.delete_group(admin.id, made.id).await,
        Err(AccessError::NoSuchGroup)
    );
    assert_eq!(
        test.db.groups_of(ann.id).await.unwrap(),
        vec![site_groups(&test, site).await.owners]
    );
    // The name is free again.
    test.db.create_group(admin.id, "finance").await.unwrap();

    let kinds: Vec<(String, serde_json::Value)> = sqlx::query_as(
        "select kind, detail from events where kind in ('group.created', 'group.deleted') \
           and node_id is null order by id",
    )
    .fetch_all(test.db.test_pool())
    .await
    .unwrap();
    assert_eq!(
        kinds.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
        ["group.created", "group.deleted", "group.created"]
    );
    assert_eq!(kinds[1].1["members"], json!(1));
}

#[tokio::test]
async fn group_names_hidden_without_manage_access() {
    let test = TestDb::create().await;
    let admin = user(&test, "Root", true).await;
    let ann = user(&test, "Ann", false).await;
    let carl = user(&test, "Carl", false).await;
    let dee = user(&test, "Dee", false).await;
    let (a, a_lib) = new_site(&test, ann.id, "Alpha").await;
    let (b, _) = new_site(&test, admin.id, "Secret").await;
    let org = test.db.create_group(admin.id, "Everyone").await.unwrap();
    let a_groups = site_groups(&test, a).await;
    let b_groups = site_groups(&test, b).await;
    test.db
        .add_member(ann.id, a_groups.visitors, carl.id)
        .await
        .unwrap();
    // Dee manages access to one folder of Alpha, and nothing else.
    let folder = put(&test, new_id(), NodeKind::Folder, Some(a_lib), a, "F").await;
    test.db
        .set_node_access(ann.id, folder, Principal::User(dee.id), Some("owner"))
        .await
        .unwrap();

    // Carl only views: no group names at all.
    assert!(test.db.groups_visible_to(carl.id).await.unwrap().is_empty());
    assert_eq!(test.db.group(carl.id, org.id).await.unwrap(), None);
    assert_eq!(test.db.group(carl.id, a_groups.owners).await.unwrap(), None);

    // Dee sees organisation groups and Alpha's (she reaches Alpha), never Secret's.
    let seen = names(&test.db.groups_visible_to(dee.id).await.unwrap());
    assert_eq!(
        seen,
        [
            "Alpha Members",
            "Alpha Owners",
            "Alpha Visitors",
            "Everyone"
        ]
    );
    assert!(
        test.db
            .group(dee.id, a_groups.members)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(test.db.group(dee.id, b_groups.owners).await.unwrap(), None);

    // The administrator sees every group.
    let all = names(&test.db.groups_visible_to(admin.id).await.unwrap());
    assert_eq!(all.len(), 7);
    assert!(all.contains(&"Secret Owners".to_owned()));

    // A disabled account sees nothing.
    test.db.set_disabled(admin.id, dee.id, true).await.unwrap();
    assert!(test.db.groups_visible_to(dee.id).await.unwrap().is_empty());
}

#[tokio::test]
async fn members_hidden_from_non_managers() {
    let test = TestDb::create().await;
    let admin = user(&test, "Root", true).await;
    let ann = user(&test, "Ann", false).await;
    let bob = user(&test, "Bob", false).await;
    let dee = user(&test, "Dee", false).await;
    let (site, library) = new_site(&test, ann.id, "Alpha").await;
    let groups = site_groups(&test, site).await;
    test.db
        .add_member(ann.id, groups.members, bob.id)
        .await
        .unwrap();
    let org = test.db.create_group(admin.id, "Everyone").await.unwrap();
    test.db.add_member(admin.id, org.id, bob.id).await.unwrap();
    let folder = put(&test, new_id(), NodeKind::Folder, Some(library), site, "F").await;
    test.db
        .set_node_access(ann.id, folder, Principal::User(dee.id), Some("owner"))
        .await
        .unwrap();

    // Dee manages access to a folder but not the site: she sees the group, not its members.
    let (shown, members) = test
        .db
        .group(dee.id, groups.members)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(shown.member_count, 1);
    assert!(members.is_empty());
    let (_, members) = test.db.group(dee.id, org.id).await.unwrap().unwrap();
    assert!(members.is_empty());

    // The site's owner sees its groups' members; organisation groups' only an administrator.
    let (_, members) = test
        .db
        .group(ann.id, groups.members)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(members.iter().map(|u| u.id).collect::<Vec<_>>(), [bob.id]);
    let (_, members) = test.db.group(ann.id, org.id).await.unwrap().unwrap();
    assert!(members.is_empty());
    let (_, members) = test.db.group(admin.id, org.id).await.unwrap().unwrap();
    assert_eq!(members.iter().map(|u| u.id).collect::<Vec<_>>(), [bob.id]);
}

#[tokio::test]
async fn assigned_group_cannot_be_deleted() {
    let test = TestDb::create().await;
    let admin = user(&test, "Root", true).await;
    let bob = user(&test, "Bob", false).await;
    let t = tree(&test, admin.id).await;
    let writers = test.db.create_group(admin.id, "Writers").await.unwrap();
    test.db
        .add_member(admin.id, writers.id, bob.id)
        .await
        .unwrap();
    test.db
        .set_node_access(admin.id, t.a, Principal::Group(writers.id), Some("editor"))
        .await
        .unwrap();
    assert_eq!(role(&test, bob.id, t.file).await, Some(Role::Editor));
    assert_eq!(
        test.db.delete_group(admin.id, writers.id).await,
        Err(AccessError::GroupInUse)
    );
    assert_eq!(role(&test, bob.id, t.file).await, Some(Role::Editor));

    test.db
        .set_node_access(admin.id, t.a, Principal::Group(writers.id), None)
        .await
        .unwrap();
    test.db.delete_group(admin.id, writers.id).await.unwrap();
    assert!(test.db.groups_of(bob.id).await.unwrap().is_empty());
    assert_eq!(
        test.db
            .set_node_access(admin.id, t.a, Principal::Group(writers.id), Some("viewer"))
            .await,
        Err(AccessError::NoSuchGroup)
    );

    // A deletion and an assignment of the same group take turns on the group's row. The
    // assignment queued first: the deletion then finds the group in use.
    let late = test.db.create_group(admin.id, "Late").await.unwrap();
    let held = group_gate(&test, late.id).await;
    let first = test.db.clone();
    let assigning = tokio::spawn(async move {
        first
            .set_node_access(admin.id, t.a, Principal::Group(late.id), Some("viewer"))
            .await
    });
    wait_blocked(&test, held.pid, 1).await;
    let second = test.db.clone();
    let deleting = tokio::spawn(async move { second.delete_group(admin.id, late.id).await });
    wait_blocked(&test, held.pid, 2).await;
    held.open().await;
    assert!(assigning.await.unwrap().is_ok());
    assert_eq!(deleting.await.unwrap(), Err(AccessError::GroupInUse));

    // The deletion queued first: the assignment then finds no such group.
    let gone = test.db.create_group(admin.id, "Gone").await.unwrap();
    let held = group_gate(&test, gone.id).await;
    let first = test.db.clone();
    let deleting = tokio::spawn(async move { first.delete_group(admin.id, gone.id).await });
    wait_blocked(&test, held.pid, 1).await;
    let second = test.db.clone();
    let assigning = tokio::spawn(async move {
        second
            .set_node_access(admin.id, t.b, Principal::Group(gone.id), Some("viewer"))
            .await
    });
    wait_blocked(&test, held.pid, 2).await;
    held.open().await;
    assert_eq!(deleting.await.unwrap(), Ok(()));
    assert_eq!(
        assigning.await.unwrap().map(|_| ()),
        Err(AccessError::NoSuchGroup)
    );
    let on_b: i64 = sqlx::query_scalar(
        "select count(*) from assignments where principal_kind = 'group' and principal_id = $1",
    )
    .bind(gone.id.0)
    .fetch_one(test.db.test_pool())
    .await
    .unwrap();
    assert_eq!(on_b, 0);
}

#[tokio::test]
async fn emptied_manager_group_leaves_scope_to_administrators() {
    let test = TestDb::create().await;
    let admin = user(&test, "Root", true).await;
    let ann = user(&test, "Ann", false).await;
    let bob = user(&test, "Bob", false).await;
    let (site, library) = new_site(&test, ann.id, "Alpha").await;
    let groups = site_groups(&test, site).await;

    // The Owners group's assignment is the site's only manager: it cannot be removed...
    assert_eq!(
        test.db
            .set_node_access(admin.id, site, Principal::Group(groups.owners), None)
            .await,
        Err(AccessError::LastManager)
    );
    // ...but the group can be emptied: stated, not prevented.
    assert_eq!(
        test.db.remove_member(ann.id, groups.owners, ann.id).await,
        Ok(true)
    );
    assert_eq!(role(&test, ann.id, library).await, None);
    assert_eq!(
        test.db.access(ann.id, site).await.map(|_| ()),
        Err(AccessError::NotFound)
    );
    // An administrator can always repair it.
    assert!(test.db.access(admin.id, site).await.is_ok());
    test.db
        .add_member(admin.id, groups.owners, bob.id)
        .await
        .unwrap();
    assert_eq!(role(&test, bob.id, library).await, Some(Role::Owner));
    assert_eq!(
        test.db.add_member(bob.id, groups.members, ann.id).await,
        Ok(true)
    );
}

/// A rename of `file` to `name` by `actor`, as the server mirrors one after its commit.
async fn rename(db: Db, actor: UserId, file: NodeId, name: &str) -> Result<(), AccessError> {
    let node = db.node(file).await.unwrap().unwrap();
    let row = NodeRow {
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
    db.apply_nodes(NodeChange {
        rows: std::slice::from_ref(&row),
        actor: Some(actor),
        needs: &[(file, Perms::EDIT)],
        ..NodeChange::default()
    })
    .await
}

#[tokio::test]
async fn removal_while_deciding_takes_turns() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    let xan = user(&test, "Xan", false).await;
    let t = tree(&test, ann.id).await;
    let groups = site_groups(&test, t.site).await;
    test.db
        .add_member(ann.id, groups.members, xan.id)
        .await
        .unwrap();
    assert_eq!(role(&test, xan.id, t.file).await, Some(Role::Editor));

    // The removal queues first on the group: the rename waits for it, then sees it and is
    // refused.
    let held = group_gate(&test, groups.members).await;
    let first = test.db.clone();
    let removal =
        tokio::spawn(async move { first.remove_member(ann.id, groups.members, xan.id).await });
    wait_blocked(&test, held.pid, 1).await;
    let renaming = tokio::spawn(rename(test.db.clone(), xan.id, t.file, "Late"));
    wait_blocked(&test, held.pid, 2).await;
    held.open().await;
    assert_eq!(removal.await.unwrap(), Ok(true));
    assert_eq!(renaming.await.unwrap(), Err(AccessError::NotFound));
    assert_eq!(test.db.node(t.file).await.unwrap().unwrap().name, "Notes");

    // The rename queues first: it is judged with Xan still a member, and the removal waits.
    test.db
        .add_member(ann.id, groups.members, xan.id)
        .await
        .unwrap();
    let held = group_gate(&test, groups.members).await;
    let renaming = tokio::spawn(rename(test.db.clone(), xan.id, t.file, "Early"));
    wait_blocked(&test, held.pid, 1).await;
    let second = test.db.clone();
    let removal =
        tokio::spawn(async move { second.remove_member(ann.id, groups.members, xan.id).await });
    wait_blocked(&test, held.pid, 2).await;
    held.open().await;
    assert_eq!(renaming.await.unwrap(), Ok(()));
    assert_eq!(removal.await.unwrap(), Ok(true));
    assert_eq!(test.db.node(t.file).await.unwrap().unwrap().name, "Early");
    assert_eq!(role(&test, xan.id, t.file).await, None);
}

#[tokio::test]
async fn membership_and_site_access_change_do_not_deadlock() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    let bob = user(&test, "Bob", false).await;
    let carl = user(&test, "Carl", false).await;
    let (site, _) = new_site(&test, ann.id, "Alpha").await;
    let groups = site_groups(&test, site).await;

    for round in 0..3 {
        // The membership change goes first: it holds the site (shared) and waits for a group;
        // the access change on the site then waits for the site. Locking groups before the
        // chain in either would deadlock here.
        let held = group_gate(&test, groups.owners).await;
        let first = test.db.clone();
        let adding =
            tokio::spawn(async move { first.add_member(ann.id, groups.members, bob.id).await });
        wait_blocked(&test, held.pid, 1).await;
        let second = test.db.clone();
        let granting = tokio::spawn(async move {
            second
                .set_node_access(ann.id, site, Principal::User(carl.id), Some("viewer"))
                .await
        });
        wait_blocked(&test, held.pid, 2).await;
        held.open().await;
        let added = tokio::time::timeout(Duration::from_secs(10), adding)
            .await
            .expect("the membership change finishes")
            .unwrap();
        let granted = tokio::time::timeout(Duration::from_secs(10), granting)
            .await
            .expect("the access change finishes")
            .unwrap();
        assert_eq!(added, Ok(true), "round {round}");
        assert!(granted.is_ok(), "round {round}: {granted:?}");

        // The access change goes first: it holds the site for update; the membership change
        // waits for the site behind it.
        let held = gate(&test, site).await;
        let first = test.db.clone();
        let revoking = tokio::spawn(async move {
            first
                .set_node_access(ann.id, site, Principal::User(carl.id), None)
                .await
        });
        wait_blocked(&test, held.pid, 1).await;
        let second = test.db.clone();
        let removing =
            tokio::spawn(async move { second.remove_member(ann.id, groups.members, bob.id).await });
        wait_blocked(&test, held.pid, 2).await;
        held.open().await;
        let revoked = tokio::time::timeout(Duration::from_secs(10), revoking)
            .await
            .expect("the access change finishes")
            .unwrap();
        let removed = tokio::time::timeout(Duration::from_secs(10), removing)
            .await
            .expect("the membership change finishes")
            .unwrap();
        assert!(revoked.is_ok(), "round {round}: {revoked:?}");
        assert_eq!(removed, Ok(true), "round {round}");
    }
    assert_eq!(role(&test, bob.id, site).await, None);
    assert_eq!(role(&test, carl.id, site).await, None);
}

#[tokio::test]
async fn membership_and_cross_site_move_do_not_deadlock() {
    let test = TestDb::create().await;
    let admin = user(&test, "Root", true).await;
    let ann = user(&test, "Ann", false).await;
    let bob = user(&test, "Bob", false).await;
    let (a, a_lib) = new_site(&test, ann.id, "A").await;
    let (b, b_lib) = new_site(&test, admin.id, "B").await;
    let groups = site_groups(&test, a).await;
    // Folder M holds F1, which holds F2; F2's id sorts before F1's, the opposite of their
    // depth. Visitors is assigned on both folders' own scopes.
    let mut ids = [Uuid::new_v4(), Uuid::new_v4()];
    ids.sort();
    let (f2, f1) = (NodeId(ids[0]), NodeId(ids[1]));
    let m = put(&test, new_id(), NodeKind::Folder, Some(a_lib), a, "M").await;
    put(&test, f1, NodeKind::Folder, Some(m), a, "F1").await;
    put(&test, f2, NodeKind::Folder, Some(f1), a, "F2").await;
    test.db
        .set_node_access(
            admin.id,
            f1,
            Principal::Group(groups.visitors),
            Some("editor"),
        )
        .await
        .unwrap();
    test.db
        .set_node_access(
            admin.id,
            f2,
            Principal::Group(groups.visitors),
            Some("viewer"),
        )
        .await
        .unwrap();
    assert!(test.db.node(f2).await.unwrap().unwrap().unique_scope);

    // The move to site B queues first on F1; then Ann adds Bob to Visitors, which is judged on
    // F1 and F2. Locking F2 before F1 would hold F2 while the move, holding F1, waits for it.
    let held = gate(&test, f1).await;
    let rows: Vec<NodeRow> = [
        (m, Some(b_lib), "M"),
        (f1, Some(m), "F1"),
        (f2, Some(f1), "F2"),
    ]
    .into_iter()
    .map(|(id, parent, name)| row(id, NodeKind::Folder, parent, b, name))
    .collect();
    let first = test.db.clone();
    let moving = tokio::spawn(async move {
        first
            .apply_nodes(NodeChange {
                rows: &rows,
                actor: Some(admin.id),
                needs: &[(m, Perms::EDIT), (b_lib, Perms::ADD)],
                ..NodeChange::default()
            })
            .await
    });
    wait_blocked(&test, held.pid, 1).await;
    let second = test.db.clone();
    let adding =
        tokio::spawn(async move { second.add_member(ann.id, groups.visitors, bob.id).await });
    wait_blocked(&test, held.pid, 2).await;
    held.open().await;
    let moved = tokio::time::timeout(Duration::from_secs(10), moving)
        .await
        .expect("the move finishes")
        .unwrap();
    let added = tokio::time::timeout(Duration::from_secs(10), adding)
        .await
        .expect("the membership change finishes")
        .unwrap();
    assert_eq!(moved, Ok(()));
    assert_eq!(added, Ok(true));
    assert_eq!(test.db.node(f2).await.unwrap().unwrap().site, b);
    assert_eq!(role(&test, bob.id, f2).await, Some(Role::Viewer));
}

#[tokio::test]
async fn member_moving_while_their_group_grows_do_not_deadlock() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    let bob = user(&test, "Bob", false).await;
    let xan = user(&test, "Xan", false).await;
    // Ann is in the Owners groups of both sites; A's Owners group is assigned on the site (on
    // the move's chain) and, copied by a break, on folder F1 inside the folder M she moves.
    let (a, a_lib) = new_site(&test, ann.id, "A").await;
    let (b, b_lib) = new_site(&test, ann.id, "B").await;
    let owners = site_groups(&test, a).await.owners;
    let m = put(&test, new_id(), NodeKind::Folder, Some(a_lib), a, "M").await;
    let f1 = put(&test, new_id(), NodeKind::Folder, Some(m), a, "F1").await;
    test.db
        .set_node_access(ann.id, f1, Principal::User(bob.id), Some("viewer"))
        .await
        .unwrap();

    // A gate shares F1: the move, which writes F1, waits for it; Ann's adding Xan to Owners,
    // which only shares F1, does not. Were the move to lock its group before the rows it
    // writes, it would wait for F1 holding Owners, while the addition, holding F1, waited for
    // Owners: a deadlock once the gate opened.
    let held = common::share_gate(&test, f1).await;
    let rows = [
        row(m, NodeKind::Folder, Some(b_lib), b, "M"),
        row(f1, NodeKind::Folder, Some(m), b, "F1"),
    ];
    let first = test.db.clone();
    let moving = tokio::spawn(async move {
        first
            .apply_nodes(NodeChange {
                rows: &rows,
                actor: Some(ann.id),
                needs: &[(m, Perms::EDIT), (b_lib, Perms::ADD)],
                ..NodeChange::default()
            })
            .await
    });
    wait_blocked(&test, held.pid, 1).await;
    let second = test.db.clone();
    let adding = tokio::spawn(async move { second.add_member(ann.id, owners, xan.id).await });
    // The addition either finishes behind the gate or is caught waiting behind it.
    for _ in 0..400 {
        if adding.is_finished() || common::blocked_behind(&test, held.pid).await >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    held.open().await;
    let moved = tokio::time::timeout(Duration::from_secs(10), moving)
        .await
        .expect("the move finishes")
        .unwrap();
    let added = tokio::time::timeout(Duration::from_secs(10), adding)
        .await
        .expect("the membership change finishes")
        .unwrap();
    assert_eq!(moved, Ok(()));
    assert_eq!(added, Ok(true));
    assert_eq!(test.db.node(f1).await.unwrap().unwrap().site, b);
    assert_eq!(role(&test, xan.id, f1).await, Some(Role::Owner));
}

#[tokio::test]
async fn cannot_add_to_a_group_beyond_own_access() {
    let test = TestDb::create().await;
    let admin = user(&test, "Root", true).await;
    let ann = user(&test, "Ann", false).await;
    let bob = user(&test, "Bob", false).await;
    let t = tree(&test, admin.id).await;
    let groups = site_groups(&test, t.site).await;
    // Ann manages the site and views it, nothing more.
    let steward = test
        .db
        .create_role(admin.id, "Steward", Perms::MANAGE_SITE)
        .await
        .unwrap();
    test.db
        .set_node_access(admin.id, t.site, Principal::User(ann.id), Some(&steward.id))
        .await
        .unwrap();

    assert_eq!(
        test.db.add_member(ann.id, groups.owners, bob.id).await,
        Err(AccessError::BeyondYourAccess)
    );
    assert_eq!(
        test.db.add_member(ann.id, groups.members, bob.id).await,
        Err(AccessError::BeyondYourAccess)
    );
    assert_eq!(
        test.db.add_member(ann.id, groups.visitors, bob.id).await,
        Ok(true)
    );
    assert_eq!(
        test.db.remove_member(ann.id, groups.visitors, bob.id).await,
        Ok(true)
    );

    // Visitors given Editor on folder A, where Ann holds only her copied Steward role: adding
    // to Visitors now gives more than she holds there.
    test.db
        .set_node_access(
            admin.id,
            t.a,
            Principal::Group(groups.visitors),
            Some("editor"),
        )
        .await
        .unwrap();
    assert_eq!(
        test.db.add_member(ann.id, groups.visitors, bob.id).await,
        Err(AccessError::BeyondYourAccess)
    );
    assert_eq!(role(&test, bob.id, t.file).await, None);

    // Administrators are exempt.
    assert_eq!(
        test.db.add_member(admin.id, groups.owners, bob.id).await,
        Ok(true)
    );
    assert_eq!(role(&test, bob.id, t.file).await, Some(Role::Owner));
}

#[tokio::test]
async fn site_names_never_collide_with_groups() {
    let test = TestDb::create().await;
    let admin = user(&test, "Root", true).await;
    let org = test
        .db
        .create_group(admin.id, "Marketing Owners")
        .await
        .unwrap();
    // A site named so that its Owners group reads like the organisation group.
    let (marketing, _) = new_site(&test, admin.id, "Marketing").await;
    let groups = site_groups(&test, marketing).await;
    let shown = test.db.groups_visible_to(admin.id).await.unwrap();
    let same: Vec<GroupId> = shown
        .iter()
        .filter(|g| g.name == "Marketing Owners")
        .map(|g| g.id)
        .collect();
    assert_eq!(same.len(), 2);
    assert!(same.contains(&org.id) && same.contains(&groups.owners));

    // Renamed, the site's groups follow, and no group is made or lost.
    put(&test, marketing, NodeKind::Site, None, marketing, "Sales").await;
    let (owners, _) = test
        .db
        .group(admin.id, groups.owners)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(owners.name, "Sales Owners");
    assert_eq!(site_groups(&test, marketing).await.owners, groups.owners);
    // An organisation group may take a name the site's groups had or have.
    test.db
        .create_group(admin.id, "Sales Members")
        .await
        .unwrap();

    // Deleted, the site keeps its groups; a new site of the same name gets its own.
    common::delete(&test, marketing).await;
    let (again, _) = new_site(&test, admin.id, "Sales").await;
    let again_groups = site_groups(&test, again).await;
    assert_ne!(again_groups.owners, groups.owners);
    site_groups(&test, marketing).await;
    let site_group_count: i64 =
        sqlx::query_scalar("select count(*) from groups where site_id is not null")
            .fetch_one(test.db.test_pool())
            .await
            .unwrap();
    assert_eq!(site_group_count, 6);
    let (again_owners, _) = test
        .db
        .group(admin.id, again_groups.owners)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(again_owners.name, "Sales Owners");
}

// The upgrade from plan 1.

/// Writes a node as plan 1 held it, without the code of this plan (which needs its tables).
async fn plan1_node(
    test: &TestDb,
    kind: NodeKind,
    parent: Option<NodeId>,
    site: Option<NodeId>,
    name: &str,
    deleted: bool,
) -> NodeId {
    let id = new_id();
    common::raw_node(test, id, kind, parent, site.unwrap_or(id), name, deleted).await
}

/// Makes `node` a scope, as plan 1's break did, with the given assignments.
async fn plan1_break(test: &TestDb, node: NodeId, grants: &[(UserId, Role)]) {
    sqlx::query("update nodes set unique_scope = true where id = $1")
        .bind(node.0)
        .execute(test.db.test_pool())
        .await
        .unwrap();
    for (user, role) in grants {
        grant(test, node, *user, *role).await;
    }
}

/// Everything each person can see of every node: their access, breadcrumb and children, and
/// their sites. Compared as text, before and after.
async fn everything(db: &Db, people: &[UserId], nodes: &[NodeId]) -> Vec<String> {
    let mut seen = Vec::new();
    for person in people {
        seen.push(format!(
            "{person:?} sites {:?}",
            db.sites_for(*person).await.unwrap()
        ));
        for node in nodes {
            seen.push(format!(
                "{person:?} {node:?} access {:?} path {:?} children {:?}",
                db.effective_access(*person, *node).await.unwrap(),
                db.path(*person, *node).await.unwrap(),
                db.children(*person, *node, None, 1000).await.unwrap(),
            ));
        }
    }
    seen
}

#[tokio::test]
async fn upgrade_keeps_everyones_access() {
    let test = TestDb::create_at(Some(4)).await;
    let admin = user(&test, "Root", true).await;
    let ann = user(&test, "Ann", false).await;
    let bob = user(&test, "Bob", false).await;
    let carl = user(&test, "Carl", false).await;
    let dee = user(&test, "Dee", false).await;
    let eve = user(&test, "Eve", false).await;
    let fay = user(&test, "Fay", false).await;
    sqlx::query("update users set disabled_at = now() where id = $1")
        .bind(eve.id.0)
        .execute(test.db.test_pool())
        .await
        .unwrap();

    // Alpha: owners, an editor and viewers on the site; a broken folder with its own grants,
    // an inheriting one, and a file in each.
    let alpha = plan1_node(&test, NodeKind::Site, None, None, "Alpha", false).await;
    for (person, role) in [
        (admin.id, Role::Owner),
        (ann.id, Role::Owner),
        (bob.id, Role::Editor),
        (carl.id, Role::Viewer),
        (eve.id, Role::Viewer),
    ] {
        grant(&test, alpha, person, role).await;
    }
    let docs = plan1_node(
        &test,
        NodeKind::Library,
        Some(alpha),
        Some(alpha),
        "Documents",
        false,
    )
    .await;
    let broken = plan1_node(
        &test,
        NodeKind::Folder,
        Some(docs),
        Some(alpha),
        "Private",
        false,
    )
    .await;
    plan1_break(
        &test,
        broken,
        &[
            (ann.id, Role::Owner),
            (bob.id, Role::Viewer),
            (dee.id, Role::Editor),
        ],
    )
    .await;
    let inside = plan1_node(
        &test,
        NodeKind::Document,
        Some(broken),
        Some(alpha),
        "a.txt",
        false,
    )
    .await;
    let open = plan1_node(
        &test,
        NodeKind::Folder,
        Some(docs),
        Some(alpha),
        "Open",
        false,
    )
    .await;
    let file = plan1_node(
        &test,
        NodeKind::Document,
        Some(open),
        Some(alpha),
        "b.txt",
        false,
    )
    .await;
    // A file broken below a folder where Fay has nothing else.
    let shared = plan1_node(
        &test,
        NodeKind::Document,
        Some(open),
        Some(alpha),
        "c.txt",
        false,
    )
    .await;
    plan1_break(
        &test,
        shared,
        &[(ann.id, Role::Owner), (fay.id, Role::Viewer)],
    )
    .await;
    // Beta, deleted.
    let beta = plan1_node(&test, NodeKind::Site, None, None, "Beta", true).await;
    grant(&test, beta, ann.id, Role::Owner).await;
    grant(&test, beta, carl.id, Role::Editor).await;
    let beta_docs = plan1_node(
        &test,
        NodeKind::Library,
        Some(beta),
        Some(beta),
        "Documents",
        false,
    )
    .await;
    // Two sites named Gamma, one deleted.
    let gamma = plan1_node(&test, NodeKind::Site, None, None, "Gamma", false).await;
    grant(&test, gamma, bob.id, Role::Owner).await;
    grant(&test, gamma, dee.id, Role::Viewer).await;
    let gamma_docs = plan1_node(
        &test,
        NodeKind::Library,
        Some(gamma),
        Some(gamma),
        "Documents",
        false,
    )
    .await;
    let old_gamma = plan1_node(&test, NodeKind::Site, None, None, "Gamma", true).await;
    grant(&test, old_gamma, carl.id, Role::Owner).await;

    let people = [admin.id, ann.id, bob.id, carl.id, dee.id, eve.id, fay.id];
    let nodes = [
        alpha, docs, broken, inside, open, file, shared, beta, beta_docs, gamma, gamma_docs,
        old_gamma,
    ];
    // Plan 1's access, worked out by this plan's code with no group membership at all: an
    // empty stand-in for the table the upgrade makes.
    sqlx::query("create table group_members (group_id uuid, user_id uuid)")
        .execute(test.db.test_pool())
        .await
        .unwrap();
    let before = everything(&test.db, &people, &nodes).await;
    sqlx::query("drop table group_members")
        .execute(test.db.test_pool())
        .await
        .unwrap();
    let below_before: Vec<(Uuid, Uuid, String)> = sqlx::query_as(
        "select a.node_id, a.principal_id, a.role_id from assignments a \
         join nodes n on n.id = a.node_id where n.parent_id is not null order by 1, 2",
    )
    .fetch_all(test.db.test_pool())
    .await
    .unwrap();

    test.db.migrate().await.unwrap();

    let after = everything(&test.db, &people, &nodes).await;
    for (was, is) in before.iter().zip(&after) {
        assert_eq!(was, is);
    }
    assert_eq!(before.len(), after.len());

    // Every site has its three groups; no person keeps an assignment on a site; the grants
    // below sites are as they were.
    for s in [alpha, beta, gamma, old_gamma] {
        site_groups(&test, s).await;
        assert_eq!(user_rows(&test, s).await, 0);
    }
    let below_after: Vec<(Uuid, Uuid, String)> = sqlx::query_as(
        "select a.node_id, a.principal_id, a.role_id from assignments a \
         join nodes n on n.id = a.node_id where n.parent_id is not null order by 1, 2",
    )
    .fetch_all(test.db.test_pool())
    .await
    .unwrap();
    assert_eq!(below_before, below_after);

    let alpha_groups = site_groups(&test, alpha).await;
    let (_, owners) = test
        .db
        .group(admin.id, alpha_groups.owners)
        .await
        .unwrap()
        .unwrap();
    let mut owner_ids: Vec<UserId> = owners.iter().map(|u| u.id).collect();
    owner_ids.sort();
    let mut expected = vec![admin.id, ann.id];
    expected.sort();
    assert_eq!(owner_ids, expected);
    let beta_groups = site_groups(&test, beta).await;
    let (_, editors) = test
        .db
        .group(admin.id, beta_groups.members)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(editors.iter().map(|u| u.id).collect::<Vec<_>>(), [carl.id]);

    let upgraded: Vec<(Option<Uuid>, serde_json::Value)> = sqlx::query_as(
        "select subject, detail from events where kind = 'group.member_added' order by id",
    )
    .fetch_all(test.db.test_pool())
    .await
    .unwrap();
    assert_eq!(upgraded.len(), 10);
    assert!(upgraded.iter().all(|(_, d)| d["upgrade"] == json!(true)));
}
