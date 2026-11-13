//! The inheritance explorer: every place below a node where inheritance is broken (every scope),
//! with its path, who is assigned there and why, who broke it and who changed it last, and what
//! restoring it would change.
//!
//! The explorer reads only scopes and their ancestors, never the files between them: it starts
//! from the live scopes (an index holds them) and walks up from each, so its cost follows the
//! number of breaks, not the size of a library. It shows only scopes the viewer has
//! `manage_access` on, and counts only what it shows. Every access it reports is worked out by
//! the one rule ([`access_step`]) from the one fragment saying which assignments apply to a
//! person (`mine!`), as the chain works it out.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use collab_core::ids::{GroupId, NodeId, UserId};
use sqlx::{PgConnection, Row};
use time::{OffsetDateTime, UtcOffset};
use uuid::Uuid;

use crate::groups::{manages, visible_groups};
use crate::scopes::{
    ASSIGNED, Link, MINE, MINE_ON_N, Standing, access_step, applies_to, chain, crumb, manager,
    source_index, standing,
};
use crate::{
    Access, AccessError, Crumb, Db, DbError, LinkKind, NodeKind, Perms, Principal, RoleRef,
};

/// A person named for display: their id and name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserRef {
    pub id: UserId,
    pub name: String,
}

/// Who an assignment names, as the explorer shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PrincipalView {
    /// A person.
    User(UserRef),
    /// A group the viewer may see, by its name as shown. A site group's name is built from its
    /// site's name and can equal an organisation group's, so a site group also carries its site
    /// (id and current name); an organisation group has `None`.
    Group {
        id: GroupId,
        name: String,
        site: Option<(NodeId, String)>,
    },
    /// A group the viewer may not see (a group of a site they cannot reach): its id only, so
    /// that no site name they could not otherwise see is shown.
    HiddenGroup(GroupId),
    /// A signed-in link's hidden group: the link, never a group with a name. `item` is the
    /// link's own item (id and current name), where it is listed and revoked; a break below it
    /// may hold a copy of its assignment.
    Link {
        link: Uuid,
        kind: LinkKind,
        expires_at: Option<OffsetDateTime>,
        item: (NodeId, String),
    },
}

impl PrincipalView {
    /// The principal shown; `None` for a link, whose group is never offered as a principal.
    #[must_use]
    pub fn principal(&self) -> Option<Principal> {
        match self {
            PrincipalView::User(user) => Some(Principal::User(user.id)),
            PrincipalView::Group { id, .. } | PrincipalView::HiddenGroup(id) => {
                Some(Principal::Group(*id))
            }
            PrincipalView::Link { .. } => None,
        }
    }
}

/// One assignment on a scope: whom it names, and the role it gives with that role's permissions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssignmentView {
    pub principal: PrincipalView,
    pub role: RoleRef,
    pub perms: Perms,
}

/// A place where inheritance is broken: a scope, as the explorer shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Break {
    /// The scope, with the viewer's access to it.
    pub node: Crumb,
    /// The nodes from the explorer's root down to the scope's parent, with the viewer's access
    /// to each (`None` where they have only limited access); empty for the root itself.
    pub path: Vec<Crumb>,
    pub kind: NodeKind,
    /// The assignments on the scope, by the principal's name.
    pub assignments: Vec<AssignmentView>,
    /// The scope the node would inherit from if inheritance were restored: the nearest scope
    /// above it. `None` for a site, and for a scope above the explorer's root that the viewer
    /// does not manage access on.
    pub inherited_from: Option<Crumb>,
    /// Who broke inheritance here and when: the latest `inheritance.broken` event on the node.
    pub broken_by: Option<(UserRef, OffsetDateTime)>,
    /// Who changed access here last and when: the latest access event (`access.*` or
    /// `inheritance.*`) on the node.
    pub changed_by: Option<(UserRef, OffsetDateTime)>,
    /// How many of the breaks the explorer shows (with the same filter) are below this one.
    pub breaks_below: i64,
    /// How many live, unexpired anyone links are on the scope itself. Anyone links on a node
    /// that inherits are listed on its access page, not here.
    pub anyone_links: i64,
}

/// What the explorer lists, below the node it starts from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExplorerFilter {
    /// Only breaks with an assignment to this principal; for a person, to them directly, to a
    /// group they are a member of whose members the viewer may see, or to a signed-in link they
    /// joined (shown as the link, never as its group). A link's group is never a choice.
    pub principal: Option<Principal>,
    /// Only breaks of these kinds; every kind when empty.
    pub kinds: Vec<NodeKind>,
}

/// What restoring inheritance on a node would change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestorePreview {
    /// The node, with the viewer's access to it.
    pub node: Crumb,
    /// The scope it would inherit from: the nearest scope above it.
    pub inherit_from: Crumb,
    /// Every person whose permissions on the node would change, by name, with their
    /// permissions now and after restoring. Everything below the node that inherits follows the
    /// node. Worked out from their direct assignments and the groups whose members the viewer
    /// may see; what other groups give is in `groups` instead, so that their members are never
    /// named.
    pub people: Vec<(UserRef, Perms, Perms)>,
    /// Every group whose members the viewer may not see whose role on the node would change,
    /// by name, with the permissions its role gives now and after restoring (`NONE` for no
    /// role).
    pub groups: Vec<(PrincipalView, Perms, Perms)>,
    /// How many breaks below the node (that the viewer manages access on) keep their own
    /// assignments, unaffected.
    pub unchanged_breaks_below: i64,
}

