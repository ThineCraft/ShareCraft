#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use collab_core::ids::{GroupId, NodeId, UserId};
use common::{
    Role, Tree, delete, gate, grant, new_id, node_events, put, role_of, tree, user, wait_blocked,
};
use records::test_support::TestDb;
use records::{
    AccessError, ExplorerFilter, Link, LinkError, LinkKind, LinkToken, NodeKind, Perms, Principal,
    PrincipalView, breaks_csv,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

fn in_days(days: i64) -> OffsetDateTime {
    OffsetDateTime::now_utc() + Duration::days(days)
}

async fn role(test: &TestDb, user: UserId, node: NodeId) -> Option<Role> {
    role_of(test.db.effective_access(user, node).await.unwrap())
}

async fn make(
    test: &TestDb,
    actor: UserId,
    node: NodeId,
    kind: LinkKind,
    role: &str,
    expires_at: Option<OffsetDateTime>,
) -> (Link, LinkToken) {
    test.db
        .create_link(actor, node, kind, role, expires_at)
        .await
        .unwrap()
}

async fn signed_in(test: &TestDb, actor: UserId, node: NodeId, role: Role) -> (Link, LinkToken) {
    make(test, actor, node, LinkKind::SignedIn, role.id(), None).await
}

/// An administrator, with anyone links switched on.
async fn anyone_links_on(test: &TestDb) -> UserId {
    let admin = user(test, "Admin", true).await;
    test.db
        .set_setting(admin.id, records::ANYONE_LINKS, json!(true))
        .await
        .unwrap();
    admin.id
}

async fn link_group(test: &TestDb, link: Uuid) -> GroupId {
    let id: Uuid = sqlx::query_scalar("select group_id from links where id = $1")
        .bind(link)
        .fetch_one(test.db.test_pool())
        .await
        .unwrap();
    GroupId(id)
}

async fn count(test: &TestDb, query: &str, id: Uuid) -> i64 {
    sqlx::query_scalar(sqlx::AssertSqlSafe(query.to_owned()))
        .bind(id)
        .fetch_one(test.db.test_pool())
        .await
        .unwrap()
}

async fn members(test: &TestDb, group: GroupId) -> i64 {
    count(
        test,
        "select count(*) from group_members where group_id = $1",
        group.0,
    )
    .await
}

async fn group_assignments(test: &TestDb, group: GroupId) -> i64 {
    count(
        test,
        "select count(*) from assignments where principal_kind = 'group' and principal_id = $1",
        group.0,
    )
    .await
}

/// How many events of `kind` name the link `link` in their detail.
async fn link_events(test: &TestDb, kind: &str, link: Uuid) -> i64 {
    sqlx::query_scalar("select count(*) from events where kind = $1 and detail->>'link' = $2")
        .bind(kind)
        .bind(link.to_string())
        .fetch_one(test.db.test_pool())
        .await
        .unwrap()
}

async fn link_now(test: &TestDb, link: Uuid) -> Link {
    test.db.link(link).await.unwrap().unwrap()
}

async fn is_scope(test: &TestDb, node: NodeId) -> bool {
    test.db.node(node).await.unwrap().unwrap().unique_scope
}

async fn folder(test: &TestDb, parent: NodeId, site: NodeId, name: &str) -> NodeId {
    put(test, new_id(), NodeKind::Folder, Some(parent), site, name).await
}

/// A text file named `name` under `parent`.
async fn file(test: &TestDb, parent: NodeId, site: NodeId, name: &str) -> NodeId {
    put(test, new_id(), NodeKind::Document, Some(parent), site, name).await
}

/// Whether the anyone link with `token` reaches `node`.
async fn reaches(test: &TestDb, token: &LinkToken, node: NodeId) -> bool {
    let grant = test.db.anyone_link(token, Some(node)).await.unwrap();
    if let Some(grant) = &grant {
        assert_eq!(grant.node.id, node);
    }
    grant.is_some()
}

/// Every child of `parent` the anyone link lists, all pages of two.
async fn anyone_listing(test: &TestDb, token: &LinkToken, parent: NodeId) -> Option<Vec<NodeId>> {
    let mut found = Vec::new();
    let mut after = None;
    loop {
        let (page, next) = test
            .db
            .anyone_children(token, parent, after.as_ref(), 2)
            .await
            .unwrap()?;
        assert!(page.len() <= 2);
        found.extend(page.into_iter().map(|listed| listed.node.id));
        match next {
            Some(next) => after = Some(next),
            None => return Some(found),
        }
    }
}

#[tokio::test]
async fn signed_in_link_grants_its_role_on_use() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    let bob = user(&test, "Bob", false).await;
    let t = tree(&test, ann.id).await;
    let (link, token) = signed_in(&test, ann.id, t.a, Role::Editor).await;
    assert_eq!(link.node, t.a);
    assert_eq!(link.kind, LinkKind::SignedIn);
    assert_eq!(link.role.as_ref().unwrap().id, "editor");
    assert_eq!(link.created_by.id, ann.id);
    assert_eq!(link.expires_at, None);
    assert_eq!(link.revoked, None);
    assert_eq!(link.members, 0);
    assert_eq!(link_events(&test, "link.created", link.id).await, 1);

    assert_eq!(role(&test, bob.id, t.file).await, None);
    assert_eq!(test.db.redeem_signed_in(bob.id, &token).await, Ok(t.a));
    for node in [t.a, t.b, t.file] {
        assert_eq!(role(&test, bob.id, node).await, Some(Role::Editor));
    }
    assert_eq!(role(&test, bob.id, t.library).await, None);
    // Opening it again changes nothing and records nothing more.
    assert_eq!(test.db.redeem_signed_in(bob.id, &token).await, Ok(t.a));
    assert_eq!(link_events(&test, "link.used", link.id).await, 1);
    let shown = test.db.links_on(ann.id, t.a).await.unwrap();
    assert_eq!(shown.len(), 1);
    assert_eq!(shown[0].members, 1);

    // Only people who manage access to the item see its links.
    assert_eq!(
        test.db.links_on(bob.id, t.a).await,
        Err(AccessError::Forbidden)
    );
    let stranger = user(&test, "Stranger", false).await;
    assert_eq!(
        test.db.links_on(stranger.id, t.a).await,
        Err(AccessError::NotFound)
    );
    // An unknown token, and a disabled account, open nothing.
    let unknown = LinkToken::parse(&"ab".repeat(32)).unwrap();
    assert_eq!(
        test.db.redeem_signed_in(stranger.id, &unknown).await,
        Err(LinkError::NotFound)
    );
}

