//! Navigation as people see it, and the changes of hubs and navigation: the global, hub and
//! site navigation resolved for one viewer (an item whose node the viewer cannot see, or that
//! is deleted, is left out, as is an item aimed at groups the viewer is in none of unless they
//! manage that navigation, and a heading left with no links); a site's automatic navigation;
//! the checks a person saving navigation must pass; and the mirror of a change of several
//! sites' rows at once (a hub registered or unregistered, a site joining or leaving one) or of
//! the global navigation.
//!
//! Audience targeting never grants or refuses access: it only hides a link.

use std::collections::{HashMap, HashSet};

use collab_core::ids::{GroupId, NodeId, UserId};
use revisions::{NavItem, NavTarget};
use serde::Serialize;
use uuid::Uuid;

use crate::events::{NewEvent, record};
use crate::sites::{GLOBAL_NAVIGATION, SiteSettings, release_hub_rows, upsert_site};
use crate::tree::NodeChange;
use crate::{AccessError, Db, DbError, Perms};

/// Where a resolved navigation item leads.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum ResolvedTarget {
    /// A node the viewer can see: `{"node": id, "kind": "library"}`.
    Node {
        /// The node's id.
        node: Uuid,
        /// `site`, `library`, `folder` or `document`, for the page to build its address.
        kind: &'static str,
    },
    /// An address: `{"url": text}`.
    Url {
        /// The address as stored.
        url: String,
    },
}

/// One item of a navigation as a viewer sees it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ResolvedItem {
    /// The item's lasting identifier (a library's id in automatic navigation).
    pub id: Uuid,
    /// What it shows: its label, or for a node target with no label the node's current name.
    pub label: String,
    /// Where it leads; `None` for a heading.
    pub target: Option<ResolvedTarget>,
    /// The viewer is in none of the item's audience and sees it only because they manage this
    /// navigation.
    pub targeted: bool,
    /// The items below it.
    pub children: Vec<ResolvedItem>,
}

/// What resolving needs to know of one viewer: who they are and the groups they are in.
#[derive(Clone, Debug)]
pub struct NavViewer {
    user: UserId,
    groups: HashSet<Uuid>,
}

/// A node target as the viewer may see it: its name and kind.
struct Seen {
    name: String,
    kind: &'static str,
}

/// A change of the rows of several sites at once, mirrored once Git has recorded it.
#[derive(Clone, Debug)]
pub struct SitesMirror<'a> {
    /// Each changed site's settings as Git now holds them; `purging` is not written.
    pub settings: &'a [SiteSettings],
    /// Who makes the change.
    pub actor: UserId,
    /// Whether `actor` must be an enabled administrator.
    pub needs_admin: bool,
    /// Nodes `actor` must hold the given permissions on.
    pub needs: &'a [(NodeId, Perms)],
    /// Sites whose rows are locked with the changed ones, unchanged: the hub a site joins.
    pub also_lock: &'a [NodeId],
    /// A hub whose members leave it (see `release_hub_rows`), recording `hub.left` for each;
    /// their settings are among `settings`, with no hub.
    pub release_hub: Option<NodeId>,
    /// The events to record with the change.
    pub events: Vec<NewEvent>,
}

/// Every node target of `items`, children included.
fn node_targets(items: &[NavItem]) -> Vec<Uuid> {
    let mut nodes = Vec::new();
    for item in items {
        for one in std::iter::once(item).chain(&item.children) {
            if let Some(NavTarget::Node(node)) = &one.target {
                nodes.push(*node);
            }
        }
    }
    nodes.sort_unstable();
    nodes.dedup();
    nodes
}

/// Every audience group of `items`, children included.
fn audience_groups(items: &[NavItem]) -> Vec<Uuid> {
    let mut groups: Vec<Uuid> = items
        .iter()
        .flat_map(|item| std::iter::once(item).chain(&item.children))
        .flat_map(|item| item.audience.iter().copied())
        .collect();
    groups.sort_unstable();
    groups.dedup();
    groups
}

