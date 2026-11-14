//! Hubs and navigation: making a site a hub and unregistering it, joining a site to a hub and
//! leaving it, the lists of hubs and of a hub's member sites, and the global, hub and site
//! navigation, answered already resolved for the viewer (so a page never receives a name the
//! viewer could not see) or, for someone who may change them, as stored for editing.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, Query, State};
use collab_core::ids::NodeId;
use records::{Db, NavViewer, Perms, ResolvedItem, SiteSettings, SiteState, User};
use revisions::{NavItem, NavTarget};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::nodes::node_id;
use super::sites::{SiteSummary, directory, manages, site_out, visible_sites};
use super::{ApiError, JsonBody, to_completion};
use crate::AppState;
use crate::auth::Signed;
use crate::sites;

/// The largest body a navigation route accepts: 100 items with addresses of 2,000 characters
/// fit with room to spare.
pub(crate) const MAX_NAVIGATION_BODY: usize = 512 * 1024;

/// What an editor sees for an audience group they may not see.
const UNSEEN_GROUP: &str = "A group you cannot see";

/// The query of the navigation reads: `edit=1` for the stored lists, for editing.
#[derive(Deserialize)]
pub(crate) struct NavQuery {
    edit: Option<String>,
}

impl NavQuery {
    /// Whether the stored lists are asked for.
    fn editing(&self) -> bool {
        matches!(self.edit.as_deref(), Some("1" | "true"))
    }
}

/// The body of `PUT /api/sites/{id}/hub`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HubBody {
    /// The hub's navigation bar.
    navigation: Vec<NavItem>,
}

/// The body of `PUT /api/sites/{id}/joined-hub`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct JoinBody {
    /// The hub to join; `null` (or absent) to leave the hub joined.
    hub: Option<String>,
}

/// The body of `PUT /api/sites/{id}/navigation` and `PUT /api/navigation`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ItemsBody {
    /// The items; `null` for a site's automatic navigation (not for the global one).
    items: Option<Vec<NavItem>>,
}

/// One row of `GET /api/hubs`.
#[derive(Serialize)]
pub(crate) struct HubOut {
    id: String,
    name: String,
    description: String,
}

/// `PUT /api/sites/{id}/hub {navigation}`: makes the site a hub with this navigation bar, or
/// changes the bar of a hub (see [`sites::set_hub`]). Administrators only (`403`, `403
/// archived` on an archived site); `404` for anything but a live site; `409 is_hub` for a site
/// that joined a hub; `422 invalid_navigation`. Answers the site as `GET /api/sites/{id}`.
pub(crate) async fn set_hub(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<HubBody>,
) -> Result<Json<Value>, ApiError> {
    let site = node_id(&id)?;
    let actor = signed.session.user;
    {
        let state = state.clone();
        let actor = actor.clone();
        to_completion(async move {
            sites::set_hub(&state, &actor, site, body.navigation).await?;
            Ok(())
        })
        .await?;
    }
    Ok(Json(site_out(&state, &actor, site).await?))
}

/// `DELETE /api/sites/{id}/hub`: unregisters the hub; its member sites leave it (see
/// [`sites::unregister_hub`]). Administrators only (`403`, `403 archived` on an archived
/// site); `404` for anything but a live site; `409 not_a_hub`. Answers the site as
/// `GET /api/sites/{id}`.
pub(crate) async fn unregister_hub(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let site = node_id(&id)?;
    let actor = signed.session.user;
    {
        let state = state.clone();
        let actor = actor.clone();
        to_completion(async move {
            sites::unregister_hub(&state, &actor, site).await?;
            Ok(())
        })
        .await?;
    }
    Ok(Json(site_out(&state, &actor, site).await?))
}

