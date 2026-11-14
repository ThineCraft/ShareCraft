//! Sites, the top level of the content tree: the site directory, one site with its settings,
//! owners, storage and quota, creating one, changing its settings, setting its quota,
//! archiving and reactivating it, and a site's three groups.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use collab_core::ids::NodeId;
use records::{Access, Db, Group, Node, Perms, SiteKind, SiteSettings, User};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use super::groups::{GroupOut, answered};
use super::nodes::{NodeOut, node_id};
use super::{ApiError, JsonBody, to_completion};
use crate::AppState;
use crate::auth::Signed;
use crate::sites::{self, SiteEdit};
use crate::tree;

/// A site named in an answer: `{id, name}`.
#[derive(Clone, Serialize)]
pub(crate) struct SiteName {
    id: String,
    name: String,
}

/// One row of `GET /api/sites`.
#[derive(Serialize)]
pub(crate) struct SiteSummary {
    id: String,
    name: String,
    /// The caller's access: `{perms, roles}`. `None` for limited access: access only somewhere
    /// inside the site.
    access: Option<Access>,
    /// `team` or `communication`.
    kind: &'static str,
    description: String,
    /// The hub the site joined, if the caller can see it.
    hub: Option<SiteName>,
    is_hub: bool,
    /// Whether it is archived; archived sites are listed only to people who manage them.
    archived: bool,
}

/// The query of `GET /api/sites`.
#[derive(Deserialize)]
pub(crate) struct DirectoryQuery {
    /// Text the site's name contains, ignoring case.
    q: Option<String>,
    /// Only the sites that joined this hub.
    hub: Option<String>,
    /// Only sites of this kind: `team` or `communication`.
    kind: Option<String>,
}

/// The sites `user` can see, each with its access and settings (the defaults of a site with
/// no row).
pub(crate) async fn visible_sites(
    db: &Db,
    user: &User,
) -> Result<Vec<(Node, Option<Access>, SiteSettings)>, ApiError> {
    let sites = db
        .sites_for(user.id)
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
    let mut settings: HashMap<NodeId, SiteSettings> = db
        .all_site_settings()
        .await
        .map_err(|e| ApiError::unavailable(&e))?
        .into_iter()
        .map(|row| (row.id, row))
        .collect();
    let defaults = db
        .regional_defaults()
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
    Ok(sites
        .into_iter()
        .map(|(node, access)| {
            let row = settings.remove(&node.id).unwrap_or_else(|| {
                SiteSettings::defaults(node.id, &defaults.language, &defaults.time_zone)
            });
            (node, access, row)
        })
        .collect())
}

/// Whether `access` includes `manage_site`, or on an archived site would include it were the
/// site not archived: who sees an archived site in the directory and a site's storage. Seeing
/// is not changing; nobody holds `manage_site` on an archived site.
pub(crate) fn manages(access: Option<&Access>) -> bool {
    access.is_some_and(|a| a.ignoring_archive().contains(Perms::MANAGE_SITE))
}

/// `GET /api/sites?q=&hub=&kind=`: the site directory, by name: the sites the caller can see,
/// each with its kind, description, the hub it joined (only if the caller can see that hub)
/// and whether it is a hub. An archived site is listed, marked, only to people who manage it.
/// `q` keeps the sites whose name contains it, ignoring case; `hub` those that joined that
/// hub (none at all when the caller cannot see the hub); `kind` those of that kind (`422 invalid_request` for another kind or a hub that is
/// not an id).
pub(crate) async fn list(
    State(state): State<AppState>,
    signed: Signed,
    Query(query): Query<DirectoryQuery>,
) -> Result<Json<Vec<SiteSummary>>, ApiError> {
    let kind = query
        .kind
        .as_deref()
        .filter(|kind| !kind.is_empty())
        .map(|kind| SiteKind::parse(kind).ok_or(ApiError::InvalidRequest))
        .transpose()?;
    let hub = query
        .hub
        .as_deref()
        .filter(|hub| !hub.is_empty())
        .map(node_id)
        .transpose()?;
    let needle = query
        .q
        .as_deref()
        .map(str::to_lowercase)
        .unwrap_or_default();
    Ok(Json(
        directory(state.db(), &signed.session.user, &needle, hub, kind).await?,
    ))
}

