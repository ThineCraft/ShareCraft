//! Many items moved, copied, deleted or restored in one request.
//!
//! `POST /api/bulk {op, items, parent?}` takes up to [`MAX_ITEMS`] items. Each is one operation
//! through the same path as the single-item route (`PATCH /api/nodes/{id}` for a move,
//! `POST /api/nodes/{id}/copy`, `DELETE /api/nodes/{id}`, `POST /api/bin/{id}/restore`), with
//! its own result, in the order given: one failing does not stop the others. An item in, or
//! going into, a library connected to a source is refused as the single-item route refuses it
//! (`source_read_only`). A move inside a read-and-write S3 library goes through the bucket, as
//! the single-item route does, taking the library's source lock once for each item.

use axum::Json;
use axum::extract::State;
use collab_core::ids::NodeId;
use records::{FileType, ScanState};
use serde::{Deserialize, Serialize};

use super::nodes::{
    copy_one, delete_checked, move_one, node_id, require_not_connected_for, restore_checked,
    restructure_place,
};
use super::{ApiError, JsonBody, to_completion};
use crate::AppState;
use crate::auth::Signed;
use crate::source_write::rename_or_move;

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
    /// For `blocked_file_type`, the type refused.
    #[serde(skip_serializing_if = "Option::is_none")]
    extension: Option<FileType>,
    /// For `file_too_large`, the library's limit in bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    limit: Option<u64>,
    /// For `held`, the scan state that holds the file's content.
    #[serde(skip_serializing_if = "Option::is_none")]
    scan: Option<ScanState>,
    /// For a copy, the new item.
    #[serde(skip_serializing_if = "Option::is_none")]
    new_id: Option<String>,
    /// For a copy or a restore whose name had to change, the name before and after.
    #[serde(skip_serializing_if = "Option::is_none")]
    renamed: Option<Renamed>,
    /// For a move in a bucket, the originals that could not be deleted. Absent when none.
    #[serde(skip_serializing_if = "Option::is_none")]
    left_behind: Option<u64>,
}

/// A name that had to change.
#[derive(Serialize)]
pub(crate) struct Renamed {
    from: String,
    to: String,
}

/// What one item's operation made: a copy's new id, and a name that had to change.
#[derive(Default)]
struct Done {
    new_id: Option<String>,
    renamed: Option<Renamed>,
    left_behind: Option<u64>,
}

/// `POST /api/bulk {op: "move" | "copy" | "delete" | "restore", items: [ids], parent?}`: each
/// item in turn, as the single-item route does it, the file policies of where it goes
/// included. `200 [{id, ok, error?, extension?, limit?, new_id?, renamed?}]` (`extension` for
/// `blocked_file_type`, `limit` for `file_too_large`)
/// (`renamed {from, to}` when a copy's or a restored item's name had to change), in the
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
        // Where a move or copy goes is judged once for every item: into a connected library,
        // each is refused as the single-item route refuses it.
        let into = match parent {
            Some(parent) => require_not_connected_for(state.db(), actor.id, parent)
                .await
                .err(),
            None => None,
        };
        for item in body.items {
            let done = match node_id(&item) {
                Ok(id) => one(&state, &actor, op, id, parent, into).await,
                Err(e) => Err(e),
            };
            results.push(match done {
                Ok(done) => ItemResult {
                    id: item,
                    ok: true,
                    error: None,
                    extension: None,
                    limit: None,
                    scan: None,
                    new_id: done.new_id,
                    renamed: done.renamed,
                    left_behind: done.left_behind,
                },
                Err(e) => ItemResult {
                    id: item,
                    ok: false,
                    error: Some(e.code()),
                    extension: match e {
                        ApiError::BlockedFileType(file_type) => Some(file_type),
                        _ => None,
                    },
                    limit: match e {
                        ApiError::FileTooLarge(limit) => Some(limit),
                        _ => None,
                    },
                    scan: match e {
                        ApiError::Held(scan) => Some(scan),
                        _ => None,
                    },
                    new_id: None,
                    renamed: None,
                    left_behind: None,
                },
            });
        }
        Ok(results)
    })
    .await?;
    Ok(Json(results))
}

/// Does `op` to `id` for `actor`. `into` is the refusal of `parent`, judged once by the caller
/// (`source_read_only` for a connected library).
async fn one(
    state: &AppState,
    actor: &records::User,
    op: BulkOp,
    id: NodeId,
    parent: Option<NodeId>,
    into: Option<ApiError>,
) -> Result<Done, ApiError> {
    match (op, parent) {
        (BulkOp::Move, Some(parent)) => {
            // Out of, into or between connected libraries: refused as the single-item route
            // refuses it. Inside a read-and-write S3 library: through the bucket.
            if restructure_place(state.db(), actor.id, id, Some(parent))
                .await?
                .is_some()
            {
                let done = rename_or_move(state, actor, id, None, Some(parent)).await?;
                return Ok(Done {
                    left_behind: (done.left_behind > 0).then_some(done.left_behind),
                    ..Done::default()
                });
            }
            if let Some(refused) = into {
                return Err(refused);
            }
            move_one(state, actor, id, parent)
                .await
                .map(|_| Done::default())
        }
        (BulkOp::Copy, Some(parent)) => {
            if let Some(refused) = into {
                return Err(refused);
            }
            let copied = copy_one(state, actor, id, parent, None).await?;
            Ok(Done {
                new_id: Some(copied.id().to_owned()),
                renamed: copied.renamed().map(|(from, to)| Renamed { from, to }),
                left_behind: None,
            })
        }
        (BulkOp::Delete, _) => delete_checked(state, actor, id)
            .await
            .map(|()| Done::default()),
        (BulkOp::Restore, _) => {
            let applied = restore_checked(state, actor, id).await?;
            let renamed = match applied.renamed {
                Some((node, from)) => {
                    let to = state
                        .db()
                        .node(node)
                        .await
                        .map_err(|e| ApiError::unavailable(&e))?
                        .map_or_else(String::new, |n| n.name);
                    Some(Renamed { from, to })
                }
                None => None,
            };
            Ok(Done {
                new_id: None,
                renamed,
                left_behind: None,
            })
        }
        (BulkOp::Move | BulkOp::Copy, None) => Err(ApiError::InvalidRequest),
    }
}
