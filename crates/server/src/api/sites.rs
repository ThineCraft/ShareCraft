//! Sites, the top level of the content tree: the ones a person can see, creating one, and a
//! site's three groups.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use collab_core::ids::NodeId;
use records::{Access, Db, Group, NodeKind, User};
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
            Ok(tree::apply(&state, &actor, op).await?)
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