/// One step of the explorer's order: folders (and libraries) before files, then by name ignoring
/// case, then by id. A scope's key is its steps from the root down; sorting by key puts every
/// scope after its ancestors, in the order of the tree.
type Step = (i16, String, String);

/// Who did something and when.
type Stamp = Option<(UserRef, OffsetDateTime)>;

/// A node met walking up from a scope.
struct Walked {
    parent: Option<NodeId>,
    kind: NodeKind,
    name: String,
    scope: bool,
    deleted: bool,
}

/// A live scope at or below the explorer's root, with the viewer's access along its path.
struct Found {
    node: Crumb,
    /// From the root down to the node's parent.
    path: Vec<Crumb>,
    /// The ids from the root down to the node itself.
    line: Vec<NodeId>,
    key: Vec<Step>,
    /// Where in `path` the nearest scope above the node is, if it is at or below the root.
    source: Option<usize>,
}

impl Found {
    fn managed(&self) -> bool {
        self.node
            .role
            .as_ref()
            .is_some_and(Access::can_manage_access)
    }
}

fn step(kind: NodeKind, name: &str, id: NodeId) -> Step {
    (
        i16::from(kind == NodeKind::Document),
        name.to_lowercase(),
        id.0.hyphenated().to_string(),
    )
}

/// Every live scope in `root`'s site with every node above it, up to `root` (walking stops
/// there) or the site. Reads the site's live scopes through their index (`nodes_site_scopes`,
/// so no other site's scopes are read), then one row per ancestor: nothing that is not a scope
/// or above one.
const WALK: &str = "\
    with recursive up(origin, id, parent_id, kind, name, unique_scope, deleted) as ( \
        select n.id, n.id, n.parent_id, n.kind, n.name, n.unique_scope, false \
        from nodes n where n.unique_scope and n.deleted_at is null and n.site_id = $1 \
        union \
        select up.origin, n.id, n.parent_id, n.kind, n.name, n.unique_scope, \
               n.deleted_at is not null \
        from up join nodes n on n.id = up.parent_id where up.id <> $2) \
    select distinct id, parent_id, kind, name, unique_scope, deleted, (id = origin) as is_origin \
    from up";

/// Every live scope at or below the last node of `root_links` (the chain from the site down to
/// the explorer's root, as `viewer` sees it), with no deleted node between it and the root,
/// each with `viewer`'s access along its path. Unordered.
async fn scopes_under(
    conn: &mut PgConnection,
    viewer: UserId,
    standing: Standing,
    root_links: &[Link],
) -> Result<Vec<Found>, sqlx::Error> {
    let (Some(site), Some(root)) = (root_links.first(), root_links.last()) else {
        return Ok(Vec::new());
    };
    let rows = sqlx::query(WALK)
        .bind(site.node.id.0)
        .bind(root.node.id.0)
        .fetch_all(&mut *conn)
        .await?;
    let mut walked: HashMap<NodeId, Walked> = HashMap::with_capacity(rows.len());
    let mut origins = Vec::new();
    for row in &rows {
        let id = NodeId(row.try_get("id")?);
        let kind: String = row.try_get("kind")?;
        let kind = NodeKind::parse(&kind)
            .ok_or_else(|| sqlx::Error::Decode(format!("unknown node kind {kind:?}").into()))?;
        if row.try_get("is_origin")? {
            origins.push(id);
        }
        walked.insert(
            id,
            Walked {
                parent: row.try_get::<Option<Uuid>, _>("parent_id")?.map(NodeId),
                kind,
                name: row.try_get("name")?,
                scope: row.try_get("unique_scope")?,
                deleted: row.try_get("deleted")?,
            },
        );
    }
    origins.sort_unstable();
    origins.dedup();
    // Each origin's line from the root down, if it is below the root with nothing deleted on
    // the way. Taking at most as many steps as there are nodes ends a walk the data made a cycle.
    let mut lines: Vec<Vec<NodeId>> = Vec::with_capacity(origins.len());
    'origins: for origin in origins {
        let mut line = vec![origin];
        let mut at = origin;
        while at != root.node.id {
            let Some(node) = walked.get(&at) else {
                continue 'origins;
            };
            match node.parent {
                Some(parent) if !node.deleted && line.len() <= walked.len() => {
                    line.push(parent);
                    at = parent;
                }
                _ => continue 'origins,
            }
        }
        line.reverse();
        lines.push(line);
    }
    // The viewer's own access on each of these scopes, from "my assignments".
    let mut own: HashMap<NodeId, Option<Access>> = HashMap::new();
    if standing == Standing::Member && !lines.is_empty() {
        let ids: Vec<Uuid> = lines
            .iter()
            .filter_map(|line| line.last())
            .map(|id| id.0)
            .collect();
        let query = format!(
            "with {MINE} select n.id, g.perms, g.role_ids, g.role_names \
             from nodes n {MINE_ON_N} where n.id = any($2)"
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(query))
            .bind(viewer.0)
            .bind(&ids)
            .fetch_all(&mut *conn)
            .await?;
        for row in &rows {
            own.insert(NodeId(row.try_get("id")?), Access::from_aggregate(row)?);
        }
    }
    let root_crumb = crumb(root);
    let mut found = Vec::with_capacity(lines.len());
    for line in lines {
        let mut crumbs = vec![root_crumb.clone()];
        let mut key = vec![step(root.node.kind, &root.node.name, root.node.id)];
        let mut access = root.role.clone();
        let mut scopes = vec![root.node.unique_scope];
        for id in line.iter().skip(1) {
            let Some(node) = walked.get(id) else {
                continue;
            };
            scopes.push(node.scope);
            let mine = own.get(id).cloned().flatten();
            access = access_step(standing, node.scope, mine, access);
            crumbs.push(Crumb {
                id: *id,
                kind: node.kind,
                name: node.name.clone(),
                role: access.clone(),
            });
            key.push(step(node.kind, &node.name, *id));
        }
        let Some(node) = crumbs.pop() else {
            continue;
        };
        scopes.pop();
        found.push(Found {
            node,
            path: crumbs,
            line,
            key,
            source: scopes.iter().rposition(|scope| *scope),
        });
    }
    Ok(found)
}

