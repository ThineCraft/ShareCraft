//! Inherited access on the content tree. A node either inherits or is a scope with its own
//! assignments; a person's access to a node is the union of the permissions of the roles of
//! every assignment naming them on the nearest scope at or above it. Breaking inheritance
//! copies the parent's assignments onto the node and restoring deletes them: neither touches
//! anything below the node, whatever lies there.
//!
//! One locking order for everyone: the node chain from the top down, then the groups involved
//! by id (once groups exist), then the roles involved by id; each row once, in the strongest
//! mode the transaction needs. A decision is worked out again once every lock is held.

use std::collections::HashMap;

use collab_core::ids::{DocId, NodeId, UserId};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{PgConnection, Postgres, Row, Transaction};
use time::OffsetDateTime;

use crate::accounts::user_from_row;
use crate::documents::parse_editor;
use crate::events::{NewEvent, record};
use crate::roles::{lock_roles, role_by_id};
use crate::tree::node_from_row;
use crate::{
    Access, AccessError, Db, DbError, Editor, Grant, Node, NodeKind, Perms, Principal, RoleDef,
    RoleRef,
};

/// How many times an access change reads and locks its chain before giving up because the
/// tree kept moving under it.
pub(crate) const LOCK_ATTEMPTS: usize = 10;

/// Every assignment with its role's name and permissions. Defines the CTE `assigned`.
macro_rules! assigned {
    () => {
        "assigned(node_id, principal_kind, principal_id, role_id, role_name, perms) as ( \
            select a.node_id, a.principal_kind, a.principal_id, r.id, r.name, r.perms \
            from assignments a join roles r on r.id = a.role_id)"
    };
}

/// "My assignments", the one place that says which assignments apply to a person: those naming
/// the person `$1` (and, once groups exist, those of the groups they belong to), each with its
/// role. Everything that works out a person's access reads it: the chain, reach, sites and
/// listings. Defines the CTEs `assigned` and `mine(node_id, role_id, role_name, perms)`.
macro_rules! mine {
    () => {
        concat!(
            assigned!(),
            ", mine(node_id, role_id, role_name, perms) as ( \
                select node_id, role_id, role_name, perms from assigned \
                where principal_kind = 'user' and principal_id = $1)"
        )
    };
}

/// The nodes `$1` can reach through their assignments: every node at or above a scope where
/// one of their assignments (see [`mine!`]) is, leaving out scopes with a deleted node at or
/// above them. Starts from the person's own assignments, so its cost follows their number, not
/// the tree's size. Walks with `union`, so it ends even if the data held a cycle. Defines the
/// CTEs of [`mine!`] and `grants`, `up`, `live` and `reach`; a query puts it after
/// `with recursive`.
pub(crate) const REACH: &str = concat!(
    mine!(),
    ", grants(node_id) as ( \
        select distinct m.node_id from mine m \
        join users u on u.id = $1 and u.disabled_at is null \
        join nodes g on g.id = m.node_id and g.unique_scope), \
    up(origin, id, parent_id, deleted) as ( \
        select n.id, n.id, n.parent_id, n.deleted_at is not null \
        from grants join nodes n on n.id = grants.node_id \
        union \
        select up.origin, n.id, n.parent_id, n.deleted_at is not null \
        from up join nodes n on n.id = up.parent_id), \
    live(origin) as (select origin from up group by origin having not bool_or(deleted)), \
    reach(id) as (select distinct id from up where origin in (select origin from live))"
);

/// `node` ($2) and everything above it; each scope once per assignment of `$1`'s on it (see
/// [`mine!`]) with that assignment's role, or once with nulls if there is none.
const CHAIN: &str = concat!(
    "with recursive ",
    mine!(),
    ", up(id, kind, parent_id, site_id, name, unique_scope, deleted, created_by, \
          created_at) as ( \
        select id, kind, parent_id, site_id, name, unique_scope, deleted_at is not null, \
               created_by, created_at \
        from nodes where id = $2 \
        union \
        select n.id, n.kind, n.parent_id, n.site_id, n.name, n.unique_scope, \
               n.deleted_at is not null, n.created_by, n.created_at \
        from nodes n join up on n.id = up.parent_id) \
    select up.*, m.role_id, m.role_name, m.perms from up \
    left join mine m on m.node_id = up.id and up.unique_scope"
);

