//! Inherited access on the content tree. A node either inherits or is a scope with its own
//! assignments in `scope_access`; a person's role on a node is their assignment on the nearest
//! scope at or above it. Breaking inheritance copies the parent's assignments onto the node and
//! restoring deletes them: neither touches anything below the node, whatever lies there.

use std::collections::HashMap;

use collab_core::ids::{DocId, NodeId, UserId};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{PgConnection, Postgres, Row, Transaction};
use time::OffsetDateTime;

use crate::accounts::user_from_row;
use crate::documents::{parse_editor, parse_role};
use crate::events::{NewEvent, record};
use crate::tree::node_from_row;
use crate::{AccessError, Db, DbError, Editor, Grant, Node, NodeKind, Role};

/// How many times an access change reads and locks its chain before giving up because the
/// tree kept moving under it.
pub(crate) const LOCK_ATTEMPTS: usize = 10;

/// The nodes `$1` can reach through their assignments: every node at or above a scope where
/// they have a row, leaving out scopes with a deleted node at or above them. Starts from the
/// person's own rows, so its cost follows their number of assignments, not the tree's size.
/// Walks with `union`, so it ends even if the data held a cycle. Defines the CTEs `grants`,
/// `up`, `live` and `reach`; a query puts it after `with recursive`.
pub(crate) const REACH: &str = "\
    grants(node_id, role) as ( \
        select sa.node_id, sa.role from scope_access sa \
        join users u on u.id = sa.user_id and u.disabled_at is null \
        join nodes g on g.id = sa.node_id and g.unique_scope \
        where sa.user_id = $1), \
    up(origin, id, parent_id, deleted) as ( \
        select n.id, n.id, n.parent_id, n.deleted_at is not null \
        from grants join nodes n on n.id = grants.node_id \
        union \
        select up.origin, n.id, n.parent_id, n.deleted_at is not null \
        from up join nodes n on n.id = up.parent_id), \
    live(origin) as (select origin from up group by origin having not bool_or(deleted)), \
    reach(id) as (select distinct id from up where origin in (select origin from live))";

/// `node` ($2) and everything above it, each with `$1`'s own assignment if it is a scope.
const CHAIN: &str = "\
    with recursive up(id, kind, parent_id, site_id, name, unique_scope, deleted, created_by, \
                      created_at) as ( \
        select id, kind, parent_id, site_id, name, unique_scope, deleted_at is not null, \
               created_by, created_at \
        from nodes where id = $2 \
        union \
        select n.id, n.kind, n.parent_id, n.site_id, n.name, n.unique_scope, \
               n.deleted_at is not null, n.created_by, n.created_at \
        from nodes n join up on n.id = up.parent_id) \
    select up.*, sa.role as granted_role from up \
    left join scope_access sa on sa.node_id = up.id and sa.user_id = $1 and up.unique_scope";

const NODE_COLUMNS: &str = "n.id, n.kind, n.parent_id, n.site_id, n.name, n.unique_scope, \
     (n.deleted_at is not null) as deleted, n.created_by, n.created_at";

/// One step of a breadcrumb: a node from a site down, with the person's role on it (`None`
/// where they have only limited access: a role somewhere below).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Crumb {
    pub id: NodeId,
    pub kind: NodeKind,
    pub name: String,
    pub role: Option<Role>,
}

/// A child in a listing, with the person's role on it (`None` for limited access) and, for a
/// file, its editor and when it was last saved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listed {
    pub node: Node,
    pub role: Option<Role>,
    pub editor: Option<Editor>,
    pub revised_at: Option<OffsetDateTime>,
}

/// Where a page of children ended: the listing continues after this child. Children are
/// ordered folders (and libraries) before files, then by name ignoring case, then by id.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    /// 0 for anything but a file, 1 for a file.
    pub kind_rank: i16,
    pub name: String,
    pub id: NodeId,
}