/// The assignments on each of `scopes`, as `viewer` may see them, by the principal's name.
pub(crate) async fn assignments_on(
    conn: &mut PgConnection,
    viewer: UserId,
    scopes: &[NodeId],
) -> Result<HashMap<NodeId, Vec<AssignmentView>>, sqlx::Error> {
    let ids: Vec<Uuid> = scopes.iter().map(|id| id.0).collect();
    let query = format!(
        "with {ASSIGNED} \
         select a.node_id, a.principal_kind, a.principal_id, a.role_id, a.role_name, a.perms, \
                u.name as user_name \
         from assigned a \
         left join users u on a.principal_kind = 'user' and u.id = a.principal_id \
         where a.node_id = any($1)"
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(query))
        .bind(&ids)
        .fetch_all(&mut *conn)
        .await?;
    let mut named_groups = Vec::new();
    for row in &rows {
        if row.try_get::<String, _>("principal_kind")? == "group" {
            named_groups.push(GroupId(row.try_get("principal_id")?));
        }
    }
    let groups = group_views(&mut *conn, viewer, named_groups).await?;
    let mut by_scope: HashMap<NodeId, Vec<(String, AssignmentView)>> = HashMap::new();
    for row in &rows {
        let id: Uuid = row.try_get("principal_id")?;
        let kind: String = row.try_get("principal_kind")?;
        let principal = if kind == "user" {
            PrincipalView::User(UserRef {
                id: UserId(id),
                name: row
                    .try_get::<Option<String>, _>("user_name")?
                    .unwrap_or_default(),
            })
        } else {
            groups
                .get(&GroupId(id))
                .cloned()
                .unwrap_or(PrincipalView::HiddenGroup(GroupId(id)))
        };
        let order = match &principal {
            PrincipalView::User(user) => user.name.to_lowercase(),
            PrincipalView::Group { name, .. } => name.to_lowercase(),
            PrincipalView::HiddenGroup(_) | PrincipalView::Link { .. } => String::new(),
        };
        by_scope
            .entry(NodeId(row.try_get("node_id")?))
            .or_default()
            .push((
                format!("{order}\u{0}{id}"),
                AssignmentView {
                    principal,
                    role: RoleRef {
                        id: row.try_get("role_id")?,
                        name: row.try_get("role_name")?,
                    },
                    perms: Perms::from_column(row.try_get("perms")?)?,
                },
            ));
    }
    Ok(by_scope
        .into_iter()
        .map(|(scope, mut list)| {
            list.sort_by(|a, b| a.0.cmp(&b.0));
            (scope, list.into_iter().map(|(_, view)| view).collect())
        })
        .collect())
}

/// How `viewer` is shown each of `ids`: a link's group as its link, by name (a site group with
/// its site) when they may see the group, [`PrincipalView::HiddenGroup`] otherwise.
async fn group_views(
    conn: &mut PgConnection,
    viewer: UserId,
    mut ids: Vec<GroupId>,
) -> Result<HashMap<GroupId, PrincipalView>, sqlx::Error> {
    ids.sort_unstable();
    ids.dedup();
    let mut views: HashMap<GroupId, PrincipalView> = ids
        .iter()
        .map(|id| (*id, PrincipalView::HiddenGroup(*id)))
        .collect();
    if ids.is_empty() {
        return Ok(views);
    }
    let raw: Vec<Uuid> = ids.iter().map(|g| g.0).collect();
    let links: Vec<(Uuid, Uuid, String, Option<OffsetDateTime>, Uuid, String)> = sqlx::query_as(
        "select l.group_id, l.id, l.kind, l.expires_at, l.node_id, n.name \
         from links l join nodes n on n.id = l.node_id where l.group_id = any($1)",
    )
    .bind(&raw)
    .fetch_all(&mut *conn)
    .await?;
    for (group, link, kind, expires_at, item, item_name) in links {
        let kind = LinkKind::parse(&kind)
            .ok_or_else(|| sqlx::Error::Decode(format!("unknown link kind {kind:?}").into()))?;
        views.insert(
            GroupId(group),
            PrincipalView::Link {
                link,
                kind,
                expires_at,
                item: (NodeId(item), item_name),
            },
        );
    }
    let groups = visible_groups(&mut *conn, viewer, Some(&ids)).await?;
    let site_ids: Vec<Uuid> = groups.iter().filter_map(|g| g.site).map(|s| s.0).collect();
    let named: Vec<(Uuid, String)> =
        sqlx::query_as("select id, name from nodes where id = any($1)")
            .bind(&site_ids)
            .fetch_all(&mut *conn)
            .await?;
    let sites: HashMap<NodeId, String> = named
        .into_iter()
        .map(|(id, name)| (NodeId(id), name))
        .collect();
    for group in groups {
        views.insert(
            group.id,
            PrincipalView::Group {
                id: group.id,
                name: group.name,
                site: group
                    .site
                    .and_then(|s| sites.get(&s).map(|name| (s, name.clone()))),
            },
        );
    }
    Ok(views)
}