#[tokio::test]
async fn link_on_inheriting_item_breaks_inheritance_first() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    let vic = user(&test, "Vic", false).await;
    let t = tree(&test, ann.id).await;
    grant(&test, t.site, vic.id, Role::Viewer).await;
    assert!(!is_scope(&test, t.a).await);
    let (link, _) = signed_in(&test, ann.id, t.a, Role::Viewer).await;
    assert!(is_scope(&test, t.a).await);
    // The inherited assignments were copied first, so nobody lost access.
    assert_eq!(role(&test, ann.id, t.a).await, Some(Role::Owner));
    assert_eq!(role(&test, vic.id, t.file).await, Some(Role::Viewer));
    let kinds: Vec<String> = node_events(&test, t.a)
        .await
        .into_iter()
        .map(|(kind, _)| kind)
        .collect();
    assert_eq!(
        kinds,
        vec!["inheritance.broken", "access.granted", "link.created"]
    );
    let group = link_group(&test, link.id).await;
    assert_eq!(group_assignments(&test, group).await, 1);

    // On a node that is already a scope, nothing more is broken.
    signed_in(&test, ann.id, t.a, Role::Editor).await;
    let broken = node_events(&test, t.a)
        .await
        .into_iter()
        .filter(|(kind, _)| kind == "inheritance.broken")
        .count();
    assert_eq!(broken, 1);
}

#[tokio::test]
async fn link_role_within_creator_access() {
    let test = TestDb::create().await;
    let admin = user(&test, "Admin", true).await;
    let ann = user(&test, "Ann", false).await;
    let cur = user(&test, "Cur", false).await;
    let ed = user(&test, "Ed", false).await;
    let bob = user(&test, "Bob", false).await;
    let t = tree(&test, ann.id).await;
    grant(&test, t.site, ed.id, Role::Editor).await;
    // A curator manages access but may not edit.
    let curator = test
        .db
        .create_role(admin.id, "Curator", Perms::MANAGE_ACCESS)
        .await
        .unwrap();
    common::grant_role(&test, t.site, cur.id, &curator.id).await;

    assert_eq!(
        test.db
            .create_link(ed.id, t.a, LinkKind::SignedIn, "viewer", None)
            .await
            .map(|_| ()),
        Err(AccessError::Forbidden)
    );
    assert_eq!(
        test.db
            .create_link(cur.id, t.a, LinkKind::SignedIn, "editor", None)
            .await
            .map(|_| ()),
        Err(AccessError::BeyondYourAccess)
    );
    assert_eq!(
        test.db
            .create_link(cur.id, t.a, LinkKind::SignedIn, "no-such-role", None)
            .await
            .map(|_| ()),
        Err(AccessError::NoSuchRole)
    );
    signed_in(&test, cur.id, t.a, Role::Viewer).await;
    // An administrator holds every permission.
    signed_in(&test, admin.id, t.b, Role::Editor).await;

    // A link made by an owner keeps giving exactly its role after the owner is lowered.
    let (link, token) = signed_in(&test, ann.id, t.a, Role::Editor).await;
    test.db.redeem_signed_in(bob.id, &token).await.unwrap();
    test.db
        .set_node_access(
            admin.id,
            t.a,
            Principal::User(ann.id),
            Some(Role::Viewer.id()),
        )
        .await
        .unwrap();
    assert_eq!(role(&test, ann.id, t.a).await, Some(Role::Viewer));
    // B was broken (by the administrator's link) before this link was made, so it reaches A.
    assert_eq!(role(&test, bob.id, t.a).await, Some(Role::Editor));
    assert_eq!(link_now(&test, link.id).await.revoked, None);
    // Lowered, Ann may make no more links there.
    assert_eq!(
        test.db
            .create_link(ann.id, t.a, LinkKind::SignedIn, "viewer", None)
            .await
            .map(|_| ()),
        Err(AccessError::Forbidden)
    );
}