/// The access the person's assignments (see [`mine!`]) give on the node `n` of the enclosing
/// query when it is a scope, as the columns [`Access::from_aggregate`] reads.
const MINE_ON_N: &str = "\
    left join lateral ( \
        select bit_or(m.perms) as perms, \
               array_agg(m.role_id order by m.role_id) as role_ids, \
               array_agg(m.role_name order by m.role_id) as role_names \
        from mine m where m.node_id = n.id and n.unique_scope) g on true";

/// Every assignment with its role (see [`assigned!`]), after `with`.
const ASSIGNED: &str = assigned!();

/// The bit of `manage_access` in a `perms` column.
const MANAGE_ACCESS_BIT: i32 = 16;

const NODE_COLUMNS: &str = "n.id, n.kind, n.parent_id, n.site_id, n.name, n.unique_scope, \
     (n.deleted_at is not null) as deleted, n.created_by, n.created_at";

/// One step of a breadcrumb: a node from a site down, with the person's access to it (`None`
/// where they have only limited access: access somewhere below).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Crumb {
    pub id: NodeId,
    pub kind: NodeKind,
    pub name: String,
    pub role: Option<Access>,
}

/// A child in a listing, with the person's access to it (`None` for limited access) and, for a
/// file, its editor and when it was last saved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listed {
    pub node: Node,
    pub role: Option<Access>,
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
    /// The people's assignments on `source`, by name.
    pub grants: Vec<Grant>,
}

/// One node of a chain from a site down, with someone's access to it.
#[derive(Clone, Debug)]
pub(crate) struct Link {
    pub(crate) node: Node,
    pub(crate) role: Option<Access>,
}

/// Whether an account may have any access, and whether it is an administrator.
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

/// The chain from the site down to `node`, with `user`'s access to each node. `None` for a
/// disabled or unknown account, or when the node is missing, has a deleted node at or above
/// it, or does not lead up to a site (as a cycle would not).
///
/// This is the one place access is decided: an enabled administrator has every permission
/// everywhere; otherwise a scope gives the union of the person's assignments on it (see
/// [`mine!`]), and any other node the access of the node above.
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
    let mut by_id: HashMap<NodeId, (Node, Vec<(RoleRef, Perms)>)> =
        HashMap::with_capacity(rows.len());
    for row in &rows {
        let found = node_from_row(row)?;
        let given = match row.try_get::<Option<String>, _>("role_id")? {
            Some(id) => Some((
                RoleRef {
                    id,
                    name: row.try_get("role_name")?,
                },
                Perms::from_column(row.try_get("perms")?)?,
            )),
            None => None,
        };
        by_id
            .entry(found.id)
            .or_insert_with(|| (found, Vec::new()))
            .1
            .extend(given);
    }
    let mut upward = Vec::with_capacity(by_id.len());
    let mut next = Some(node);
    while let Some(id) = next {
        // Taking each node out means a cycle finds its start gone and ends here.
        let Some((found, given)) = by_id.remove(&id) else {
            return Ok(None);
        };
        if found.deleted {
            return Ok(None);
        }
        next = found.parent;
        upward.push((found, given));
    }
    if upward.last().map(|(top, _)| top.kind) != Some(NodeKind::Site) {
        return Ok(None);
    }
    let mut role = None;
    Ok(Some(
        upward
            .into_iter()
            .rev()
            .map(|(node, given)| {
                if admin {
                    role = Some(Access::administrator());
                } else if node.unique_scope {
                    role = Access::union_of(given);
                }
                Link {
                    node,
                    role: role.clone(),
                }
            })
            .collect(),
    ))
}

