//! Sharing links. A signed-in link is a hidden group (`groups.purpose = 'link'`, no name and no
//! site) assigned the link's role on the shared item; opening the link while signed in makes the
//! person a member, and from then on the one access decision applies unchanged. An anyone link
//! is not a group: its token is checked on every request it makes ([`Db::anyone_link`]), and it
//! reaches only what a Viewer assignment on the item would reach. Only a SHA-256 hash of a link's
//! token is stored.
//!
//! Links follow the one locking order: the item's chain, then the link's row (`FOR UPDATE` to
//! revoke, `FOR SHARE` to redeem), then the groups by id, then the roles by id.

use std::fmt;

use collab_core::ids::{DocId, GroupId, NodeId, UserId};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use crate::accounts::{hex32, parse_hex32};
use crate::documents::parse_editor;
use crate::events::{NewEvent, record};
use crate::explorer::UserRef;
use crate::groups::{all_locked, groups_judged_by, lock_groups};
use crate::roles::{lock_roles, role_by_id};
use crate::scopes::{
    CHILD_ORDER, DEAD, LOCK_ATTEMPTS, NODE_COLUMNS, break_locked, chain, document_of, judged_roles,
    lock_segment, lock_top_down, manager, scopes_of, segment, source_index, with_principal,
};
use crate::settings::sharing_in;
use crate::tree::node_from_row;
use crate::{
    Access, AccessError, Cursor, Db, DbError, Listed, Node, Perms, Principal, RoleDef, RoleRef,
};

/// What kind of link.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkKind {
    /// For people signed in to the organisation: opening it adds them to its hidden group.
    SignedIn,
    /// For anyone with the address, without an account: view and download only, always
    /// expiring, and only while the organisation allows anyone links.
    Anyone,
}

impl LinkKind {
    /// `signed_in` or `anyone`, as stored in `links.kind`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            LinkKind::SignedIn => "signed_in",
            LinkKind::Anyone => "anyone",
        }
    }

    /// The kind stored as `text`, if it is one.
    #[must_use]
    pub fn parse(text: &str) -> Option<LinkKind> {
        match text {
            "signed_in" => Some(LinkKind::SignedIn),
            "anyone" => Some(LinkKind::Anyone),
            _ => None,
        }
    }
}

/// A sharing link, as shown to the people who manage access to its item. Never holds the
/// token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub id: Uuid,
    /// The shared item.
    pub node: NodeId,
    pub kind: LinkKind,
    /// The role it gives; `None` only for a revoked link whose role has since been deleted.
    pub role: Option<RoleRef>,
    pub created_by: UserRef,
    pub created_at: OffsetDateTime,
    pub expires_at: Option<OffsetDateTime>,
    /// Who revoked it (`None` for the server, as when it expired), when, and why.
    pub revoked: Option<(Option<UserRef>, OffsetDateTime, String)>,
    /// How many people have joined it (always 0 for an anyone link and once revoked).
    pub members: i64,
}

/// The secret in a link's address, `/l/<token>`. Only its SHA-256 is stored, so a link's
/// address is shown once, when it is made. Never printed: `Debug` hides it.
#[derive(Clone, PartialEq, Eq)]
pub struct LinkToken([u8; 32]);

impl LinkToken {
    /// 32 random bytes from the operating system.
    pub(crate) fn generate() -> Result<LinkToken, getrandom::Error> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes)?;
        Ok(LinkToken(bytes))
    }

    /// The token as 64 lowercase hexadecimal characters, for the link's address.
    #[must_use]
    pub fn as_url_value(&self) -> String {
        hex32(&self.0)
    }

    /// Reads a token from a link's address; `None` unless it is exactly 64 hexadecimal
    /// characters.
    #[must_use]
    pub fn parse(text: &str) -> Option<LinkToken> {
        parse_hex32(text).map(LinkToken)
    }

    /// The SHA-256 of the token, as stored.
    pub(crate) fn hash(&self) -> Vec<u8> {
        Sha256::digest(self.0).to_vec()
    }
}

impl fmt::Debug for LinkToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LinkToken(<redacted>)")
    }
}

/// Why opening a signed-in link failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LinkError {
    /// Unknown, revoked, expired or an anyone link, or its item is deleted; deliberately not
    /// distinguished.
    #[error("this link does not work")]
    NotFound,
    #[error(transparent)]
    Db(#[from] DbError),
}

impl From<sqlx::Error> for LinkError {
    fn from(e: sqlx::Error) -> Self {
        LinkError::Db(e.into())
    }
}

/// What an anyone link lets its holder see: the link, the shared item (`root`) and the node
/// asked for, at or below it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnyoneGrant {
    pub link: Uuid,
    pub root: NodeId,
    pub node: Node,
}