/// `PUT /api/sites/{id}/joined-hub {hub: id | null}`: joins the site to a hub, or leaves the
/// one it joined (see [`sites::join_hub`]). Needs `manage_site` on the site (`403`, `403
/// archived` on an archived site) and `view` on the hub (`404`); `409 is_hub` when the site is
/// a hub, `409 not_a_hub` when the other site is not one. Answers the site as
/// `GET /api/sites/{id}`.
pub(crate) async fn join_hub(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<JoinBody>,
) -> Result<Json<Value>, ApiError> {
    let site = node_id(&id)?;
    let hub = body.hub.as_deref().map(node_id).transpose()?;
    let actor = signed.session.user;
    {
        let state = state.clone();
        let actor = actor.clone();
        to_completion(async move {
            sites::join_hub(&state, &actor, site, hub).await?;
            Ok(())
        })
        .await?;
    }
    Ok(Json(site_out(&state, &actor, site).await?))
}

/// `GET /api/hubs`: the hubs the caller can see, by name, each with its description; an
/// archived hub only to people who manage it.
pub(crate) async fn hubs(
    State(state): State<AppState>,
    signed: Signed,
) -> Result<Json<Vec<HubOut>>, ApiError> {
    let sites = visible_sites(state.db(), &signed.session.user).await?;
    Ok(Json(
        sites
            .into_iter()
            .filter(|(_, access, settings)| {
                settings.is_hub && (settings.archived_at.is_none() || manages(access.as_ref()))
            })
            .map(|(node, _, settings)| HubOut {
                id: node.id.0.to_string(),
                name: node.name,
                description: settings.description,
            })
            .collect(),
    ))
}

/// `GET /api/hubs/{id}/sites`: the hub's member sites the caller can see, as the site
/// directory lists them. `404` when the caller cannot see the hub; `409 not_a_hub` for a site
/// that is not a hub.
pub(crate) async fn hub_sites(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
) -> Result<Json<Vec<SiteSummary>>, ApiError> {
    let hub = node_id(&id)?;
    let user = signed.session.user;
    let db = state.db();
    let sites = visible_sites(db, &user).await?;
    let (_, _, settings) = sites
        .iter()
        .find(|(node, _, _)| node.id == hub)
        .ok_or(ApiError::NotFound)?;
    if !settings.is_hub {
        return Err(ApiError::NotAHub);
    }
    Ok(Json(directory(db, &user, "", Some(hub), None).await?))
}

/// Whether `user` is an enabled administrator; a signed-in account is enabled.
fn admin(user: &User) -> bool {
    user.is_admin && !user.disabled
}

/// `items` resolved for `viewer` (see [`Db::resolve_navigation`]).
async fn resolved(
    db: &Db,
    viewer: &NavViewer,
    items: &[NavItem],
    manages: bool,
) -> Result<Vec<ResolvedItem>, ApiError> {
    db.resolve_navigation(viewer, items, manages)
        .await
        .map_err(|e| ApiError::unavailable(&e))
}

/// `items` as stored, for `editor`: each node target with the node's current name if they may
/// view it (`null` otherwise), and each audience group with its name if they may see it
/// (otherwise "A group you cannot see").
async fn for_editing(db: &Db, editor: &User, items: &[NavItem]) -> Result<Value, ApiError> {
    let nodes = db
        .visible_target_names(editor.id, items)
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
    let groups: HashMap<Uuid, String> = db
        .groups_visible_to(editor.id)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
        .into_iter()
        .map(|g| (g.id.0, g.name))
        .collect();
    fn one(item: &NavItem, nodes: &HashMap<Uuid, String>, groups: &HashMap<Uuid, String>) -> Value {
        let target = match &item.target {
            None => Value::Null,
            Some(NavTarget::Url(url)) => json!({ "url": url }),
            Some(NavTarget::Node(node)) => json!({ "node": node, "name": nodes.get(node) }),
        };
        let audience: Vec<Value> = item
            .audience
            .iter()
            .map(|g| {
                json!({
                    "id": g,
                    "name": groups.get(g).map_or(UNSEEN_GROUP, String::as_str),
                })
            })
            .collect();
        let children: Vec<Value> = item
            .children
            .iter()
            .map(|child| one(child, nodes, groups))
            .collect();
        json!({
            "id": item.id,
            "label": item.label,
            "target": target,
            "audience": audience,
            "children": children,
        })
    }
    Ok(Value::Array(
        items
            .iter()
            .map(|item| one(item, &nodes, &groups))
            .collect(),
    ))
}

