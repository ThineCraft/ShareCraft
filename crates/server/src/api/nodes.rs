//! The content tree: a node and its breadcrumb, its children, creating libraries and folders,
//! renaming, moving and deleting, and who may access a node.
//!
//! `/api/docs/{id}/access…` are served by the same handlers: a file's node identifier is its
//! document identifier.

use std::fmt::Write as _;

use axum::Json;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use collab_core::ids::{DocId, NodeId, UserId};
use records::{Crumb, Cursor, Db, Editor, Node, NodeKind, Role};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::{ApiError, JsonBody, parse_id, recheck_below, to_completion};
use crate::AppState;
use crate::auth::Signed;
use crate::room::AccessChange;
use crate::tree::{self, TreeOp};

/// Children listed when the request names no limit.
const DEFAULT_LIMIT: u32 = 100;
/// Most children listed at once.
const MAX_LIMIT: u32 = 500;

/// An RFC 3339 timestamp, or `None` when there is no time (or it cannot be written).
fn rfc3339(time: Option<time::OffsetDateTime>) -> Option<String> {
    time.and_then(|time| {
        time.format(&time::format_description::well_known::Rfc3339)
            .ok()
    })
}

/// A node id from a path, or `422 invalid_request`.
fn node_id(text: &str) -> Result<NodeId, ApiError> {
    parse_id(text).map(NodeId)
}

/// A node as answered by the routes that create or change one.
#[derive(Serialize)]
pub(crate) struct NodeOut {
    id: String,
    kind: NodeKind,
    name: String,
    /// `None` for a site.
    parent: Option<String>,
    site: String,
    /// Whether the node takes its access from a scope above it.
    inherits: bool,
}

impl From<Node> for NodeOut {
    fn from(node: Node) -> Self {
        NodeOut {
            id: node.id.0.to_string(),
            kind: node.kind,
            name: node.name,
            parent: node.parent.map(|p| p.0.to_string()),
            site: node.site.0.to_string(),
            inherits: !node.unique_scope,
        }
    }
}

/// One step of a breadcrumb, or the scope access comes from.
#[derive(Serialize)]
pub(crate) struct CrumbOut {
    id: String,
    kind: NodeKind,
    name: String,
    /// `None` where the person has limited access only: a role somewhere below.
    role: Option<Role>,
}

impl From<Crumb> for CrumbOut {
    fn from(crumb: Crumb) -> Self {
        CrumbOut {
            id: crumb.id.0.to_string(),
            kind: crumb.kind,
            name: crumb.name,
            role: crumb.role,
        }
    }
}

/// `GET /api/nodes/{id}`.
#[derive(Serialize)]
pub(crate) struct NodeDetail {
    id: String,
    kind: NodeKind,
    name: String,
    /// `None` for limited access.
    role: Option<Role>,
    /// For a file, the editor that opens it.
    #[serde(skip_serializing_if = "Option::is_none")]
    editor: Option<Editor>,
    /// From the site down to the node itself.
    path: Vec<CrumbOut>,
}

/// `GET /api/nodes/{id}`: the node, the caller's role on it and the breadcrumb to it. `404`
/// unless the caller has a role on it or somewhere below it.
pub(crate) async fn show(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
) -> Result<Json<NodeDetail>, ApiError> {
    let id = node_id(&id)?;
    let db = state.db();
    let path = db
        .path(signed.session.user.id, id)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
        .ok_or(ApiError::NotFound)?;
    let last = path.last().cloned().ok_or(ApiError::NotFound)?;
    let editor = if last.kind == NodeKind::Document {
        db.document(DocId(id.0))
            .await
            .map_err(|e| ApiError::unavailable(&e))?
            .map(|document| document.editor)
    } else {
        None
    };
    Ok(Json(NodeDetail {
        id: last.id.0.to_string(),
        kind: last.kind,
        name: last.name,
        role: last.role,
        editor,
        path: path.into_iter().map(CrumbOut::from).collect(),
    }))
}