#[tokio::test]
async fn link_roles_never_manage() {
    let test = TestDb::create().await;
    let admin = anyone_links_on(&test).await;
    let ann = user(&test, "Ann", false).await;
    let t = tree(&test, ann.id).await;
    assert_eq!(
        test.db
            .create_link(ann.id, t.a, LinkKind::SignedIn, "owner", None)
            .await
            .map(|_| ()),
        Err(AccessError::LinkRoleTooHigh)
    );
    let steward = test
        .db
        .create_role(admin, "Steward", Perms::MANAGE_SITE)
        .await
        .unwrap();
    assert_eq!(
        test.db
            .create_link(admin, t.a, LinkKind::SignedIn, &steward.id, None)
            .await
            .map(|_| ()),
        Err(AccessError::LinkRoleTooHigh)
    );
    // Anyone links view only.
    assert_eq!(
        test.db
            .create_link(ann.id, t.a, LinkKind::Anyone, "editor", Some(in_days(1)))
            .await
            .map(|_| ()),
        Err(AccessError::LinkRoleTooHigh)
    );
    // Nothing was made by the refusals.
    assert!(!is_scope(&test, t.a).await);

    // A role a live link gives may not become a managing one.
    let reviewer = test
        .db
        .create_role(admin, "Reviewer", Perms::EDIT)
        .await
        .unwrap();
    let (link, _) = make(&test, ann.id, t.a, LinkKind::SignedIn, &reviewer.id, None).await;
    assert_eq!(
        test.db
            .change_role(admin, &reviewer.id, None, Some(Perms::MANAGE_ACCESS))
            .await
            .map(|_| ()),
        Err(AccessError::LinkRoleTooHigh)
    );
    test.db
        .change_role(admin, &reviewer.id, Some("Checker"), None)
        .await
        .unwrap();
    // Nor be deleted while a live link gives it; once revoked, it may.
    test.db
        .revoke_link(Some(ann.id), link.id, "manager")
        .await
        .unwrap();
    test.db.delete_role(admin, &reviewer.id).await.unwrap();
    assert_eq!(link_now(&test, link.id).await.role, None);
}

#[tokio::test]
async fn link_groups_cannot_be_changed_elsewhere() {
    let test = TestDb::create().await;
    let admin = user(&test, "Admin", true).await;
    let ann = user(&test, "Ann", false).await;
    let bob = user(&test, "Bob", false).await;
    let t = tree(&test, ann.id).await;
    let (link, token) = signed_in(&test, ann.id, t.a, Role::Editor).await;
    test.db.redeem_signed_in(bob.id, &token).await.unwrap();
    let group = link_group(&test, link.id).await;

    for actor in [ann.id, admin.id] {
        assert_eq!(
            test.db
                .set_node_access(actor, t.b, Principal::Group(group), Some("viewer"))
                .await,
            Err(AccessError::LinkGroup)
        );
        assert_eq!(
            test.db
                .set_node_access(actor, t.a, Principal::Group(group), None)
                .await,
            Err(AccessError::LinkGroup)
        );
    }
    assert_eq!(
        test.db.add_member(admin.id, group, admin.id).await,
        Err(AccessError::LinkGroup)
    );
    assert_eq!(
        test.db.remove_member(admin.id, group, bob.id).await,
        Err(AccessError::LinkGroup)
    );
    assert_eq!(
        test.db.delete_group(admin.id, group).await,
        Err(AccessError::LinkGroup)
    );
    assert_eq!(
        test.db.manages_group(admin.id, group).await,
        Err(AccessError::LinkGroup)
    );
    assert_eq!(members(&test, group).await, 1);
    assert_eq!(group_assignments(&test, group).await, 1);

    // Never listed, never offered in a filter.
    for viewer in [admin.id, ann.id] {
        let listed = test.db.groups_visible_to(viewer).await.unwrap();
        assert!(listed.iter().all(|g| g.id != group));
        assert_eq!(test.db.group(viewer, group).await.unwrap(), None);
    }
    for filter in [Principal::Group(group), Principal::User(bob.id)] {
        let filter = ExplorerFilter {
            principal: Some(filter),
            kinds: Vec::new(),
        };
        let (found, _) = test
            .db
            .breaks(admin.id, t.site, &filter, None, 50)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found, Vec::new());
    }
}

#[tokio::test]
async fn revoke_removes_copies_below() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    let bob = user(&test, "Bob", false).await;
    let t = tree(&test, ann.id).await;
    let (link, token) = signed_in(&test, ann.id, t.a, Role::Editor).await;
    test.db.redeem_signed_in(bob.id, &token).await.unwrap();
    // Breaks below the item after the link was made copy its assignment.
    assert!(test.db.break_inheritance(ann.id, t.b).await.unwrap());
    assert!(test.db.break_inheritance(ann.id, t.file).await.unwrap());
    let group = link_group(&test, link.id).await;
    assert_eq!(group_assignments(&test, group).await, 3);
    assert_eq!(role(&test, bob.id, t.file).await, Some(Role::Editor));

    let mut removed = test
        .db
        .revoke_link(Some(ann.id), link.id, "manager")
        .await
        .unwrap()
        .unwrap();
    removed.sort();
    let mut expected = vec![t.a, t.b, t.file];
    expected.sort();
    assert_eq!(removed, expected);
    for node in [t.a, t.b, t.file] {
        assert_eq!(role(&test, bob.id, node).await, None);
    }
    assert_eq!(group_assignments(&test, group).await, 0);
    assert_eq!(members(&test, group).await, 0);
    // Ann keeps her own access everywhere.
    assert_eq!(role(&test, ann.id, t.file).await, Some(Role::Owner));
}