/// The site directory for `user` (see [`list`]): the sites they can see whose name contains
/// `needle` (lower-cased), that joined `hub` if given (none when they cannot see the hub), of
/// `kind` if given; an archived site only to people who manage it.
pub(crate) async fn directory(
    db: &Db,
    user: &User,
    needle: &str,
    hub: Option<NodeId>,
    kind: Option<SiteKind>,
) -> Result<Vec<SiteSummary>, ApiError> {
    let sites = visible_sites(db, user).await?;
    let names: HashMap<NodeId, String> = sites
        .iter()
        .map(|(node, _, _)| (node.id, node.name.clone()))
        .collect();
    // A hub the caller cannot see is not confirmed by listing the sites that joined it.
    if hub.is_some_and(|hub| !names.contains_key(&hub)) {
        return Ok(Vec::new());
    }
    Ok(sites
        .into_iter()
        .filter(|(node, access, settings)| {
            (settings.archived_at.is_none() || manages(access.as_ref()))
                && node.name.to_lowercase().contains(needle)
                && kind.is_none_or(|kind| settings.kind == kind)
                && hub.is_none_or(|hub| settings.joined_hub == Some(hub))
        })
        .map(|(node, access, settings)| SiteSummary {
            id: node.id.0.to_string(),
            hub: settings.joined_hub.and_then(|hub| {
                names.get(&hub).map(|name| SiteName {
                    id: hub.0.to_string(),
                    name: name.clone(),
                })
            }),
            name: node.name,
            access,
            kind: settings.kind.as_str(),
            description: settings.description,
            is_hub: settings.is_hub,
            archived: settings.archived_at.is_some(),
        })
        .collect())
}

/// The body of `POST /api/sites`.
#[derive(Deserialize)]
pub(crate) struct NewSite {
    name: String,
    /// A built-in template's name or a saved template's id; `team` when absent.
    template: Option<String>,
    /// Given fields override the template's.
    description: Option<String>,
    language: Option<String>,
    time_zone: Option<String>,
}

/// `POST /api/sites`: the new site, with its groups.
#[derive(Serialize)]
pub(crate) struct NewSiteOut {
    #[serde(flatten)]
    site: NodeOut,
    /// Its Owners, Members and Visitors groups.
    groups: Vec<GroupOut>,
}

/// The groups of `site` that `user` may see, Owners, Members and Visitors in that order.
async fn site_groups(db: &Db, user: &User, site: NodeId) -> Result<Vec<GroupOut>, ApiError> {
    let mut groups: Vec<Group> = db
        .groups_visible_to(user.id)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
        .into_iter()
        .filter(|group| group.site == Some(site))
        .collect();
    groups.sort_by_key(|group| group.purpose);
    answered(db, user, groups).await
}

/// `POST /api/sites {name, template?, description?, language?, time_zone?}`: creates a site
/// from a template (`team` when none is given; a built-in's name or a saved template's id),
/// with the template's libraries, folders, kind, language, time zone and navigation, the
/// fields given overriding the template's, and its three groups, Owners, Members and Visitors,
/// assigned Owner, Editor and Viewer on it; the creator becomes a member of its Owners group
/// (see [`tree::create_site`]). Administrators only (`403`). `422 invalid_request` for a
/// template that is neither a built-in nor an id, or a description over 500 characters, `404`
/// for an unknown or deleted template, `422 invalid_language` or `invalid_time_zone`. `201`
/// with the site and its groups.
pub(crate) async fn create(
    State(state): State<AppState>,
    signed: Signed,
    JsonBody(new): JsonBody<NewSite>,
) -> Result<Response, ApiError> {
    let actor = signed.session.user;
    let creator = actor.clone();
    let site = {
        let state = state.clone();
        to_completion(async move {
            let mut shape =
                crate::templates::shape_for(&state, &actor, new.template.as_deref()).await?;
            if let Some(description) = new.description {
                shape.description = description;
            }
            if let Some(language) = new.language {
                shape.language = language;
            }
            if let Some(time_zone) = new.time_zone {
                shape.time_zone = time_zone;
            }
            let id = NodeId(Uuid::new_v4());
            Ok(tree::create_site(&state, &actor, id, &new.name, shape)
                .await?
                .node)
        })
        .await?
    };
    let groups = site_groups(state.db(), &creator, site.id).await?;
    let out = NewSiteOut {
        site: NodeOut::from(site),
        groups,
    };
    Ok((StatusCode::CREATED, Json(out)).into_response())
}

