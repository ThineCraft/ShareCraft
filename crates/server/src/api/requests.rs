//! Requests for access: asking for an item one cannot open, the open requests a manager may
//! decide, approving or declining one, and one's own decided requests.
//!
//! Asking never says whether the item exists: every submission that passes the daily limit and
//! the message length gets the same answer.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use records::{AccessRequest, MyRequest, RequestDecision, RoleRef};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::explorer::UserOut;
use super::nodes::{CrumbOut, node_id, rfc3339};
use super::{ApiError, JsonBody, parse_id, recheck_below, to_completion};
use crate::AppState;
use crate::auth::Signed;

/// What every accepted request for access is told, whether or not the item exists.
pub(crate) const REQUEST_SENT: &str =
    "If this item exists, the people who manage it will see your request.";

/// The body of `POST /api/nodes/{id}/requests`.
#[derive(Deserialize)]
pub(crate) struct NewRequest {
    /// At most 500 characters.
    message: Option<String>,
}

/// `POST /api/nodes/{id}/requests {message?}`: asks for access to the node. `202` with
/// `{message}` (always the same) whether the node exists, does not, or is one the caller can
/// already open; `429 too_many_requests` after 20 submissions in a day (checked before
/// anything about the node); `400 message_too_long` over 500 characters.
pub(crate) async fn create(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
    JsonBody(new): JsonBody<NewRequest>,
) -> Result<Response, ApiError> {
    let node = node_id(&id)?;
    state
        .db()
        .request_access(signed.session.user.id, node, new.message.as_deref())
        .await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "message": REQUEST_SENT })),
    )
        .into_response())
}

/// An open request as the people who may decide it see it.
#[derive(Serialize)]
pub(crate) struct OpenOut {
    id: String,
    /// The item, with the caller's access to it.
    node: CrumbOut,
    /// From its site down to its parent.
    path: Vec<CrumbOut>,
    requester: UserOut,
    message: Option<String>,
    created_at: Option<String>,
}

impl From<AccessRequest> for OpenOut {
    fn from(request: AccessRequest) -> Self {
        OpenOut {
            id: request.id.to_string(),
            node: request.node.into(),
            path: request.path.into_iter().map(CrumbOut::from).collect(),
            requester: (&request.requester).into(),
            message: request.message,
            created_at: rfc3339(Some(request.created_at)),
        }
    }
}

/// `GET /api/requests`: the open requests the caller may decide (on items where they have
/// `manage_access`), newest first.
pub(crate) async fn open(
    State(state): State<AppState>,
    signed: Signed,
) -> Result<Json<Vec<OpenOut>>, ApiError> {
    let open = state
        .db()
        .open_requests(signed.session.user.id)
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
    Ok(Json(open.into_iter().map(OpenOut::from).collect()))
}

/// One of the caller's own decided requests.
#[derive(Serialize)]
pub(crate) struct MineOut {
    id: String,
    /// The item, only while the caller can see it.
    node: Option<CrumbOut>,
    path: Vec<CrumbOut>,
    /// `approved` or `declined`.
    decision: &'static str,
    role: Option<RoleRef>,
    reply: Option<String>,
    created_at: Option<String>,
    decided_at: Option<String>,
}

impl From<MyRequest> for MineOut {
    fn from(request: MyRequest) -> Self {
        MineOut {
            id: request.id.to_string(),
            node: request.node.map(CrumbOut::from),
            path: request.path.into_iter().map(CrumbOut::from).collect(),
            decision: match request.decision {
                RequestDecision::Approved => "approved",
                RequestDecision::Declined => "declined",
            },
            role: request.role,
            reply: request.reply,
            created_at: rfc3339(Some(request.created_at)),
            decided_at: rfc3339(Some(request.decided_at)),
        }
    }
}

/// `GET /api/requests/mine`: the caller's own requests that someone decided, most recent
/// first. Open requests are never listed: only a decision says that an item exists.
pub(crate) async fn mine(
    State(state): State<AppState>,
    signed: Signed,
) -> Result<Json<Vec<MineOut>>, ApiError> {
    let mine = state
        .db()
        .my_requests(signed.session.user.id)
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
    Ok(Json(mine.into_iter().map(MineOut::from).collect()))
}

/// The body of `POST /api/requests/{id}/approve`.
#[derive(Deserialize)]
pub(crate) struct Approval {
    /// A role id.
    role: String,
}

/// `POST /api/requests/{id}/approve {role}`: gives the requester `role` directly on the item.
/// Needs `manage_access` there (`404` for a request one cannot see, `403`); the role must be
/// within the caller's own access (`403 beyond_your_access`); `409 already_has` when the
/// requester's access already covers it (never a downgrade; the request stays open); `409
/// already_decided`. Open documents below the item are rechecked before the answer. `204`.
pub(crate) async fn approve(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
    JsonBody(approval): JsonBody<Approval>,
) -> Result<StatusCode, ApiError> {
    let id = parse_id(&id)?;
    let actor = signed.session.user.id;
    to_completion(async move {
        let db = state.db();
        db.approve_request(actor, id, &approval.role).await?;
        let node = db
            .request_node(id)
            .await
            .map_err(|e| ApiError::unavailable(&e))?;
        if let Some(node) = node {
            recheck_below(&state, node).await;
        }
        Ok(())
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The body of `POST /api/requests/{id}/decline`.
#[derive(Deserialize)]
pub(crate) struct Decline {
    /// At most 500 characters; the requester sees it.
    reply: Option<String>,
}

/// `POST /api/requests/{id}/decline {reply?}`: declines the request. Needs `manage_access` on
/// the item (`404`, `403`); `409 already_decided`; `400 message_too_long`. `204`.
pub(crate) async fn decline(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
    JsonBody(decline): JsonBody<Decline>,
) -> Result<StatusCode, ApiError> {
    let id = parse_id(&id)?;
    state
        .db()
        .decline_request(signed.session.user.id, id, decline.reply.as_deref())
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