#[tokio::test]
async fn restore_that_drops_a_link_revokes_it() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    let bob = user(&test, "Bob", false).await;
    let t = tree(&test, ann.id).await;

    // The link's only assignment is on B: restoring B revokes it.
    let (link, token) = signed_in(&test, ann.id, t.b, Role::Editor).await;
    test.db.redeem_signed_in(bob.id, &token).await.unwrap();
    assert!(test.db.restore_inheritance(ann.id, t.b).await.unwrap());
    let now = link_now(&test, link.id).await;
    let (by, _, reason) = now.revoked.unwrap();
    assert_eq!(by.map(|u| u.id), Some(ann.id));
    assert_eq!(reason, "inheritance restored");
    assert_eq!(role(&test, bob.id, t.file).await, None);
    assert_eq!(members(&test, link_group(&test, link.id).await).await, 0);
    assert_eq!(
        test.db.redeem_signed_in(bob.id, &token).await,
        Err(LinkError::NotFound)
    );

    // A link on A whose copy on B goes keeps working on A.
    let (link, token) = signed_in(&test, ann.id, t.a, Role::Editor).await;
    test.db.redeem_signed_in(bob.id, &token).await.unwrap();
    assert!(test.db.break_inheritance(ann.id, t.b).await.unwrap());
    assert!(test.db.restore_inheritance(ann.id, t.b).await.unwrap());
    assert_eq!(link_now(&test, link.id).await.revoked, None);
    assert_eq!(role(&test, bob.id, t.file).await, Some(Role::Editor));
}

#[tokio::test]
async fn user_with_enough_access_is_not_added() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    let ed = user(&test, "Ed", false).await;
    let vic = user(&test, "Vic", false).await;
    let t = tree(&test, ann.id).await;
    grant(&test, t.site, ed.id, Role::Editor).await;
    grant(&test, t.site, vic.id, Role::Viewer).await;
    let (link, token) = signed_in(&test, ann.id, t.a, Role::Editor).await;
    let group = link_group(&test, link.id).await;
    assert_eq!(test.db.redeem_signed_in(ed.id, &token).await, Ok(t.a));
    assert_eq!(test.db.redeem_signed_in(ann.id, &token).await, Ok(t.a));
    assert_eq!(members(&test, group).await, 0);
    assert_eq!(link_events(&test, "link.used", link.id).await, 0);
    // Someone with less access is added.
    assert_eq!(test.db.redeem_signed_in(vic.id, &token).await, Ok(t.a));
    assert_eq!(members(&test, group).await, 1);
    let access = test
        .db
        .effective_access(vic.id, t.file)
        .await
        .unwrap()
        .unwrap();
    assert!(access.perms.contains(Perms::EDIT));
}

#[tokio::test]
async fn revoke_removes_members_and_assignment() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    let bob = user(&test, "Bob", false).await;
    let cat = user(&test, "Cat", false).await;
    let t = tree(&test, ann.id).await;
    let (link, token) = signed_in(&test, ann.id, t.a, Role::Editor).await;
    test.db.redeem_signed_in(bob.id, &token).await.unwrap();
    test.db.redeem_signed_in(cat.id, &token).await.unwrap();
    let group = link_group(&test, link.id).await;
    assert_eq!(members(&test, group).await, 2);

    let removed = test
        .db
        .revoke_link(Some(ann.id), link.id, "manager")
        .await
        .unwrap();
    assert_eq!(removed, Some(vec![t.a]));
    assert_eq!(members(&test, group).await, 0);
    assert_eq!(group_assignments(&test, group).await, 0);
    for person in [bob.id, cat.id] {
        assert_eq!(role(&test, person, t.file).await, None);
    }
    let now = link_now(&test, link.id).await;
    let (by, _, reason) = now.revoked.unwrap();
    assert_eq!(by.map(|u| u.id), Some(ann.id));
    assert_eq!(reason, "manager");
    assert_eq!(now.members, 0);
    let events = node_events(&test, t.a).await;
    let revoked: Vec<_> = events
        .iter()
        .filter(|(kind, _)| kind == "link.revoked")
        .collect();
    assert_eq!(revoked.len(), 1);
    assert_eq!(revoked[0].1["reason"], "manager");
    assert_eq!(revoked[0].1["members"], 2);
    assert!(events.iter().any(|(kind, detail)| kind == "access.revoked"
        && detail["group"] == json!(group.0)
        && detail["reason"] == "link revoked"));
    // The token works no more, and the item keeps its own permissions.
    assert_eq!(
        test.db.redeem_signed_in(bob.id, &token).await,
        Err(LinkError::NotFound)
    );
    assert!(is_scope(&test, t.a).await);
}

