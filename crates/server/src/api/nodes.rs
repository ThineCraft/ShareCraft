//! The content tree: a node and its breadcrumb, its children, creating libraries and folders,
//! renaming, moving, copying and deleting, and who may access a node.
//!
//! `/api/docs/{id}/access…` are served by the same handlers: a file's node identifier is its
//! document identifier.

use std::collections::HashMap;
use std::fmt::Write as _;

use axum::Json;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use collab_core::ids::{DocId, GroupId, NodeId, UserId};
use protocol::RemovedReason;
use records::{
    Access, AccessError, AssignmentView, Crumb, Cursor, Db, Editor, LibraryKind, LinkKind, Listed,
    Node, NodeKind, PendingState, Perms, Principal, PrincipalView, RoleRef, SourceKind, SourceMode,
    User, WriteState,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::groups::SiteRef;
use super::{ApiError, JsonBody, parse_id, recheck_below, to_completion};
use crate::AppState;
use crate::auth::Signed;
use crate::room::AccessChange;
use crate::source_write::{
    Place, Store, copy_across, copy_within, create_folder, delete_to_holding, rename_or_move,
    restore_from_holding, writable_place,
};
use crate::tree::{self, TreeOp};

/// Children listed when the request names no limit.
const DEFAULT_LIMIT: u32 = 100;
/// Most children listed at once.
const MAX_LIMIT: u32 = 500;
/// Children listed at once in the grid view of an image library, when the request names no
/// limit, and at most.
const GRID_LIMIT: u32 = 200;

/// An RFC 3339 timestamp, or `None` when there is no time (or it cannot be written).
pub(crate) fn rfc3339(time: Option<time::OffsetDateTime>) -> Option<String> {
    time.and_then(|time| {
        time.format(&time::format_description::well_known::Rfc3339)
            .ok()
    })
}

/// A node id from a path, or `422 invalid_request`.
pub(crate) fn node_id(text: &str) -> Result<NodeId, ApiError> {
    parse_id(text).map(NodeId)
}

/// `409 source_read_only` if `node` is, or lies in, a library connected to a source, whatever
/// its mode: used by the live room's checkpoint routes, since a file of a connected library
/// never opens in a live room. One query, never cached: a library connected or disconnected a
/// moment ago is judged as it now is.
///
/// The guard belongs in the HTTP handlers (and the editor's seat, see [`read_only_seat`]),
/// never in [`tree::apply`], [`move_one`], [`delete_one`] or the upload functions, which the
/// sync engine calls directly to change the library.
pub(crate) async fn require_not_connected(db: &Db, node: NodeId) -> Result<(), ApiError> {
    match db
        .source_library_of(node)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
    {
        Some(_) => Err(ApiError::SourceReadOnly),
        None => Ok(()),
    }
}

/// The refusing form of [`writable_place`], for a route that changes nothing in a connected
/// library: `Ok` outside one; inside, what [`writable_place`] tells someone who cannot see
/// the node (`404`) and a library that is not read and write (`409 source_read_only`), and
/// `409 source_action_unavailable` for a read-and-write library, blocked or not (the action is
/// unavailable whatever its state).
pub(crate) async fn require_not_connected_for(
    db: &Db,
    user: UserId,
    node: NodeId,
) -> Result<(), ApiError> {
    match writable_place(db, user, node).await {
        Ok(Place::Ordinary) => Ok(()),
        Ok(Place::External(_)) | Err(ApiError::SourceWriteBlocked) => {
            Err(ApiError::SourceActionUnavailable)
        }
        Err(e) => Err(e),
    }
}

/// Where a rename of `id`, or a move of it into `parent` (or, with no `parent`, a delete of
/// it), by `user` is made: `Ok(None)` in the
/// ordinary tree (a `parent` in a connected library is refused as [`require_not_connected_for`]
/// refuses it), `Ok(Some(library))` in the bucket of the read-and-write S3 library `id` is in
/// (see [`rename_or_move`]). Refused inside a connected library: what [`writable_place`] tells
/// someone who cannot see `id` (`404`) and a library that is not read and write (`409
/// source_read_only`); `409 source_write_blocked` while its writes are blocked; a library node
/// itself `409 source_read_only`; a `parent` anywhere but that same library `409
/// source_action_unavailable` (moves between libraries stay refused).
pub(crate) async fn restructure_place(
    db: &Db,
    user: UserId,
    id: NodeId,
    parent: Option<NodeId>,
) -> Result<Option<NodeId>, ApiError> {
    let library = match writable_place(db, user, id).await {
        Ok(Place::Ordinary) => {
            if let Some(parent) = parent {
                require_not_connected_for(db, user, parent).await?;
            }
            return Ok(None);
        }
        Ok(Place::External(Store::S3(target))) => target.library,
        Err(ApiError::SourceWriteBlocked) => {
            let library = db
                .source_library_of(id)
                .await
                .map_err(|e| ApiError::unavailable(&e))?;
            return Err(if library == Some(id) {
                ApiError::SourceReadOnly
            } else {
                ApiError::SourceWriteBlocked
            });
        }
        Err(e) => return Err(e),
    };
    if library == id {
        return Err(ApiError::SourceReadOnly);
    }
    if let Some(parent) = parent {
        match writable_place(db, user, parent).await {
            Ok(Place::External(Store::S3(target))) if target.library == library => {}
            Err(ApiError::NotFound) => return Err(ApiError::NotFound),
            _ => return Err(ApiError::SourceActionUnavailable),
        }
    }
    Ok(Some(library))
}

/// `access` to a file of a connected library, as its editor's seat gets it: without `add`,
/// `edit` and `delete`, so it opens read-only and is never given the editing seat. What else it
/// holds (viewing, managing access) is kept.
pub(crate) fn read_only_seat(access: Access) -> Access {
    let changes =
        (Perms::ADD.bits() | Perms::EDIT.bits() | Perms::DELETE.bits()) & !Perms::VIEW.bits();
    let perms = Perms::from_bits(access.perms.bits() & !changes).unwrap_or(Perms::VIEW);
    Access { perms, ..access }
}

/// Whether the editor of `doc` may get the editing seat: `doc` lies outside every connected
/// library, or in a read-and-write S3 library whose writes are not blocked. Any other
/// connected library (GitHub, file share, read-only S3, blocked) gives a read-only seat
/// ([`read_only_seat`]). If it cannot be told, the seat is read-only: the safe side. Read
/// afresh, so a mode switch or a block narrows or widens open seats at the next re-check.
pub(crate) async fn seat_may_edit(db: &Db, doc: DocId) -> bool {
    match writable_library(db, doc).await {
        Ok(may) => may,
        Err(e) => {
            eprintln!("sharecraft-server: database failure: {e}");
            false
        }
    }
}

async fn writable_library(db: &Db, doc: DocId) -> Result<bool, records::DbError> {
    let Some(library) = db.source_library_of(doc.into()).await? else {
        return Ok(true);
    };
    let read_write = matches!(
        db.source_of(library).await?,
        Some((config, _)) if config.kind == SourceKind::S3 && config.mode == SourceMode::ReadWrite
    );
    if !read_write {
        return Ok(false);
    }
    Ok(!matches!(
        db.write_state(library).await?,
        Some((WriteState::Blocked, _))
    ))
}

/// `access`, read for `doc`'s editor seat: as read when the seat may edit (see
/// [`seat_may_edit`]), otherwise made read-only (see [`read_only_seat`]).
pub(crate) async fn seat_access(db: &Db, doc: DocId, access: Option<Access>) -> Option<Access> {
    let access = access?;
    if seat_may_edit(db, doc).await {
        Some(access)
    } else {
        Some(read_only_seat(access))
    }
}

/// [`delete_one`] for a request. Inside a read-and-write S3 library the item's objects go to
/// the library's holding folder first (see [`delete_to_holding`]); in any other connected
/// library it is refused, as [`restructure_place`] refuses a rename (`source_read_only`,
/// `source_write_blocked`, and a library node itself `source_read_only`).
pub(crate) async fn delete_checked(
    state: &AppState,
    actor: &User,
    id: NodeId,
) -> Result<(), ApiError> {
    if restructure_place(state.db(), actor.id, id, None)
        .await?
        .is_some()
    {
        delete_to_holding(state, actor, id).await?;
        return Ok(());
    }
    delete_one(state, actor, id).await
}

/// A restore from the recycle bin for a request. An entry whose objects a read-and-write S3
/// library holds comes back through its bucket (see [`restore_from_holding`]); any other entry
/// is refused when it would go back into a connected library (it is judged by where it was,
/// which is where it returns), so an entry the listing put in the bin, whose objects are gone,
/// stays there.
pub(crate) async fn restore_checked(
    state: &AppState,
    actor: &User,
    id: NodeId,
) -> Result<tree::Applied, ApiError> {
    let db = state.db();
    let held = db
        .held(id)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
        .is_some();
    if held {
        let restored = restore_from_holding(state, actor, id).await?;
        let node = db
            .node(restored.node)
            .await
            .map_err(|e| ApiError::unavailable(&e))?
            .ok_or(ApiError::NotFound)?;
        return Ok(tree::Applied {
            node,
            deleted: Vec::new(),
            renamed: restored.renamed.map(|from| (restored.node, from)),
        });
    }
    require_not_connected_for(db, actor.id, id).await?;
    Ok(tree::apply(state, actor, TreeOp::Restore { id }).await?)
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
    /// After a rename or move in a bucket: the originals that could not be deleted, which the
    /// next listing shows as new files. Absent when none.
    #[serde(skip_serializing_if = "Option::is_none")]
    left_behind: Option<u64>,
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
            left_behind: None,
        }
    }
}

