//! Many items moved, copied, deleted or restored in one request.
//!
//! `POST /api/bulk {op, items, parent?}` takes up to [`MAX_ITEMS`] items. Each is one operation
//! through the same path as the single-item route (`PATCH /api/nodes/{id}` for a move,
//! `POST /api/nodes/{id}/copy`, `DELETE /api/nodes/{id}`, `POST /api/bin/{id}/restore`), with
//! its own result, in the order given: one failing does not stop the others.

use axum::Json;
use axum::extract::State;
use collab_core::ids::NodeId;
use serde::{Deserialize, Serialize};

use super::nodes::{copy_one, delete_one, move_one, node_id};
use super::{ApiError, JsonBody, to_completion};
use crate::AppState;
use crate::auth::Signed;
use crate::tree::{self, TreeOp};

/// Most items one request may name.
pub(crate) const MAX_ITEMS: usize = 500;
/// Largest body accepted: [`MAX_ITEMS`] identifiers with room to spare.
pub(crate) const MAX_BODY: usize = 64 * 1024;

/// What a bulk request does to each item.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum BulkOp {
    Move,
    Copy,
    Delete,
    Restore,
}

/// The body of `POST /api/bulk`.
#[derive(Deserialize)]
pub(crate) struct BulkBody {
    op: BulkOp,
    /// The items, by id.
    items: Vec<String>,
    /// Where a move or copy goes: a library or folder.
    parent: Option<String>,
}

/// One item's result.
#[derive(Serialize)]
pub(crate) struct ItemResult {
    /// The item, as given.
    id: String,
    ok: bool,
    /// The error code, as the single-item route would answer it.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'static str>,
    /// For a copy, the new item.
    #[serde(skip_serializing_if = "Option::is_none")]
    new_id: Option<String>,
}

/// `POST /api/bulk {op: "move" | "copy" | "delete" | "restore", items: [ids], parent?}`: each
/// item in turn, as the single-item route does it. `200 [{id, ok, error?, new_id?}]`, in the
/// order given. `422 invalid_request` for more than 500 items, or a move or copy without a
/// `parent` (or with one that is not an id); nothing is done then.
pub(crate) async fn run(
    State(state): State<AppState>,
    signed: Signed,
    JsonBody(body): JsonBody<BulkBody>,
) -> Result<Json<Vec<ItemResult>>, ApiError> {
    if body.items.len() > MAX_ITEMS {
        return Err(ApiError::InvalidRequest);
    }
    let parent = match (body.op, body.parent.as_deref()) {
        (BulkOp::Move | BulkOp::Copy, Some(parent)) => Some(node_id(parent)?),
        (BulkOp::Move | BulkOp::Copy, None) => return Err(ApiError::InvalidRequest),
        (BulkOp::Delete | BulkOp::Restore, _) => None,
    };
    let actor = signed.session.user;
    let op = body.op;
    let results = to_completion(async move {
        let mut results = Vec::with_capacity(body.items.len());
        for item in body.items {
            let done = match node_id(&item) {
                Ok(id) => one(&state, &actor, op, id, parent).await,
                Err(e) => Err(e),
            };
            results.push(match done {
                Ok(new_id) => ItemResult {
                    id: item,
                    ok: true,
                    error: None,
                    new_id,
                },
                Err(e) => ItemResult {
                    id: item,
                    ok: false,
                    error: Some(e.code()),
                    new_id: None,
                },
            });
        }
        Ok(results)
    })
    .await?;
    Ok(Json(results))
}

/// Does `op` to `id` for `actor`; for a copy, answers the new item's id.
async fn one(
    state: &AppState,
    actor: &records::User,
    op: BulkOp,
    id: NodeId,
    parent: Option<NodeId>,
) -> Result<Option<String>, ApiError> {
    match (op, parent) {
        (BulkOp::Move, Some(parent)) => move_one(state, actor, id, parent).await.map(|_| None),
        (BulkOp::Copy, Some(parent)) => copy_one(state, actor, id, parent, None)
            .await
            .map(|copied| Some(copied.id().to_owned())),
        (BulkOp::Delete, _) => delete_one(state, actor, id).await.map(|()| None),
        (BulkOp::Restore, _) => tree::apply(state, actor, TreeOp::Restore { id })
            .await
            .map(|_| None)
            .map_err(ApiError::from),
        (BulkOp::Move | BulkOp::Copy, None) => Err(ApiError::InvalidRequest),
    }
}
