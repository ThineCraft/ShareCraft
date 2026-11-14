//! Sites, the top level of the content tree: the ones a person can see, one site with its
//! storage and quota, creating one, setting its quota, and a site's three groups.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use collab_core::ids::NodeId;
use records::{Access, Db, Group, NodeKind, Perms, User};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::groups::{GroupOut, answered};
use super::nodes::{NodeOut, node_id};
use super::{ApiError, JsonBody, to_completion};
use crate::AppState;
use crate::auth::Signed;
use crate::tree::{self, TreeOp};

/// One row of `GET /api/sites`.
#[derive(Serialize)]
pub(crate) struct SiteSummary {
    id: String,
    name: String,
    /// The caller's access: `{perms, roles}`. `None` for limited access: access only somewhere
    /// inside the site.
    access: Option<Access>,
}

/// `GET /api/sites`: the sites the caller can see, by name.
pub(crate) async fn list(
    State(state): State<AppState>,
    signed: Signed,
) -> Result<Json<Vec<SiteSummary>>, ApiError> {
    let sites = state
        .db()
        .sites_for(signed.session.user.id)
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
    Ok(Json(
        sites
            .into_iter()
            .map(|(site, access)| SiteSummary {
                id: site.id.0.to_string(),
                name: site.name,
                access,
            })
            .collect(),
    ))
}

/// The body of `POST /api/sites`.
#[derive(Deserialize)]
pub(crate) struct NewSite {
    name: String,
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

/// `POST /api/sites {name}`: creates a site with a library named Documents and its three
/// groups, Owners, Members and Visitors, assigned Owner, Editor and Viewer on it; the creator
/// becomes a member of its Owners group. Administrators only. `201` with the site and its
/// groups.
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
            let op = TreeOp::Create {
                parent: None,
                kind: NodeKind::Site,
                name: new.name,
                id: NodeId(Uuid::new_v4()),
            };
            Ok(tree::apply(&state, &actor, op).await?.node)
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

/// `GET /api/sites/{id}`: the site as `GET /api/sites` lists it, and for people who may manage
/// it, its storage: `used` (bytes held by every version of every item, live and in its bins),
/// `quota` (`null` for no limit) and `quota_warning` (from 90% of the quota, for the site page
/// to warn its owners). `404` when the caller may see nothing of it, or there is no such site.
pub(crate) async fn show(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let site = node_id(&id)?;
    let db = state.db();
    let (node, access) = db
        .sites_for(signed.session.user.id)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
        .into_iter()
        .find(|(node, _)| node.id == site)
        .ok_or(ApiError::NotFound)?;
    let manages = access
        .as_ref()
        .is_some_and(|a| a.perms.contains(Perms::MANAGE_SITE));
    let id = node.id.0.to_string();
    if !manages {
        return Ok(Json(
            serde_json::json!({ "id": id, "name": node.name, "access": access }),
        ));
    }
    let usage = db
        .site_usage(site)
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
    Ok(Json(serde_json::json!({
        "id": id,
        "name": node.name,
        "access": access,
        "used": usage.used,
        "quota": usage.quota,
        "quota_warning": usage.near_quota(),
    })))
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
        return Err(ApiError::Forbidden);
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