/// One step of a breadcrumb, or the scope access comes from.
#[derive(Serialize)]
pub(crate) struct CrumbOut {
    id: String,
    kind: NodeKind,
    name: String,
    /// The person's access: `{perms, roles}`. `None` where they have limited access only:
    /// access somewhere below.
    access: Option<Access>,
}

impl From<Crumb> for CrumbOut {
    fn from(crumb: Crumb) -> Self {
        CrumbOut {
            id: crumb.id.0.to_string(),
            kind: crumb.kind,
            name: crumb.name,
            access: crumb.role,
        }
    }
}

/// `GET /api/nodes/{id}`.
#[derive(Serialize)]
pub(crate) struct NodeDetail {
    id: String,
    kind: NodeKind,
    name: String,
    /// The caller's access; `None` for limited access.
    access: Option<Access>,
    /// For a file, the editor that opens it.
    #[serde(skip_serializing_if = "Option::is_none")]
    editor: Option<Editor>,
    /// From the site down to the node itself.
    path: Vec<CrumbOut>,
}

/// `GET /api/nodes/{id}`: the node, the caller's access to it and the breadcrumb to it. `404`
/// unless the caller has access to it or somewhere below it.
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
        access: last.role,
        editor,
        path: path.into_iter().map(CrumbOut::from).collect(),
    }))
}

/// Query of `GET /api/nodes/{id}/children`.
#[derive(Deserialize)]
pub(crate) struct ChildrenQuery {
    after: Option<String>,
    limit: Option<u32>,
    /// `grid` for the thumbnail grid of an image library, `photos` for its Album view.
    view: Option<String>,
}

