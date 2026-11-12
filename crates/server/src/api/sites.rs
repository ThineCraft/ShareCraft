//! Sites, the top level of the content tree: the ones a person can see, and creating one.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use collab_core::ids::NodeId;
use records::NodeKind;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::nodes::NodeOut;
use super::{ApiError, JsonBody, LegacyRole, to_completion};
use crate::AppState;
use crate::auth::Signed;
use crate::tree::{self, TreeOp};

/// One row of `GET /api/sites`.
#[derive(Serialize)]
pub(crate) struct SiteSummary {
    id: String,
    name: String,
    /// `None` for limited access: access only somewhere inside the site.
    role: Option<LegacyRole>,
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
                role: access.as_ref().map(LegacyRole::of),
            })
            .collect(),
    ))
}

/// The body of `POST /api/sites`.
#[derive(Deserialize)]
pub(crate) struct NewSite {
    name: String,
}

/// `POST /api/sites {name}`: creates a site with a library named Documents. Administrators
/// only; the creator becomes the site's owner. `201` with the site.
pub(crate) async fn create(
    State(state): State<AppState>,
    signed: Signed,
    JsonBody(new): JsonBody<NewSite>,
) -> Result<Response, ApiError> {
    let actor = signed.session.user;
    let site = to_completion(async move {
        let op = TreeOp::Create {
            parent: None,
            kind: NodeKind::Site,
            name: new.name,
            id: NodeId(Uuid::new_v4()),
        };
        Ok(tree::apply(&state, &actor, op).await?)
    })
    .await?;
    Ok((StatusCode::CREATED, Json(NodeOut::from(site))).into_response())
}