/// A live link whose row and group a transaction holds `FOR UPDATE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LockedLink {
    pub(crate) id: Uuid,
    pub(crate) group: GroupId,
}

/// The live signed-in links whose group has an assignment on `node`, by id; with `lock`, their
/// rows are locked `FOR UPDATE`, in that order.
pub(crate) async fn links_assigned_on(
    conn: &mut PgConnection,
    node: NodeId,
    lock: bool,
) -> Result<Vec<LockedLink>, sqlx::Error> {
    let statement = if lock {
        "select l.id, l.group_id from links l \
         where l.revoked_at is null and l.group_id in ( \
             select a.principal_id from assignments a \
             where a.node_id = $1 and a.principal_kind = 'group') \
         order by l.id for update"
    } else {
        "select l.id, l.group_id from links l \
         where l.revoked_at is null and l.group_id in ( \
             select a.principal_id from assignments a \
             where a.node_id = $1 and a.principal_kind = 'group') \
         order by l.id"
    };
    let rows: Vec<(Uuid, Uuid)> = sqlx::query_as(statement)
        .bind(node.0)
        .fetch_all(conn)
        .await?;
    Ok(rows
        .into_iter()
        .map(|(id, group)| LockedLink {
            id,
            group: GroupId(group),
        })
        .collect())
}

/// The link groups with an assignment on `node`, by id: breaking inheritance below `node`
/// copies their assignments, so it locks them `FOR SHARE` against a revoke deleting them.
pub(crate) async fn link_groups_on(
    conn: &mut PgConnection,
    node: NodeId,
) -> Result<Vec<GroupId>, sqlx::Error> {
    let groups: Vec<Uuid> = sqlx::query_scalar(
        "select a.principal_id from assignments a join groups g on g.id = a.principal_id \
         where a.node_id = $1 and a.principal_kind = 'group' and g.purpose = 'link' \
         order by a.principal_id",
    )
    .bind(node.0)
    .fetch_all(conn)
    .await?;
    Ok(groups.into_iter().map(GroupId).collect())
}

/// Revokes the link `link`, whose row and (for a signed-in link) group the caller holds
/// `FOR UPDATE`: deletes every assignment of its group, wherever a break copied it, and every
/// membership, marks the link revoked by `actor` (`None` for the server) for `reason`, and
/// records `access.revoked` for each assignment and one `link.revoked`. `None` if it was already
/// revoked; otherwise the nodes whose assignments it removed, by id.
pub(crate) async fn revoke_locked(
    conn: &mut PgConnection,
    actor: Option<UserId>,
    link: Uuid,
    reason: &str,
) -> Result<Option<Vec<NodeId>>, sqlx::Error> {
    let row: Option<(Uuid, String, Option<Uuid>, bool)> = sqlx::query_as(
        "select node_id, kind, group_id, revoked_at is not null from links where id = $1",
    )
    .bind(link)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((item, kind, group, revoked)) = row else {
        return Ok(None);
    };
    if revoked {
        return Ok(None);
    }
    let mut removed: Vec<(Uuid, String, String)> = Vec::new();
    let mut members = 0;
    if let Some(group) = group {
        removed = sqlx::query_as(
            "delete from assignments a using nodes n \
             where n.id = a.node_id and a.principal_kind = 'group' and a.principal_id = $1 \
             returning a.node_id, n.kind, a.role_id",
        )
        .bind(group)
        .fetch_all(&mut *conn)
        .await?;
        removed.sort();
        members = sqlx::query("delete from group_members where group_id = $1")
            .bind(group)
            .execute(&mut *conn)
            .await?
            .rows_affected();
    }
    sqlx::query(
        "update links set revoked_at = now(), revoked_by = $2, revoke_reason = $3 where id = $1",
    )
    .bind(link)
    .bind(actor.map(|a| a.0))
    .bind(reason)
    .execute(&mut *conn)
    .await?;
    if let Some(group) = group {
        for (node, node_kind, role) in &removed {
            record(
                conn,
                NewEvent {
                    kind: "access.revoked",
                    actor,
                    subject: None,
                    document: (node_kind == "document").then_some(DocId(*node)),
                    node: Some(NodeId(*node)),
                    detail: with_principal(
                        Principal::Group(GroupId(group)),
                        json!({ "role": role, "reason": "link revoked", "link": link }),
                    ),
                },
            )
            .await?;
        }
    }
    let item_kind: Option<String> = sqlx::query_scalar("select kind from nodes where id = $1")
        .bind(item)
        .fetch_optional(&mut *conn)
        .await?;
    record(
        conn,
        NewEvent {
            kind: "link.revoked",
            actor,
            subject: None,
            document: (item_kind.as_deref() == Some("document")).then_some(DocId(item)),
            node: Some(NodeId(item)),
            detail: json!({
                "link": link,
                "kind": kind,
                "reason": reason,
                "members": members,
                "assignments": removed.len(),
            }),
        },
    )
    .await?;
    let mut nodes: Vec<NodeId> = removed.iter().map(|(node, _, _)| NodeId(*node)).collect();
    nodes.dedup();
    Ok(Some(nodes))
}