impl ChildrenQuery {
    /// The page asked for: where it starts and how many children at most. `422` for a limit
    /// outside 1 to 500 or an `after` that is not a `next`.
    pub(crate) fn page(&self) -> Result<(Option<Cursor>, u32), ApiError> {
        self.page_of(DEFAULT_LIMIT, MAX_LIMIT)
    }

    /// The page asked for, `default` children when no limit is named and at most `most`.
    fn page_of(&self, default: u32, most: u32) -> Result<(Option<Cursor>, u32), ApiError> {
        let limit = self.limit.unwrap_or(default);
        if !(1..=most).contains(&limit) {
            return Err(ApiError::InvalidRequest);
        }
        let after = match self.after.as_deref() {
            Some(text) => Some(decode_cursor(text).ok_or(ApiError::InvalidRequest)?),
            None => None,
        };
        Ok((after, limit))
    }
}

/// One child in `GET /api/nodes/{id}/children`.
#[derive(Serialize)]
pub(crate) struct ChildOut {
    id: String,
    kind: NodeKind,
    name: String,
    /// The caller's access; `None` for limited access.
    access: Option<Access>,
    #[serde(skip_serializing_if = "Option::is_none")]
    editor: Option<Editor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    revised_at: Option<String>,
    inherits: bool,
    /// `blocked` for a file the file policies where it is now refuse (restored from the bin,
    /// or there before its library's settings or the organisation's block list changed); it
    /// stays, marked. Absent otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    policy: Option<&'static str>,
    /// `held` for a file whose current version the malware scanner holds, `quarantined` for
    /// one whose current version is infected. Absent when it is available.
    #[serde(skip_serializing_if = "Option::is_none")]
    scan: Option<&'static str>,
    /// The threat's name, for a quarantined file.
    #[serde(skip_serializing_if = "Option::is_none")]
    threat: Option<String>,
    /// `true` for a file whose thumbnail can be asked for (`GET /api/docs/{id}/thumbnail`): an
    /// image of a type thumbnails are made for, with a version whose content is available.
    /// Absent otherwise. Whether one can be made still depends on its bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    thumbnail: Option<bool>,
    /// In the Album view, a picture's shape as shown and when it was taken (see
    /// [`crate::photos`]). Absent when its headers tell nothing, and for files on a share.
    #[serde(skip_serializing_if = "Option::is_none")]
    photo: Option<crate::photos::PhotoFacts>,
    /// `true` when this file's bytes live on a file share.
    #[serde(skip_serializing_if = "Option::is_none")]
    external: Option<bool>,
    /// Why an external file cannot currently be read.
    #[serde(skip_serializing_if = "Option::is_none")]
    unavailable: Option<&'static str>,
    /// `saving` for a file of a read-and-write S3 library whose change is accepted and not yet
    /// written to the bucket, `failed` when writing it failed and is waiting for a retry.
    #[serde(skip_serializing_if = "Option::is_none")]
    saving: Option<&'static str>,
    /// With `saving`: whether the caller may Retry or Discard it (its author, or an owner of the
    /// library): the buttons are shown only when the server would accept them.
    #[serde(skip_serializing_if = "Option::is_none")]
    may_resolve: Option<bool>,
}

impl From<Listed> for ChildOut {
    fn from(child: Listed) -> Self {
        ChildOut {
            id: child.node.id.0.to_string(),
            kind: child.node.kind,
            name: child.node.name,
            access: child.role,
            editor: child.editor,
            revised_at: rfc3339(child.revised_at),
            inherits: !child.node.unique_scope,
            policy: None,
            scan: None,
            threat: None,
            thumbnail: None,
            photo: None,
            external: None,
            unavailable: None,
            saving: None,
            may_resolve: None,
        }
    }
}

/// `GET /api/nodes/{id}/children`.
#[derive(Serialize)]
pub(crate) struct ChildrenPage {
    items: Vec<ChildOut>,
    /// Passed as `after` for the next page; `None` on the last page.
    next: Option<String>,
}

impl ChildrenPage {
    /// The page of `listed`, continuing after `next` if there is more.
    pub(crate) fn of(listed: Vec<Listed>, next: Option<&Cursor>) -> ChildrenPage {
        ChildrenPage {
            items: listed.into_iter().map(ChildOut::from).collect(),
            next: next.map(encode_cursor),
        }
    }

    /// Each file marked `saving` also says whether `user` may Retry or Discard its change: the
    /// change's author, or someone who may manage access to its library.
    pub(crate) async fn with_resolvable(mut self, db: &Db, user: UserId) -> Result<Self, ApiError> {
        for item in &mut self.items {
            if item.saving.is_none() {
                continue;
            }
            let Ok(id) = Uuid::parse_str(&item.id) else {
                continue;
            };
            let Some(pending) = db
                .pending_write(NodeId(id))
                .await
                .map_err(|e| ApiError::unavailable(&e))?
            else {
                continue;
            };
            item.may_resolve = Some(if pending.by == user {
                true
            } else {
                db.effective_access(user, pending.library)
                    .await
                    .map_err(|e| ApiError::unavailable(&e))?
                    .is_some_and(|held| held.can_manage_access())
            });
        }
        Ok(self)
    }