impl Db {
    /// What resolving navigation for `user` needs: the groups they are in.
    pub async fn nav_viewer(&self, user: UserId) -> Result<NavViewer, DbError> {
        let groups = self
            .groups_of(user)
            .await?
            .into_iter()
            .map(|g: GroupId| g.0)
            .collect();
        Ok(NavViewer { user, groups })
    }

    /// The nodes among `nodes` that `user` may view (and that are not deleted), with their
    /// current names and kinds.
    async fn seen_nodes(
        &self,
        user: UserId,
        nodes: &[Uuid],
    ) -> Result<HashMap<Uuid, Seen>, DbError> {
        let mut seen = HashMap::new();
        for id in nodes {
            let Some(access) = self.effective_access(user, NodeId(*id)).await? else {
                continue;
            };
            if !access.perms.contains(Perms::VIEW) {
                continue;
            }
            if let Some(node) = self.node(NodeId(*id)).await?
                && !node.deleted
            {
                seen.insert(
                    *id,
                    Seen {
                        name: node.name,
                        kind: node.kind.as_str(),
                    },
                );
            }
        }
        Ok(seen)
    }

    /// `items` as `viewer` sees them. An item whose node they cannot view, or which is deleted,
    /// is left out, with its children. An item with an audience they are in none of is left
    /// out too, unless `manages` (they manage this navigation), when it comes marked
    /// `targeted`. A heading left with no children is left out. A node target with no label
    /// shows the node's current name. Nothing the viewer could not see is named.
    pub async fn resolve_navigation(
        &self,
        viewer: &NavViewer,
        items: &[NavItem],
        manages: bool,
    ) -> Result<Vec<ResolvedItem>, DbError> {
        let seen = self.seen_nodes(viewer.user, &node_targets(items)).await?;
        let resolve = |item: &NavItem| -> Option<ResolvedItem> {
            let outside = !item.audience.is_empty()
                && !item.audience.iter().any(|g| viewer.groups.contains(g));
            if outside && !manages {
                return None;
            }
            let (label, target) = match &item.target {
                None => (item.label.clone(), None),
                Some(NavTarget::Url(url)) => (
                    item.label.clone(),
                    Some(ResolvedTarget::Url { url: url.clone() }),
                ),
                Some(NavTarget::Node(node)) => {
                    let found = seen.get(node)?;
                    let label = if item.label.is_empty() {
                        found.name.clone()
                    } else {
                        item.label.clone()
                    };
                    (
                        label,
                        Some(ResolvedTarget::Node {
                            node: *node,
                            kind: found.kind,
                        }),
                    )
                }
            };
            Some(ResolvedItem {
                id: item.id,
                label,
                target,
                targeted: outside,
                children: Vec::new(),
            })
        };
        let mut out = Vec::new();
        for item in items {
            let Some(mut resolved) = resolve(item) else {
                continue;
            };
            resolved.children = item.children.iter().filter_map(&resolve).collect();
            if resolved.target.is_none() && resolved.children.is_empty() {
                continue;
            }
            out.push(resolved);
        }
        Ok(out)
    }

    /// The automatic navigation of `site` as `user` sees it: every library of the site they
    /// can view, by name, each a link to it (its id the item's).
    pub async fn automatic_navigation(
        &self,
        user: UserId,
        site: NodeId,
    ) -> Result<Vec<ResolvedItem>, DbError> {
        let libraries: Vec<Uuid> = sqlx::query_scalar(
            "select id from nodes \
             where parent_id = $1 and kind = 'library' and deleted_at is null \
             order by lower(name), id",
        )
        .bind(site.0)
        .fetch_all(self.pool())
        .await?;
        let mut seen = self.seen_nodes(user, &libraries).await?;
        Ok(libraries
            .into_iter()
            .filter_map(|id| {
                seen.remove(&id).map(|found| ResolvedItem {
                    id,
                    label: found.name,
                    target: Some(ResolvedTarget::Node {
                        node: id,
                        kind: found.kind,
                    }),
                    targeted: false,
                    children: Vec::new(),
                })
            })
            .collect())
    }

