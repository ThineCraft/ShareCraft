//! The content tree: sites, libraries, folders and files, mirrored from Git into PostgreSQL.

use collab_core::ids::{NodeId, UserId};
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::documents::UNIQUE_VIOLATION;
use crate::events::{NewEvent, record};
use crate::groups::{
    add_site_owner, all_locked, ensure_site_groups, groups_judged_by, lock_groups,
};
use crate::roles::lock_roles;
use crate::scopes::{LOCK_ATTEMPTS, access_in, judged_roles, lock_segment, scopes_of, segment};
use crate::{AccessError, Db, DbError, Editor, Perms};

const MAX_NAME_CHARS: usize = 120;
const REFUSED_CHARS: [char; 9] = ['"', '*', ':', '<', '>', '?', '/', '\\', '|'];

/// What a node of the tree is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeKind {
    Site,
    Library,
    Folder,
    Document,
}

impl NodeKind {
    fn as_str(self) -> &'static str {
        match self {
            NodeKind::Site => "site",
            NodeKind::Library => "library",
            NodeKind::Folder => "folder",
            NodeKind::Document => "document",
        }
    }

    fn parse(text: &str) -> Option<NodeKind> {
        match text {
            "site" => Some(NodeKind::Site),
            "library" => Some(NodeKind::Library),
            "folder" => Some(NodeKind::Folder),
            "document" => Some(NodeKind::Document),
            _ => None,
        }
    }
}

/// A node as PostgreSQL holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    pub id: NodeId,
    pub kind: NodeKind,
    /// `None` exactly for a site.
    pub parent: Option<NodeId>,
    /// The site the node is in; a site's own id for the site itself.
    pub site: NodeId,
    pub name: String,
    /// Whether the node is a permission scope: it has its own assignments rather than
    /// inheriting. Sites always are.
    pub unique_scope: bool,
    pub deleted: bool,
    pub created_by: Option<UserId>,
    pub created_at: OffsetDateTime,
}

/// A node as Git holds it, which the projection writes into PostgreSQL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeRow {
    pub id: NodeId,
    pub kind: NodeKind,
    pub parent: Option<NodeId>,
    pub site: NodeId,
    pub name: String,
    pub deleted_at: Option<OffsetDateTime>,
    pub deleted_by: Option<UserId>,
    pub created_by: Option<UserId>,
    pub created_at: OffsetDateTime,
}

/// Checks a name for any node: 1 to 120 characters after trimming, no control characters,
/// none of `" * : < > ? / \ |`, and no trailing dot. Returns the trimmed name.
pub fn validate_node_name(name: &str) -> Result<&str, AccessError> {
    let name = name.trim();
    let length = name.chars().count();
    let well_formed = (1..=MAX_NAME_CHARS).contains(&length)
        && !name.ends_with('.')
        && !name
            .chars()
            .any(|c| c.is_control() || REFUSED_CHARS.contains(&c));
    if well_formed {
        Ok(name)
    } else {
        Err(AccessError::InvalidName)
    }
}

pub(crate) fn node_from_row(row: &PgRow) -> Result<Node, sqlx::Error> {
    let kind: String = row.try_get("kind")?;
    Ok(Node {
        id: NodeId(row.try_get("id")?),
        kind: NodeKind::parse(&kind)
            .ok_or_else(|| sqlx::Error::Decode(format!("unknown node kind {kind:?}").into()))?,
        parent: row.try_get::<Option<Uuid>, _>("parent_id")?.map(NodeId),
        site: NodeId(row.try_get("site_id")?),
        name: row.try_get("name")?,
        unique_scope: row.try_get("unique_scope")?,
        deleted: row.try_get("deleted")?,
        created_by: row.try_get::<Option<Uuid>, _>("created_by")?.map(UserId),
        created_at: row.try_get("created_at")?,
    })
}