/// `GET /api/navigation`: the global navigation resolved for the caller, `{items}`. With
/// `?edit=1`, for administrators only (`403`), the stored items for editing.
pub(crate) async fn global(
    State(state): State<AppState>,
    signed: Signed,
    Query(query): Query<NavQuery>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        global_out(&state, &signed.session.user, query.editing()).await?,
    ))
}

/// What `GET /api/navigation` answers `user` (see [`global`]).
async fn global_out(state: &AppState, user: &User, editing: bool) -> Result<Value, ApiError> {
    let db = state.db();
    let items = db
        .global_navigation()
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
    if editing {
        if !admin(user) {
            return Err(ApiError::Forbidden);
        }
        return Ok(json!({ "items": for_editing(db, user, &items).await? }));
    }
    let viewer = db
        .nav_viewer(user.id)
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
    let items = resolved(db, &viewer, &items, admin(user)).await?;
    Ok(json!({ "items": items }))
}

/// `PUT /api/navigation {items}`: sets the global navigation (see
/// [`sites::set_global_navigation`]). Administrators only (`403`); `422 invalid_navigation`
/// (`items` may not be `null`). Answers as `GET /api/navigation`.
pub(crate) async fn set_global(
    State(state): State<AppState>,
    signed: Signed,
    JsonBody(body): JsonBody<ItemsBody>,
) -> Result<Json<Value>, ApiError> {
    let actor = signed.session.user;
    let items = body.items.ok_or(ApiError::InvalidNavigation)?;
    {
        let state = state.clone();
        let actor = actor.clone();
        to_completion(async move {
            sites::set_global_navigation(&state, &actor, items).await?;
            Ok(())
        })
        .await?;
    }
    Ok(Json(global_out(&state, &actor, false).await?))
}

/// The hub bar shown on a site with `settings`: the site's own if it is a hub, otherwise the
/// bar of the hub it joined while that hub is a live hub; with the hub's id and name only if
/// `user` can see the hub (`null` for both otherwise).
async fn hub_bar(
    db: &Db,
    user: &User,
    viewer: &NavViewer,
    settings: &SiteSettings,
    seen: &HashMap<NodeId, String>,
) -> Result<Value, ApiError> {
    let unavailable = |e: records::DbError| ApiError::unavailable(&e);
    let hub = if settings.is_hub {
        Some(settings.clone())
    } else if let Some(joined) = settings.joined_hub {
        match db.site_state(joined).await.map_err(unavailable)? {
            Some(SiteState::Live) => db
                .site_settings(joined)
                .await
                .map_err(unavailable)?
                .filter(|hub| hub.is_hub),
            _ => None,
        }
    } else {
        None
    };
    let Some(hub) = hub else {
        return Ok(Value::Null);
    };
    let items = resolved(
        db,
        viewer,
        hub.hub_navigation.as_deref().unwrap_or_default(),
        admin(user),
    )
    .await?;
    let (id, name) = match seen.get(&hub.id) {
        Some(name) => (Some(hub.id.0.to_string()), Some(name.clone())),
        None => (None, None),
    };
    Ok(json!({ "id": id, "name": name, "items": items }))
}