/// The groups `user` is a member of whose members `viewer` may see (see [`members_visible`]).
/// A link's group is never among them: it is never offered in a filter (see
/// [`links_joined_by`]).
async fn groups_seen_through(
    conn: &mut PgConnection,
    viewer: UserId,
    user: UserId,
) -> Result<HashSet<GroupId>, sqlx::Error> {
    let member_of: Vec<(Uuid, Option<Uuid>)> = sqlx::query_as(
        "select g.id, g.site_id from group_members m join groups g on g.id = m.group_id \
         where m.user_id = $1 and g.purpose is distinct from 'link' order by g.id",
    )
    .bind(user.0)
    .fetch_all(&mut *conn)
    .await?;
    members_visible(conn, viewer, member_of).await
}

/// The signed-in links `user` joined (is a member of the hidden group of), by link id: the
/// person filter matches a break where one of them is assigned, shown as the link. The viewer
/// manages access on every break the explorer shows, and so may see whom a link there reaches.
async fn links_joined_by(
    conn: &mut PgConnection,
    user: UserId,
) -> Result<HashSet<Uuid>, sqlx::Error> {
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "select l.id from links l join group_members m on m.group_id = l.group_id \
         where m.user_id = $1",
    )
    .bind(user.0)
    .fetch_all(conn)
    .await?;
    Ok(ids.into_iter().collect())
}

/// Those of `groups` (each with its site, `None` for an organisation group) whose members
/// `viewer` may see: every group for an administrator; otherwise the groups of the sites where
/// `viewer` manages the site's groups (the rule of [`Db::group`]).
async fn members_visible(
    conn: &mut PgConnection,
    viewer: UserId,
    groups: Vec<(Uuid, Option<Uuid>)>,
) -> Result<HashSet<GroupId>, sqlx::Error> {
    let member_of = groups;
    let mut decided: HashMap<Option<Uuid>, bool> = HashMap::new();
    let mut seen = HashSet::new();
    for (group, site) in member_of {
        let may = match decided.get(&site) {
            Some(may) => *may,
            None => {
                let may = manages(&mut *conn, viewer, site.map(NodeId)).await?;
                decided.insert(site, may);
                may
            }
        };
        if may {
            seen.insert(GroupId(group));
        }
    }
    Ok(seen)
}

/// The latest `inheritance.broken` event and the latest access event on each of `nodes`, with
/// the person who made each.
async fn events_on(
    conn: &mut PgConnection,
    nodes: &[NodeId],
) -> Result<HashMap<NodeId, (Stamp, Stamp)>, sqlx::Error> {
    let ids: Vec<Uuid> = nodes.iter().map(|id| id.0).collect();
    let rows = sqlx::query(
        "select n.id, bu.id as b_id, bu.name as b_name, b.at as b_at, \
                cu.id as c_id, cu.name as c_name, c.at as c_at \
         from unnest($1::uuid[]) as n(id) \
         left join lateral (select e.actor, e.at from events e \
                            where e.node_id = n.id and e.kind = 'inheritance.broken' \
                            order by e.id desc limit 1) b on true \
         left join users bu on bu.id = b.actor \
         left join lateral (select e.actor, e.at from events e \
                            where e.node_id = n.id \
                              and (e.kind like 'access.%' or e.kind like 'inheritance.%') \
                            order by e.id desc limit 1) c on true \
         left join users cu on cu.id = c.actor",
    )
    .bind(&ids)
    .fetch_all(&mut *conn)
    .await?;
    let by = |row: &sqlx::postgres::PgRow, prefix: &str| -> Result<Stamp, sqlx::Error> {
        let id: Option<Uuid> = row.try_get(format!("{prefix}_id").as_str())?;
        let name: Option<String> = row.try_get(format!("{prefix}_name").as_str())?;
        let at: Option<OffsetDateTime> = row.try_get(format!("{prefix}_at").as_str())?;
        Ok(match (id, name, at) {
            (Some(id), Some(name), Some(at)) => Some((
                UserRef {
                    id: UserId(id),
                    name,
                },
                at,
            )),
            _ => None,
        })
    };
    let mut events = HashMap::with_capacity(rows.len());
    for row in &rows {
        events.insert(NodeId(row.try_get("id")?), (by(row, "b")?, by(row, "c")?));
    }
    Ok(events)
}