/// A change to the tree as PostgreSQL applies it once Git has recorded it: the rows, their
/// events, and what the person making it must still be allowed to do, judged in the same
/// transaction with the rows concerned locked.
#[derive(Clone, Debug, Default)]
pub struct NodeChange<'a> {
    /// The nodes as Git now holds them, each parent before its children.
    pub rows: &'a [NodeRow],
    /// The events to record with the change.
    pub events: Vec<NewEvent>,
    /// Who makes the change; `None` for the server itself.
    pub actor: Option<UserId>,
    /// Whether `actor` must be an enabled administrator.
    pub needs_admin: bool,
    /// Nodes `actor` must hold the given permissions on. Each node's chain, from the scope
    /// that decides its access down to it, is locked in this order (the node itself
    /// `FOR UPDATE` if it is one of `rows`, everything else `FOR SHARE`), then the groups and
    /// the roles giving the access, before it is judged: list ancestors before descendants, as
    /// access changes lock them.
    pub needs: &'a [(NodeId, Perms)],
    /// The first row's node is a new site: makes this person (its creator) a member of its
    /// Owners group. Every site row gets its three groups, with their assignments, in the same
    /// transaction, whether or not this is given.
    pub site_groups: Option<UserId>,
    /// Records the first row's node as a new document opened in this editor, created by
    /// `actor`.
    pub document: Option<Editor>,
}