/// Query of `GET /api/nodes/{id}/children`.
#[derive(Deserialize)]
pub(crate) struct ChildrenQuery {
    after: Option<String>,
    limit: Option<u32>,
}

/// One child in `GET /api/nodes/{id}/children`.
#[derive(Serialize)]
pub(crate) struct ChildOut {
    id: String,
    kind: NodeKind,
    name: String,
    /// `None` for limited access.
    role: Option<Role>,
    #[serde(skip_serializing_if = "Option::is_none")]
    editor: Option<Editor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    revised_at: Option<String>,
    inherits: bool,
}

/// `GET /api/nodes/{id}/children`.
#[derive(Serialize)]
pub(crate) struct ChildrenPage {
    items: Vec<ChildOut>,
    /// Passed as `after` for the next page; `None` on the last page.
    next: Option<String>,
}

/// `GET /api/nodes/{id}/children?after=&limit=`: one page of the children the caller can see,
/// folders before files, then by name. `limit` is 1 to 500 (100 if absent); `after` is the
/// `next` of the previous page.
pub(crate) async fn children(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
    query: Result<Query<ChildrenQuery>, QueryRejection>,
) -> Result<Json<ChildrenPage>, ApiError> {
    let id = node_id(&id)?;
    let Query(query) = query.map_err(|_| ApiError::BadRequest)?;
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(ApiError::InvalidRequest);
    }
    let after = match query.after.as_deref() {
        Some(text) => Some(decode_cursor(text).ok_or(ApiError::InvalidRequest)?),
        None => None,
    };
    let (listed, next) = state
        .db()
        .children(signed.session.user.id, id, after.as_ref(), limit)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
        .ok_or(ApiError::NotFound)?;
    let items = listed
        .into_iter()
        .map(|child| ChildOut {
            id: child.node.id.0.to_string(),
            kind: child.node.kind,
            name: child.node.name,
            role: child.role,
            editor: child.editor,
            revised_at: rfc3339(child.revised_at),
            inherits: !child.node.unique_scope,
        })
        .collect();
    Ok(Json(ChildrenPage {
        items,
        next: next.as_ref().map(encode_cursor),
    }))
}