/// Who has access to a node and where it comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeAccess {
    /// Whether the node takes its access from a scope above it.
    pub inherits: bool,
    /// The scope that decides access to the node: the node itself unless it inherits.
    pub source: Crumb,
    /// The assignments on `source`, by name.
    pub grants: Vec<Grant>,
}

/// One node of a chain from a site down, with someone's role on it.
#[derive(Clone, Debug)]
pub(crate) struct Link {
    pub(crate) node: Node,
    pub(crate) role: Option<Role>,
}

/// Whether an account may have any role, and whether it is an administrator.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Standing {
    Nobody,
    Member,
    Admin,
}

async fn standing(conn: &mut PgConnection, user: UserId) -> Result<Standing, sqlx::Error> {
    let row: Option<(bool, bool)> =
        sqlx::query_as("select is_admin, disabled_at is null from users where id = $1")
            .bind(user.0)
            .fetch_optional(conn)
            .await?;
    Ok(match row {
        Some((true, true)) => Standing::Admin,
        Some((false, true)) => Standing::Member,
        _ => Standing::Nobody,
    })
}

/// The chain from the site down to `node`, with `user`'s role on each node. `None` for a
/// disabled or unknown account, or when the node is missing, has a deleted node at or above
/// it, or does not lead up to a site (as a cycle would not).
///
/// This is the one place a role is decided: an enabled administrator is owner of everything;
/// otherwise a scope gives the person's assignment on it, and any other node the role of the
/// node above.
pub(crate) async fn chain(
    conn: &mut PgConnection,
    user: UserId,
    node: NodeId,
) -> Result<Option<Vec<Link>>, sqlx::Error> {
    let admin = match standing(conn, user).await? {
        Standing::Nobody => return Ok(None),
        Standing::Member => false,
        Standing::Admin => true,
    };
    let rows = sqlx::query(CHAIN)
        .bind(user.0)
        .bind(node.0)
        .fetch_all(&mut *conn)
        .await?;
    let mut by_id = HashMap::with_capacity(rows.len());
    for row in &rows {
        let found = node_from_row(row)?;
        let grant = row
            .try_get::<Option<String>, _>("granted_role")?
            .as_deref()
            .map(parse_role)
            .transpose()?;
        by_id.insert(found.id, (found, grant));
    }
    let mut upward = Vec::with_capacity(by_id.len());
    let mut next = Some(node);
    while let Some(id) = next {
        // Taking each node out means a cycle finds its start gone and ends here.
        let Some((found, grant)) = by_id.remove(&id) else {
            return Ok(None);
        };
        if found.deleted {
            return Ok(None);
        }
        next = found.parent;
        upward.push((found, grant));
    }
    if upward.last().map(|(top, _)| top.kind) != Some(NodeKind::Site) {
        return Ok(None);
    }
    let mut role = None;
    Ok(Some(
        upward
            .into_iter()
            .rev()
            .map(|(node, grant)| {
                if admin {
                    role = Some(Role::Owner);
                } else if node.unique_scope {
                    role = grant;
                }
                Link { node, role }
            })
            .collect(),
    ))
}

/// What `user` may do with `node`, on `conn`.
pub(crate) async fn role_in(
    conn: &mut PgConnection,
    user: UserId,
    node: NodeId,
) -> Result<Option<Role>, sqlx::Error> {
    Ok(chain(conn, user, node)
        .await?
        .and_then(|links| links.last().and_then(|link| link.role)))
}

/// Whether `node` is at or above a live scope where `user` has an assignment.
async fn in_reach(
    conn: &mut PgConnection,
    user: UserId,
    node: NodeId,
) -> Result<bool, sqlx::Error> {
    let query = format!("with recursive {REACH} select exists (select 1 from reach where id = $2)");
    sqlx::query_scalar(sqlx::AssertSqlSafe(query))
        .bind(user.0)
        .bind(node.0)
        .fetch_one(conn)
        .await
}

