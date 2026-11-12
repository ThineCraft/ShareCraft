//! Groups of people, which can be given roles like a person. Every site has three groups,
//! Owners, Members and Visitors, assigned Owner, Editor and Viewer on the site; a site group
//! stores its site and purpose, and its name is built from the site's current name when read.
//! Administrators create organisation groups (groups with no site), which have stored names.
//!
//! A membership change follows the one locking order: the site's chain, then the groups
//! involved in one pass by id (the group changed `FOR UPDATE`, those the actor is judged through
//! `FOR SHARE`), then the roles involved; the decision is worked out again once every lock is
//! held.

use std::collections::BTreeMap;

use collab_core::ids::{GroupId, NodeId, UserId};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};
use uuid::Uuid;

use crate::accounts::user_from_row;
use crate::documents::UNIQUE_VIOLATION;
use crate::events::{NewEvent, record};
use crate::roles::lock_roles;
use crate::scopes::{
    LOCK_ATTEMPTS, MANAGE_ACCESS_BIT, MANAGE_SITE_BIT, REACH, Standing, access_in, judged_roles,
    lock_top_down, standing,
};
use crate::{AccessError, Db, DbError, Perms, User};

const MAX_GROUP_NAME_CHARS: usize = 60;

/// What a site's group is for: each site has one of each.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SiteGroup {
    /// Assigned Owner on the site.
    Owners,
    /// Assigned Editor on the site.
    Members,
    /// Assigned Viewer on the site.
    Visitors,
}

impl SiteGroup {
    /// Every purpose, in the order they are shown.
    pub const ALL: [SiteGroup; 3] = [SiteGroup::Owners, SiteGroup::Members, SiteGroup::Visitors];

    /// `owners`, `members` or `visitors`, as stored in `groups.purpose`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            SiteGroup::Owners => "owners",
            SiteGroup::Members => "members",
            SiteGroup::Visitors => "visitors",
        }
    }

    /// The purpose stored as `text`, if it is one.
    #[must_use]
    pub fn parse(text: &str) -> Option<SiteGroup> {
        SiteGroup::ALL.into_iter().find(|p| p.as_str() == text)
    }

    /// The built-in role the group has on its site: `owner`, `editor` or `viewer`.
    #[must_use]
    pub fn role(self) -> &'static str {
        match self {
            SiteGroup::Owners => "owner",
            SiteGroup::Members => "editor",
            SiteGroup::Visitors => "viewer",
        }
    }

    /// The word after the site's name in the group's name.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            SiteGroup::Owners => "Owners",
            SiteGroup::Members => "Members",
            SiteGroup::Visitors => "Visitors",
        }
    }
}

/// A group, as shown.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Group {
    pub id: GroupId,
    /// An organisation group's stored name; for a site group, the site's current name followed
    /// by `Owners`, `Members` or `Visitors`.
    pub name: String,
    /// The site of a site group; `None` for an organisation group.
    pub site: Option<NodeId>,
    /// The current name of the site of a site group; `None` for an organisation group. A site
    /// group's name can equal an organisation group's, so it is shown with its site.
    pub site_name: Option<String>,
    /// What a site group is for; `None` for an organisation group.
    pub purpose: Option<SiteGroup>,
    /// How many people are members.
    pub member_count: i64,
}

/// Groups `g`, each with its site `s` (for a site group) and its name as shown, `gn`.
const GROUP_FROM: &str = "groups g left join nodes s on s.id = g.site_id \
     cross join lateral (select coalesce(g.name, s.name || ' ' || \
         case g.purpose when 'owners' then 'Owners' when 'members' then 'Members' \
                        else 'Visitors' end) as display_name) gn";

/// The columns [`group_from_row`] reads, from [`GROUP_FROM`].
const GROUP_COLUMNS: &str = "g.id, g.site_id, s.name as site_name, g.purpose, gn.display_name, \
     (select count(*) from group_members m where m.group_id = g.id) as member_count";

fn group_from_row(row: &PgRow) -> Result<Group, sqlx::Error> {
    let purpose = row
        .try_get::<Option<String>, _>("purpose")?
        .map(|text| {
            SiteGroup::parse(&text)
                .ok_or_else(|| sqlx::Error::Decode(format!("unknown purpose {text:?}").into()))
        })
        .transpose()?;
    Ok(Group {
        id: GroupId(row.try_get("id")?),
        name: row.try_get("display_name")?,
        site: row.try_get::<Option<Uuid>, _>("site_id")?.map(NodeId),
        site_name: row.try_get("site_name")?,
        purpose,
        member_count: row.try_get("member_count")?,
    })
}