/// Inserts or updates `row`, and for a document node the name of its `documents` row. A site
/// gets the groups it does not have yet (see [`ensure_site_groups`]).
pub(crate) async fn upsert_node(conn: &mut PgConnection, row: &NodeRow) -> Result<(), AccessError> {
    let result = sqlx::query(
        "insert into nodes \
             (id, kind, parent_id, site_id, name, unique_scope, deleted_at, deleted_by, \
              created_by, created_at) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
         on conflict (id) do update set \
             kind = excluded.kind, parent_id = excluded.parent_id, \
             site_id = excluded.site_id, name = excluded.name, \
             deleted_at = excluded.deleted_at, deleted_by = excluded.deleted_by",
    )
    .bind(row.id.0)
    .bind(row.kind.as_str())
    .bind(row.parent.map(|p| p.0))
    .bind(row.site.0)
    .bind(&row.name)
    .bind(row.kind == NodeKind::Site)
    .bind(row.deleted_at)
    .bind(row.deleted_by.map(|u| u.0))
    .bind(row.created_by.map(|u| u.0))
    .bind(row.created_at)
    .execute(&mut *conn)
    .await;
    match result {
        Ok(_) => {}
        Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some(UNIQUE_VIOLATION) => {
            return Err(AccessError::NameTaken);
        }
        Err(e) => return Err(e.into()),
    }
    if row.kind == NodeKind::Site {
        ensure_site_groups(&mut *conn, row.id).await?;
    }
    if row.kind == NodeKind::Document {
        sqlx::query("update documents set name = $2 where id = $1")
            .bind(row.id.0)
            .bind(&row.name)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

/// Locks what `change` needs and judges its actor's access. Each needed node's chain is locked
/// from its source scope down, as access changes lock it (ancestors `FOR SHARE`, a node the
/// change writes `FOR UPDATE`), in `needs` order; then the groups the actor is a member of that
/// are assigned on those chains' scopes, by id, `FOR SHARE`; then the roles giving that access,
/// by id, `FOR SHARE`; then the access is worked out again and judged. So an assignment changed
/// anywhere above a needed node, a membership ended of a group giving the access, or a role
/// changed that gives it, either waits for this change or is seen by it. Returns `false` if a
/// chain or the groups moved while they were being locked: the caller starts over.
async fn judge(conn: &mut PgConnection, change: &NodeChange<'_>) -> Result<bool, AccessError> {
    if !change.needs_admin && change.needs.is_empty() {
        return Ok(true);
    }
    let actor = change.actor.ok_or(AccessError::Forbidden)?;
    if change.needs_admin {
        let admin: Option<bool> = sqlx::query_scalar(
            "select is_admin and disabled_at is null from users where id = $1 for share",
        )
        .bind(actor.0)
        .fetch_optional(&mut *conn)
        .await?;
        if admin != Some(true) {
            return Err(AccessError::Forbidden);
        }
    }
    let mut involved = Vec::new();
    let mut scopes = Vec::new();
    for (id, _) in change.needs {
        let written = change.rows.iter().any(|row| row.id == *id);
        let Some(links) = lock_segment(&mut *conn, actor, *id, false, written).await? else {
            return Ok(false);
        };
        scopes.extend(scopes_of(&segment(&links, false)));
        involved.extend(judged_roles(&links));
    }
    let judged = groups_judged_by(&mut *conn, actor, &scopes).await?;
    lock_groups(&mut *conn, judged.iter().copied(), None).await?;
    lock_roles(&mut *conn, involved).await?;
    if !all_locked(
        &groups_judged_by(&mut *conn, actor, &scopes).await?,
        &judged,
    ) {
        return Ok(false);
    }
    // Every chain and role is locked now, so none of this access can change before the commit.
    for (id, needed) in change.needs {
        match access_in(&mut *conn, actor, *id).await? {
            None => return Err(AccessError::NotFound),
            Some(access) if !access.perms.contains(*needed) => {
                return Err(AccessError::Forbidden);
            }
            Some(_) => {}
        }
    }
    Ok(true)
}

impl Db {
    /// Applies one node as Git holds it, in one transaction: inserts or updates the row (kind,
    /// parent, site, name, deleted), writes `event` if given, and for a document node updates
    /// the name of the `documents` row with that id, if there is one, so rooms, downloads and
    /// saves use the new name. A new site row is a scope; an update never changes
    /// `unique_scope`. A name already used by an undeleted sibling gives `NameTaken`.
    pub async fn apply_node(
        &self,
        row: &NodeRow,
        event: Option<NewEvent>,
    ) -> Result<(), AccessError> {
        self.apply_nodes(NodeChange {
            rows: std::slice::from_ref(row),
            events: event.into_iter().collect(),
            ..NodeChange::default()
        })
        .await
    }

    /// Applies `change` in one transaction: locks and judges what it needs (`NotFound` if the
    /// actor no longer has access to a node it needs, `Forbidden` if it lacks a permission or
    /// they are no longer an enabled administrator when that is needed), records the new
    /// document if any (`Exists` if its id is taken), applies each row as
    /// [`Db::apply_node`] does (a site row gets its three groups), makes the new site's creator a
    /// member of its Owners group if asked, and writes the events.
    ///
    /// A database failure before the change was judged allowed (the connection lost while
    /// locking, or the tree changing on every attempt) is `Unjudged`: nothing was written, and
    /// the change must be treated as refused. `Db` is a failure after it was judged allowed.
    pub async fn apply_nodes(&self, change: NodeChange<'_>) -> Result<(), AccessError> {
        let mut tx = self.judged(&change).await.map_err(|e| match e {
            AccessError::Db(e) => AccessError::Unjudged(e),
            other => other,
        })?;
        if let (Some(editor), Some(first)) = (change.document, change.rows.first()) {
            let inserted = sqlx::query(
                "insert into documents (id, name, editor, created_by) values ($1, $2, $3, $4)",
            )
            .bind(first.id.0)
            .bind(&first.name)
            .bind(editor.id())
            .bind(change.actor.map(|u| u.0))
            .execute(&mut *tx)
            .await;
            match inserted {
                Ok(_) => {}
                Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some(UNIQUE_VIOLATION) => {
                    return Err(AccessError::Exists);
                }
                Err(e) => return Err(e.into()),
            }
        }
        for row in change.rows {
            upsert_node(&mut tx, row).await?;
        }
        if let (Some(creator), Some(first)) = (change.site_groups, change.rows.first()) {
            add_site_owner(&mut tx, first.id, creator, change.actor).await?;
        }
        for event in change.events {
            record(&mut tx, event).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// A transaction in which `change` has been judged allowed, with what it needs locked.
    async fn judged(
        &self,
        change: &NodeChange<'_>,
    ) -> Result<sqlx::Transaction<'static, sqlx::Postgres>, AccessError> {
        for _ in 0..LOCK_ATTEMPTS {
            let mut tx = self.pool().begin().await?;
            if judge(&mut tx, change).await? {
                return Ok(tx);
            }
            tx.rollback().await?;
        }
        Err(DbError::new("the tree kept changing while a change was judged").into())
    }

    /// Every node as PostgreSQL holds it, deleted or not, for comparing with Git.
    pub async fn node_rows(&self) -> Result<Vec<NodeRow>, DbError> {
        let rows = sqlx::query(
            "select id, kind, parent_id, site_id, name, deleted_at, deleted_by, created_by, \
                    created_at from nodes",
        )
        .fetch_all(self.pool())
        .await?;
        rows.iter()
            .map(|row| {
                let kind: String = row.try_get("kind")?;
                Ok(NodeRow {
                    id: NodeId(row.try_get("id")?),
                    kind: NodeKind::parse(&kind).ok_or_else(|| {
                        sqlx::Error::Decode(format!("unknown node kind {kind:?}").into())
                    })?,
                    parent: row.try_get::<Option<Uuid>, _>("parent_id")?.map(NodeId),
                    site: NodeId(row.try_get("site_id")?),
                    name: row.try_get("name")?,
                    deleted_at: row.try_get("deleted_at")?,
                    deleted_by: row.try_get::<Option<Uuid>, _>("deleted_by")?.map(UserId),
                    created_by: row.try_get::<Option<Uuid>, _>("created_by")?.map(UserId),
                    created_at: row.try_get("created_at")?,
                })
            })
            .collect::<Result<_, sqlx::Error>>()
            .map_err(DbError::from)
    }

    /// Whether no undeleted site other than `except` is named `name`, ignoring case.
    pub async fn site_name_free(
        &self,
        name: &str,
        except: Option<NodeId>,
    ) -> Result<bool, DbError> {
        let taken: bool = sqlx::query_scalar(
            "select exists (select 1 from nodes \
                 where parent_id is null and lower(name) = lower($1) and deleted_at is null \
                   and ($2::uuid is null or id <> $2))",
        )
        .bind(name)
        .bind(except.map(|e| e.0))
        .fetch_one(self.pool())
        .await?;
        Ok(!taken)
    }

    /// The node with this id, deleted or not, whoever may see it.
    pub async fn node(&self, id: NodeId) -> Result<Option<Node>, DbError> {
        let row = sqlx::query(
            "select id, kind, parent_id, site_id, name, unique_scope, \
                    (deleted_at is not null) as deleted, created_by, created_at \
             from nodes where id = $1",
        )
        .bind(id.0)
        .fetch_optional(self.pool())
        .await?;
        Ok(row.as_ref().map(node_from_row).transpose()?)
    }

    /// Whether `node` is `ancestor` or lies anywhere below it. False if `node` does not exist.
    /// The walk ends even if the data held a cycle.
    pub async fn is_at_or_below(&self, node: NodeId, ancestor: NodeId) -> Result<bool, DbError> {
        let found: bool = sqlx::query_scalar(
            "with recursive up(id, parent_id) as ( \
                 select id, parent_id from nodes where id = $1 \
                 union \
                 select n.id, n.parent_id from nodes n join up on n.id = up.parent_id \
             ) select exists (select 1 from up where id = $2)",
        )
        .bind(node.0)
        .bind(ancestor.0)
        .fetch_one(self.pool())
        .await?;
        Ok(found)
    }

    /// Whether `id` has any undeleted child.
    pub async fn has_live_children(&self, id: NodeId) -> Result<bool, DbError> {
        let found: bool = sqlx::query_scalar(
            "select exists (select 1 from nodes where parent_id = $1 and deleted_at is null)",
        )
        .bind(id.0)
        .fetch_one(self.pool())
        .await?;
        Ok(found)
    }

    /// Whether no undeleted child of `parent`, other than `except`, is named `name`, ignoring
    /// case.
    pub async fn name_free(
        &self,
        parent: NodeId,
        name: &str,
        except: Option<NodeId>,
    ) -> Result<bool, DbError> {
        let taken: bool = sqlx::query_scalar(
            "select exists (select 1 from nodes \
                 where parent_id = $1 and lower(name) = lower($2) and deleted_at is null \
                   and ($3::uuid is null or id <> $3))",
        )
        .bind(parent.0)
        .bind(name)
        .bind(except.map(|e| e.0))
        .fetch_one(self.pool())
        .await?;
        Ok(!taken)
    }
}