/// The index in `links` of the nearest scope at or above the last node; with `above`, strictly
/// above it. The last node itself when there is none (a site has nothing above).
fn source_index(links: &[Link], above: bool) -> usize {
    let last = links.len().saturating_sub(1);
    let searched = if above {
        links.get(..last).unwrap_or_default()
    } else {
        links
    };
    searched
        .iter()
        .rposition(|link| link.node.unique_scope)
        .unwrap_or(last)
}

/// The nodes from the source scope down to the node, as an access change locks them, with
/// whether each is a scope.
fn segment(links: &[Link], above: bool) -> Vec<(NodeId, bool)> {
    links
        .get(source_index(links, above)..)
        .unwrap_or_default()
        .iter()
        .map(|link| (link.node.id, link.node.unique_scope))
        .collect()
}

/// `NotFound` unless the chain exists and its person has a role on the last node, `Forbidden`
/// unless that role is owner.
fn owner(links: Option<Vec<Link>>) -> Result<Vec<Link>, AccessError> {
    let links = links.ok_or(AccessError::NotFound)?;
    match links.last().and_then(|link| link.role) {
        None => Err(AccessError::NotFound),
        Some(role) if role.can_manage() => Ok(links),
        Some(_) => Err(AccessError::Forbidden),
    }
}

/// The event's document: a file's node is its document.
fn document_of(node: &Node) -> Option<DocId> {
    (node.kind == NodeKind::Document).then_some(DocId(node.id.0))
}

fn crumb(link: &Link) -> Crumb {
    Crumb {
        id: link.node.id,
        kind: link.node.kind,
        name: link.node.name.clone(),
        role: link.role,
    }
}

/// Makes `node` a scope with a copy of the assignments of `source`, and records it. Writes one
/// row per copied assignment, the node's row and one event, whatever lies below the node.
async fn break_locked(
    tx: &mut PgConnection,
    actor: UserId,
    node: &Node,
    source: NodeId,
) -> Result<(), sqlx::Error> {
    let copied = sqlx::query(
        "insert into scope_access (node_id, user_id, role, granted_by, granted_at) \
         select $1, user_id, role, granted_by, granted_at from scope_access where node_id = $2",
    )
    .bind(node.id.0)
    .bind(source.0)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    sqlx::query("update nodes set unique_scope = true where id = $1")
        .bind(node.id.0)
        .execute(&mut *tx)
        .await?;
    record(
        tx,
        NewEvent {
            kind: "inheritance.broken",
            actor: Some(actor),
            subject: None,
            document: document_of(node),
            node: Some(node.id),
            detail: json!({ "from": source.0, "copied": copied }),
        },
    )
    .await
}

impl Db {
    /// What `user` may do with `node`: the one access decision. `None` for a disabled or
    /// unknown account, a missing node, or one with a deleted node at or above it; `Owner`
    /// for an enabled administrator; otherwise the person's assignment on the nearest scope at
    /// or above the node, if any.
    pub async fn effective_role(
        &self,
        user: UserId,
        node: NodeId,
    ) -> Result<Option<Role>, DbError> {
        let mut conn = self.pool().acquire().await?;
        Ok(role_in(&mut conn, user, node).await?)
    }

    /// The breadcrumb from the site down to `node`, with `user`'s role on each step. `None`
    /// unless the person has a role on the node or limited access to it (a role somewhere
    /// below it).
    pub async fn path(&self, user: UserId, node: NodeId) -> Result<Option<Vec<Crumb>>, DbError> {
        let mut conn = self.pool().acquire().await?;
        let Some(links) = chain(&mut conn, user, node).await? else {
            return Ok(None);
        };
        let has_role = links.last().and_then(|link| link.role).is_some();
        if !has_role && !in_reach(&mut conn, user, node).await? {
            return Ok(None);
        }
        Ok(Some(links.iter().map(crumb).collect()))
    }