/// What `user` may do with `node`, on `conn`.
pub(crate) async fn access_in(
    conn: &mut PgConnection,
    user: UserId,
    node: NodeId,
) -> Result<Option<Access>, sqlx::Error> {
    Ok(chain(conn, user, node)
        .await?
        .and_then(|mut links| links.pop().and_then(|link| link.role)))
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

/// The ids of the roles that give the access in `links`: what a decision on them is judged by.
pub(crate) fn judged_roles(links: &[Link]) -> impl Iterator<Item = String> + '_ {
    links
        .iter()
        .filter_map(|link| link.role.as_ref())
        .flat_map(|access| access.roles.iter().map(|role| role.id.clone()))
}

/// `NotFound` unless the chain exists and its person has access to the last node, `Forbidden`
/// unless that access includes `manage_access`.
fn manager(links: Option<Vec<Link>>) -> Result<Vec<Link>, AccessError> {
    let links = links.ok_or(AccessError::NotFound)?;
    match links.last().and_then(|link| link.role.as_ref()) {
        None => Err(AccessError::NotFound),
        Some(access) if access.can_manage_access() => Ok(links),
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
        role: link.role.clone(),
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
        "insert into assignments \
             (node_id, principal_kind, principal_id, role_id, granted_by, granted_at) \
         select $1, principal_kind, principal_id, role_id, granted_by, granted_at \
         from assignments where node_id = $2",
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

/// The role `principal`'s assignment on scope `node` gives, with its permissions, if any.
async fn assignment_on(
    conn: &mut PgConnection,
    node: NodeId,
    principal: Principal,
) -> Result<Option<(RoleRef, Perms)>, sqlx::Error> {
    let query = format!(
        "with {ASSIGNED} select role_id, role_name, perms from assigned \
         where node_id = $1 and principal_kind = $2 and principal_id = $3"
    );
    let row: Option<(String, String, i32)> = sqlx::query_as(sqlx::AssertSqlSafe(query))
        .bind(node.0)
        .bind(principal.kind())
        .bind(principal.id())
        .fetch_optional(conn)
        .await?;
    row.map(|(id, name, perms)| Ok((RoleRef { id, name }, Perms::from_column(perms)?)))
        .transpose()
}

/// How many assignments on scope `node` give a role with `manage_access`.
async fn managers_on(conn: &mut PgConnection, node: NodeId) -> Result<i64, sqlx::Error> {
    let query = format!(
        "with {ASSIGNED} select count(*) from assigned \
         where node_id = $1 and perms & {MANAGE_ACCESS_BIT} <> 0"
    );
    sqlx::query_scalar(sqlx::AssertSqlSafe(query))
        .bind(node.0)
        .fetch_one(conn)
        .await
}

/// `detail` with the group the assignment names, if it names one.
fn with_principal(principal: Principal, mut detail: serde_json::Value) -> serde_json::Value {
    if let (Principal::Group(group), Some(map)) = (principal, detail.as_object_mut()) {
        map.insert("group".to_owned(), json!(group.0));
    }
    detail
}

impl Db {
    /// What `user` may do with `node`: the one access decision. `None` for a disabled or
    /// unknown account, a missing node, or one with a deleted node at or above it; every
    /// permission (shown as Owner) for an enabled administrator; otherwise the union of the
    /// person's assignments on the nearest scope at or above the node, if any.
    pub async fn effective_access(
        &self,
        user: UserId,
        node: NodeId,
    ) -> Result<Option<Access>, DbError> {
        let mut conn = self.pool().acquire().await?;
        Ok(access_in(&mut conn, user, node).await?)
    }

    /// The breadcrumb from the site down to `node`, with `user`'s access to each step. `None`
    /// unless the person has access to the node or limited access to it (access somewhere
    /// below it).
    pub async fn path(&self, user: UserId, node: NodeId) -> Result<Option<Vec<Crumb>>, DbError> {
        let mut conn = self.pool().acquire().await?;
        let Some(links) = chain(&mut conn, user, node).await? else {
            return Ok(None);
        };
        let has_access = links.last().and_then(|link| link.role.as_ref()).is_some();
        if !has_access && !in_reach(&mut conn, user, node).await? {
            return Ok(None);
        }
        Ok(Some(links.iter().map(crumb).collect()))
    }

    /// The sites `user` can see, by name: those they have access to, and those where they have
    /// access only somewhere below (`None`, limited access).
    pub async fn sites_for(&self, user: UserId) -> Result<Vec<(Node, Option<Access>)>, DbError> {
        let mut conn = self.pool().acquire().await?;
        let admin = match standing(&mut conn, user).await? {
            Standing::Nobody => return Ok(Vec::new()),
            Standing::Member => false,
            Standing::Admin => true,
        };
        let query = format!(
            "with recursive {REACH} \
             select {NODE_COLUMNS}, g.perms, g.role_ids, g.role_names \
             from nodes n {MINE_ON_N} \
             where n.parent_id is null and n.deleted_at is null \
               and ($2 or g.perms is not null or n.id in (select id from reach)) \
             order by lower(n.name), n.id"
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(query))
            .bind(user.0)
            .bind(admin)
            .fetch_all(&mut *conn)
            .await?;
        rows.iter()
            .map(|row| {
                let access = if admin {
                    Some(Access::administrator())
                } else {
                    Access::from_aggregate(row)?
                };
                Ok((node_from_row(row)?, access))
            })
            .collect::<Result<_, sqlx::Error>>()
            .map_err(DbError::from)
    }

    /// One page of the undeleted children of `parent` that `user` can see, after `after`, at
    /// most `limit` (at least one), and where the next page starts if there is one. `None` if
    /// the person can reach neither the parent nor anything below it.
    ///
    /// With access to the parent, every child that inherits is shown with that access, and a
    /// child that is a scope is shown if the person has access to it or below it. With limited
    /// access to the parent, only the children that lead to something the person can reach.
    pub async fn children(
        &self,
        user: UserId,
        parent: NodeId,
        after: Option<&Cursor>,
        limit: u32,
    ) -> Result<Option<(Vec<Listed>, Option<Cursor>)>, DbError> {
        let mut conn = self.pool().acquire().await?;
        let Some(mut links) = chain(&mut conn, user, parent).await? else {
            return Ok(None);
        };
        let parent_access = links.pop().and_then(|link| link.role);
        if parent_access.is_none() && !in_reach(&mut conn, user, parent).await? {
            return Ok(None);
        }
        // `chain` already refused a disabled or unknown account.
        let admin = standing(&mut conn, user).await? == Standing::Admin;
        let limit = limit.max(1);
        let query = format!(
            "with recursive {REACH} \
             select {NODE_COLUMNS}, d.editor, d.revised_at, kinds.kind_rank, \
                    g.perms, g.role_ids, g.role_names \
             from nodes n \
             cross join lateral (select (case when n.kind = 'document' then 1 else 0 end)::int2 \
                                 as kind_rank) kinds \
             {MINE_ON_N} \
             left join documents d on d.id = n.id and n.kind = 'document' \
             where n.parent_id = $2 and n.deleted_at is null \
               and ($5::int2 is null \
                    or (kinds.kind_rank, lower(n.name), n.id) > ($5, lower($6), $7)) \
               and ($3 \
                    or (case when n.unique_scope then g.perms is not null else $4 end) \
                    or n.id in (select id from reach)) \
             order by kinds.kind_rank, lower(n.name), n.id \
             limit $8"
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(query))
            .bind(user.0)
            .bind(parent.0)
            .bind(admin)
            .bind(parent_access.is_some())
            .bind(after.map(|c| c.kind_rank))
            .bind(after.map(|c| c.name.as_str()))
            .bind(after.map(|c| c.id.0))
            .bind(i64::from(limit) + 1)
            .fetch_all(&mut *conn)
            .await?;
        let mut listed = Vec::with_capacity(rows.len());
        let mut ranks = Vec::with_capacity(rows.len());
        for row in &rows {
            let node = node_from_row(row)?;
            let role = if admin {
                Some(Access::administrator())
            } else if node.unique_scope {
                Access::from_aggregate(row)?
            } else {
                parent_access.clone()
            };
            let editor = row
                .try_get::<Option<String>, _>("editor")?
                .as_deref()
                .map(parse_editor)
                .transpose()?;
            ranks.push(row.try_get::<i16, _>("kind_rank")?);
            listed.push(Listed {
                node,
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

    /// Who has access to `node` and where it comes from. `actor` must manage access to it.
    pub async fn access(&self, actor: UserId, node: NodeId) -> Result<NodeAccess, AccessError> {
        let mut conn = self.pool().acquire().await?;
        let links = manager(chain(&mut conn, actor, node).await?)?;
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

    /// Gives `principal` the role `role` (a role id) on `node`, or removes their assignment
    /// when it is `None`. `actor` must manage access to the node, and may only give a role
    /// whose permissions they hold there themselves (`BeyondYourAccess`; an administrator holds
    /// every permission). `NoSuchUser` or `NoSuchGroup` for an unknown principal, `NoSuchRole`
    /// for an unknown role. If the node inherits, inheritance is first broken in the same
    /// transaction, unless the change would leave the principal's assignment as it is. A change
    /// that would leave the scope with no assignment whose role includes `manage_access` is
    /// refused (`LastManager`). Returns the role of the principal's assignment on the scope
    /// deciding the node, before and after.
    ///
    /// Judged with the chain from the source scope down to the node locked, then the roles
    /// involved, so concurrent changes take turns, a manager removed by an earlier change gets
    /// `NotFound`, and the last manager of a scope cannot be removed by two calls at once.
    pub async fn set_node_access(
        &self,
        actor: UserId,
        node: NodeId,
        principal: Principal,
        role: Option<&str>,
    ) -> Result<(Option<RoleRef>, Option<RoleRef>), AccessError> {
        // Cheap early refusal so that people with no access never wait for a lock. Not
        // authoritative: the check under the locks is.
        {
            let mut conn = self.pool().acquire().await?;
            manager(chain(&mut conn, actor, node).await?)?;
        }
        let (mut tx, links) = self.lock_chain(actor, node, false, role).await?;
        let links = manager(Some(links))?;
        match principal {
            Principal::User(user) => {
                let exists: Option<bool> =
                    sqlx::query_scalar("select true from users where id = $1")
                        .bind(user.0)
                        .fetch_optional(&mut *tx)
                        .await?;
                if exists.is_none() {
                    return Err(AccessError::NoSuchUser);
                }
            }
            // No group exists before groups are introduced.
            Principal::Group(_) => return Err(AccessError::NoSuchGroup),
        }
        let new = match role {
            Some(id) => Some(
                role_by_id(&mut tx, id)
                    .await?
                    .ok_or(AccessError::NoSuchRole)?,
            ),
            None => None,
        };
        let (Some(target), Some(source)) = (links.last(), links.get(source_index(&links, false)))
        else {
            return Err(AccessError::NotFound);
        };
        if let (Some(new), Some(held)) = (&new, &target.role)
            && !held.perms.contains(new.perms)
        {
            return Err(AccessError::BeyondYourAccess);
        }
        let current = assignment_on(&mut tx, source.node.id, principal).await?;
        let before = current.as_ref().map(|(role, _)| role.clone());
        let after = new.as_ref().map(RoleDef::role_ref);
        if before.as_ref().map(|r| &r.id) == after.as_ref().map(|r| &r.id) {
            return Ok((before.clone(), before));
        }
        if !target.node.unique_scope {
            break_locked(&mut tx, actor, &target.node, source.node.id).await?;
        }
        let was_manager = current
            .as_ref()
            .is_some_and(|(_, perms)| perms.contains(Perms::MANAGE_ACCESS));
        let stays_manager = new
            .as_ref()
            .is_some_and(|r| r.perms.contains(Perms::MANAGE_ACCESS));
        if was_manager && !stays_manager && managers_on(&mut tx, node).await? <= 1 {
            return Err(AccessError::LastManager);
        }
        let (kind, detail) = match (&before, &after) {
            (None, Some(new)) => {
                sqlx::query(
                    "insert into assignments \
                         (node_id, principal_kind, principal_id, role_id, granted_by) \
                     values ($1, $2, $3, $4, $5)",
                )
                .bind(node.0)
                .bind(principal.kind())
                .bind(principal.id())
                .bind(&new.id)
                .bind(actor.0)
                .execute(&mut *tx)
                .await?;
                ("access.granted", json!({ "role": new.id }))
            }
            (Some(old), Some(new)) => {
                sqlx::query(
                    "update assignments set role_id = $4, granted_by = $5, granted_at = now() \
                     where node_id = $1 and principal_kind = $2 and principal_id = $3",
                )
                .bind(node.0)
                .bind(principal.kind())
                .bind(principal.id())
                .bind(&new.id)
                .bind(actor.0)
                .execute(&mut *tx)
                .await?;
                ("access.changed", json!({ "from": old.id, "to": new.id }))
            }
            (Some(old), None) => {
                sqlx::query(
                    "delete from assignments \
                     where node_id = $1 and principal_kind = $2 and principal_id = $3",
                )
                .bind(node.0)
                .bind(principal.kind())
                .bind(principal.id())
                .execute(&mut *tx)
                .await?;
                ("access.revoked", json!({ "role": old.id }))
            }
            (None, None) => return Ok((None, None)),
        };
        let subject = match principal {
            Principal::User(user) => Some(user),
            Principal::Group(_) => None,
        };
        record(
            &mut tx,
            NewEvent {
                kind,
                actor: Some(actor),
                subject,
                document: document_of(&target.node),
                node: Some(node),
                detail: with_principal(principal, detail),
            },
        )
        .await?;
        tx.commit().await?;
        Ok((before, after))
    }

    /// Makes `node` a scope of its own, starting with a copy of the assignments it inherited.
    /// `actor` must manage access to it. `false` if the node already is a scope (a site always
    /// is). Nothing below the node is read or written.
    pub async fn break_inheritance(
        &self,
        actor: UserId,
        node: NodeId,
    ) -> Result<bool, AccessError> {
        {
            let mut conn = self.pool().acquire().await?;
            manager(chain(&mut conn, actor, node).await?)?;
        }
        let (mut tx, links) = self.lock_chain(actor, node, false, None).await?;
        let links = manager(Some(links))?;
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

    /// Makes `node` inherit again, deleting its own assignments. `actor` must manage access to
    /// the node and to the scope above it (`Forbidden` otherwise), so nobody restores
    /// themselves out of access by accident. `false` if the node already inherits; `IsSite`
    /// for a site. Nothing below the node is read or written.
    pub async fn restore_inheritance(
        &self,
        actor: UserId,
        node: NodeId,
    ) -> Result<bool, AccessError> {
        {
            let mut conn = self.pool().acquire().await?;
            manager(chain(&mut conn, actor, node).await?)?;
        }
        let (mut tx, links) = self.lock_chain(actor, node, true, None).await?;
        let links = manager(Some(links))?;
        let Some(target) = links.last() else {
            return Err(AccessError::NotFound);
        };
        if target.node.kind == NodeKind::Site {
            return Err(AccessError::IsSite);
        }
        if !target.node.unique_scope {
            return Ok(false);
        }
        let parent_manages = links
            .len()
            .checked_sub(2)
            .and_then(|i| links.get(i))
            .and_then(|link| link.role.as_ref())
            .is_some_and(Access::can_manage_access);
        if !parent_manages {
            return Err(AccessError::Forbidden);
        }
        let removed = sqlx::query("delete from assignments where node_id = $1")
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

    /// Everyone with an assignment of their own on the scope that decides access to `node`.
    pub async fn scope_users(&self, node: NodeId) -> Result<Vec<UserId>, DbError> {
        let users: Vec<uuid::Uuid> = sqlx::query_scalar(
            "with recursive up(id, parent_id, unique_scope) as ( \
                 select id, parent_id, unique_scope from nodes where id = $1 \
                 union \
                 select n.id, n.parent_id, n.unique_scope from nodes n \
                 join up on n.id = up.parent_id where not up.unique_scope \
             ) select a.principal_id from up join assignments a on a.node_id = up.id \
               where up.unique_scope and a.principal_kind = 'user' order by a.principal_id",
        )
        .bind(node.0)
        .fetch_all(self.pool())
        .await?;
        Ok(users.into_iter().map(UserId).collect())
    }

    /// Opens a transaction and locks, in the one locking order, the chain from `node`'s source
    /// scope down to `node` (top-down: `node` `FOR UPDATE`, the rest `FOR SHARE`), then, by id
    /// and `FOR SHARE`, the roles involved: those giving `actor` access along the chain, those
    /// of the assignments on the source scope, and `role` if given. With `above`, the source is
    /// the nearest scope strictly above `node` (restoring is judged there). Reads the chain
    /// again once everything is locked, and starts over if it changed meanwhile, so the chain
    /// returned, as `actor` sees it, holds until the transaction ends. The chain is a handful
    /// of rows, whatever lies below.
    pub(crate) async fn lock_chain(
        &self,
        actor: UserId,
        node: NodeId,
        above: bool,
        role: Option<&str>,
    ) -> Result<(Transaction<'static, Postgres>, Vec<Link>), AccessError> {
        for _ in 0..LOCK_ATTEMPTS {
            let mut tx = self.pool().begin().await?;
            if let Some(seen) = lock_segment(&mut tx, actor, node, above, true).await? {
                let wanted = segment(&seen, above);
                let source = wanted.first().map_or(node, |(id, _)| *id);
                let on_source: Vec<String> = sqlx::query_scalar(
                    "select distinct role_id from assignments where node_id = $1",
                )
                .bind(source.0)
                .fetch_all(&mut *tx)
                .await?;
                let involved: Vec<String> = judged_roles(&seen)
                    .chain(on_source)
                    .chain(role.map(str::to_owned))
                    .collect();
                lock_roles(&mut tx, involved).await?;
                // Worked out again now that every lock is held.
                let now = chain(&mut tx, actor, node)
                    .await?
                    .ok_or(AccessError::NotFound)?;
                if segment(&now, above) == wanted {
                    return Ok((tx, now));
                }
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
/// Roles are not locked here: the caller locks them once every chain it needs is locked.
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

/// The people's assignments on scope `node`, by name.
async fn grants_on(conn: &mut PgConnection, node: NodeId) -> Result<Vec<Grant>, sqlx::Error> {
    let rows = sqlx::query(
        "select u.id, u.email, u.name, u.is_admin, (u.disabled_at is not null) as disabled, \
                r.id as role_id, r.name as role_name, r.perms \
         from assignments a \
         join users u on a.principal_kind = 'user' and u.id = a.principal_id \
         join roles r on r.id = a.role_id \
         where a.node_id = $1 order by u.name, u.id",
    )
    .bind(node.0)
    .fetch_all(conn)
    .await?;
    rows.iter()
        .map(|row| {
            Ok(Grant {
                user: user_from_row(row)?,
                role: RoleRef {
                    id: row.try_get("role_id")?,
                    name: row.try_get("role_name")?,
                },
                perms: Perms::from_column(row.try_get("perms")?)?,
            })
        })
        .collect()
}