/// Locks, `FOR UPDATE` and by id, the live links at or below any of `deleting` (the nodes a
/// tree change marks deleted), and returns them with their groups: a tree change takes them
/// after the rows it writes and before any group, and revokes them once the rows are written.
/// Reads only the live links of the sites concerned and the nodes above them.
pub(crate) async fn lock_links_below(
    conn: &mut PgConnection,
    deleting: &[NodeId],
    sites: &[NodeId],
) -> Result<Vec<(Uuid, Option<GroupId>)>, sqlx::Error> {
    if deleting.is_empty() {
        return Ok(Vec::new());
    }
    let deleting: Vec<Uuid> = deleting.iter().map(|id| id.0).collect();
    let sites: Vec<Uuid> = sites.iter().map(|id| id.0).collect();
    let doomed: Vec<Uuid> = sqlx::query_scalar(
        "with recursive candidate(id, node_id) as ( \
             select l.id, l.node_id from links l join nodes n on n.id = l.node_id \
             where l.revoked_at is null \
               and (n.site_id = any($2) \
                    or n.site_id in (select site_id from nodes where id = any($1)))), \
         up(origin, id, parent_id) as ( \
             select n.id, n.id, n.parent_id from nodes n \
             where n.id in (select node_id from candidate) \
             union \
             select up.origin, n.id, n.parent_id from up join nodes n on n.id = up.parent_id) \
         select distinct c.id from candidate c join up on up.origin = c.node_id \
         where up.id = any($1) order by c.id",
    )
    .bind(&deleting)
    .bind(&sites)
    .fetch_all(&mut *conn)
    .await?;
    if doomed.is_empty() {
        return Ok(Vec::new());
    }
    let rows: Vec<(Uuid, Option<Uuid>)> = sqlx::query_as(
        "select id, group_id from links where id = any($1) and revoked_at is null \
         order by id for update",
    )
    .bind(&doomed)
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, group)| (id, group.map(GroupId)))
        .collect())
}

/// Revokes, for `reason`, the links `doomed` (locked by [`lock_links_below`]), then closes every
/// open access request on a node with a deleted node at or above it in `sites`: run by a tree
/// change once its rows are written.
pub(crate) async fn after_deletion(
    conn: &mut PgConnection,
    actor: Option<UserId>,
    doomed: &[(Uuid, Option<GroupId>)],
    sites: &[NodeId],
) -> Result<(), sqlx::Error> {
    for (link, _) in doomed {
        revoke_locked(&mut *conn, actor, *link, "item deleted").await?;
    }
    let sites: Vec<Uuid> = sites.iter().map(|id| id.0).collect();
    let query = format!(
        "with recursive held(node_id) as ( \
             select distinct r.node_id from access_requests r join nodes n on n.id = r.node_id \
             where r.decided_at is null and n.site_id = any($1)), {DEAD} \
         update access_requests set decided_at = now(), decision = 'closed' \
         where decided_at is null and node_id in (select origin from dead)"
    );
    sqlx::query(sqlx::AssertSqlSafe(query))
        .bind(&sites)
        .execute(conn)
        .await?;
    Ok(())
}

/// Whether a node, read through the columns `id` and `unique_scope` of `$t` (a table alias),
/// has permissions of its own below an anyone link's item (the CTE `link(id, root)`): such a
/// node, and everything below it, is out of the link's reach. The one condition for every
/// anonymous route and listing.
macro_rules! own_permissions_below_root {
    ($t:literal) => {
        concat!(
            "(",
            $t,
            ".unique_scope and ",
            $t,
            ".id <> (select root from link))"
        )
    };
}

/// The live anyone link with the token hash `$1`, unexpired at `$2`, while the organisation
/// allows anyone links: the CTE `link(id, root)`, empty otherwise.
const ANYONE_LINK: &str = "\
    link(id, root) as ( \
        select l.id, l.node_id from links l \
        where l.token_hash = $1 and l.kind = 'anyone' and l.revoked_at is null \
          and l.expires_at > $2 \
          and exists (select 1 from settings s \
                      where s.key = 'anyone_links' and s.value = 'true'::jsonb))";