    /// The sites `user` can see, by name: those they have a role on, and those where they
    /// have a role only somewhere below (`None`, limited access).
    pub async fn sites_for(&self, user: UserId) -> Result<Vec<(Node, Option<Role>)>, DbError> {
        let mut conn = self.pool().acquire().await?;
        let admin = match standing(&mut conn, user).await? {
            Standing::Nobody => return Ok(Vec::new()),
            Standing::Member => false,
            Standing::Admin => true,
        };
        let query = format!(
            "with recursive {REACH} \
             select {NODE_COLUMNS}, \
                    case when $2 then 'owner' else sa.role end as granted_role \
             from nodes n \
             left join scope_access sa on sa.node_id = n.id and sa.user_id = $1 \
             where n.parent_id is null and n.deleted_at is null \
               and ($2 or sa.role is not null or n.id in (select id from reach)) \
             order by lower(n.name), n.id"
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(query))
            .bind(user.0)
            .bind(admin)
            .fetch_all(&mut *conn)
            .await?;
        rows.iter()
            .map(|row| {
                let role = row
                    .try_get::<Option<String>, _>("granted_role")?
                    .as_deref()
                    .map(parse_role)
                    .transpose()?;
                Ok((node_from_row(row)?, role))
            })
            .collect::<Result<_, sqlx::Error>>()
            .map_err(DbError::from)
    }

    /// One page of the undeleted children of `parent` that `user` can see, after `after`, at
    /// most `limit` (at least one), and where the next page starts if there is one. `None` if
    /// the person can reach neither the parent nor anything below it.
    ///
    /// With a role on the parent, every child that inherits is shown with that role, and a
    /// child that is a scope is shown if the person has a role on it or below it. With limited
    /// access to the parent, only the children that lead to something the person can reach.
    pub async fn children(
        &self,
        user: UserId,
        parent: NodeId,
        after: Option<&Cursor>,
        limit: u32,
    ) -> Result<Option<(Vec<Listed>, Option<Cursor>)>, DbError> {
        let mut conn = self.pool().acquire().await?;
        let Some(links) = chain(&mut conn, user, parent).await? else {
            return Ok(None);
        };
        let parent_role = links.last().and_then(|link| link.role);
        if parent_role.is_none() && !in_reach(&mut conn, user, parent).await? {
            return Ok(None);
        }
        // `chain` already refused a disabled or unknown account.
        let admin = standing(&mut conn, user).await? == Standing::Admin;
        let limit = limit.max(1);
        let query = format!(
            "with recursive {REACH} \
             select {NODE_COLUMNS}, d.editor, d.revised_at, kinds.kind_rank, \
                    case when $3 then 'owner' when n.unique_scope then sa.role \
                         else $4::text end as granted_role \
             from nodes n \
             cross join lateral (select (case when n.kind = 'document' then 1 else 0 end)::int2 \
                                 as kind_rank) kinds \
             left join scope_access sa on sa.node_id = n.id and sa.user_id = $1 \
             left join documents d on d.id = n.id and n.kind = 'document' \
             where n.parent_id = $2 and n.deleted_at is null \
               and ($5::int2 is null \
                    or (kinds.kind_rank, lower(n.name), n.id) > ($5, lower($6), $7)) \
               and ($3 \
                    or (case when n.unique_scope then sa.role else $4::text end) is not null \
                    or n.id in (select id from reach)) \
             order by kinds.kind_rank, lower(n.name), n.id \
             limit $8"
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(query))
            .bind(user.0)
            .bind(parent.0)
            .bind(admin)
            .bind(parent_role.map(Role::as_str))
            .bind(after.map(|c| c.kind_rank))
            .bind(after.map(|c| c.name.as_str()))
            .bind(after.map(|c| c.id.0))
            .bind(i64::from(limit) + 1)
            .fetch_all(&mut *conn)
            .await?;
        let mut listed = Vec::with_capacity(rows.len());
        let mut ranks = Vec::with_capacity(rows.len());
        for row in &rows {
            let role = row
                .try_get::<Option<String>, _>("granted_role")?
                .as_deref()
                .map(parse_role)
                .transpose()?;
            let editor = row
                .try_get::<Option<String>, _>("editor")?
                .as_deref()
                .map(parse_editor)
                .transpose()?;
            ranks.push(row.try_get::<i16, _>("kind_rank")?);
            listed.push(Listed {
                node: node_from_row(row)?,
                role,
                editor,
                revised_at: row.try_get("revised_at")?,
            });
        }
        let page = usize::try_from(limit).unwrap_or(usize::MAX);
        let next = if listed.len() > page {
            listed.truncate(page);
            listed
                .last()
                .zip(ranks.get(page.saturating_sub(1)))
                .map(|(last, rank)| Cursor {
                    kind_rank: *rank,
                    name: last.node.name.clone(),
                    id: last.node.id,
                })
        } else {
            None
        };
        Ok(Some((listed, next)))
    }