#[tokio::test]
async fn revoke_is_idempotent() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    let ed = user(&test, "Ed", false).await;
    let t = tree(&test, ann.id).await;
    grant(&test, t.site, ed.id, Role::Editor).await;
    let (link, _) = signed_in(&test, ann.id, t.a, Role::Viewer).await;
    // Only someone who manages access to the item may revoke.
    assert_eq!(
        test.db.revoke_link(Some(ed.id), link.id, "manager").await,
        Err(AccessError::Forbidden)
    );
    assert_eq!(
        test.db
            .revoke_link(Some(ann.id), Uuid::new_v4(), "manager")
            .await,
        Err(AccessError::NotFound)
    );
    assert!(
        test.db
            .revoke_link(Some(ann.id), link.id, "manager")
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        test.db.revoke_link(Some(ann.id), link.id, "manager").await,
        Ok(None)
    );
    assert_eq!(
        test.db.revoke_link(None, link.id, "expired").await,
        Ok(None)
    );
    assert_eq!(link_events(&test, "link.revoked", link.id).await, 1);
    let (_, _, reason) = link_now(&test, link.id).await.revoked.unwrap();
    assert_eq!(reason, "manager");
}

#[tokio::test]
async fn anyone_links_need_the_setting_and_an_expiry() {
    let test = TestDb::create().await;
    let admin = user(&test, "Admin", true).await;
    let ann = user(&test, "Ann", false).await;
    let t = tree(&test, ann.id).await;
    let settings = test.db.sharing_settings().await.unwrap();
    assert!(!settings.anyone_links);
    assert_eq!(settings.anyone_link_max_days, 30);
    let attempt = |expires: Option<OffsetDateTime>| {
        let db = test.db.clone();
        async move {
            db.create_link(ann.id, t.a, LinkKind::Anyone, "viewer", expires)
                .await
                .map(|_| ())
        }
    };
    assert_eq!(
        attempt(Some(in_days(1))).await,
        Err(AccessError::AnyoneLinksOff)
    );

    // Only administrators change settings, and only known ones, with values of their type.
    assert_eq!(
        test.db
            .set_setting(ann.id, records::ANYONE_LINKS, json!(true))
            .await,
        Err(AccessError::Forbidden)
    );
    for (key, value) in [
        (records::ANYONE_LINKS, json!("yes")),
        (records::ANYONE_LINK_MAX_DAYS, json!(0)),
        (records::ANYONE_LINK_MAX_DAYS, json!(366)),
        (records::ANYONE_LINK_MAX_DAYS, json!(2.5)),
        ("colour", json!(true)),
    ] {
        assert_eq!(
            test.db.set_setting(admin.id, key, value).await,
            Err(AccessError::InvalidSetting)
        );
    }
    test.db
        .set_setting(admin.id, records::ANYONE_LINKS, json!(true))
        .await
        .unwrap();
    assert_eq!(
        test.db.setting(records::ANYONE_LINKS).await.unwrap(),
        Some(json!(true))
    );
    let changed: Vec<serde_json::Value> =
        sqlx::query_scalar("select detail from events where kind = 'setting.changed'")
            .fetch_all(test.db.test_pool())
            .await
            .unwrap();
    assert_eq!(
        changed,
        vec![json!({ "key": "anyone_links", "from": false, "to": true })]
    );

    assert_eq!(attempt(None).await, Err(AccessError::ExpiryRequired));
    assert_eq!(
        attempt(Some(in_days(31))).await,
        Err(AccessError::ExpiryTooLong)
    );
    assert_eq!(
        attempt(Some(in_days(-1))).await,
        Err(AccessError::ExpiryInPast)
    );
    assert_eq!(
        test.db
            .create_link(ann.id, t.a, LinkKind::SignedIn, "viewer", Some(in_days(-1)))
            .await
            .map(|_| ()),
        Err(AccessError::ExpiryInPast)
    );
    let expires = in_days(29);
    let (link, _) = make(
        &test,
        ann.id,
        t.a,
        LinkKind::Anyone,
        "viewer",
        Some(expires),
    )
    .await;
    assert_eq!(link.kind, LinkKind::Anyone);
    assert_eq!(link.role.as_ref().unwrap().id, "viewer");
    assert!(link.expires_at.is_some());
    // An anyone link is no group and changes no permissions.
    assert!(!is_scope(&test, t.a).await);
    assert_eq!(
        count(
            &test,
            "select count(*) from links where id = $1 and group_id is null",
            link.id
        )
        .await,
        1
    );

    test.db
        .set_setting(admin.id, records::ANYONE_LINK_MAX_DAYS, json!(60))
        .await
        .unwrap();
    assert_eq!(attempt(Some(in_days(45))).await, Ok(()));
    assert_eq!(
        attempt(Some(in_days(61))).await,
        Err(AccessError::ExpiryTooLong)
    );
}

