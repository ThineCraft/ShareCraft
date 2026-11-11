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

use collab_core::ids::{DocId, NodeId, UserId};
use records::test_support::TestDb;
use records::{Editor, NewUser, NodeKind, NodeRow, Role, User};
use sqlx::Connection;
use time::OffsetDateTime;
use uuid::Uuid;

const PASSWORD: &str = "correct horse battery";

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
    let role = match role {
        Role::Viewer => "viewer",
        Role::Editor => "editor",
        Role::Owner => "owner",
    };
    sqlx::query("insert into scope_access (node_id, user_id, role) values ($1, $2, $3)")
        .bind(node.0)
        .bind(user.0)
        .bind(role)
        .execute(test.db.test_pool())
        .await
        .unwrap();
}

/// A site named `name`, owned by `owner` if given.
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
    let mut conn = sqlx::PgConnection::connect(&test.url()).await.unwrap();
    let pid: i32 = sqlx::query_scalar("select pg_backend_pid()")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    sqlx::query("begin").execute(&mut conn).await.unwrap();
    sqlx::query("select id from nodes where id = $1 for update")
        .bind(node.0)
        .execute(&mut conn)
        .await
        .unwrap();
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