/// How many live, unexpired anyone links are on each of `nodes`, by node; nodes with none are
/// left out.
async fn anyone_links_on(
    conn: &mut PgConnection,
    nodes: &[NodeId],
) -> Result<HashMap<NodeId, i64>, sqlx::Error> {
    let ids: Vec<Uuid> = nodes.iter().map(|id| id.0).collect();
    let rows: Vec<(Uuid, i64)> = sqlx::query_as(
        "select node_id, count(*) from links \
         where node_id = any($1) and kind = 'anyone' and revoked_at is null \
           and expires_at > now() \
         group by node_id",
    )
    .bind(&ids)
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(node, count)| (NodeId(node), count))
        .collect())
}

/// The cursor for the page after the scope with `key`.
fn cursor_of(key: &[Step]) -> String {
    serde_json::to_string(key).unwrap_or_default()
}

/// Whether `text` is a cursor of the inheritance explorer, as [`Db::breaks`] hands one out. A
/// cursor that is not one is refused by [`Db::breaks`] with an error, which callers taking the
/// cursor from a request can tell apart from a database failure by asking this first.
#[must_use]
pub fn is_explorer_cursor(text: &str) -> bool {
    serde_json::from_str::<Vec<Step>>(text).is_ok()
}

/// See [`Db::breaks`]; on `conn`.
pub(crate) async fn breaks_in(
    conn: &mut PgConnection,
    viewer: UserId,
    root: NodeId,
    filter: &ExplorerFilter,
    after: Option<&str>,
    limit: u32,
) -> Result<Option<(Vec<Break>, Option<String>)>, DbError> {
    let page = usize::try_from(limit.max(1)).unwrap_or(usize::MAX);
    breaks_window(conn, viewer, root, filter, after, Some(page)).await
}

/// The breaks of [`Db::breaks`] after `after`: at most `limit` of them and the cursor of the
/// next page, or every one (and no cursor) when `limit` is `None`. Walks the scopes once; reads
/// the assignments of every scope only to filter by principal, otherwise of those returned.
async fn breaks_window(
    conn: &mut PgConnection,
    viewer: UserId,
    root: NodeId,
    filter: &ExplorerFilter,
    after: Option<&str>,
    limit: Option<usize>,
) -> Result<Option<(Vec<Break>, Option<String>)>, DbError> {
    let after: Option<Vec<Step>> = after
        .map(serde_json::from_str)
        .transpose()
        .map_err(|_| DbError::new("not a cursor of the inheritance explorer"))?;
    let standing = standing(&mut *conn, viewer).await?;
    let Some(links) = chain(&mut *conn, viewer, root).await? else {
        return Ok(None);
    };
    let manages_root = links
        .last()
        .and_then(|link| link.role.as_ref())
        .is_some_and(Access::can_manage_access);
    if !manages_root {
        return Ok(None);
    }
    let mut shown: Vec<Found> = scopes_under(&mut *conn, viewer, standing, &links)
        .await?
        .into_iter()
        .filter(|found| found.managed())
        .filter(|found| filter.kinds.is_empty() || filter.kinds.contains(&found.node.kind))
        .collect();
    let mut assignments = HashMap::new();
    if let Some(wanted) = filter.principal {
        let ids: Vec<NodeId> = shown.iter().map(|found| found.node.id).collect();
        assignments = assignments_on(&mut *conn, viewer, &ids).await?;
        let (through, joined) = match wanted {
            Principal::User(user) => (
                groups_seen_through(&mut *conn, viewer, user).await?,
                links_joined_by(&mut *conn, user).await?,
            ),
            Principal::Group(_) => (HashSet::new(), HashSet::new()),
        };
        shown.retain(|found| {
            assignments.get(&found.node.id).is_some_and(|list| {
                list.iter()
                    .any(|a| match (&a.principal, a.principal.principal()) {
                        // A link the person joined: matched, and shown as the link.
                        (PrincipalView::Link { link, .. }, _) => joined.contains(link),
                        (_, Some(named)) if named == wanted => true,
                        (_, Some(Principal::Group(group))) => through.contains(&group),
                        (_, Some(Principal::User(_)) | None) => false,
                    })
            })
        });
    }
    shown.sort_by(|a, b| a.key.cmp(&b.key));
    // In the tree's order, the breaks below a break come right after it.
    let below: Vec<i64> = (0..shown.len())
        .map(|i| {
            let Some(this) = shown.get(i) else {
                return 0;
            };
            let count = shown
                .get(i + 1..)
                .unwrap_or_default()
                .iter()
                .take_while(|other| other.line.starts_with(&this.line))
                .count();
            i64::try_from(count).unwrap_or(i64::MAX)
        })
        .collect();
    let start = after.as_ref().map_or(0, |after| {
        shown.partition_point(|found| {
            found.key.as_slice().cmp(after.as_slice()) != Ordering::Greater
        })
    });
    let end = limit.map_or(shown.len(), |page| {
        start.saturating_add(page).min(shown.len())
    });
    let next = (end < shown.len())
        .then(|| {
            shown
                .get(end.saturating_sub(1))
                .map(|found| cursor_of(&found.key))
        })
        .flatten();
    let page_ids: Vec<NodeId> = shown
        .get(start..end)
        .unwrap_or_default()
        .iter()
        .map(|found| found.node.id)
        .collect();
    if filter.principal.is_none() {
        assignments = assignments_on(&mut *conn, viewer, &page_ids).await?;
    }
    let mut events = events_on(&mut *conn, &page_ids).await?;
    let mut anyone = anyone_links_on(&mut *conn, &page_ids).await?;
    // The scope each would inherit from, when it is above the root.
    let above_root = (links.len() > 1)
        .then(|| links.get(source_index(&links, true)))
        .flatten()
        .filter(|link| link.node.unique_scope)
        .filter(|link| link.role.as_ref().is_some_and(Access::can_manage_access))
        .map(crumb);
    let mut breaks = Vec::with_capacity(page_ids.len());
    for (found, breaks_below) in shown
        .into_iter()
        .zip(below)
        .skip(start)
        .take(end.saturating_sub(start))
    {
        let inherited_from = match found.source.and_then(|i| found.path.get(i)) {
            Some(source) => Some(source.clone()),
            None if found.node.kind == NodeKind::Site => None,
            None => above_root.clone(),
        };
        let (broken_by, changed_by) = events.remove(&found.node.id).unwrap_or((None, None));
        breaks.push(Break {
            kind: found.node.kind,
            assignments: assignments.remove(&found.node.id).unwrap_or_default(),
            inherited_from,
            broken_by,
            changed_by,
            breaks_below,
            anyone_links: anyone.remove(&found.node.id).unwrap_or(0),
            path: found.path,
            node: found.node,
        });
    }
    Ok(Some((breaks, next)))
}

