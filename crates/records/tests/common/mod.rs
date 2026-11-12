//! Helpers shared by the records tests: accounts, a small content tree, and waiting for
//! queries blocked on a lock.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::time::Duration;

use collab_core::ids::{DocId, GroupId, NodeId, UserId};
use records::test_support::TestDb;
use records::{Access, AccessError, Editor, NewUser, NodeKind, NodeRow, RoleRef, User};
use sqlx::Connection;
use time::OffsetDateTime;
use uuid::Uuid;

const PASSWORD: &str = "correct horse battery";

/// The built-in roles, by which plan 1's tests name access.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    Viewer,
    Editor,
    Owner,
}

impl Role {
    /// The built-in role's id.
    pub fn id(self) -> &'static str {
        match self {
            Role::Viewer => "viewer",
            Role::Editor => "editor",
            Role::Owner => "owner",
        }
    }

    /// The built-in role with this id; panics for any other.
    pub fn from_id(id: &str) -> Role {
        match id {
            "viewer" => Role::Viewer,
            "editor" => Role::Editor,
            "owner" => Role::Owner,
            other => panic!("{other:?} is not a built-in role"),
        }
    }

    /// The one built-in role giving `access`; panics unless exactly one built-in role gives
    /// it, with exactly that role's permissions.
    pub fn of(access: &Access) -> Role {
        let [only] = access.roles.as_slice() else {
            panic!("expected one role, got {access:?}");
        };
        let role = Role::from_id(&only.id);
        let builtin = records::RoleDef::builtin(role.id()).unwrap();
        assert_eq!(access.perms, builtin.perms, "{access:?}");
        role
    }
}

/// The built-in role giving `access`, if any (see [`Role::of`]).
pub fn role_of(access: Option<Access>) -> Option<Role> {
    access.as_ref().map(Role::of)
}

/// A change of assignment, with the roles before and after named as built-in roles.
pub fn changed(
    result: Result<(Option<RoleRef>, Option<RoleRef>), AccessError>,
) -> Result<(Option<Role>, Option<Role>), AccessError> {
    result.map(|(before, after)| {
        (
            before.map(|r| Role::from_id(&r.id)),
            after.map(|r| Role::from_id(&r.id)),
        )
    })
}

pub async fn user(test: &TestDb, name: &str, is_admin: bool) -> User {
    test.db
        .create_user(
            None,
            NewUser {
                email: &format!("{}@x.test", name.to_lowercase()),
                name,
                password: PASSWORD,
                is_admin,
            },
        )
        .await
        .unwrap()
}

pub fn new_id() -> NodeId {
    NodeId(Uuid::new_v4())
}

/// Writes one node as the projection from Git would.
pub async fn put(
    test: &TestDb,
    id: NodeId,
    kind: NodeKind,
    parent: Option<NodeId>,
    site: NodeId,
    name: &str,
) -> NodeId {
    test.db
        .apply_node(
            &NodeRow {
                id,
                kind,
                parent,
                site,
                name: name.to_string(),
                deleted_at: None,
                deleted_by: None,
                created_by: None,
                created_at: OffsetDateTime::now_utc(),
            },
            None,
        )
        .await
        .unwrap();
    id
}

/// Writes one node row directly, as plan 1 held it (a site is a scope), without the code of
/// later plans: for building a database migrated only part of the way. Deleted if `deleted`.
pub async fn raw_node(
    test: &TestDb,
    id: NodeId,
    kind: NodeKind,
    parent: Option<NodeId>,
    site: NodeId,
    name: &str,
    deleted: bool,
) -> NodeId {
    let kind = match kind {
        NodeKind::Site => "site",
        NodeKind::Library => "library",
        NodeKind::Folder => "folder",
        NodeKind::Document => "document",
    };
    sqlx::query(
        "insert into nodes (id, kind, parent_id, site_id, name, unique_scope, deleted_at, \
                            created_at) \
         values ($1, $2, $3, $4, $5, $6, case when $7 then now() end, now())",
    )
    .bind(id.0)
    .bind(kind)
    .bind(parent.map(|p| p.0))
    .bind(site.0)
    .bind(name)
    .bind(kind == "site")
    .bind(deleted)
    .execute(test.db.test_pool())
    .await
    .unwrap();
    id
}

/// Marks a node deleted, as the projection would.
pub async fn delete(test: &TestDb, id: NodeId) {
    let node = test.db.node(id).await.unwrap().unwrap();
    test.db
        .apply_node(
            &NodeRow {
                id,
                kind: node.kind,
                parent: node.parent,
                site: node.site,
                name: node.name,
                deleted_at: Some(OffsetDateTime::now_utc()),
                deleted_by: None,
                created_by: node.created_by,
                created_at: node.created_at,
            },
            None,
        )
        .await
        .unwrap();
}

/// Writes an assignment directly, for setting up a scope that has none yet.
pub async fn grant(test: &TestDb, node: NodeId, user: UserId, role: Role) {
    grant_role(test, node, user, role.id()).await;
}

/// Writes a person's assignment of the role `role_id` directly.
pub async fn grant_role(test: &TestDb, node: NodeId, user: UserId, role_id: &str) {
    sqlx::query(
        "insert into assignments (node_id, principal_kind, principal_id, role_id) \
         values ($1, 'user', $2, $3)",
    )
    .bind(node.0)
    .bind(user.0)
    .bind(role_id)
    .execute(test.db.test_pool())
    .await
    .unwrap();
}