#[tokio::test]
async fn anyone_link_reaches_only_below_its_item() {
    let test = TestDb::create().await;
    anyone_links_on(&test).await;
    let ann = user(&test, "Ann", false).await;
    let t = tree(&test, ann.id).await;
    let c = folder(&test, t.a, t.site, "C").await;
    let in_c = file(&test, c, t.site, "In C").await;
    let beside = file(&test, t.a, t.site, "Beside").await;
    let elsewhere = folder(&test, t.library, t.site, "Elsewhere").await;
    let (link, token) = make(
        &test,
        ann.id,
        t.a,
        LinkKind::Anyone,
        "viewer",
        Some(in_days(7)),
    )
    .await;

    let grant = test.db.anyone_link(&token, None).await.unwrap().unwrap();
    assert_eq!((grant.link, grant.root, grant.node.id), (link.id, t.a, t.a));
    for node in [t.a, t.b, t.file, c, in_c, beside] {
        assert!(reaches(&test, &token, node).await, "{node:?}");
    }
    for node in [t.site, t.library, elsewhere, new_id()] {
        assert!(!reaches(&test, &token, node).await, "{node:?}");
    }
    let mut listing = anyone_listing(&test, &token, t.a).await.unwrap();
    listing.sort();
    let mut expected = vec![t.b, c, beside];
    expected.sort();
    assert_eq!(listing, expected);
    assert_eq!(anyone_listing(&test, &token, t.library).await, None);

    // A subfolder with its own permissions is out of reach, with all below it.
    assert!(test.db.break_inheritance(ann.id, t.b).await.unwrap());
    assert!(!reaches(&test, &token, t.b).await);
    assert!(!reaches(&test, &token, t.file).await);
    assert_eq!(anyone_listing(&test, &token, t.b).await, None);
    assert!(
        !anyone_listing(&test, &token, t.a)
            .await
            .unwrap()
            .contains(&t.b)
    );
    // The item itself having its own permissions does not matter.
    assert!(test.db.break_inheritance(ann.id, t.a).await.unwrap());
    assert!(reaches(&test, &token, beside).await);

    // A file moved out of the item is out of reach.
    put(
        &test,
        beside,
        NodeKind::Document,
        Some(elsewhere),
        t.site,
        "Beside",
    )
    .await;
    assert!(!reaches(&test, &token, beside).await);

    // A deleted folder hides what is below it.
    delete(&test, c).await;
    assert!(!reaches(&test, &token, c).await);
    assert!(!reaches(&test, &token, in_c).await);
    assert_eq!(anyone_listing(&test, &token, c).await, None);
    assert_eq!(anyone_listing(&test, &token, t.a).await, Some(Vec::new()));

    // A signed-in link's token is no anyone link.
    let (_, signed) = signed_in(&test, ann.id, t.a, Role::Viewer).await;
    assert_eq!(test.db.anyone_link(&signed, None).await.unwrap(), None);
    // A deleted ancestor stops it altogether.
    delete(&test, t.library).await;
    assert!(!reaches(&test, &token, t.a).await);
}

#[tokio::test]
async fn anyone_link_stops_when_setting_off_and_returns_when_on() {
    let test = TestDb::create().await;
    let admin = anyone_links_on(&test).await;
    let ann = user(&test, "Ann", false).await;
    let t = tree(&test, ann.id).await;
    let (link, token) = make(
        &test,
        ann.id,
        t.a,
        LinkKind::Anyone,
        "viewer",
        Some(in_days(7)),
    )
    .await;
    assert!(reaches(&test, &token, t.file).await);
    assert_eq!(
        test.db.link_kind(&token).await.unwrap().map(|(k, _)| k),
        Some(LinkKind::Anyone)
    );
    test.db
        .set_setting(admin, records::ANYONE_LINKS, json!(false))
        .await
        .unwrap();
    assert!(!reaches(&test, &token, t.a).await);
    assert!(!reaches(&test, &token, t.file).await);
    assert_eq!(anyone_listing(&test, &token, t.a).await, None);
    assert_eq!(test.db.link_kind(&token).await.unwrap(), None);
    // Not revoked: it works again when switched back on.
    assert_eq!(link_now(&test, link.id).await.revoked, None);
    test.db
        .set_setting(admin, records::ANYONE_LINKS, json!(true))
        .await
        .unwrap();
    assert!(reaches(&test, &token, t.file).await);
}