/// `GET /api/sites/{id}/groups`: the site's Owners, Members and Visitors groups, for its Groups
/// page, each saying whether the caller may change its members. Decided by the same rule as
/// `GET /api/groups` (whoever may see a site's groups there sees them here): `404` when the
/// caller may see none of them, or there is no such site.
pub(crate) async fn groups(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
) -> Result<Json<Vec<GroupOut>>, ApiError> {
    let site = node_id(&id)?;
    let user = signed.session.user;
    let groups = site_groups(state.db(), &user, site).await?;
    if groups.is_empty() {
        return Err(ApiError::NotFound);
    }
    Ok(Json(groups))
}

/// `GET /api/sites/{id}`: the site as `GET /api/sites` lists it, with its `kind`,
/// `description`, `language`, `time_zone`, `is_hub`, `hub` (`{id, name}` of the hub it joined,
/// if the caller can see it), `archived` (`{at, by}`, or `null`) and `owners` (the names of
/// the enabled members of its Owners group and of the people assigned Owner on it), and for
/// people who may manage it, its storage: `used` (bytes held by every version of every item,
/// live and in its bins), `quota` (`null` for no limit) and `quota_warning` (from 90% of the
/// quota, for the site page to warn its owners). `404` when the caller may see nothing of it,
/// or there is no such site.
pub(crate) async fn show(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let site = node_id(&id)?;
    Ok(Json(site_out(&state, &signed.session.user, site).await?))
}

/// The site `site` as `GET /api/sites/{id}` answers it for `user`.
pub(crate) async fn site_out(
    state: &AppState,
    user: &User,
    site: NodeId,
) -> Result<Value, ApiError> {
    let db = state.db();
    let sites = visible_sites(db, user).await?;
    let hub_name = |hub: NodeId| {
        sites
            .iter()
            .find(|(node, _, _)| node.id == hub)
            .map(|(node, _, _)| json!({ "id": hub.0.to_string(), "name": node.name }))
    };
    let (node, access, settings) = sites
        .iter()
        .find(|(node, _, _)| node.id == site)
        .ok_or(ApiError::NotFound)?;
    let owners: Vec<String> = db
        .site_owners(site)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
        .into_iter()
        .map(|(_, name)| name)
        .collect();
    let archived = match settings.archived_at {
        None => Value::Null,
        Some(at) => {
            let by = match settings.archived_by {
                Some(by) => db
                    .user(by)
                    .await
                    .map_err(|e| ApiError::unavailable(&e))?
                    .map(|u| u.name),
                None => None,
            };
            json!({ "at": at.format(&Rfc3339).unwrap_or_default(), "by": by })
        }
    };
    let mut out = json!({
        "id": node.id.0.to_string(),
        "name": node.name,
        "access": access,
        "kind": settings.kind.as_str(),
        "description": settings.description,
        "language": settings.language,
        "time_zone": settings.time_zone,
        "is_hub": settings.is_hub,
        "hub": settings.joined_hub.and_then(hub_name),
        "archived": archived,
        "owners": owners,
    });
    if manages(access.as_ref()) {
        let usage = db
            .site_usage(site)
            .await
            .map_err(|e| ApiError::unavailable(&e))?;
        if let Some(fields) = out.as_object_mut() {
            fields.insert("used".to_owned(), json!(usage.used));
            fields.insert("quota".to_owned(), json!(usage.quota));
            fields.insert("quota_warning".to_owned(), json!(usage.near_quota()));
        }
    }
    Ok(out)
}

/// The body of `PATCH /api/sites/{id}`: the settings to change.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SiteChangeBody {
    name: Option<String>,
    description: Option<String>,
    language: Option<String>,
    time_zone: Option<String>,
}