/// Of the nodes listed by `held(node_id)` (a CTE the query defines after [`ANYONE_LINK`]),
/// those an anyone link reaches: the CTE `reachable(id)`. A node is reachable when it is the
/// link's item or below it, no node at or above it is deleted (the [`DEAD`] test), and no node
/// strictly below the item and at or above it has permissions of its own: exactly what a Viewer
/// assignment on the item would reach. Walks up from each held node and stops at the item.
const ANYONE_REACH: &str = concat!(
    "toward_root(origin, id, parent_id, unique_scope) as ( \
        select n.id, n.id, n.parent_id, n.unique_scope from nodes n \
        where n.id in (select node_id from held) \
        union \
        select t.origin, n.id, n.parent_id, n.unique_scope \
        from toward_root t join nodes n on n.id = t.parent_id \
        where t.id <> (select root from link)), \
    reachable(id) as ( \
        select t.origin from toward_root t group by t.origin \
        having bool_or(t.id = (select root from link)) \
           and not bool_or(",
    own_permissions_below_root!("t"),
    ") and t.origin not in (select origin from dead))"
);

/// The columns of a link as [`link_from_row`] reads them, from `links l` with its role `r`, its
/// creator `cu`, who revoked it `ru`, and its member count.
const LINK_COLUMNS: &str = "l.id, l.node_id, l.kind, r.id as role_id, r.name as role_name, \
     cu.id as creator_id, cu.name as creator_name, l.created_at, l.expires_at, l.revoked_at, \
     ru.id as revoker_id, ru.name as revoker_name, l.revoke_reason, \
     (select count(*) from group_members m where m.group_id = l.group_id) as members";

const LINK_FROM: &str = "links l left join roles r on r.id = l.role_id \
     left join users cu on cu.id = l.created_by left join users ru on ru.id = l.revoked_by";

fn link_from_row(row: &PgRow) -> Result<Link, sqlx::Error> {
    let kind: String = row.try_get("kind")?;
    let role = match row.try_get::<Option<String>, _>("role_id")? {
        Some(id) => Some(RoleRef {
            id,
            name: row.try_get("role_name")?,
        }),
        None => None,
    };
    let revoked = match row.try_get::<Option<OffsetDateTime>, _>("revoked_at")? {
        Some(at) => {
            let by = match row.try_get::<Option<Uuid>, _>("revoker_id")? {
                Some(id) => Some(UserRef {
                    id: UserId(id),
                    name: row.try_get("revoker_name")?,
                }),
                None => None,
            };
            let reason: Option<String> = row.try_get("revoke_reason")?;
            Some((by, at, reason.unwrap_or_default()))
        }
        None => None,
    };
    Ok(Link {
        id: row.try_get("id")?,
        node: NodeId(row.try_get("node_id")?),
        kind: LinkKind::parse(&kind)
            .ok_or_else(|| sqlx::Error::Decode(format!("unknown link kind {kind:?}").into()))?,
        role,
        created_by: UserRef {
            id: UserId(
                row.try_get::<Option<Uuid>, _>("creator_id")?
                    .unwrap_or_default(),
            ),
            name: row
                .try_get::<Option<String>, _>("creator_name")?
                .unwrap_or_default(),
        },
        created_at: row.try_get("created_at")?,
        expires_at: row.try_get("expires_at")?,
        revoked,
        members: row.try_get("members")?,
    })
}

async fn link_by_id(conn: &mut PgConnection, id: Uuid) -> Result<Option<Link>, sqlx::Error> {
    let query = format!("select {LINK_COLUMNS} from {LINK_FROM} where l.id = $1");
    let row = sqlx::query(sqlx::AssertSqlSafe(query))
        .bind(id)
        .fetch_optional(conn)
        .await?;
    row.as_ref().map(link_from_row).transpose()
}

/// Whether `role` would make its holders managers: a link's role may not.
fn manages(role: &RoleDef) -> bool {
    role.perms.contains(Perms::MANAGE_ACCESS) || role.perms.contains(Perms::MANAGE_SITE)
}