    /// Who has access to `node` and where it comes from. `actor` must be an owner of it.
    pub async fn access(&self, actor: UserId, node: NodeId) -> Result<NodeAccess, AccessError> {
        let mut conn = self.pool().acquire().await?;
        let links = owner(chain(&mut conn, actor, node).await?)?;
        let source = links
            .get(source_index(&links, false))
            .ok_or(AccessError::NotFound)?;
        let inherits = links.last().is_some_and(|link| !link.node.unique_scope);
        let grants = grants_on(&mut conn, source.node.id).await?;
        Ok(NodeAccess {
            inherits,
            source: crumb(source),
            grants,
        })
    }

    /// Gives `user` the role `role` on `node`, or removes their assignment when it is `None`.
    /// `actor` must be an owner. If the node inherits, inheritance is first broken in the same
    /// transaction, unless the change would leave the person's role as it is. Returns the
    /// person's role on the node before and after (an administrator stays `Owner` either way).
    ///
    /// Judged with the chain from the source scope down to the node locked, so concurrent
    /// changes take turns, an owner removed by an earlier change gets `NotFound`, and the last
    /// owner assignment of a scope cannot be removed by two calls at once.
    pub async fn set_node_access(
        &self,
        actor: UserId,
        node: NodeId,
        user: UserId,
        role: Option<Role>,
    ) -> Result<(Option<Role>, Option<Role>), AccessError> {
        // Cheap early refusal so that people with no access never wait for a lock. Not
        // authoritative: the check under the locks is.
        {
            let mut conn = self.pool().acquire().await?;
            owner(chain(&mut conn, actor, node).await?)?;
        }
        let (mut tx, links) = self.lock_chain(actor, node, false).await?;
        let links = owner(Some(links))?;
        let exists: Option<bool> = sqlx::query_scalar("select true from users where id = $1")
            .bind(user.0)
            .fetch_optional(&mut *tx)
            .await?;
        if exists.is_none() {
            return Err(AccessError::NoSuchUser);
        }
        let (Some(target), Some(source)) = (links.last(), links.get(source_index(&links, false)))
        else {
            return Err(AccessError::NotFound);
        };
        let current: Option<String> =
            sqlx::query_scalar("select role from scope_access where node_id = $1 and user_id = $2")
                .bind(source.node.id.0)
                .bind(user.0)
                .fetch_optional(&mut *tx)
                .await?;
        let current = current.as_deref().map(parse_role).transpose()?;
        let before = role_in(&mut tx, user, node).await?;
        if current == role {
            return Ok((before, before));
        }
        if !target.node.unique_scope {
            break_locked(&mut tx, actor, &target.node, source.node.id).await?;
        }
        if current == Some(Role::Owner) && role != Some(Role::Owner) {
            let owners: i64 = sqlx::query_scalar(
                "select count(*) from scope_access where node_id = $1 and role = 'owner'",
            )
            .bind(node.0)
            .fetch_one(&mut *tx)
            .await?;
            if owners <= 1 {
                return Err(AccessError::LastOwner);
            }
        }
        let (kind, detail) = match (current, role) {
            (None, Some(new)) => {
                sqlx::query(
                    "insert into scope_access (node_id, user_id, role, granted_by) \
                     values ($1, $2, $3, $4)",
                )
                .bind(node.0)
                .bind(user.0)
                .bind(new.as_str())
                .bind(actor.0)
                .execute(&mut *tx)
                .await?;
                ("access.granted", json!({ "role": new.as_str() }))
            }
            (Some(old), Some(new)) => {
                sqlx::query(
                    "update scope_access set role = $3, granted_by = $4, granted_at = now() \
                     where node_id = $1 and user_id = $2",
                )
                .bind(node.0)
                .bind(user.0)
                .bind(new.as_str())
                .bind(actor.0)
                .execute(&mut *tx)
                .await?;
                (
                    "access.changed",
                    json!({ "from": old.as_str(), "to": new.as_str() }),
                )
            }
            (Some(old), None) => {
                sqlx::query("delete from scope_access where node_id = $1 and user_id = $2")
                    .bind(node.0)
                    .bind(user.0)
                    .execute(&mut *tx)
                    .await?;
                ("access.revoked", json!({ "role": old.as_str() }))
            }
            (None, None) => return Ok((before, before)),
        };
        record(
            &mut tx,
            NewEvent {
                kind,
                actor: Some(actor),
                subject: Some(user),
                document: document_of(&target.node),
                node: Some(node),
                detail,
            },
        )
        .await?;
        let after = role_in(&mut tx, user, node).await?;
        tx.commit().await?;
        Ok((before, after))
    }