#[tokio::test]
async fn expired_links_are_revoked_by_the_sweep() {
    let test = TestDb::create().await;
    anyone_links_on(&test).await;
    let ann = user(&test, "Ann", false).await;
    let bob = user(&test, "Bob", false).await;
    let t = tree(&test, ann.id).await;
    let soon = OffsetDateTime::now_utc() + Duration::hours(1);
    let (short, token) = make(&test, ann.id, t.a, LinkKind::SignedIn, "editor", Some(soon)).await;
    let (long, _) = make(
        &test,
        ann.id,
        t.a,
        LinkKind::Anyone,
        "viewer",
        Some(in_days(2)),
    )
    .await;
    let (never, _) = signed_in(&test, ann.id, t.a, Role::Viewer).await;
    test.db.redeem_signed_in(bob.id, &token).await.unwrap();
    assert_eq!(role(&test, bob.id, t.file).await, Some(Role::Editor));

    let now = OffsetDateTime::now_utc();
    assert_eq!(
        test.db.expired_links(now).await.unwrap(),
        Vec::<Uuid>::new()
    );
    let later = now + Duration::hours(2);
    let expired = test.db.expired_links(later).await.unwrap();
    assert_eq!(expired, vec![short.id]);
    for id in expired {
        assert_eq!(
            test.db.revoke_link(None, id, "expired").await.unwrap(),
            Some(vec![t.a])
        );
    }
    assert_eq!(role(&test, bob.id, t.file).await, None);
    let (by, _, reason) = link_now(&test, short.id).await.revoked.unwrap();
    assert_eq!((by, reason.as_str()), (None, "expired"));
    let actor: Option<Uuid> = sqlx::query_scalar(
        "select actor from events where kind = 'link.revoked' and detail->>'link' = $1",
    )
    .bind(short.id.to_string())
    .fetch_one(test.db.test_pool())
    .await
    .unwrap();
    assert_eq!(actor, None);
    assert_eq!(link_now(&test, long.id).await.revoked, None);
    assert_eq!(link_now(&test, never.id).await.revoked, None);
    let far = now + Duration::days(3);
    assert_eq!(test.db.expired_links(far).await.unwrap(), vec![long.id]);

    // A link past its expiry stops at once, before any sweep.
    let (late, late_token) = make(
        &test,
        ann.id,
        t.a,
        LinkKind::Anyone,
        "viewer",
        Some(in_days(1)),
    )
    .await;
    let (late_in, late_in_token) = make(
        &test,
        ann.id,
        t.a,
        LinkKind::SignedIn,
        "viewer",
        Some(in_days(1)),
    )
    .await;
    assert!(reaches(&test, &late_token, t.a).await);
    sqlx::query("update links set expires_at = now() - interval '1 second' where id = any($1)")
        .bind(vec![late.id, late_in.id])
        .execute(test.db.test_pool())
        .await
        .unwrap();
    assert!(!reaches(&test, &late_token, t.a).await);
    assert_eq!(
        test.db.redeem_signed_in(bob.id, &late_in_token).await,
        Err(LinkError::NotFound)
    );
    assert_eq!(test.db.link_kind(&late_in_token).await.unwrap(), None);
}

#[tokio::test]
async fn deleting_an_item_revokes_its_links_and_closes_requests() {
    let test = TestDb::create().await;
    anyone_links_on(&test).await;
    let ann = user(&test, "Ann", false).await;
    let bob = user(&test, "Bob", false).await;
    let t = tree(&test, ann.id).await;
    let (on_b, token) = signed_in(&test, ann.id, t.b, Role::Editor).await;
    test.db.redeem_signed_in(bob.id, &token).await.unwrap();
    let (anyone_b, anyone_token) = make(
        &test,
        ann.id,
        t.b,
        LinkKind::Anyone,
        "viewer",
        Some(in_days(3)),
    )
    .await;
    let (on_file, _) = signed_in(&test, ann.id, t.file, Role::Viewer).await;
    let (on_a, _) = signed_in(&test, ann.id, t.a, Role::Viewer).await;
    let mut requests = Vec::new();
    for node in [t.a, t.b, t.file] {
        let id = Uuid::new_v4();
        sqlx::query("insert into access_requests (id, node_id, requester) values ($1, $2, $3)")
            .bind(id)
            .bind(node.0)
            .bind(bob.id.0)
            .execute(test.db.test_pool())
            .await
            .unwrap();
        requests.push(id);
    }

    delete(&test, t.b).await;
    for link in [on_b.id, anyone_b.id, on_file.id] {
        let (by, _, reason) = link_now(&test, link).await.revoked.unwrap();
        assert_eq!((by, reason.as_str()), (None, "item deleted"));
    }
    assert_eq!(link_now(&test, on_a.id).await.revoked, None);
    assert_eq!(members(&test, link_group(&test, on_b.id).await).await, 0);
    assert!(!reaches(&test, &anyone_token, t.b).await);
    let decisions: Vec<(Uuid, Option<String>, bool)> = sqlx::query_as(
        "select id, decision, decided_at is not null from access_requests where id = any($1)",
    )
    .bind(&requests)
    .fetch_all(test.db.test_pool())
    .await
    .unwrap();
    for (id, decision, decided) in decisions {
        if id == requests[0] {
            assert_eq!((decision, decided), (None, false));
        } else {
            assert_eq!((decision.as_deref(), decided), (Some("closed"), true));
        }
    }
}