    /// The page, each file in it that the file policies where it is (in `parent`) now refuse
    /// marked `"policy": "blocked"`, and each file the malware scanner holds marked `"scan":
    /// "held"`, or `"scan": "quarantined"` with its `threat`.
    pub(crate) async fn marked(mut self, db: &Db, parent: NodeId) -> Result<Self, ApiError> {
        let files: Vec<DocId> = self
            .items
            .iter()
            .filter(|item| item.kind == NodeKind::Document)
            .filter_map(|item| Uuid::parse_str(&item.id).ok().map(DocId))
            .collect();
        if files.is_empty() {
            return Ok(self);
        }
        let mut marks = db
            .scan_marks(&files)
            .await
            .map_err(|e| ApiError::unavailable(&e))?;
        for item in &mut self.items {
            let mark = Uuid::parse_str(&item.id)
                .ok()
                .and_then(|id| marks.remove(&DocId(id)));
            (item.scan, item.threat) = super::docs::scan_fields(mark);
            if item.kind == NodeKind::Document
                && let Ok(id) = Uuid::parse_str(&item.id)
            {
                if let Some(external) = db
                    .external_item(NodeId(id))
                    .await
                    .map_err(|e| ApiError::unavailable(&e))?
                {
                    item.external = Some(true);
                    item.unavailable = external.unavailable.map(super::docs::unavailable_name);
                }
                // A change accepted and not yet written to the bucket (also of a new file,
                // which has no external item yet).
                if let Some(pending) = db
                    .pending_write(NodeId(id))
                    .await
                    .map_err(|e| ApiError::unavailable(&e))?
                {
                    item.saving = Some(if pending.state == PendingState::Failed {
                        "failed"
                    } else {
                        "saving"
                    });
                }
            }
        }
        let Some((library, org)) = tree::policies_at(db, parent).await? else {
            return Ok(self);
        };
        for item in &mut self.items {
            if item.kind == NodeKind::Document
                && records::check_name(&item.name, &library, &org).is_err()
            {
                item.policy = Some("blocked");
            }
        }
        Ok(self)
    }

    /// The page, each file in it whose thumbnail can be asked for marked `"thumbnail": true`:
    /// an image of a type thumbnails are made for, with a version, not held by the malware
    /// scanner. Run after [`ChildrenPage::marked`], which marks the held ones.
    fn with_thumbnails(mut self) -> Self {
        for item in &mut self.items {
            if item.kind == NodeKind::Document
                && item.scan.is_none()
                && item.revised_at.is_some()
                && crate::previews::has_thumbnail(&item.name)
            {
                item.thumbnail = Some(true);
            }
        }
        self
    }
}

impl ChildrenPage {
    /// The page, each picture in it whose thumbnail can be asked for (see
    /// [`ChildrenPage::with_thumbnails`]) given its `photo` facts, read once from its current
    /// version's headers and kept (see [`crate::photos::photo_facts`]). Files on a share are
    /// left out: reading them would fetch them. One that cannot be read is left without.
    async fn with_photos(mut self, state: &AppState) -> Result<Self, ApiError> {
        let wanted: Vec<(usize, DocId)> = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.thumbnail == Some(true) && item.external.is_none())
            .filter_map(|(at, item)| Uuid::parse_str(&item.id).ok().map(|id| (at, DocId(id))))
            .collect();
        if wanted.is_empty() {
            return Ok(self);
        }
        let blobs = std::sync::Arc::clone(state.blobs());
        let found = super::content::blocking(state, move |store| {
            let mut found = Vec::new();
            for (at, doc) in wanted {
                let Ok(Some(stored)) = store.load(doc) else {
                    continue;
                };
                let revisions::Content::Blob(blob) = stored.content else {
                    continue;
                };
                match crate::photos::photo_facts(blobs.store(), &blob.sha256) {
                    Ok(facts) if facts.width.is_some() => found.push((at, facts)),
                    Ok(_) => {}
                    Err(e) => eprintln!(
                        "sharecraft-server: cannot read the photo facts of content {}: {e}",
                        blob.hex()
                    ),
                }
            }
            Ok(found)
        })
        .await?;
        for (at, facts) in found {
            if let Some(item) = self.items.get_mut(at) {
                item.photo = Some(facts);
            }
        }
        Ok(self)
    }
}

/// `GET /api/nodes/{id}/children?after=&limit=&view=`: one page of the children the caller
/// can see, folders before files, then by name. `limit` is 1 to 500 (100 if absent); `after`
/// is the `next` of the previous page. `view=grid`, for an image library or a folder in one
/// only (`422` elsewhere, and for any other view), pages the same way with 1 to 200 (200 if
/// absent). `view=photos` is the grid's listing with each picture's `photo`: `{width, height,
/// taken_at?}` (see [`ChildrenPage::with_photos`]). A file the file policies there now refuse
/// is marked `"policy": "blocked"`, and one whose thumbnail can be asked for `"thumbnail": true`.
pub(crate) async fn children(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
    query: Result<Query<ChildrenQuery>, QueryRejection>,
) -> Result<Json<ChildrenPage>, ApiError> {
    let id = node_id(&id)?;
    let Query(query) = query.map_err(|_| ApiError::BadRequest)?;
    let db = state.db();
    let (grid, photos) = match query.view.as_deref() {
        None => (false, false),
        Some("grid") => (true, false),
        Some("photos") => (true, true),
        Some(_) => return Err(ApiError::InvalidRequest),
    };
    let (after, limit) = if grid {
        query.page_of(GRID_LIMIT, GRID_LIMIT)?
    } else {
        query.page()?
    };
    let (listed, next) = db
        .children(signed.session.user.id, id, after.as_ref(), limit)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
        .ok_or(ApiError::NotFound)?;
    // Judged once the caller is known to see the place, so a refusal tells nothing more.
    if grid && !in_image_library(db, id).await? {
        return Err(ApiError::InvalidRequest);
    }
    let page = ChildrenPage::of(listed, next.as_ref())
        .marked(db, id)
        .await?
        .with_resolvable(db, signed.session.user.id)
        .await?
        .with_thumbnails();
    if photos {
        return Ok(Json(page.with_photos(&state).await?));
    }
    Ok(Json(page))
}