/// See [`Db::break_counts`]; on `conn`.
pub(crate) async fn break_counts_in(
    conn: &mut PgConnection,
    viewer: UserId,
    parent: NodeId,
) -> Result<Vec<(NodeId, i64)>, DbError> {
    let standing = standing(&mut *conn, viewer).await?;
    let Some(links) = chain(&mut *conn, viewer, parent).await? else {
        return Ok(Vec::new());
    };
    // Inside the viewer's reach as a manager: they manage access at or above `parent`.
    let inside = links
        .iter()
        .any(|link| link.role.as_ref().is_some_and(Access::can_manage_access));
    if !inside {
        return Ok(Vec::new());
    }
    let mut counts: HashMap<NodeId, (Step, i64)> = HashMap::new();
    for found in scopes_under(&mut *conn, viewer, standing, &links).await? {
        if !found.managed() {
            continue;
        }
        if let (Some(child), Some(key)) = (found.line.get(1), found.key.get(1)) {
            counts.entry(*child).or_insert_with(|| (key.clone(), 0)).1 += 1;
        }
    }
    let mut counts: Vec<(NodeId, (Step, i64))> = counts.into_iter().collect();
    counts.sort_by(|a, b| a.1.0.cmp(&b.1.0));
    Ok(counts
        .into_iter()
        .map(|(child, (_, count))| (child, count))
        .collect())
}

/// Everyone named on either of the scopes `$1` (an array) directly or through one of the groups
/// `$4` (an array), with their standing and the permissions the assignments that apply to them
/// (see `applies_to!`), among those naming them directly or one of `$4`, give on the scope `$2`
/// and on the scope `$3`.
const PEOPLE_ON_TWO_SCOPES: &str = concat!(
    "with named(user_id) as ( \
         select a.principal_id from assignments a \
         where a.node_id = any($1) and a.principal_kind = 'user' \
         union \
         select m.user_id from assignments a \
         join group_members m on a.principal_kind = 'group' and m.group_id = a.principal_id \
         where a.node_id = any($1) and a.principal_id = any($4)) \
     select u.id, u.name, u.is_admin, (u.disabled_at is null) as enabled, \
            (select bit_or(r.perms) from assignments a join roles r on r.id = a.role_id \
             where a.node_id = $2 and (a.principal_kind = 'user' or a.principal_id = any($4)) \
               and ",
    applies_to!("u.id"),
    ") as before, \
            (select bit_or(r.perms) from assignments a join roles r on r.id = a.role_id \
             where a.node_id = $3 and (a.principal_kind = 'user' or a.principal_id = any($4)) \
               and ",
    applies_to!("u.id"),
    ") as after \
     from named join users u on u.id = named.user_id \
     order by lower(u.name), u.id"
);
/// The permissions a person of `standing` has on a scope where the assignments that apply to
/// them give `given` (a `bit_or` of their roles' permissions, `None` for no assignment).
fn on_scope(standing: Standing, given: Option<i32>) -> Result<Perms, sqlx::Error> {
    let own = given
        .map(Perms::from_column)
        .transpose()?
        .map(|perms| Access {
            perms,
            roles: Vec::new(),
        });
    Ok(access_step(standing, true, own, None).map_or(Perms::NONE, |access| access.perms))
}

/// What would change on the scope `node` if it took the assignments of the scope `from`
/// instead, as `viewer` may see it: every person whose permissions would change, by name, with
/// their permissions before and after; and every group whose members `viewer` may not see whose
/// role would change, with its permissions before and after (`NONE` where it has none), by
/// name. A person's permissions are worked out from their direct assignments and the groups
/// whose members `viewer` may see; what a hidden group gives them is reported only as that
/// group's change, so that no member of it is named.
async fn changes_on_restore(
    conn: &mut PgConnection,
    viewer: UserId,
    node: NodeId,
    from: NodeId,
) -> Result<
    (
        Vec<(UserRef, Perms, Perms)>,
        Vec<(PrincipalView, Perms, Perms)>,
    ),
    sqlx::Error,