impl Db {
    /// Makes a link to `node` giving `role` (a role id), expiring at `expires_at` if given, and
    /// answers it with its token, which is never stored or shown again. `actor` must manage
    /// access to the node (`NotFound`, `Forbidden`). `AnyoneLinksOff` for an anyone link while
    /// the organisation does not allow them; `NoSuchRole` for an unknown role;
    /// `LinkRoleTooHigh` for a role with `manage_access` or `manage_site`, or any role but
    /// Viewer for an anyone link; `BeyondYourAccess` for a role with a permission the actor
    /// does not hold there (an administrator holds every one); `ExpiryInPast` for an expiry
    /// that has passed; for an anyone link, `ExpiryRequired` without an expiry and
    /// `ExpiryTooLong` for one further away than `anyone_link_max_days`.
    ///
    /// A signed-in link gets a hidden group assigned the role on the node, as
    /// [`Db::set_node_access`] assigns one, breaking inheritance first if the node inherits.
    /// Records `link.created` (and for a signed-in link the assignment's `access.granted`).
    /// Judged with the node's chain locked, then the groups and the roles involved.
    pub async fn create_link(
        &self,
        actor: UserId,
        node: NodeId,
        kind: LinkKind,
        role: &str,
        expires_at: Option<OffsetDateTime>,
    ) -> Result<(Link, LinkToken), AccessError> {
        // Cheap early refusal so that people with no access never wait for a lock.
        {
            let mut conn = self.pool().acquire().await?;
            manager(chain(&mut conn, actor, node).await?)?;
        }
        let (mut tx, links, _) = self
            .lock_chain(actor, node, false, Some(role), None, false)
            .await?;
        let links = manager(Some(links))?;
        let settings = sharing_in(&mut tx, true).await?;
        if kind == LinkKind::Anyone && !settings.anyone_links {
            return Err(AccessError::AnyoneLinksOff);
        }
        let role = role_by_id(&mut tx, role)
            .await?
            .ok_or(AccessError::NoSuchRole)?;
        if manages(&role) || (kind == LinkKind::Anyone && role.id != "viewer") {
            return Err(AccessError::LinkRoleTooHigh);
        }
        let (Some(target), Some(source)) = (links.last(), links.get(source_index(&links, false)))
        else {
            return Err(AccessError::NotFound);
        };
        if !target
            .role
            .as_ref()
            .is_some_and(|held| held.perms.contains(role.perms))
        {
            return Err(AccessError::BeyondYourAccess);
        }
        let now = OffsetDateTime::now_utc();
        if expires_at.is_some_and(|at| at <= now) {
            return Err(AccessError::ExpiryInPast);
        }
        if kind == LinkKind::Anyone {
            let at = expires_at.ok_or(AccessError::ExpiryRequired)?;
            if at > now + Duration::days(settings.anyone_link_max_days) {
                return Err(AccessError::ExpiryTooLong);
            }
        }
        let token = LinkToken::generate().map_err(DbError::new)?;
        let id = Uuid::new_v4();
        let mut group = None;
        if kind == LinkKind::SignedIn {
            if !target.node.unique_scope {
                break_locked(&mut tx, actor, &target.node, source.node.id).await?;
            }
            let made = GroupId(Uuid::new_v4());
            sqlx::query("insert into groups (id, purpose, created_by) values ($1, 'link', $2)")
                .bind(made.0)
                .bind(actor.0)
                .execute(&mut *tx)
                .await?;
            sqlx::query(
                "insert into assignments \
                     (node_id, principal_kind, principal_id, role_id, granted_by) \
                 values ($1, 'group', $2, $3, $4)",
            )
            .bind(node.0)
            .bind(made.0)
            .bind(&role.id)
            .bind(actor.0)
            .execute(&mut *tx)
            .await?;
            record(
                &mut tx,
                NewEvent {
                    kind: "access.granted",
                    actor: Some(actor),
                    subject: None,
                    document: document_of(&target.node),
                    node: Some(node),
                    detail: with_principal(
                        Principal::Group(made),
                        json!({ "role": role.id, "link": id }),
                    ),
                },
            )
            .await?;
            group = Some(made);
        }
        sqlx::query(
            "insert into links \
                 (id, node_id, kind, role_id, token_hash, group_id, created_by, expires_at) \
             values ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(id)
        .bind(node.0)
        .bind(kind.as_str())
        .bind(&role.id)
        .bind(token.hash())
        .bind(group.map(|g| g.0))
        .bind(actor.0)
        .bind(expires_at)
        .execute(&mut *tx)
        .await?;
        record(
            &mut tx,
            NewEvent {
                kind: "link.created",
                actor: Some(actor),
                subject: None,
                document: document_of(&target.node),
                node: Some(node),
                detail: json!({
                    "link": id,
                    "kind": kind.as_str(),
                    "role": role.id,
                    "expires_at": expires_at.map(|at| at.unix_timestamp()),
                }),
            },
        )
        .await?;
        let link = link_by_id(&mut tx, id)
            .await?
            .ok_or_else(|| DbError::new("the link just made could not be read"))?;
        tx.commit().await?;
        Ok((link, token))
    }

    /// Every link made on `node`, live or revoked, newest first. `actor` must manage access to
    /// the node (`NotFound`, `Forbidden`).
    pub async fn links_on(&self, actor: UserId, node: NodeId) -> Result<Vec<Link>, AccessError> {
        let mut conn = self.pool().acquire().await?;
        manager(chain(&mut conn, actor, node).await?)?;
        let query = format!(
            "select {LINK_COLUMNS} from {LINK_FROM} where l.node_id = $1 \
             order by l.created_at desc, l.id"
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(query))
            .bind(node.0)
            .fetch_all(&mut *conn)
            .await?;
        Ok(rows
            .iter()
            .map(link_from_row)
            .collect::<Result<_, sqlx::Error>>()?)
    }

    /// The link `link`, live or revoked, whoever may see it. For the server, to find a link's
    /// item before judging a revoke.
    pub async fn link(&self, link: Uuid) -> Result<Option<Link>, DbError> {
        let mut conn = self.pool().acquire().await?;
        Ok(link_by_id(&mut conn, link).await?)
    }

    /// Revokes `link` at once: deletes every assignment of its group (on its item, and wherever
    /// a break below copied it) and every membership, and marks it revoked for `reason`
    /// (`manager`, `expired`, ...). With `actor` (`None` for the server, as the expiry sweeper),
    /// they must manage access to the link's item (`NotFound`, `Forbidden`); `NotFound` for an
    /// unknown link. `None` if it was already revoked; otherwise the nodes whose assignments it
    /// removed, so that the server rechecks open sessions there. Records `link.revoked`.
    ///
    /// Locks the item's chain, then the link's row `FOR UPDATE`, then its group `FOR UPDATE`
    /// (with the groups and roles the actor is judged through): two revokes of one link take
    /// turns, and the second finds it revoked. One link per transaction.
    pub async fn revoke_link(
        &self,
        actor: Option<UserId>,
        link: Uuid,
        reason: &str,
    ) -> Result<Option<Vec<NodeId>>, AccessError> {
        let row: Option<(Uuid, Option<Uuid>)> =
            sqlx::query_as("select node_id, group_id from links where id = $1")
                .bind(link)
                .fetch_optional(self.pool())
                .await?;
        let (node, group) = row.ok_or(AccessError::NotFound)?;
        let (node, group) = (NodeId(node), group.map(GroupId));
        if let Some(actor) = actor {
            let mut conn = self.pool().acquire().await?;
            manager(chain(&mut conn, actor, node).await?)?;
        }
        for _ in 0..LOCK_ATTEMPTS {
            let mut tx = self.pool().begin().await?;
            match actor {
                Some(actor) => {
                    let Some(seen) = lock_segment(&mut tx, actor, node, false, false).await? else {
                        tx.rollback().await?;
                        continue;
                    };
                    manager(Some(seen.clone()))?;
                    sqlx::query("select id from links where id = $1 for update")
                        .bind(link)
                        .execute(&mut *tx)
                        .await?;
                    let scopes = scopes_of(&segment(&seen, false));
                    let judged = groups_judged_by(&mut tx, actor, &scopes).await?;
                    lock_groups(&mut tx, judged.iter().copied(), group).await?;
                    lock_roles(&mut tx, judged_roles(&seen).collect::<Vec<_>>()).await?;
                    let now = chain(&mut tx, actor, node)
                        .await?
                        .ok_or(AccessError::NotFound)?;
                    let now_judged = groups_judged_by(&mut tx, actor, &scopes).await?;
                    if segment(&now, false) != segment(&seen, false)
                        || !all_locked(&now_judged, &judged)
                    {
                        tx.rollback().await?;
                        continue;
                    }
                    manager(Some(now))?;
                }
                None => {
                    lock_top_down(&mut tx, &[node], false).await?;
                    sqlx::query("select id from links where id = $1 for update")
                        .bind(link)
                        .execute(&mut *tx)
                        .await?;
                    lock_groups(&mut tx, [], group).await?;
                }
            }
            let revoked = revoke_locked(&mut tx, actor, link, reason).await?;
            tx.commit().await?;
            return Ok(revoked);
        }
        Err(DbError::new("the tree kept changing while a link was being revoked").into())
    }

    /// Opens the signed-in link with `token` for `user`: makes them a member of its group,
    /// unless their access to its item already covers the link's role, and answers the item.
    /// Records `link.used` when they join. `NotFound` for an unknown, revoked or expired token,
    /// an anyone link's token, a disabled account, or an item with a deleted node at or above
    /// it.
    ///
    /// Locks the item's chain, then the link's row `FOR SHARE` (and checks again that it is live
    /// and unexpired), then its group `FOR UPDATE` with the groups the person is judged through,
    /// then the roles involved.
    pub async fn redeem_signed_in(
        &self,
        user: UserId,
        token: &LinkToken,
    ) -> Result<NodeId, LinkError> {
        let row: Option<(Uuid, Uuid, Option<Uuid>, Option<String>)> = sqlx::query_as(
            "select id, node_id, group_id, role_id from links \
             where token_hash = $1 and kind = 'signed_in' and revoked_at is null \
               and (expires_at is null or expires_at > now())",
        )
        .bind(token.hash())
        .fetch_optional(self.pool())
        .await?;
        let Some((link, node, Some(group), Some(role))) = row else {
            return Err(LinkError::NotFound);
        };
        let (node, group) = (NodeId(node), GroupId(group));
        for _ in 0..LOCK_ATTEMPTS {
            let mut tx = self.pool().begin().await?;
            let seen = match lock_segment(&mut tx, user, node, false, false).await {
                Ok(Some(seen)) => seen,
                Ok(None) => {
                    tx.rollback().await?;
                    continue;
                }
                Err(AccessError::Db(e)) => return Err(LinkError::Db(e)),
                Err(_) => return Err(LinkError::NotFound),
            };
            let live: Option<bool> = sqlx::query_scalar(
                "select revoked_at is null and (expires_at is null or expires_at > now()) \
                 from links where id = $1 for share",
            )
            .bind(link)
            .fetch_optional(&mut *tx)
            .await?;
            if live != Some(true) {
                return Err(LinkError::NotFound);
            }
            let scopes = scopes_of(&segment(&seen, false));
            let judged = groups_judged_by(&mut tx, user, &scopes).await?;
            lock_groups(&mut tx, judged.iter().copied(), Some(group)).await?;
            let roles: Vec<String> = judged_roles(&seen)
                .chain(std::iter::once(role.clone()))
                .collect();
            lock_roles(&mut tx, roles).await?;
            let Some(now) = chain(&mut tx, user, node).await? else {
                return Err(LinkError::NotFound);
            };
            let now_judged = groups_judged_by(&mut tx, user, &scopes).await?;
            if segment(&now, false) != segment(&seen, false) || !all_locked(&now_judged, &judged) {
                tx.rollback().await?;
                continue;
            }
            let Some(role) = role_by_id(&mut tx, &role).await? else {
                return Err(LinkError::NotFound);
            };
            let covered = now
                .last()
                .and_then(|link| link.role.as_ref())
                .is_some_and(|held| held.perms.contains(role.perms));
            if !covered {
                let added = sqlx::query(
                    "insert into group_members (group_id, user_id) values ($1, $2) \
                     on conflict (group_id, user_id) do nothing",
                )
                .bind(group.0)
                .bind(user.0)
                .execute(&mut *tx)
                .await?
                .rows_affected();
                if added > 0 {
                    let item = now.last().map(|link| link.node.clone());
                    record(
                        &mut tx,
                        NewEvent {
                            kind: "link.used",
                            actor: Some(user),
                            subject: Some(user),
                            document: item.as_ref().and_then(document_of),
                            node: Some(node),
                            detail: json!({ "link": link, "group": group.0 }),
                        },
                    )
                    .await?;
                }
            }
            tx.commit().await?;
            return Ok(node);
        }
        Err(DbError::new("the tree kept changing while a link was being opened").into())
    }

    /// What the link with `token` is, while it works: its kind and role. `None` for an unknown,
    /// revoked or expired token, an anyone link while the organisation does not allow them, or
    /// a link whose item has a deleted node at or above it. For the link's landing page; says
    /// nothing about the item.
    pub async fn link_kind(
        &self,
        token: &LinkToken,
    ) -> Result<Option<(LinkKind, RoleRef)>, DbError> {
        let row: Option<(Uuid, String, String, String)> = sqlx::query_as(
            "select l.node_id, l.kind, r.id, r.name from links l join roles r on r.id = l.role_id \
             where l.token_hash = $1 and l.revoked_at is null \
               and (l.expires_at is null or l.expires_at > now()) \
               and (l.kind = 'signed_in' or exists (select 1 from settings s \
                    where s.key = 'anyone_links' and s.value = 'true'::jsonb))",
        )
        .bind(token.hash())
        .fetch_optional(self.pool())
        .await?;
        let Some((node, kind, id, name)) = row else {
            return Ok(None);
        };
        let query = format!(
            "with recursive held(node_id) as (select $1::uuid), {DEAD} \
             select exists (select 1 from dead)"
        );
        let dead: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(query))
            .bind(node)
            .fetch_one(self.pool())
            .await?;
        if dead {
            return Ok(None);
        }
        Ok(LinkKind::parse(&kind).map(|kind| (kind, RoleRef { id, name })))
    }