/// Makes the three groups of the site `site` that it does not have yet, each with its
/// assignment on the site, and records `group.created` for each. Returns how many it made. A
/// group that exists is left as it is, assignments included, so running this again never gives
/// back an assignment someone removed.
pub(crate) async fn ensure_site_groups(
    conn: &mut PgConnection,
    site: NodeId,
) -> Result<u64, sqlx::Error> {
    let made: Vec<(Uuid, String)> = sqlx::query_as(
        "with made as ( \
             insert into groups (id, site_id, purpose) \
             select gen_random_uuid(), $1, p from unnest(array['owners', 'members', 'visitors']) p \
             on conflict (site_id, purpose) do nothing \
             returning id, purpose), \
         given as ( \
             insert into assignments (node_id, principal_kind, principal_id, role_id) \
             select $1, 'group', id, case purpose when 'owners' then 'owner' \
                                     when 'members' then 'editor' else 'viewer' end \
             from made) \
         select id, purpose from made order by id",
    )
    .bind(site.0)
    .fetch_all(&mut *conn)
    .await?;
    for (id, purpose) in &made {
        record(
            conn,
            NewEvent {
                kind: "group.created",
                actor: None,
                subject: None,
                document: None,
                node: Some(site),
                detail: json!({ "id": id, "site": site.0, "purpose": purpose }),
            },
        )
        .await?;
    }
    Ok(u64::try_from(made.len()).unwrap_or(u64::MAX))
}

/// Makes `user` a member of the Owners group of `site`, recording `group.member_added` by
/// `actor`. Nothing happens if they already are one.
pub(crate) async fn add_site_owner(
    conn: &mut PgConnection,
    site: NodeId,
    user: UserId,
    actor: Option<UserId>,
) -> Result<(), sqlx::Error> {
    let added: Option<Uuid> = sqlx::query_scalar(
        "insert into group_members (group_id, user_id, added_by) \
         select id, $2, $3 from groups where site_id = $1 and purpose = 'owners' \
         on conflict (group_id, user_id) do nothing returning group_id",
    )
    .bind(site.0)
    .bind(user.0)
    .bind(actor.map(|a| a.0))
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(group) = added {
        record(
            conn,
            NewEvent {
                kind: "group.member_added",
                actor,
                subject: Some(user),
                document: None,
                node: Some(site),
                detail: json!({ "group": group, "purpose": "owners" }),
            },
        )
        .await?;
    }
    Ok(())
}

/// The groups with an assignment on one of `scopes` that `user` is a member of, by id: the
/// groups a decision on those scopes judges `user` through.
pub(crate) async fn groups_judged_by(
    conn: &mut PgConnection,
    user: UserId,
    scopes: &[NodeId],
) -> Result<Vec<GroupId>, sqlx::Error> {
    if scopes.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<Uuid> = scopes.iter().map(|s| s.0).collect();
    let groups: Vec<Uuid> = sqlx::query_scalar(
        "select distinct a.principal_id from assignments a \
         join group_members m on m.group_id = a.principal_id and m.user_id = $1 \
         where a.node_id = any($2) and a.principal_kind = 'group' \
         order by a.principal_id",
    )
    .bind(user.0)
    .bind(&ids)
    .fetch_all(conn)
    .await?;
    Ok(groups.into_iter().map(GroupId).collect())
}