> {
    let rows: Vec<(Uuid, Uuid, Option<Uuid>, i32)> = sqlx::query_as(
        "select a.node_id, a.principal_id, g.site_id, r.perms \
         from assignments a join roles r on r.id = a.role_id \
         join groups g on g.id = a.principal_id \
         where a.node_id = any($1) and a.principal_kind = 'group' order by a.principal_id",
    )
    .bind(vec![node.0, from.0])
    .fetch_all(&mut *conn)
    .await?;
    let groups: Vec<(Uuid, Option<Uuid>)> = rows
        .iter()
        .map(|(_, group, site, _)| (*group, *site))
        .collect();
    let seen = members_visible(&mut *conn, viewer, groups).await?;
    let seen_ids: Vec<Uuid> = seen.iter().map(|g| g.0).collect();
    let people_rows = sqlx::query(PEOPLE_ON_TWO_SCOPES)
        .bind(vec![node.0, from.0])
        .bind(node.0)
        .bind(from.0)
        .bind(&seen_ids)
        .fetch_all(&mut *conn)
        .await?;
    let mut people = Vec::new();
    for row in &people_rows {
        let standing = Standing::of(row.try_get("is_admin")?, row.try_get("enabled")?);
        let before = on_scope(standing, row.try_get("before")?)?;
        let after = on_scope(standing, row.try_get("after")?)?;
        if before != after {
            people.push((
                UserRef {
                    id: UserId(row.try_get("id")?),
                    name: row.try_get("name")?,
                },
                before,
                after,
            ));
        }
    }
    let mut hidden: HashMap<GroupId, (Perms, Perms)> = HashMap::new();
    for (scope, group, _, perms) in rows {
        let group = GroupId(group);
        if seen.contains(&group) {
            continue;
        }
        let perms = Perms::from_column(perms)?;
        let entry = hidden.entry(group).or_insert((Perms::NONE, Perms::NONE));
        if scope == node.0 {
            entry.0 = perms;
        } else {
            entry.1 = perms;
        }
    }
    hidden.retain(|_, (before, after)| before != after);
    let views = group_views(&mut *conn, viewer, hidden.keys().copied().collect()).await?;
    let mut changed: Vec<(String, PrincipalView, Perms, Perms)> = hidden
        .into_iter()
        .map(|(group, (before, after))| {
            let view = views
                .get(&group)
                .cloned()
                .unwrap_or(PrincipalView::HiddenGroup(group));
            let order = match &view {
                PrincipalView::Group { name, .. } => name.to_lowercase(),
                _ => String::new(),
            };
            (format!("{order}\u{0}{}", group.0), view, before, after)
        })
        .collect();
    changed.sort_by(|a, b| a.0.cmp(&b.0));
    Ok((
        people,
        changed
            .into_iter()
            .map(|(_, view, before, after)| (view, before, after))
            .collect(),
    ))
}
#[cfg(feature = "test-support")]
impl Db {
    /// The plan PostgreSQL chooses for the explorer's walk of the live scopes of `site` and the
    /// nodes above them, up to `root`, one plan line per line. For tests of its index.
    pub async fn explain_walk(&self, site: NodeId, root: NodeId) -> Result<String, DbError> {
        let lines: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("explain {WALK}")))
            .bind(site.0)
            .bind(root.0)
            .fetch_all(self.pool())
            .await?;
        Ok(lines.join(
            "
",
        ))
    }
}