    /// What the anyone link with `token` lets its holder see of `node` (the link's item when
    /// `None`), at this instant: `Some` only while the organisation allows anyone links, the
    /// link is live and unexpired, and `node` is the item or below it, with no deleted node at
    /// or above it and no node strictly below the item and at or above it with permissions of
    /// its own. The one check every anonymous route makes.
    pub async fn anyone_link(
        &self,
        token: &LinkToken,
        node: Option<NodeId>,
    ) -> Result<Option<AnyoneGrant>, DbError> {
        let mut conn = self.pool().acquire().await?;
        Ok(anyone_grant(&mut conn, token, node, OffsetDateTime::now_utc()).await?)
    }

    /// One page of the children of `parent` that the anyone link with `token` reaches (see
    /// [`Db::anyone_link`]), in the order of [`Db::children`]: after `after`, at most `limit`
    /// (at least one), and where the next page starts if there is one. `None` unless the link
    /// reaches `parent`. A child with permissions of its own is left out, with everything below
    /// it. Each child is shown with Viewer access.
    pub async fn anyone_children(
        &self,
        token: &LinkToken,
        parent: NodeId,
        after: Option<&Cursor>,
        limit: u32,
    ) -> Result<Option<(Vec<Listed>, Option<Cursor>)>, DbError> {
        let mut conn = self.pool().acquire().await?;
        let now = OffsetDateTime::now_utc();
        if anyone_grant(&mut conn, token, Some(parent), now)
            .await?
            .is_none()
        {
            return Ok(None);
        }
        let limit = limit.max(1);
        let from_cursor = if after.is_some() {
            format!("and ({CHILD_ORDER}) > ($5, lower($6), $7)")
        } else {
            String::new()
        };
        // The parent is proved reachable again in the same statement; each child is then
        // reachable exactly when it is live and has no permissions of its own.
        let query = format!(
            "with recursive {ANYONE_LINK}, held(node_id) as (select $3::uuid), {DEAD}, \
             {ANYONE_REACH} \
             select {NODE_COLUMNS}, d.editor, d.revised_at, \
                    (case when n.kind = 'document' then 1 else 0 end)::int2 as kind_rank \
             from nodes n left join documents d on d.id = n.id and n.kind = 'document' \
             where n.parent_id = $3 and n.deleted_at is null {from_cursor} \
               and not {} \
               and exists (select 1 from reachable where id = $3) \
             order by {CHILD_ORDER} \
             limit $4",
            own_permissions_below_root!("n")
        );
        let mut statement = sqlx::query(sqlx::AssertSqlSafe(query))
            .bind(token.hash())
            .bind(now)
            .bind(parent.0)
            .bind(i64::from(limit) + 1);
        if let Some(cursor) = after {
            statement = statement
                .bind(i32::from(cursor.kind_rank))
                .bind(cursor.name.clone())
                .bind(cursor.id.0);
        }
        let rows = statement.fetch_all(&mut *conn).await?;
        let viewer = RoleDef::builtin("viewer").map(|role| Access::from_role(&role));
        let mut listed = Vec::with_capacity(rows.len());
        let mut ranks = Vec::with_capacity(rows.len());
        for row in &rows {
            let editor = row
                .try_get::<Option<String>, _>("editor")?
                .as_deref()
                .map(parse_editor)
                .transpose()?;
            ranks.push(row.try_get::<i16, _>("kind_rank")?);
            listed.push(Listed {
                node: node_from_row(row)?,
                role: viewer.clone(),
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

    /// The live links whose expiry is at or before `now`, oldest expiry first, read without
    /// locks: the expiry sweeper revokes each with [`Db::revoke_link`] (no actor, reason
    /// `expired`), one link per transaction.
    pub async fn expired_links(&self, now: OffsetDateTime) -> Result<Vec<Uuid>, DbError> {
        Ok(sqlx::query_scalar(
            "select id from links where revoked_at is null and expires_at <= $1 \
             order by expires_at, id",
        )
        .bind(now)
        .fetch_all(self.pool())
        .await?)
    }
}

/// See [`Db::anyone_link`]; at the instant `now`.
async fn anyone_grant(
    conn: &mut PgConnection,
    token: &LinkToken,
    node: Option<NodeId>,
    now: OffsetDateTime,
) -> Result<Option<AnyoneGrant>, sqlx::Error> {
    let query = format!(
        "with recursive {ANYONE_LINK}, \
         held(node_id) as (select coalesce($3::uuid, (select root from link)) \
                           where exists (select 1 from link)), \
         {DEAD}, {ANYONE_REACH} \
         select (select id from link) as link_id, (select root from link) as root_id, \
                {NODE_COLUMNS} \
         from reachable join nodes n on n.id = reachable.id"
    );
    let row = sqlx::query(sqlx::AssertSqlSafe(query))
        .bind(token.hash())
        .bind(now)
        .bind(node.map(|n| n.0))
        .fetch_optional(conn)
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    Ok(Some(AnyoneGrant {
        link: row.try_get("link_id")?,
        root: NodeId(row.try_get("root_id")?),
        node: node_from_row(&row)?,
    }))
}