/// Takes every group's assignment off `node`, so that only people are assigned there, as in
/// plan 1: for tests of the rules on people's assignments (such as keeping a manager) that a
/// site's Owners group, which manages too, would otherwise answer.
pub async fn without_groups(test: &TestDb, node: NodeId) {
    sqlx::query("delete from assignments where node_id = $1 and principal_kind = 'group'")
        .bind(node.0)
        .execute(test.db.test_pool())
        .await
        .unwrap();
}

/// A site named `name`, owned by `owner` if given. Written as reconcile writes a site, so it
/// also has its three groups, with nobody in them.
pub async fn site(test: &TestDb, name: &str, owner: Option<UserId>) -> NodeId {
    let id = new_id();
    put(test, id, NodeKind::Site, None, id, name).await;
    if let Some(owner) = owner {
        grant(test, id, owner, Role::Owner).await;
    }
    id
}

/// A text document named `name` created by `creator`, placed under `parent` in `site`.
pub async fn document_in(
    test: &TestDb,
    creator: UserId,
    parent: NodeId,
    site: NodeId,
    name: &str,
) -> DocId {
    let id = DocId(Uuid::new_v4());
    test.db
        .create_document(creator, id, name, Editor::Text)
        .await
        .unwrap();
    put(
        test,
        id.into(),
        NodeKind::Document,
        Some(parent),
        site,
        name,
    )
    .await;
    id
}

/// Site → library "Documents" → folder "A" → folder "B" → text document "Notes".
pub struct Tree {
    pub site: NodeId,
    pub library: NodeId,
    pub a: NodeId,
    pub b: NodeId,
    pub doc: DocId,
    pub file: NodeId,
}

/// Builds a [`Tree`] whose site is owned by `owner`.
pub async fn tree(test: &TestDb, owner: UserId) -> Tree {
    let site = site(test, "Site", Some(owner)).await;
    let library = put(
        test,
        new_id(),
        NodeKind::Library,
        Some(site),
        site,
        "Documents",
    )
    .await;
    let a = put(test, new_id(), NodeKind::Folder, Some(library), site, "A").await;
    let b = put(test, new_id(), NodeKind::Folder, Some(a), site, "B").await;
    let doc = document_in(test, owner, b, site, "Notes").await;
    Tree {
        site,
        library,
        a,
        b,
        doc,
        file: doc.into(),
    }
}

/// The kind and detail of every event about `node`, oldest first.
pub async fn node_events(test: &TestDb, node: NodeId) -> Vec<(String, serde_json::Value)> {
    sqlx::query_as("select kind, detail from events where node_id = $1 order by id")
        .bind(node.0)
        .fetch_all(test.db.test_pool())
        .await
        .unwrap()
}

/// A connection holding a lock, and its backend's process id.
pub struct Gate {
    pub conn: sqlx::PgConnection,
    pub pid: i32,
}

/// Opens a connection, begins a transaction on it and locks `node`'s row `FOR UPDATE`.
pub async fn gate(test: &TestDb, node: NodeId) -> Gate {
    let mut gate = open_gate(test).await;
    sqlx::query("select id from nodes where id = $1 for update")
        .bind(node.0)
        .execute(&mut gate.conn)
        .await
        .unwrap();
    gate
}

/// Opens a connection, begins a transaction on it and locks the row of the role `id`
/// `FOR UPDATE`.
pub async fn role_gate(test: &TestDb, id: &str) -> Gate {
    let mut gate = open_gate(test).await;
    sqlx::query("select id from roles where id = $1 for update")
        .bind(id)
        .execute(&mut gate.conn)
        .await
        .unwrap();
    gate
}

/// Opens a connection, begins a transaction on it and locks the row of the group `id`
/// `FOR UPDATE`.
pub async fn group_gate(test: &TestDb, id: GroupId) -> Gate {
    let mut gate = open_gate(test).await;
    sqlx::query("select id from groups where id = $1 for update")
        .bind(id.0)
        .execute(&mut gate.conn)
        .await
        .unwrap();
    gate
}

async fn open_gate(test: &TestDb) -> Gate {
    let mut conn = sqlx::PgConnection::connect(&test.url()).await.unwrap();
    let pid: i32 = sqlx::query_scalar("select pg_backend_pid()")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    sqlx::query("begin").execute(&mut conn).await.unwrap();
    Gate { conn, pid }
}

impl Gate {
    /// Commits the gate's transaction, releasing its lock.
    pub async fn open(mut self) {
        sqlx::query("commit").execute(&mut self.conn).await.unwrap();
    }
}

/// Waits until `count` backends are blocked, directly or through one another, behind the
/// backend `pid`. Only backends of this test can be blocked behind its gate.
pub async fn wait_blocked(test: &TestDb, pid: i32, count: i64) {
    let mut blocked = 0i64;
    for _ in 0..400 {
        blocked = sqlx::query_scalar(
            "with recursive w(pid) as ( \
                 select pid from pg_stat_activity where $1 = any(pg_blocking_pids(pid)) \
                 union \
                 select a.pid from pg_stat_activity a join w on w.pid = any(pg_blocking_pids(a.pid)) \
             ) select count(*) from w",
        )
        .bind(pid)
        .fetch_one(test.db.test_pool())
        .await
        .unwrap();
        if blocked >= count {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("expected {count} blocked behind the gate, saw {blocked}");
}