/// `GET /api/sites/{id}/navigation`: the navigation shown on the site, resolved for the caller:
/// `{global, hub: {id, name, items} | null, local, automatic}`. `local` is the site's own
/// navigation, or with `automatic` every library of the site the caller can view. An item the
/// caller may not see is left out; one aimed at groups they are in none of is left out unless
/// they manage that navigation (`manage_site` on the site for the local one, administrators for
/// the hub and global ones), when it comes with `"targeted": true`. `404` when the caller cannot
/// see the site.
///
/// With `?edit=1`, the stored lists the caller may change, for editing (see [`for_editing`]):
/// `local` (`null` when automatic) for people with `manage_site`, `global` and `hub` (the
/// site's own bar, `null` if it is not a hub) for administrators; lists they may not change are
/// left out. `403` (`403 archived` on an archived site) when they may change none.
pub(crate) async fn site(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
    Query(query): Query<NavQuery>,
) -> Result<Json<Value>, ApiError> {
    let site = node_id(&id)?;
    Ok(Json(
        site_navigation(&state, &signed.session.user, site, query.editing()).await?,
    ))
}

/// What `GET /api/sites/{id}/navigation` answers `user` (see [`site`]).
async fn site_navigation(
    state: &AppState,
    user: &User,
    site: NodeId,
    editing: bool,
) -> Result<Value, ApiError> {
    let db = state.db();
    let unavailable = |e: records::DbError| ApiError::unavailable(&e);
    let sites = visible_sites(db, user).await?;
    let seen: HashMap<NodeId, String> = sites
        .iter()
        .map(|(node, _, _)| (node.id, node.name.clone()))
        .collect();
    let (_, access, settings) = sites
        .iter()
        .find(|(node, _, _)| node.id == site)
        .ok_or(ApiError::NotFound)?;
    if editing {
        let may_local = access
            .as_ref()
            .is_some_and(|a| a.perms.contains(Perms::MANAGE_SITE));
        if !may_local && !admin(user) {
            return Err(match access {
                Some(access) if manages(Some(access)) => access.refusal().into(),
                _ => ApiError::Forbidden,
            });
        }
        let mut out = serde_json::Map::new();
        out.insert("automatic".to_owned(), json!(settings.navigation.is_none()));
        if may_local {
            let local = match &settings.navigation {
                Some(items) => for_editing(db, user, items).await?,
                None => Value::Null,
            };
            out.insert("local".to_owned(), local);
        }
        if admin(user) {
            let global = db.global_navigation().await.map_err(unavailable)?;
            out.insert("global".to_owned(), for_editing(db, user, &global).await?);
            let hub = match &settings.hub_navigation {
                Some(items) if settings.is_hub => for_editing(db, user, items).await?,
                _ => Value::Null,
            };
            out.insert("hub".to_owned(), hub);
        }
        return Ok(Value::Object(out));
    }

    let viewer = db.nav_viewer(user.id).await.map_err(unavailable)?;
    let global = db.global_navigation().await.map_err(unavailable)?;
    let global = resolved(db, &viewer, &global, admin(user)).await?;
    let hub = hub_bar(db, user, &viewer, settings, &seen).await?;
    let local = match &settings.navigation {
        Some(items) => resolved(db, &viewer, items, manages(access.as_ref())).await?,
        None => db
            .automatic_navigation(user.id, site)
            .await
            .map_err(unavailable)?,
    };
    Ok(json!({
        "global": global,
        "hub": hub,
        "local": local,
        "automatic": settings.navigation.is_none(),
    }))
}

/// `PUT /api/sites/{id}/navigation {items | null}`: sets the site's own navigation, or with
/// `null` makes it automatic (see [`sites::set_navigation`]). Needs `manage_site` (`403`, `403
/// archived` on an archived site; `404` when the caller cannot see the site); `422
/// invalid_navigation`. Answers as `GET /api/sites/{id}/navigation`.
pub(crate) async fn set_site(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<ItemsBody>,
) -> Result<Json<Value>, ApiError> {
    let site = node_id(&id)?;
    let actor = signed.session.user;
    {
        let state = state.clone();
        let actor = actor.clone();
        to_completion(async move {
            sites::set_navigation(&state, &actor, site, body.items).await?;
            Ok(())
        })
        .await?;
    }
    Ok(Json(site_navigation(&state, &actor, site, false).await?))
}