    /// Makes `node` a scope of its own, starting with a copy of the assignments it inherited.
    /// `actor` must be an owner. `false` if the node already is a scope (a site always is).
    /// Nothing below the node is read or written.
    pub async fn break_inheritance(
        &self,
        actor: UserId,
        node: NodeId,
    ) -> Result<bool, AccessError> {
        {
            let mut conn = self.pool().acquire().await?;
            owner(chain(&mut conn, actor, node).await?)?;
        }
        let (mut tx, links) = self.lock_chain(actor, node, false).await?;
        let links = owner(Some(links))?;
        let (Some(target), Some(source)) = (links.last(), links.get(source_index(&links, false)))
        else {
            return Err(AccessError::NotFound);
        };
        if target.node.unique_scope {
            return Ok(false);
        }
        break_locked(&mut tx, actor, &target.node, source.node.id).await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Makes `node` inherit again, deleting its own assignments. `actor` must be an owner of
    /// the node and of the scope above it (`Forbidden` otherwise), so nobody restores
    /// themselves out of access by accident. `false` if the node already inherits; `IsSite`
    /// for a site. Nothing below the node is read or written.
    pub async fn restore_inheritance(
        &self,
        actor: UserId,
        node: NodeId,
    ) -> Result<bool, AccessError> {
        {
            let mut conn = self.pool().acquire().await?;
            owner(chain(&mut conn, actor, node).await?)?;
        }
        let (mut tx, links) = self.lock_chain(actor, node, true).await?;
        let links = owner(Some(links))?;
        let Some(target) = links.last() else {
            return Err(AccessError::NotFound);
        };
        if target.node.kind == NodeKind::Site {
            return Err(AccessError::IsSite);
        }
        if !target.node.unique_scope {
            return Ok(false);
        }
        let parent_role = links
            .len()
            .checked_sub(2)
            .and_then(|i| links.get(i))
            .and_then(|link| link.role);
        if !parent_role.is_some_and(Role::can_manage) {
            return Err(AccessError::Forbidden);
        }
        let removed = sqlx::query("delete from scope_access where node_id = $1")
            .bind(node.0)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        sqlx::query("update nodes set unique_scope = false where id = $1")
            .bind(node.0)
            .execute(&mut *tx)
            .await?;
        record(
            &mut tx,
            NewEvent {
                kind: "inheritance.restored",
                actor: Some(actor),
                subject: None,
                document: document_of(&target.node),
                node: Some(node),
                detail: json!({ "removed": removed }),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Everyone with an assignment on the scope that decides access to `node`.
    pub async fn scope_users(&self, node: NodeId) -> Result<Vec<UserId>, DbError> {
        let users: Vec<uuid::Uuid> = sqlx::query_scalar(
            "with recursive up(id, parent_id, unique_scope) as ( \
                 select id, parent_id, unique_scope from nodes where id = $1 \
                 union \
                 select n.id, n.parent_id, n.unique_scope from nodes n \
                 join up on n.id = up.parent_id where not up.unique_scope \
             ) select sa.user_id from up join scope_access sa on sa.node_id = up.id \
               where up.unique_scope order by sa.user_id",
        )
        .bind(node.0)
        .fetch_all(self.pool())
        .await?;
        Ok(users.into_iter().map(UserId).collect())
    }

    /// Opens a transaction and locks the chain from `node`'s source scope down to `node`,
    /// top-down: `node` `FOR UPDATE`, the rest `FOR SHARE`. With `above`, the source is the
    /// nearest scope strictly above `node` (restoring is judged there). Reads the chain again
    /// under the locks, and starts over if it changed meanwhile, so the chain returned, as
    /// `actor` sees it, holds until the transaction ends. Everyone locks ancestors before
    /// descendants, so these locks cannot deadlock one another. The chain is a handful of
    /// rows, whatever lies below.
    pub(crate) async fn lock_chain(
        &self,
        actor: UserId,
        node: NodeId,
        above: bool,
    ) -> Result<(Transaction<'static, Postgres>, Vec<Link>), AccessError> {
        for _ in 0..LOCK_ATTEMPTS {
            let mut tx = self.pool().begin().await?;
            if let Some(now) = lock_segment(&mut tx, actor, node, above, true).await? {
                return Ok((tx, now));
            }
            tx.rollback().await?;
        }
        Err(DbError::new("the tree kept changing while access was being changed").into())
    }
}

/// Locks, on `conn`, the chain from `node`'s source scope down to `node`, top-down: `node`
/// `FOR UPDATE` when `write` (`FOR SHARE` otherwise), the rest `FOR SHARE`. With `above`, the
/// source is the nearest scope strictly above `node`. Reads the chain again under the locks
/// and returns it, as `actor` sees it; `None` if it changed meanwhile, when the caller starts
/// over in a new transaction. `NotFound` if `node` does not lead up to a site for `actor`.
/// Everyone locks ancestors before descendants, so these locks cannot deadlock one another.
pub(crate) async fn lock_segment(
    conn: &mut PgConnection,
    actor: UserId,
    node: NodeId,
    above: bool,
    write: bool,
) -> Result<Option<Vec<Link>>, AccessError> {
    let seen = chain(conn, actor, node)
        .await?
        .ok_or(AccessError::NotFound)?;
    let wanted = segment(&seen, above);
    for (id, _) in &wanted {
        let statement = if *id == node && write {
            "select id from nodes where id = $1 for update"
        } else {
            "select id from nodes where id = $1 for share"
        };
        sqlx::query(statement)
            .bind(id.0)
            .execute(&mut *conn)
            .await?;
    }
    let now = chain(conn, actor, node)
        .await?
        .ok_or(AccessError::NotFound)?;
    Ok((segment(&now, above) == wanted).then_some(now))
}

/// The assignments on scope `node`, by name.
async fn grants_on(conn: &mut PgConnection, node: NodeId) -> Result<Vec<Grant>, sqlx::Error> {
    let rows = sqlx::query(
        "select u.id, u.email, u.name, u.is_admin, (u.disabled_at is not null) as disabled, \
         sa.role \
         from scope_access sa join users u on u.id = sa.user_id \
         where sa.node_id = $1 order by u.name, u.id",
    )
    .bind(node.0)
    .fetch_all(conn)
    .await?;
    rows.iter()
        .map(|row| {
            let role: String = row.try_get("role")?;
            Ok(Grant {
                user: user_from_row(row)?,
                role: parse_role(&role)?,
            })
        })
        .collect()
}