/// Locks the groups `shared` `FOR SHARE` and `exclusive` `FOR UPDATE`, in one pass ordered by
/// id, each once in the strongest mode asked for. Ids that are not groups are passed over.
pub(crate) async fn lock_groups(
    conn: &mut PgConnection,
    shared: impl IntoIterator<Item = GroupId>,
    exclusive: Option<GroupId>,
) -> Result<(), sqlx::Error> {
    let mut modes: BTreeMap<GroupId, bool> = shared.into_iter().map(|g| (g, false)).collect();
    if let Some(group) = exclusive {
        modes.insert(group, true);
    }
    for (group, write) in modes {
        let statement = if write {
            "select id from groups where id = $1 for update"
        } else {
            "select id from groups where id = $1 for share"
        };
        sqlx::query(statement)
            .bind(group.0)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

/// Whether every group in `now` is in `locked`: a decision judged through a group that was not
/// locked must be worked out again.
pub(crate) fn all_locked(now: &[GroupId], locked: &[GroupId]) -> bool {
    now.iter().all(|g| locked.contains(g))
}

/// Whether `user` is an enabled administrator, locking their row `FOR SHARE` so that it stays
/// so until the transaction ends.
async fn locked_admin(conn: &mut PgConnection, user: UserId) -> Result<bool, sqlx::Error> {
    let admin: Option<bool> = sqlx::query_scalar(
        "select is_admin and disabled_at is null from users where id = $1 for share",
    )
    .bind(user.0)
    .fetch_optional(conn)
    .await?;
    Ok(admin == Some(true))
}

/// The site and purpose of the group `id`; `NoSuchGroup` if there is none.
async fn group_place(
    conn: &mut PgConnection,
    id: GroupId,
) -> Result<(Option<NodeId>, Option<SiteGroup>), AccessError> {
    let row: Option<(Option<Uuid>, Option<String>)> =
        sqlx::query_as("select site_id, purpose from groups where id = $1")
            .bind(id.0)
            .fetch_optional(conn)
            .await?;
    let (site, purpose) = row.ok_or(AccessError::NoSuchGroup)?;
    Ok((
        site.map(NodeId),
        purpose.as_deref().and_then(SiteGroup::parse),
    ))
}

/// The scopes where the group `id` has an assignment, with the permissions of its role there,
/// by node id.
async fn assigned_scopes(
    conn: &mut PgConnection,
    id: GroupId,
) -> Result<Vec<(NodeId, String, Perms)>, sqlx::Error> {
    let rows: Vec<(Uuid, String, i32)> = sqlx::query_as(
        "select a.node_id, r.id, r.perms from assignments a join roles r on r.id = a.role_id \
         where a.principal_kind = 'group' and a.principal_id = $1 order by a.node_id",
    )
    .bind(id.0)
    .fetch_all(conn)
    .await?;
    rows.into_iter()
        .map(|(node, role, perms)| Ok((NodeId(node), role, Perms::from_column(perms)?)))
        .collect()
}

/// Checks a group's name: 1 to 60 characters after trimming, no control characters. Returns
/// the trimmed name.
fn validate_group_name(name: &str) -> Result<&str, AccessError> {
    let name = name.trim();
    let length = name.chars().count();
    if (1..=MAX_GROUP_NAME_CHARS).contains(&length) && !name.chars().any(char::is_control) {
        Ok(name)
    } else {
        Err(AccessError::InvalidName)
    }
}

/// Whether `user` manages the members of a group of `site` (`None` for an organisation group):
/// an enabled administrator manages every group, and someone with `manage_site` on a site its
/// groups.
pub(crate) async fn manages(
    conn: &mut PgConnection,
    user: UserId,
    site: Option<NodeId>,
) -> Result<bool, sqlx::Error> {
    match standing(&mut *conn, user).await? {
        Standing::Admin => Ok(true),
        Standing::Nobody => Ok(false),
        Standing::Member => match site {
            None => Ok(false),
            Some(site) => Ok(access_in(conn, user, site)
                .await?
                .is_some_and(|access| access.can_manage_site())),
        },
    }
}

/// Which change of membership is being made.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Membership {
    Add,
    Remove,
}

impl Db {
    /// The groups `user` may see, by name: an enabled administrator sees every group; someone
    /// with `manage_access` somewhere sees the organisation groups and the groups of the sites
    /// they can reach; and whoever has `manage_site` on a live site sees that site's groups,
    /// whose members they manage. A site's name is never shown to someone who could not
    /// otherwise see the site.
    pub async fn groups_visible_to(&self, user: UserId) -> Result<Vec<Group>, DbError> {
        let mut conn = self.pool().acquire().await?;
        Ok(visible_groups(&mut conn, user, None).await?)
    }

    /// The group `id` and its members, by name, if `user` may see the group (see
    /// [`Db::groups_visible_to`]). The members are listed only for someone who manages the
    /// group (an administrator, or someone with `manage_site` on its site); anyone else gets an
    /// empty list.
    pub async fn group(
        &self,
        user: UserId,
        id: GroupId,
    ) -> Result<Option<(Group, Vec<User>)>, DbError> {
        let mut conn = self.pool().acquire().await?;
        let Some(group) = visible_groups(&mut conn, user, Some(&[id])).await?.pop() else {
            return Ok(None);
        };
        if !manages(&mut conn, user, group.site).await? {
            return Ok(Some((group, Vec::new())));
        }
        let rows = sqlx::query(
            "select u.id, u.email, u.name, u.is_admin, (u.disabled_at is not null) as disabled \
             from group_members m join users u on u.id = m.user_id \
             where m.group_id = $1 order by lower(u.name), u.id",
        )
        .bind(id.0)
        .fetch_all(&mut *conn)
        .await?;
        let members = rows
            .iter()
            .map(user_from_row)
            .collect::<Result<_, sqlx::Error>>()?;
        Ok(Some((group, members)))
    }

    /// Whether `user` manages the members of `group`: an enabled administrator manages every
    /// group, someone with `manage_site` on a site its groups. `NoSuchGroup` if there is no such
    /// group. [`Db::add_member`] and [`Db::remove_member`] decide again under their locks.
    pub async fn manages_group(&self, user: UserId, group: GroupId) -> Result<bool, AccessError> {
        let mut conn = self.pool().acquire().await?;
        let (site, _) = group_place(&mut conn, group).await?;
        Ok(manages(&mut conn, user, site).await?)
    }

    /// The groups `user` is a member of, by id.
    pub async fn groups_of(&self, user: UserId) -> Result<Vec<GroupId>, DbError> {
        let ids: Vec<Uuid> = sqlx::query_scalar(
            "select group_id from group_members where user_id = $1 order by group_id",
        )
        .bind(user.0)
        .fetch_all(self.pool())
        .await?;
        Ok(ids.into_iter().map(GroupId).collect())
    }

    /// Creates an organisation group (a group with no site) named `name`, with no members.
    /// `actor` must be an enabled administrator (`Forbidden`). `InvalidName` for a name that is
    /// empty or longer than 60 characters after trimming, `NameTaken` for one another
    /// organisation group has, ignoring case. Records `group.created`.
    pub async fn create_group(&self, actor: UserId, name: &str) -> Result<Group, AccessError> {
        let name = validate_group_name(name)?;
        let mut tx = self.pool().begin().await?;
        if !locked_admin(&mut tx, actor).await? {
            return Err(AccessError::Forbidden);
        }
        let id = Uuid::new_v4();
        let inserted = sqlx::query("insert into groups (id, name, created_by) values ($1, $2, $3)")
            .bind(id)
            .bind(name)
            .bind(actor.0)
            .execute(&mut *tx)
            .await;
        match inserted {
            Ok(_) => {}
            Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some(UNIQUE_VIOLATION) => {
                return Err(AccessError::NameTaken);
            }
            Err(e) => return Err(e.into()),
        }
        record(
            &mut tx,
            NewEvent {
                kind: "group.created",
                actor: Some(actor),
                subject: None,
                document: None,
                node: None,
                detail: json!({ "id": id, "name": name }),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(Group {
            id: GroupId(id),
            name: name.to_owned(),
            site: None,
            site_name: None,
            purpose: None,
            member_count: 0,
        })
    }

    /// Deletes the organisation group `group` and its memberships. `actor` must be an enabled
    /// administrator (`Forbidden`); `NoSuchGroup` for an unknown group, `SiteGroup` for a
    /// site's group, `GroupInUse` while any assignment names it. Takes the group's row
    /// `FOR UPDATE`, so an assignment being given to it either finishes first (and the group is
    /// in use) or waits and finds it gone. Records `group.deleted`.
    pub async fn delete_group(&self, actor: UserId, group: GroupId) -> Result<(), AccessError> {
        let mut tx = self.pool().begin().await?;
        if !locked_admin(&mut tx, actor).await? {
            return Err(AccessError::Forbidden);
        }
        let row: Option<(Option<String>, Option<Uuid>)> =
            sqlx::query_as("select name, site_id from groups where id = $1 for update")
                .bind(group.0)
                .fetch_optional(&mut *tx)
                .await?;
        let (name, site) = row.ok_or(AccessError::NoSuchGroup)?;
        if site.is_some() {
            return Err(AccessError::SiteGroup);
        }
        let in_use: bool = sqlx::query_scalar(
            "select exists (select 1 from assignments \
                            where principal_kind = 'group' and principal_id = $1)",
        )
        .bind(group.0)
        .fetch_one(&mut *tx)
        .await?;
        if in_use {
            return Err(AccessError::GroupInUse);
        }
        let members = sqlx::query("delete from group_members where group_id = $1")
            .bind(group.0)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        sqlx::query("delete from groups where id = $1")
            .bind(group.0)
            .execute(&mut *tx)
            .await?;
        record(
            &mut tx,
            NewEvent {
                kind: "group.deleted",
                actor: Some(actor),
                subject: None,
                document: None,
                node: None,
                detail: json!({ "id": group.0, "name": name, "members": members }),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Makes `user` a member of `group`; `false` if they already were one. `actor` must be an
    /// enabled administrator, or have `manage_site` on the site of a site group (`Forbidden`;
    /// `NotFound` if they cannot see the site). Someone who is not an administrator may only
    /// add a person to a group whose role, on every scope where the group is assigned, has no
    /// permission they do not hold there themselves (`BeyondYourAccess`). `NoSuchGroup` or
    /// `NoSuchUser` for an unknown group or person. Records `group.member_added`.
    pub async fn add_member(
        &self,
        actor: UserId,
        group: GroupId,
        user: UserId,
    ) -> Result<bool, AccessError> {
        self.change_membership(actor, group, user, Membership::Add)
            .await
    }

    /// Ends `user`'s membership of `group`; `false` if they were not a member. `actor` must be
    /// an enabled administrator, or have `manage_site` on the site of a site group
    /// (`Forbidden`; `NotFound` if they cannot see the site). `NoSuchGroup` for an unknown
    /// group. Emptying a group that is a scope's only manager is allowed: an administrator can
    /// always repair the scope. Records `group.member_removed`.
    pub async fn remove_member(
        &self,
        actor: UserId,
        group: GroupId,
        user: UserId,
    ) -> Result<bool, AccessError> {
        self.change_membership(actor, group, user, Membership::Remove)
            .await
    }

    async fn change_membership(
        &self,
        actor: UserId,
        group: GroupId,
        user: UserId,
        change: Membership,
    ) -> Result<bool, AccessError> {
        for _ in 0..LOCK_ATTEMPTS {
            let mut tx = self.pool().begin().await?;
            let (site, purpose) = group_place(&mut tx, group).await?;
            if locked_admin(&mut tx, actor).await? {
                lock_groups(&mut tx, [], Some(group)).await?;
            } else {
                let site = site.ok_or(AccessError::Forbidden)?;
                if !judge_membership(&mut tx, actor, group, site, change).await? {
                    tx.rollback().await?;
                    continue;
                }
            }
            // The group's row is held `FOR UPDATE` now: it cannot have gone.
            group_place(&mut tx, group).await?;
            let changed = match change {
                Membership::Add => {
                    let exists: Option<bool> =
                        sqlx::query_scalar("select true from users where id = $1")
                            .bind(user.0)
                            .fetch_optional(&mut *tx)
                            .await?;
                    if exists.is_none() {
                        return Err(AccessError::NoSuchUser);
                    }
                    sqlx::query(
                        "insert into group_members (group_id, user_id, added_by) \
                         values ($1, $2, $3) on conflict (group_id, user_id) do nothing",
                    )
                    .bind(group.0)
                    .bind(user.0)
                    .bind(actor.0)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected()
                }
                Membership::Remove => {
                    sqlx::query("delete from group_members where group_id = $1 and user_id = $2")
                        .bind(group.0)
                        .bind(user.0)
                        .execute(&mut *tx)
                        .await?
                        .rows_affected()
                }
            };
            if changed == 0 {
                return Ok(false);
            }
            let kind = match change {
                Membership::Add => "group.member_added",
                Membership::Remove => "group.member_removed",
            };
            record(
                &mut tx,
                NewEvent {
                    kind,
                    actor: Some(actor),
                    subject: Some(user),
                    document: None,
                    node: site,
                    detail: json!({
                        "group": group.0,
                        "purpose": purpose.map(SiteGroup::as_str),
                    }),
                },
            )
            .await?;
            tx.commit().await?;
            return Ok(true);
        }
        Err(DbError::new("access kept changing while a membership was being changed").into())
    }
}

/// Locks and judges a membership change of the site group `group` by `actor`, who is not an
/// administrator, in the one locking order: the site's chain (the site, `FOR SHARE`) and, when
/// adding, every other scope where the group is assigned (`FOR SHARE`, by id); then, in one
/// pass by id, `group` `FOR UPDATE` and the groups `actor` is judged through on those scopes
/// `FOR SHARE`; then the roles involved. Then works the decision out again: `manage_site` on
/// the site and, when adding, the subset rule on every scope. Returns `false` if what it
/// locked changed meanwhile: the caller starts over.
async fn judge_membership(
    conn: &mut PgConnection,
    actor: UserId,
    group: GroupId,
    site: NodeId,
    change: Membership,
) -> Result<bool, AccessError> {
    let site_chain = crate::scopes::lock_segment(&mut *conn, actor, site, false, false).await?;
    let Some(site_chain) = site_chain else {
        return Ok(false);
    };
    let assigned = match change {
        Membership::Add => assigned_scopes(&mut *conn, group).await?,
        Membership::Remove => Vec::new(),
    };
    let mut scopes = vec![site];
    scopes.extend(
        assigned
            .iter()
            .map(|(node, _, _)| *node)
            .filter(|n| *n != site),
    );
    // Ancestors first, as every chain and every tree change writing several rows locks them:
    // ordered by id alone, this could hold a folder while a move held its parent and waited
    // for it. All node rows (the site's chain, then these, `FOR SHARE`) are taken before any
    // group, as every transaction takes them: a tree change locks its chains and every row it
    // writes before the groups it is judged through, so it never holds a group while waiting
    // for one of these, and a writer of several rows locks them in this same order and never
    // writes a site row with them.
    lock_top_down(&mut *conn, scopes.get(1..).unwrap_or_default(), false).await?;
    let judged = groups_judged_by(&mut *conn, actor, &scopes).await?;
    lock_groups(&mut *conn, judged.iter().copied(), Some(group)).await?;
    let mut roles: Vec<String> = judged_roles(&site_chain).collect();
    roles.extend(assigned.iter().map(|(_, role, _)| role.clone()));
    let mut held = Vec::with_capacity(assigned.len());
    for (node, _, _) in &assigned {
        if let Some(links) = crate::scopes::chain(&mut *conn, actor, *node).await? {
            roles.extend(judged_roles(&links));
        }
        held.push(*node);
    }
    lock_roles(&mut *conn, roles).await?;
    // Every lock is held: work it all out again.
    let now_judged = groups_judged_by(&mut *conn, actor, &scopes).await?;
    if !all_locked(&now_judged, &judged) {
        return Ok(false);
    }
    let now_assigned = match change {
        Membership::Add => assigned_scopes(&mut *conn, group).await?,
        Membership::Remove => Vec::new(),
    };
    if now_assigned.iter().any(|(node, _, _)| !held.contains(node)) {
        return Ok(false);
    }
    match access_in(&mut *conn, actor, site).await? {
        None => return Err(AccessError::NotFound),
        Some(access) if !access.can_manage_site() => return Err(AccessError::Forbidden),
        Some(_) => {}
    }
    for (node, _, perms) in &now_assigned {
        let holds = access_in(&mut *conn, actor, *node)
            .await?
            .is_some_and(|access| access.perms.contains(*perms));
        if !holds {
            return Err(AccessError::BeyondYourAccess);
        }
    }
    Ok(true)
}

/// The groups `user` may see (see [`Db::groups_visible_to`]), by name; only those among `only`
/// when given.
pub(crate) async fn visible_groups(
    conn: &mut PgConnection,
    user: UserId,
    only: Option<&[GroupId]>,
) -> Result<Vec<Group>, sqlx::Error> {
    let admin = match standing(&mut *conn, user).await? {
        Standing::Nobody => return Ok(Vec::new()),
        Standing::Member => false,
        Standing::Admin => true,
    };
    let query = format!(
        "with recursive {REACH}, \
         manages(yes) as (select exists (select 1 from mine m \
                                         where m.perms & {MANAGE_ACCESS_BIT} <> 0 \
                                           and m.node_id in (select origin from live))) \
         select {GROUP_COLUMNS} from {GROUP_FROM} \
         where ($3::uuid[] is null or g.id = any($3)) \
           and ($2 \
                or ((select yes from manages) \
                    and (g.site_id is null or g.site_id in (select id from reach))) \
                or g.site_id in (select m.node_id from mine m \
                                 join nodes site on site.id = m.node_id \
                                 where site.parent_id is null and site.deleted_at is null \
                                   and m.perms & {MANAGE_SITE_BIT} <> 0)) \
         order by lower(gn.display_name), g.id"
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(query))
        .bind(user.0)
        .bind(admin)
        .bind(only.map(|ids| ids.iter().map(|g| g.0).collect::<Vec<Uuid>>()))
        .fetch_all(conn)
        .await?;
    rows.iter().map(group_from_row).collect()
}