impl Db {
    /// One page of the breaks the explorer shows from `root`: every live scope strictly below
    /// `root`, and `root` itself when it is a scope, that `viewer` has `manage_access` on, kept
    /// by `filter`, in the order of the tree (each after its ancestors; folders before files,
    /// then by name ignoring case). At most `limit` (at least one), after the cursor `after`
    /// (from a previous page), and the cursor of the next page if there is one. `None` when
    /// the viewer lacks `manage_access` on `root`.
    ///
    /// Reads the live scopes and the nodes above them, and their assignments: its cost follows
    /// the number of breaks, never the number of files.
    pub async fn breaks(
        &self,
        viewer: UserId,
        root: NodeId,
        filter: &ExplorerFilter,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Option<(Vec<Break>, Option<String>)>, DbError> {
        let mut conn = self.pool().acquire().await?;
        breaks_in(&mut conn, viewer, root, filter, after, limit).await
    }

    /// Every break of [`Db::breaks`] from `root` with `filter`, in the order of the tree, read
    /// at once: for the CSV export, whose cost then follows the number of breaks once rather
    /// than once per page. `None` when the viewer lacks `manage_access` on `root`.
    pub async fn all_breaks(
        &self,
        viewer: UserId,
        root: NodeId,
        filter: &ExplorerFilter,
    ) -> Result<Option<Vec<Break>>, DbError> {
        let mut conn = self.pool().acquire().await?;
        Ok(breaks_window(&mut conn, viewer, root, filter, None, None)
            .await?
            .map(|(breaks, _)| breaks))
    }

    /// For the explorer's tree: for each child of `parent` with breaks at or below it that
    /// `viewer` has `manage_access` on, how many; children with none are left out. In the
    /// order of the tree. Empty unless the viewer manages access at or above `parent`. Built
    /// from the live scopes and their ancestors, never by walking files.
    pub async fn break_counts(
        &self,
        viewer: UserId,
        parent: NodeId,
    ) -> Result<Vec<(NodeId, i64)>, DbError> {
        let mut conn = self.pool().acquire().await?;
        break_counts_in(&mut conn, viewer, parent).await
    }

    /// What restoring inheritance on `node` would change: every person named on the node or on
    /// the scope it would inherit from, directly or through a group, whose permissions would
    /// change, with their permissions now and after (a person in two groups is judged by the
    /// union of both, as access is). Only groups whose members `viewer` may see count towards a
    /// person; any other group whose role would change is listed as a group, its members never
    /// named. Everything below the node that inherits follows it; the
    /// breaks below keep their own assignments and are counted as unaffected. `viewer` must
    /// manage access to the node and to the scope above it, as restoring requires (`NotFound`
    /// or `Forbidden`); `IsSite` for a site. For a node that inherits nothing changes.
    pub async fn restore_preview(
        &self,
        viewer: UserId,
        node: NodeId,
    ) -> Result<RestorePreview, AccessError> {
        let mut conn = self.pool().acquire().await?;
        let standing = standing(&mut conn, viewer).await?;
        let links = manager(chain(&mut conn, viewer, node).await?)?;
        let Some(target) = links.last() else {
            return Err(AccessError::NotFound);
        };
        if target.node.kind == NodeKind::Site {
            return Err(AccessError::IsSite);
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
        let from = links
            .get(source_index(&links, true))
            .ok_or(AccessError::NotFound)?;
        let (people, groups) = if target.node.unique_scope {
            changes_on_restore(&mut conn, viewer, node, from.node.id).await?
        } else {
            (Vec::new(), Vec::new())
        };
        let below = scopes_under(&mut conn, viewer, standing, &links)
            .await?
            .iter()
            .filter(|found| found.managed() && found.node.id != node)
            .count();
        Ok(RestorePreview {
            node: crumb(target),
            inherit_from: crumb(from),
            people,
            groups,
            unchanged_breaks_below: i64::try_from(below).unwrap_or(i64::MAX),
        })
    }
}

/// The breaks as CSV (RFC 4180, lines ending CRLF): a header, then one row per assignment with
/// the break's path (from the explorer's root, names joined by ` / `), its kind, the principal's
/// kind (`person`, `group` or `site group`) and name (a group the viewer may not see by its id),
/// the role, its permissions (names separated by spaces), and who broke inheritance there and
/// when (UTC). A break with no assignment gets one row with the principal's cells empty. A cell
/// starting with `=`, `+`, `-`, `@`, a tab or a carriage return is prefixed with an apostrophe
/// so that a spreadsheet does not run it as a formula.
#[must_use]
pub fn breaks_csv(breaks: &[Break]) -> String {
    let mut out = String::new();
    push_row(
        &mut out,
        &[
            "path",
            "kind",
            "principal kind",
            "principal",
            "role",
            "permissions",
            "broken by",
            "broken at",
        ],
    );
    for found in breaks {
        let path = found
            .path
            .iter()
            .chain(std::iter::once(&found.node))
            .map(|crumb| crumb.name.as_str())
            .collect::<Vec<_>>()
            .join(" / ");
        let (by, at) = match &found.broken_by {
            Some((user, at)) => (user.name.clone(), utc(*at)),
            None => (String::new(), String::new()),
        };
        let kind = found.kind.as_str();
        if found.assignments.is_empty() {
            push_row(&mut out, &[&path, kind, "", "", "", "", &by, &at]);
        }
        for assignment in &found.assignments {
            let (principal_kind, principal) = match &assignment.principal {
                PrincipalView::User(user) => ("person", user.name.clone()),
                PrincipalView::Group {
                    name, site: None, ..
                } => ("group", name.clone()),
                PrincipalView::Group {
                    name,
                    site: Some(_),
                    ..
                } => ("site group", name.clone()),
                PrincipalView::HiddenGroup(id) => ("group", id.0.hyphenated().to_string()),
                PrincipalView::Link {
                    link,
                    kind,
                    expires_at,
                    ..
                } => {
                    let kind = match kind {
                        LinkKind::SignedIn => "signed-in link",
                        LinkKind::Anyone => "anyone link",
                    };
                    let expires = expires_at
                        .map(|at| format!(" expires {}", utc(at)))
                        .unwrap_or_default();
                    ("link", format!("{kind} {}{expires}", link.hyphenated()))
                }
            };
            push_row(
                &mut out,
                &[
                    &path,
                    kind,
                    principal_kind,
                    &principal,
                    &assignment.role.name,
                    &assignment.perms.names().join(" "),
                    &by,
                    &at,
                ],
            );
        }
    }
    out
}

fn push_row(out: &mut String, cells: &[&str]) {
    for (i, cell) in cells.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&csv_cell(cell));
    }
    out.push_str("\r\n");
}

/// One cell: neutralised if a spreadsheet would take it for a formula, then quoted if needed.
fn csv_cell(text: &str) -> String {
    let mut cell = String::with_capacity(text.len() + 3);
    if text.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        cell.push('\'');
    }
    cell.push_str(text);
    if cell.contains([',', '"', '\r', '\n']) {
        format!("\"{}\"", cell.replace('"', "\"\""))
    } else {
        cell
    }
}

/// `at` in UTC, as `YYYY-MM-DDTHH:MM:SSZ`.
fn utc(at: OffsetDateTime) -> String {
    let at = at.to_offset(UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        at.year(),
        u8::from(at.month()),
        at.day(),
        at.hour(),
        at.minute(),
        at.second()
    )
}