#[tokio::test]
async fn tokens_are_stored_hashed() {
    let test = TestDb::create().await;
    anyone_links_on(&test).await;
    let ann = user(&test, "Ann", false).await;
    let bob = user(&test, "Bob", false).await;
    let t = tree(&test, ann.id).await;
    let (signed, signed_token) = signed_in(&test, ann.id, t.a, Role::Viewer).await;
    let (anyone, anyone_token) = make(
        &test,
        ann.id,
        t.a,
        LinkKind::Anyone,
        "viewer",
        Some(in_days(1)),
    )
    .await;
    test.db
        .redeem_signed_in(bob.id, &signed_token)
        .await
        .unwrap();
    assert_ne!(signed_token, anyone_token);
    for (link, token) in [(signed.id, &signed_token), (anyone.id, &anyone_token)] {
        let text = token.as_url_value();
        assert_eq!(text.len(), 64);
        assert!(
            text.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
        assert_eq!(LinkToken::parse(&text).as_ref(), Some(token));
        let bytes: Vec<u8> = (0..32)
            .map(|i| u8::from_str_radix(&text[2 * i..2 * i + 2], 16).unwrap())
            .collect();
        let stored: Vec<u8> = sqlx::query_scalar("select token_hash from links where id = $1")
            .bind(link)
            .fetch_one(test.db.test_pool())
            .await
            .unwrap();
        assert_eq!(stored, Sha256::digest(&bytes).to_vec());
        // Nowhere in the database, nor in what is printed.
        let found: bool = sqlx::query_scalar(
            "select exists (select 1 from links l where row_to_json(l)::text ilike '%' || $1 || '%') \
                 or exists (select 1 from events e where row_to_json(e)::text ilike '%' || $1 || '%')",
        )
        .bind(&text)
        .fetch_one(test.db.test_pool())
        .await
        .unwrap();
        assert!(!found);
        assert!(!format!("{token:?}").contains(&text));
        assert!(!format!("{:?}", test.db.link(link).await.unwrap()).contains(&text));
    }
    assert_eq!(LinkToken::parse(&"a".repeat(63)), None);
    assert_eq!(LinkToken::parse(&"g".repeat(64)), None);
}

#[tokio::test]
async fn sweeper_and_revoke_take_turns() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    let bob = user(&test, "Bob", false).await;
    let t = tree(&test, ann.id).await;
    let soon = OffsetDateTime::now_utc() + Duration::minutes(5);
    let (link, token) = make(&test, ann.id, t.a, LinkKind::SignedIn, "editor", Some(soon)).await;
    test.db.redeem_signed_in(bob.id, &token).await.unwrap();

    // Both start while the item's row is held, so both are waiting when it is let go.
    let held = gate(&test, t.a).await;
    let first = test.db.clone();
    let manager =
        tokio::spawn(async move { first.revoke_link(Some(ann.id), link.id, "manager").await });
    let second = test.db.clone();
    let sweeper = tokio::spawn(async move { second.revoke_link(None, link.id, "expired").await });
    wait_blocked(&test, held.pid, 2).await;
    held.open().await;
    let results = [
        manager.await.unwrap().unwrap(),
        sweeper.await.unwrap().unwrap(),
    ];
    assert_eq!(
        results.iter().filter(|r| r.is_some()).count(),
        1,
        "{results:?}"
    );
    assert!(results.contains(&Some(vec![t.a])));
    assert_eq!(link_events(&test, "link.revoked", link.id).await, 1);
    assert!(link_now(&test, link.id).await.revoked.is_some());
    assert_eq!(role(&test, bob.id, t.file).await, None);
}

#[tokio::test]
async fn explorer_lists_links_without_naming_groups() {
    let test = TestDb::create().await;
    anyone_links_on(&test).await;
    let ann = user(&test, "Ann", false).await;
    let t: Tree = tree(&test, ann.id).await;
    let expires = in_days(5);
    let (link, _) = make(
        &test,
        ann.id,
        t.a,
        LinkKind::SignedIn,
        "editor",
        Some(expires),
    )
    .await;
    make(
        &test,
        ann.id,
        t.a,
        LinkKind::Anyone,
        "viewer",
        Some(in_days(2)),
    )
    .await;
    // An anyone link on a node that inherits is not a break.
    make(
        &test,
        ann.id,
        t.file,
        LinkKind::Anyone,
        "viewer",
        Some(in_days(2)),
    )
    .await;
    assert!(!is_scope(&test, t.file).await);

    let (breaks, _) = test
        .db
        .breaks(ann.id, t.site, &ExplorerFilter::default(), None, 50)
        .await
        .unwrap()
        .unwrap();
    let ids: Vec<NodeId> = breaks.iter().map(|b| b.node.id).collect();
    assert_eq!(ids, vec![t.site, t.a]);
    let on_a = &breaks[1];
    assert_eq!(on_a.anyone_links, 1);
    assert_eq!(breaks[0].anyone_links, 0);
    let shown: Vec<&PrincipalView> = on_a.assignments.iter().map(|a| &a.principal).collect();
    let as_link: Vec<_> = shown
        .iter()
        .filter_map(|p| match p {
            PrincipalView::Link {
                link,
                kind,
                expires_at,
            } => Some((*link, *kind, expires_at.map(|at| at.unix_timestamp()))),
            _ => None,
        })
        .collect();
    assert_eq!(
        as_link,
        vec![(link.id, LinkKind::SignedIn, Some(expires.unix_timestamp()))]
    );
    let group = link_group(&test, link.id).await;
    assert!(shown.iter().all(|p| match p {
        PrincipalView::Group { id, .. } | PrincipalView::HiddenGroup(id) => *id != group,
        _ => true,
    }));
    // The access page shows it the same way.
    let access = test.db.access(ann.id, t.a).await.unwrap();
    assert!(
        access
            .assignments
            .iter()
            .any(|a| matches!(a.principal, PrincipalView::Link { link: id, .. } if id == link.id))
    );
    assert!(breaks_csv(&breaks).contains("link,signed-in link,Editor"));
    let links = test.db.links_on(ann.id, t.a).await.unwrap();
    assert_eq!(links.len(), 2);
}