/// `PATCH /api/sites/{id} {name?, description?, language?, time_zone?}`: changes the site's
/// settings (see [`sites::change`]), for people with `manage_site` on it (`403`; `404` when
/// they cannot see it). One commit, recorded as `site.changed`. `422 invalid_name`,
/// `invalid_request` (a description over 500 characters), `invalid_language` or
/// `invalid_time_zone` for a value, `409 name_taken` for another site's name. Answers the
/// site as `GET /api/sites/{id}` does.
pub(crate) async fn change(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<SiteChangeBody>,
) -> Result<Json<Value>, ApiError> {
    let site = node_id(&id)?;
    let actor = signed.session.user;
    let edit = SiteEdit {
        name: body.name,
        description: body.description,
        language: body.language,
        time_zone: body.time_zone,
    };
    {
        let state = state.clone();
        let actor = actor.clone();
        to_completion(async move {
            sites::change(&state, &actor, site, edit).await?;
            Ok(())
        })
        .await?;
    }
    Ok(Json(site_out(&state, &actor, site).await?))
}

/// The body of `PUT /api/sites/{id}/quota`.
#[derive(Deserialize)]
pub(crate) struct QuotaBody {
    /// The most the site may hold, in bytes; `null` for no limit.
    bytes: Option<u64>,
}

/// `PUT /api/sites/{id}/quota {bytes | null}`: sets the site's storage quota. Administrators
/// only (`403`); `404` for anything but a site. A quota below what the site holds is allowed:
/// the site then takes nothing more until space is freed. Records `quota.changed`. `204`.
pub(crate) async fn set_quota(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<QuotaBody>,
) -> Result<Response, ApiError> {
    let site = node_id(&id)?;
    let actor = signed.session.user;
    if !actor.is_admin {
        return Err(refused_on(state.db(), &actor, site).await?);
    }
    to_completion(async move {
        state
            .db()
            .set_site_quota(actor.id, site, body.bytes)
            .await?;
        Ok(StatusCode::NO_CONTENT.into_response())
    })
    .await
}

/// The refusal of a route only administrators may use, for `user`, who is not one: `403
/// archived` when they can see `site` and it is archived (so the page can say why nothing
/// changes), `403 forbidden` otherwise.
async fn refused_on(db: &Db, user: &User, site: NodeId) -> Result<ApiError, ApiError> {
    let access = db
        .effective_access(user.id, site)
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
    Ok(match access {
        Some(access) => access.refusal().into(),
        None => ApiError::Forbidden,
    })
}

/// `POST /api/sites/{id}/archive`: archives the site (see [`sites::archive`]): read-only for
/// everyone, administrators included, hidden from the directory but for its managers, its open
/// editing sessions ended (`archived`), its uploads abandoned and its anyone links answering as
/// expired ones do, until it is reactivated. Administrators only (`403`); `404` for anything but
/// a live site. One commit, recorded as `site.archived`; archiving an archived site changes
/// nothing. Answers the site as `GET /api/sites/{id}` does.
pub(crate) async fn archive(
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
            sites::archive(&state, &actor, site).await?;
            Ok(())
        })
        .await?;
    }
    Ok(Json(site_out(&state, &actor, site).await?))
}

/// `DELETE /api/sites/{id}`: deletes the site (see [`sites::delete`]). Administrators only
/// (`403`, before anything else); `409 site_deleted` for a deleted site; `404` for anything but
/// a site. One commit, recorded as `site.deleted`; its editing sessions end and its uploads are
/// abandoned. Everything in it then answers `404` to everyone; administrators restore or purge
/// it from `/api/admin/sites`. Answers `204`.
pub(crate) async fn remove(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let site = node_id(&id)?;
    let actor = signed.session.user;
    to_completion(async move {
        sites::delete(&state, &actor, site).await?;
        Ok(())
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/sites/{id}/reactivate`: makes the archived site writable again, with every
/// permission as it was (see [`sites::reactivate`]). Administrators only (`403`); `404` for
/// anything but a live site. One commit, recorded as `site.reactivated`; reactivating a site
/// that is not archived changes nothing. Answers the site as `GET /api/sites/{id}` does.
pub(crate) async fn reactivate(
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
            sites::reactivate(&state, &actor, site).await?;
            Ok(())
        })
        .await?;
    }
    Ok(Json(site_out(&state, &actor, site).await?))
}