/// A cursor as the opaque text a page passes back: its JSON, in hexadecimal.
fn encode_cursor(cursor: &Cursor) -> String {
    let json = serde_json::to_vec(cursor).unwrap_or_default();
    let mut text = String::with_capacity(json.len() * 2);
    for byte in json {
        // Writing to a string cannot fail.
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// The cursor `text` holds; `None` if it holds none.
fn decode_cursor(text: &str) -> Option<Cursor> {
    if !text.len().is_multiple_of(2) || !text.is_ascii() {
        return None;
    }
    let bytes = (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(text.get(at..at + 2)?, 16).ok())
        .collect::<Option<Vec<u8>>>()?;
    let cursor: Cursor = serde_json::from_slice(&bytes).ok()?;
    matches!(cursor.kind_rank, 0 | 1).then_some(cursor)
}

/// The body of `POST /api/nodes/{id}/children`.
#[derive(Deserialize)]
pub(crate) struct NewChild {
    kind: String,
    name: String,
}

/// `POST /api/nodes/{id}/children {kind, name}`: creates a library in a site (owners of the
/// site) or a folder in a library or folder (its editors). `201` with the node.
pub(crate) async fn create(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
    JsonBody(new): JsonBody<NewChild>,
) -> Result<Response, ApiError> {
    let parent = node_id(&id)?;
    let kind: NodeKind =
        serde_json::from_value(Value::String(new.kind)).map_err(|_| ApiError::InvalidRequest)?;
    let actor = signed.session.user;
    let node = to_completion(async move {
        let op = TreeOp::Create {
            parent: Some(parent),
            kind,
            name: new.name,
            id: NodeId(Uuid::new_v4()),
        };
        Ok(tree::apply(&state, &actor, op).await?)
    })
    .await?;
    Ok((StatusCode::CREATED, Json(NodeOut::from(node))).into_response())
}

/// The body of `PATCH /api/nodes/{id}`.
#[derive(Deserialize)]
pub(crate) struct NodeChange {
    name: Option<String>,
    /// The library or folder to move the node into.
    parent: Option<String>,
}

/// `PATCH /api/nodes/{id} {name?, parent?}`: renames the node, moves it, or both (the rename
/// first; if the move is then refused, the rename stays). Answers the node as it then is.
pub(crate) async fn change(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
    JsonBody(change): JsonBody<NodeChange>,
) -> Result<Json<NodeOut>, ApiError> {
    let id = node_id(&id)?;
    let parent = change.parent.as_deref().map(node_id).transpose()?;
    if change.name.is_none() && parent.is_none() {
        return Err(ApiError::InvalidRequest);
    }
    let actor = signed.session.user;
    let node = to_completion(async move {
        let mut node = None;
        if let Some(name) = change.name {
            let renamed = tree::apply(&state, &actor, TreeOp::Rename { id, name }).await?;
            if renamed.kind == NodeKind::Document {
                after_rename(&state, DocId(id.0)).await;
            }
            node = Some(renamed);
        }
        if let Some(parent) = parent {
            let moved = tree::apply(&state, &actor, TreeOp::Move { id, parent }).await;
            // Checked whatever the answer: a change that failed after it was stored must not
            // leave anyone with rights they lost.
            recheck_below(&state, id).await;
            node = Some(moved?);
        }
        node.ok_or(ApiError::InvalidRequest)
    })
    .await?;
    Ok(Json(NodeOut::from(node)))
}

/// Called after the file `doc` was renamed: its room, if open, stores later revisions under
/// the name now recorded. The name is read and sent under the document's access lock, so the
/// room hears two renames in the order they were recorded.
async fn after_rename(state: &AppState, doc: DocId) {
    let rooms = state.rooms();
    let _in_order = rooms.order_access(doc).await;
    match state.db().document(doc).await {
        Ok(Some(document)) => rooms.send_to(
            doc,
            AccessChange::Renamed {
                name: document.name,
            },
        ),
        Ok(None) => {}
        Err(e) => eprintln!("sharecraft-server: database failure: {e}"),
    }
}

/// `DELETE /api/nodes/{id}`: deletes a file or an empty folder (editors). Everyone connected to
/// a deleted file is removed with `Removed { deleted }` before the answer. `204`.
pub(crate) async fn remove(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let id = node_id(&id)?;
    let actor = signed.session.user;
    to_completion(async move {
        let deleted = tree::apply(&state, &actor, TreeOp::Delete { id }).await?;
        if deleted.kind == NodeKind::Document {
            // Removal is final, so it needs no ordering with other access changes.
            state.rooms().send_to(DocId(id.0), AccessChange::Deleted);
        }
        Ok(())
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// A person in an access list.
#[derive(Serialize)]
pub(crate) struct Person {
    id: String,
    email: String,
    name: String,
}

/// One assignment in an access list.
#[derive(Serialize)]
pub(crate) struct AccessRow {
    user: Person,
    role: Role,
}

/// `GET /api/nodes/{id}/access`.
#[derive(Serialize)]
pub(crate) struct AccessOut {
    /// Whether the node takes its access from a scope above it.
    inherits: bool,
    /// The scope that decides access to the node: the node itself unless it inherits.
    source: CrumbOut,
    /// The assignments on `source`.
    grants: Vec<AccessRow>,
}

/// `GET /api/nodes/{id}/access` (and `/api/docs/{id}/access`): whether the node inherits, the
/// scope its access comes from, and the assignments there. Owners only.
pub(crate) async fn access(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
) -> Result<Json<AccessOut>, ApiError> {
    let id = node_id(&id)?;
    let access = state.db().access(signed.session.user.id, id).await?;
    Ok(Json(AccessOut {
        inherits: access.inherits,
        source: access.source.into(),
        grants: access
            .grants
            .into_iter()
            .map(|grant| AccessRow {
                user: Person {
                    id: grant.user.id.0.to_string(),
                    email: grant.user.email,
                    name: grant.user.name,
                },
                role: grant.role,
            })
            .collect(),
    }))
}

/// Succeeds if `user` may manage `node`: `NotFound` when they have no role on it, `Forbidden`
/// when their role is lower.
pub(super) async fn require_owner(db: &Db, user: UserId, node: NodeId) -> Result<(), ApiError> {
    match db
        .effective_role(user, node)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
    {
        None => Err(ApiError::NotFound),
        Some(held) if !held.can_manage() => Err(ApiError::Forbidden),
        Some(_) => Ok(()),
    }
}

/// The body of `PUT /api/nodes/{id}/access`.
#[derive(Deserialize)]
pub(crate) struct Share {
    email: String,
    role: String,
}

/// `PUT /api/nodes/{id}/access {email, role}` (and `/api/docs/{id}/access`): gives the person
/// with this email a role on the node, breaking inheritance first if the node inherits.
pub(crate) async fn share(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
    JsonBody(share): JsonBody<Share>,
) -> Result<StatusCode, ApiError> {
    let node = node_id(&id)?;
    let role: Role =
        serde_json::from_value(Value::String(share.role)).map_err(|_| ApiError::InvalidRequest)?;
    let actor = signed.session.user.id;
    let db = state.db();
    // The email is looked up only for someone who may manage access, so that nobody else can
    // learn which emails have accounts. The records layer decides again when it makes the change.
    require_owner(db, actor, node).await?;
    let user = db
        .user_by_email(&share.email)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
        .ok_or(ApiError::NoSuchUser)?;
    change_access(state, actor, node, user.id, Some(role)).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Stores `actor`'s change of `user`'s access to `node` (`None` removes their assignment) and
/// tells the rooms of the open documents at or below it, finishing both even if the request
/// is cancelled.
pub(super) async fn change_access(
    state: AppState,
    actor: UserId,
    node: NodeId,
    user: UserId,
    role: Option<Role>,
) -> Result<(), ApiError> {
    to_completion(async move {
        state.db().set_node_access(actor, node, user, role).await?;
        recheck_below(&state, node).await;
        Ok(())
    })
    .await
}

/// `DELETE /api/nodes/{id}/access/{user_id}` (and `/api/docs/…`): removes a person's
/// assignment on the node, breaking inheritance first if the node inherits.
pub(crate) async fn unshare(
    State(state): State<AppState>,
    signed: Signed,
    Path((id, user_id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let node = node_id(&id)?;
    let user = UserId(parse_id(&user_id)?);
    change_access(state, signed.session.user.id, node, user, None).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The body of `POST /api/nodes/{id}/inheritance`.
#[derive(Deserialize)]
pub(crate) struct Inheritance {
    inherit: bool,
}

/// `POST /api/nodes/{id}/inheritance {inherit}`: `false` makes the node a scope of its own
/// with a copy of the assignments it inherited; `true` makes it inherit again, dropping its
/// own. Owners only (restoring also needs owner of the scope above). `204`, also when the node
/// already was as asked.
pub(crate) async fn inheritance(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<Inheritance>,
) -> Result<StatusCode, ApiError> {
    let node = node_id(&id)?;
    let actor = signed.session.user;
    to_completion(async move {
        let db = state.db();
        if body.inherit {
            db.restore_inheritance(actor.id, node).await?;
        } else {
            db.break_inheritance(actor.id, node).await?;
        }
        recheck_below(&state, node).await;
        Ok(())
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}