    /// The names of the nodes `items` targets that `user` may view, by id: what an editor of
    /// the navigation is shown beside each node target.
    pub async fn visible_target_names(
        &self,
        user: UserId,
        items: &[NavItem],
    ) -> Result<HashMap<Uuid, String>, DbError> {
        Ok(self
            .seen_nodes(user, &node_targets(items))
            .await?
            .into_iter()
            .map(|(id, seen)| (id, seen.name))
            .collect())
    }

    /// Checks what `user` saves into a navigation beyond its structure (see
    /// [`Db::validate_navigation`]): they may view every node it targets, so nobody puts a name
    /// they cannot see into navigation, and every group of an audience exists (a sharing link's
    /// hidden group never does). `InvalidNavigation` otherwise, whichever it is, so a refusal
    /// says nothing of what exists.
    pub async fn check_navigation_saver(
        &self,
        user: UserId,
        items: &[NavItem],
    ) -> Result<(), AccessError> {
        let nodes = node_targets(items);
        if self.seen_nodes(user, &nodes).await?.len() != nodes.len() {
            return Err(AccessError::InvalidNavigation);
        }
        let groups = audience_groups(items);
        if !groups.is_empty() {
            let found: i64 = sqlx::query_scalar(
                "select count(*) from groups g \
                 where g.id = any($1) and g.purpose is distinct from 'link'",
            )
            .bind(&groups)
            .fetch_one(self.pool())
            .await?;
            if usize::try_from(found).ok() != Some(groups.len()) {
                return Err(AccessError::InvalidNavigation);
            }
        }
        Ok(())
    }

    /// Mirrors one committed change of several sites' rows, in one transaction: judges what it
    /// needs as a tree change does (`NotFound`, `Forbidden` or `Archived`), the rows locked
    /// `FOR UPDATE` by id; the members of `release_hub` leave it (with `hub.left`); then each
    /// site's row is written and the events recorded. A database failure before the change
    /// was judged allowed is `Unjudged`.
    pub async fn apply_sites_mirror(&self, change: SitesMirror<'_>) -> Result<(), AccessError> {
        let mut sites: Vec<NodeId> = change
            .settings
            .iter()
            .map(|s| s.id)
            .chain(change.also_lock.iter().copied())
            .collect();
        sites.sort_unstable();
        sites.dedup();
        let (mut tx, _) = self
            .judged(&NodeChange {
                actor: Some(change.actor),
                needs_admin: change.needs_admin,
                needs: change.needs,
                sites: &sites,
                ..NodeChange::default()
            })
            .await
            .map_err(|e| match e {
                AccessError::Db(e) => AccessError::Unjudged(e),
                other => other,
            })?;
        if let Some(hub) = change.release_hub {
            release_hub_rows(&mut tx, Some(change.actor), hub).await?;
        }
        for settings in change.settings {
            upsert_site(&mut tx, settings, false).await?;
        }
        for event in change.events {
            record(&mut tx, event).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Mirrors a committed change of the global navigation made by `actor`, who must be an
    /// enabled administrator (`Forbidden`), with `event`, in one transaction. A database
    /// failure before the change was judged allowed is `Unjudged`.
    pub async fn apply_global_navigation_by(
        &self,
        actor: UserId,
        items: &[NavItem],
        event: NewEvent,
    ) -> Result<(), AccessError> {
        let value = serde_json::to_value(items).map_err(DbError::new)?;
        let (mut tx, _) = self
            .judged(&NodeChange {
                actor: Some(actor),
                needs_admin: true,
                ..NodeChange::default()
            })
            .await
            .map_err(|e| match e {
                AccessError::Db(e) => AccessError::Unjudged(e),
                other => other,
            })?;
        sqlx::query(
            "insert into settings (key, value, changed_at) values ($1, $2, now()) \
             on conflict (key) do update set value = excluded.value, \
                 changed_at = excluded.changed_at",
        )
        .bind(GLOBAL_NAVIGATION)
        .bind(&value)
        .execute(&mut *tx)
        .await?;
        record(&mut tx, event).await?;
        tx.commit().await?;
        Ok(())
    }
}