/// Whether `node` is an image library or lies in one.
async fn in_image_library(db: &Db, node: NodeId) -> Result<bool, ApiError> {
    let Some(library) = db
        .library_of(node)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
    else {
        return Ok(false);
    };
    let settings = db
        .library_settings(library)
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
    Ok(settings.is_some_and(|settings| settings.kind == LibraryKind::Image))
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

/// `POST /api/nodes/{id}/children {kind, name}`: creates a library in a site (with
/// `manage_site` on it) or a folder in a library or folder (with `add` on it). `201` with the
/// node.
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
    match writable_place(state.db(), actor.id, parent).await? {
        Place::Ordinary => {}
        // A folder goes to the store as a marker; nothing else is made there.
        Place::External(Store::S3(target)) if kind == NodeKind::Folder => {
            let node = to_completion(async move {
                create_folder(&state, &actor, target.library, parent, &new.name).await
            })
            .await?;
            return Ok((StatusCode::CREATED, Json(NodeOut::from(node))).into_response());
        }
        Place::External(_) => return Err(ApiError::SourceActionUnavailable),
    }
    let node = to_completion(async move {
        let op = TreeOp::Create {
            parent: Some(parent),
            kind,
            name: new.name,
            id: NodeId(Uuid::new_v4()),
        };
        Ok(tree::apply(&state, &actor, op).await?.node)
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
/// first; if the move is then refused, the rename stays). Answers the node as it then is. A
/// file's new name must keep its editor's extension, and a file no editor opens may not take
/// one (`422 wrong_extension`); the file policies of the library it is in, or goes into, hold
/// (`422 blocked_file_type {extension}`, `413 file_too_large {limit}`).
///
/// In a read-and-write S3 library the bucket is changed first and the tree second, under the
/// library's source lock (see [`rename_or_move`]); the answer then carries `left_behind` when
/// originals could not be deleted. Moves into, out of or between connected libraries stay
/// refused (see [`restructure_place`]).
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
    // Both the item and where it would go: nothing is renamed if the move would be refused.
    let in_bucket = restructure_place(state.db(), actor.id, id, parent).await?;
    let (node, left_behind) = to_completion(async move {
        if in_bucket.is_some() {
            let done = rename_or_move(&state, &actor, id, change.name.as_deref(), parent).await?;
            let node = state
                .db()
                .node(done.node)
                .await
                .map_err(|e| ApiError::unavailable(&e))?
                .ok_or(ApiError::NotFound)?;
            return Ok((node, done.left_behind));
        }
        let mut node = None;
        if let Some(name) = change.name {
            let renamed = tree::apply(&state, &actor, TreeOp::Rename { id, name })
                .await
                .map_err(|e| match e {
                    AccessError::WrongExtension => ApiError::WrongExtension,
                    e => e.into(),
                })?
                .node;
            if renamed.kind == NodeKind::Document {
                after_rename(&state, DocId(id.0)).await;
            }
            node = Some(renamed);
        }
        if let Some(parent) = parent {
            node = Some(move_one(&state, &actor, id, parent).await?);
        }
        Ok((node.ok_or(ApiError::InvalidRequest)?, 0))
    })
    .await?;
    let mut out = NodeOut::from(node);
    out.left_behind = (left_behind > 0).then_some(left_behind);
    Ok(Json(out))
}

/// Moves `id` into `parent` for `actor`, then rechecks the open documents below it whatever
/// the answer: a move that failed after it was stored must not leave anyone with rights they
/// lost. Run inside [`to_completion`].
pub(crate) async fn move_one(
    state: &AppState,
    actor: &User,
    id: NodeId,
    parent: NodeId,
) -> Result<Node, ApiError> {
    let moved = tree::apply(state, actor, TreeOp::Move { id, parent }).await;
    recheck_below(state, id).await;
    Ok(moved?.node)
}

/// The body of `POST /api/nodes/{id}/copy`.
#[derive(Deserialize)]
pub(crate) struct CopyBody {
    /// The library or folder the copy goes into.
    parent: String,
    /// The copy's name; the original's if absent.
    name: Option<String>,
}

/// A copy whose name had to change.
#[derive(Serialize)]
pub(crate) struct RenamedOut {
    /// The name asked for (the original's when none was).
    from: String,
    to: String,
}

/// `POST /api/nodes/{id}/copy`.
#[derive(Serialize)]
pub(crate) struct Copied {
    /// The new item.
    node: NodeOut,
    /// Set when refused characters were replaced or the name was taken there.
    renamed: Option<RenamedOut>,
}

/// `POST /api/nodes/{id}/copy {parent, name?}`: copies the file or folder into the library or
/// folder `parent` (see [`tree::apply`]): `view` on it and `add` on `parent` are needed. `201
/// {node, renamed}`; `renamed` says so when the name had to change. A file whose latest
/// version the malware scanner holds is not copied: `423 held {scan}`.
pub(crate) async fn copy(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<CopyBody>,
) -> Result<Response, ApiError> {
    let id = node_id(&id)?;
    let parent = node_id(&body.parent)?;
    let actor = signed.session.user;
    let copied =
        to_completion(async move { copy_checked(&state, &actor, id, parent, body.name).await })
            .await?;
    Ok((StatusCode::CREATED, Json(copied)).into_response())
}

/// How a copy is made (see [`copy_place`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CopyRoute {
    /// An ordinary copy in the tree ([`copy_one`]).
    Ordinary,
    /// Inside one read-and-write S3 library, in its bucket ([`copy_within`]).
    Within,
    /// Between libraries through ShareCraft: out of an S3 library, or into a read-and-write S3
    /// library ([`copy_across`]).
    Across,
}

/// How a copy of `id` into `parent` by `user` is made: inside one read-and-write S3 library in
/// its bucket; into a read-and-write S3 library from anywhere else, or out of an S3 library
/// (either mode) into an ordinary place, through ShareCraft; anything else as an ordinary
/// copy (a copy out of a GitHub library is one; out of a file share it is refused there, see
/// [`require_share_copyable_for`]). Refused as [`writable_place`] judges `parent`: `404`,
/// `409 source_read_only` into any other connected library, `409 source_write_blocked` into
/// one whose writes are blocked.
pub(crate) async fn copy_place(
    db: &Db,
    user: UserId,
    id: NodeId,
    parent: NodeId,
) -> Result<CopyRoute, ApiError> {
    let unavailable = |e: records::DbError| ApiError::unavailable(&e);
    match writable_place(db, user, parent).await? {
        Place::External(Store::S3(target)) => match writable_place(db, user, id).await {
            Ok(Place::External(Store::S3(source))) if source.library == target.library => {
                Ok(CopyRoute::Within)
            }
            Err(ApiError::NotFound) => Err(ApiError::NotFound),
            _ => Ok(CopyRoute::Across),
        },
        Place::Ordinary => {
            let Some(library) = db.source_library_of(id).await.map_err(unavailable)? else {
                return Ok(CopyRoute::Ordinary);
            };
            match db.source_of(library).await.map_err(unavailable)? {
                Some((config, _)) if config.kind == SourceKind::S3 => Ok(CopyRoute::Across),
                _ => Ok(CopyRoute::Ordinary),
            }
        }
    }
}

/// Copies `id` into `parent` for `actor`, named `name` or as it is, wherever it is: as
/// [`copy_place`] routes it, in a bucket ([`copy_within`]), between libraries
/// ([`copy_across`]) or as an ordinary copy ([`copy_one`]). Run inside [`to_completion`].
pub(crate) async fn copy_checked(
    state: &AppState,
    actor: &User,
    id: NodeId,
    parent: NodeId,
    name: Option<String>,
) -> Result<Copied, ApiError> {
    let copy = match copy_place(state.db(), actor.id, id, parent).await? {
        CopyRoute::Ordinary => return copy_one(state, actor, id, parent, name).await,
        CopyRoute::Within => copy_within(state, actor, id, parent, name.as_deref()).await?,
        CopyRoute::Across => copy_across(state, actor, id, parent, name.as_deref()).await?,
    };
    let node = state
        .db()
        .node(copy)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
        .ok_or(ApiError::NotFound)?;
    Ok(Copied {
        node: NodeOut::from(node),
        renamed: None,
    })
}

/// Copies `id` into `parent` for `actor`, named `name` or as it is. Run inside
/// [`to_completion`]. The caller routes a copy into a connected library or out of an S3
/// library elsewhere first (see [`copy_place`]); a copy out of a GitHub library is an ordinary
/// copy.
pub(crate) async fn copy_one(
    state: &AppState,
    actor: &User,
    id: NodeId,
    parent: NodeId,
    name: Option<String>,
) -> Result<Copied, ApiError> {
    require_share_copyable_for(state.db(), actor.id, id).await?;
    let applied = tree::apply(state, actor, TreeOp::Copy { id, parent, name })
        .await
        .map_err(|e| match e {
            // The name asked for has another extension than the file's.
            AccessError::WrongExtension => ApiError::InvalidName,
            e => e.into(),
        })?;
    Ok(Copied::from(applied))
}

/// Refuses copies out of file-share libraries, while preserving GitHub's existing copy behavior
/// (copies out of S3 libraries go through [`copy_across`]). Access is checked before the source
/// kind is disclosed.
async fn require_share_copyable_for(db: &Db, user: UserId, node: NodeId) -> Result<(), ApiError> {
    let Some(library) = db
        .source_library_of(node)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
    else {
        return Ok(());
    };
    let Some((source, _)) = db
        .source_of(library)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
    else {
        return Ok(());
    };
    if source.kind != SourceKind::Share {
        return Ok(());
    }
    if db
        .effective_access(user, node)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
        .is_none()
    {
        return Err(ApiError::NotFound);
    }
    Err(ApiError::SourceCopyUnavailable)
}

impl From<tree::Applied> for Copied {
    fn from(applied: tree::Applied) -> Self {
        let renamed = applied.renamed.map(|(_, from)| RenamedOut {
            from,
            to: applied.node.name.clone(),
        });
        Copied {
            node: NodeOut::from(applied.node),
            renamed,
        }
    }
}

impl Copied {
    /// The new item's id.
    pub(crate) fn id(&self) -> &str {
        &self.node.id
    }

    /// The name asked for and the one given, if they differ.
    pub(crate) fn renamed(&self) -> Option<(String, String)> {
        self.renamed
            .as_ref()
            .map(|r| (r.from.clone(), r.to.clone()))
    }
}

/// Called after the file `doc` was renamed: its room, if open, stores later revisions under
/// the name now recorded. The name is read and sent under the document's access lock, so the
/// room hears two renames in the order they were recorded.
pub(crate) async fn after_rename(state: &AppState, doc: DocId) {
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

/// `DELETE /api/nodes/{id}`: deletes a file, or a folder with every live item in it, to the
/// first stage of the site's recycle bin (editors), or a library with everything in it (people
/// with `manage_site` on the site, and `delete` on every place inside with permissions of its
/// own: `403 forbidden_below` otherwise); `400 is_site` for a site. Everyone connected to a
/// deleted file, in the folder or library or below it too, is removed with
/// `Removed { deleted }` before the answer. `204`.
pub(crate) async fn remove(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let id = node_id(&id)?;
    let actor = signed.session.user;
    to_completion(async move { delete_checked(&state, &actor, id).await }).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Deletes `id` for `actor` to the recycle bin, removing everyone connected to a file it
/// deleted. Run inside [`to_completion`].
pub(crate) async fn delete_one(state: &AppState, actor: &User, id: NodeId) -> Result<(), ApiError> {
    let applied = match tree::apply(state, actor, TreeOp::Delete { id }).await {
        Ok(applied) => applied,
        Err(e) => {
            // A delete that failed after Git recorded it may still have been repaired into
            // PostgreSQL from Git: then its files are gone, and so must their rooms be.
            end_deleted_sessions_below(state, id).await;
            return Err(e.into());
        }
    };
    // Removal is final, so it needs no ordering with other access changes.
    for doc in applied.deleted {
        state.rooms().send_to(doc, AccessChange::Deleted);
    }
    // An administrator deleted a whole site: everything under way in it ends.
    if applied.node.kind == NodeKind::Site {
        crate::sites::end_site_activity(state, id, RemovedReason::Deleted).await;
    }
    Ok(())
}

/// Ends, with `Removed { deleted }`, the session of every open file at or below `node` that
/// PostgreSQL holds deleted.
pub(crate) async fn end_deleted_sessions_below(state: &AppState, node: NodeId) {
    let db = state.db();
    for doc in state.rooms().open_documents() {
        let below = db.is_at_or_below(doc.into(), node).await.unwrap_or(false);
        if below
            && let Ok(Some(found)) = db.node(doc.into()).await
            && found.deleted
        {
            state.rooms().send_to(doc, AccessChange::Deleted);
        }
    }
}

/// Whom an assignment names, as answered: `{kind: "user", id, name, email?}` for a person
/// (`email` where it is known), `{kind: "group", id, name, site}` for a group the caller may see
/// (`site` is `{id, name}` for a site's group, `null` for an organisation group: a site group's
/// name can equal an organisation group's), and `{kind: "group", id, name: null, hidden: true}`
/// for a group the caller may not see, so that no site name they could not otherwise see is
/// shown. A signed-in link is `{kind: "link", id: <link id>, name: null, link_kind, expires_at,
/// item: {id, name}}` (`expires_at` RFC 3339 or `null`; `item` the link's own item, where it is
/// listed and revoked, which for a break below holding a copy is not the node asked about, with
/// `name` `null` when the caller may not see that item): the link, never its hidden group.
#[derive(Serialize)]
pub(crate) struct PrincipalOut {
    kind: &'static str,
    id: String,
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    email: Option<String>,
    /// Present for a group the caller may see.
    #[serde(skip_serializing_if = "Option::is_none")]
    site: Option<Option<SiteRef>>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    hidden: bool,
    /// Present for a link: what kind it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    link_kind: Option<LinkKind>,
    /// Present for a link: when it expires, `null` for never.
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_at: Option<Option<String>>,
    /// Present for a link: its own item, where it is listed and revoked.
    #[serde(skip_serializing_if = "Option::is_none")]
    item: Option<ItemRef>,
}

/// A link's own item as answered: `{id, name}`, `name` `null` when the caller may not see the
/// item (a break moved out from under it keeps its copy of the link's assignment).
#[derive(Serialize)]
pub(crate) struct ItemRef {
    id: String,
    name: Option<String>,
}

impl PrincipalOut {
    /// `view` as answered, with the person's email from `emails` when it is there.
    pub(crate) fn of(view: &PrincipalView, emails: &HashMap<UserId, String>) -> PrincipalOut {
        match view {
            PrincipalView::User(user) => PrincipalOut {
                kind: "user",
                id: user.id.0.to_string(),
                name: Some(user.name.clone()),
                email: emails.get(&user.id).cloned(),
                site: None,
                hidden: false,
                link_kind: None,
                expires_at: None,
                item: None,
            },
            PrincipalView::Group { id, name, site } => PrincipalOut {
                kind: "group",
                id: id.0.to_string(),
                name: Some(name.clone()),
                email: None,
                site: Some(site.as_ref().map(|(id, name)| SiteRef::new(*id, name))),
                hidden: false,
                link_kind: None,
                expires_at: None,
                item: None,
            },
            PrincipalView::HiddenGroup(id) => PrincipalOut {
                kind: "group",
                id: id.0.to_string(),
                name: None,
                email: None,
                site: None,
                hidden: true,
                link_kind: None,
                expires_at: None,
                item: None,
            },
            // The link, never its group.
            PrincipalView::Link {
                link,
                kind,
                expires_at,
                item,
            } => PrincipalOut {
                kind: "link",
                id: link.to_string(),
                name: None,
                email: None,
                site: None,
                hidden: false,
                link_kind: Some(*kind),
                expires_at: Some(rfc3339(*expires_at)),
                item: Some(ItemRef {
                    id: item.0.0.to_string(),
                    name: item.1.clone(),
                }),
            },
        }
    }
}

/// One assignment: whom it names, the role it gives and that role's permissions.
#[derive(Serialize)]
pub(crate) struct AssignmentOut {
    principal: PrincipalOut,
    role: RoleRef,
    perms: Perms,
}

impl AssignmentOut {
    /// `view` as answered (see [`PrincipalOut::of`]).
    pub(crate) fn of(view: &AssignmentView, emails: &HashMap<UserId, String>) -> AssignmentOut {
        AssignmentOut {
            principal: PrincipalOut::of(&view.principal, emails),
            role: view.role.clone(),
            perms: view.perms,
        }
    }
}

/// `GET /api/nodes/{id}/access`.
#[derive(Serialize)]
pub(crate) struct AccessOut {
    /// Whether the node takes its access from a scope above it.
    inherits: bool,
    /// The scope that decides access to the node: the node itself unless it inherits.
    source: CrumbOut,
    /// The assignments on `source`, to people and groups, by the principal's name.
    assignments: Vec<AssignmentOut>,
}

/// `GET /api/nodes/{id}/access` (and `/api/docs/{id}/access`): whether the node inherits, the
/// scope its access comes from, and the assignments there. Needs `manage_access`.
pub(crate) async fn access(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
) -> Result<Json<AccessOut>, ApiError> {
    let id = node_id(&id)?;
    let access = state.db().access(signed.session.user.id, id).await?;
    let emails: HashMap<UserId, String> = access
        .grants
        .into_iter()
        .map(|grant| (grant.user.id, grant.user.email))
        .collect();
    Ok(Json(AccessOut {
        inherits: access.inherits,
        source: access.source.into(),
        assignments: access
            .assignments
            .iter()
            .map(|view| AssignmentOut::of(view, &emails))
            .collect(),
    }))
}

/// Succeeds if `user` may manage access to `node`: `NotFound` when they have no access to it,
/// `Forbidden` when it lacks `manage_access` (`Archived` on an archived site). For changing
/// access.
pub(super) async fn require_manager(db: &Db, user: UserId, node: NodeId) -> Result<(), ApiError> {
    match db
        .effective_access(user, node)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
    {
        None => Err(ApiError::NotFound),
        Some(held) if !held.can_manage_access() => Err(held.refusal().into()),
        Some(_) => Ok(()),
    }
}

/// [`require_manager`] for reading what managers read (the explorer, a document's events): on
/// an archived site the `manage_access` the person would hold were it not archived is enough.
pub(super) async fn require_reading_manager(
    db: &Db,
    user: UserId,
    node: NodeId,
) -> Result<(), ApiError> {
    match db
        .effective_access(user, node)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
    {
        None => Err(ApiError::NotFound),
        Some(held) if !held.ignoring_archive().contains(Perms::MANAGE_ACCESS) => {
            Err(ApiError::Forbidden)
        }
        Some(_) => Ok(()),
    }
}

/// The body of `PUT /api/nodes/{id}/access`: a person by `email` or a group by id (`group`),
/// exactly one of them, and a role id.
#[derive(Deserialize)]
pub(crate) struct Share {
    email: Option<String>,
    group: Option<String>,
    role: String,
}

/// `PUT /api/nodes/{id}/access {email | group, role}` (and `/api/docs/{id}/access`): gives the
/// person with this email, or the group, the role `role` (`viewer`, `editor`, `owner` or a
/// custom role's id) on the node, breaking inheritance first if the node inherits. Nobody but
/// an administrator may give a role with permissions they do not hold there
/// (`403 beyond_your_access`). A group must be one the caller may see (`404` otherwise).
pub(crate) async fn share(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
    JsonBody(share): JsonBody<Share>,
) -> Result<StatusCode, ApiError> {
    let node = node_id(&id)?;
    let actor = signed.session.user.id;
    let db = state.db();
    let principal = match (share.email, share.group) {
        (Some(email), None) => {
            // The email is looked up only for someone who may manage access, so that nobody
            // else can learn which emails have accounts. The records layer decides again when
            // it makes the change.
            require_manager(db, actor, node).await?;
            let user = db
                .user_by_email(&email)
                .await
                .map_err(|e| ApiError::unavailable(&e))?
                .ok_or(ApiError::NoSuchUser)?;
            Principal::User(user.id)
        }
        (None, Some(group)) => {
            let group = GroupId(parse_id(&group)?);
            require_manager(db, actor, node).await?;
            db.group(actor, group)
                .await
                .map_err(|e| ApiError::unavailable(&e))?
                .ok_or(ApiError::NotFound)?;
            Principal::Group(group)
        }
        _ => return Err(ApiError::InvalidRequest),
    };
    change_access(state, actor, node, principal, Some(share.role)).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Stores `actor`'s change of `principal`'s access to `node` (`None` removes their assignment)
/// and tells the rooms of the open documents at or below it, finishing both even if the
/// request is cancelled.
pub(super) async fn change_access(
    state: AppState,
    actor: UserId,
    node: NodeId,
    principal: Principal,
    role: Option<String>,
) -> Result<(), ApiError> {
    to_completion(async move {
        state
            .db()
            .set_node_access(actor, node, principal, role.as_deref())
            .await?;
        recheck_below(&state, node).await;
        Ok(())
    })
    .await
}

/// `DELETE /api/nodes/{id}/access/{kind}/{principal_id}` (and `/api/docs/…`): removes the
/// assignment on the node of a person (`kind` `user`) or a group (`kind` `group`), breaking
/// inheritance first if the node inherits. `404` for any other `kind`.
pub(crate) async fn unshare(
    State(state): State<AppState>,
    signed: Signed,
    Path((id, kind, principal_id)): Path<(String, String, String)>,
) -> Result<StatusCode, ApiError> {
    let node = node_id(&id)?;
    let principal = match kind.as_str() {
        "user" => Principal::User(UserId(parse_id(&principal_id)?)),
        "group" => Principal::Group(GroupId(parse_id(&principal_id)?)),
        _ => return Err(ApiError::NotFound),
    };
    change_access(state, signed.session.user.id, node, principal, None).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The body of `POST /api/nodes/{id}/inheritance`.
#[derive(Deserialize)]
pub(crate) struct Inheritance {
    inherit: bool,
}

/// `POST /api/nodes/{id}/inheritance {inherit}`: `false` makes the node a scope of its own
/// with a copy of the assignments it inherited; `true` makes it inherit again, dropping its
/// own. Needs `manage_access` (restoring also on the scope above). `204`, also when the node
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
