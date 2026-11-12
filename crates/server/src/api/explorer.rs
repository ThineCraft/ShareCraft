//! The inheritance explorer: every place below a node where inheritance is broken, with its
//! path, who is assigned there and why, who broke it and who changed it last; how many breaks
//! lie below each child of a node; the same as CSV; and what restoring inheritance on a node
//! would change.
//!
//! Everything shown is limited to the nodes the caller has `manage_access` on: the records
//! layer leaves the others out, and counts only what it shows.

use std::collections::HashMap;

use axum::Json;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::http::header::{CONTENT_DISPOSITION, CONTENT_TYPE};
use axum::response::{IntoResponse, Response};
use collab_core::ids::{GroupId, NodeId, UserId};
use records::{Break, ExplorerFilter, NodeKind, Perms, Principal, UserRef};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;

use super::nodes::{AssignmentOut, CrumbOut, PrincipalOut, node_id, require_manager};
use super::{ApiError, parse_id};
use crate::AppState;
use crate::auth::Signed;

/// Breaks listed when the request names no limit.
const DEFAULT_LIMIT: u32 = 100;
/// Most breaks listed at once.
const MAX_LIMIT: u32 = 500;

/// A person named for display.
#[derive(Serialize)]
pub(crate) struct UserOut {
    id: String,
    name: String,
}

impl From<&UserRef> for UserOut {
    fn from(user: &UserRef) -> Self {
        UserOut {
            id: user.id.0.to_string(),
            name: user.name.clone(),
        }
    }
}

/// Who did something and when (RFC 3339).
#[derive(Serialize)]
pub(crate) struct StampOut {
    user: UserOut,
    at: Option<String>,
}

fn stamp(stamp: Option<&(UserRef, OffsetDateTime)>) -> Option<StampOut> {
    stamp.map(|(user, at)| StampOut {
        user: user.into(),
        at: at
            .format(&time::format_description::well_known::Rfc3339)
            .ok(),
    })
}

/// One break as answered.
#[derive(Serialize)]
pub(crate) struct BreakOut {
    /// The scope, with the caller's access to it.
    node: CrumbOut,
    /// From the explorer's root down to the scope's parent; empty for the root itself.
    path: Vec<CrumbOut>,
    kind: NodeKind,
    assignments: Vec<AssignmentOut>,
    /// The scope it would inherit from if inheritance were restored; `null` for a site, and for
    /// a scope above the root the caller does not manage access on.
    inherited_from: Option<CrumbOut>,
    broken_by: Option<StampOut>,
    changed_by: Option<StampOut>,
    /// How many of the breaks shown (with the same filter) are below this one.
    breaks_below: i64,
}

impl From<Break> for BreakOut {
    fn from(found: Break) -> Self {
        let no_emails = HashMap::new();
        BreakOut {
            assignments: found
                .assignments
                .iter()
                .map(|view| AssignmentOut::of(view, &no_emails))
                .collect(),
            broken_by: stamp(found.broken_by.as_ref()),
            changed_by: stamp(found.changed_by.as_ref()),
            node: found.node.into(),
            path: found.path.into_iter().map(CrumbOut::from).collect(),
            kind: found.kind,
            inherited_from: found.inherited_from.map(CrumbOut::from),
            breaks_below: found.breaks_below,
        }
    }
}

/// Query of `GET /api/explorer/{root}` and its export.
#[derive(Deserialize)]
pub(crate) struct BreaksQuery {
    /// `user:<id>` or `group:<id>`: only breaks with an assignment to them.
    principal: Option<String>,
    /// Comma-separated node kinds (`site`, `library`, `folder`, `document`): only breaks of
    /// these kinds.
    kind: Option<String>,
    after: Option<String>,
    limit: Option<u32>,
}

/// The filter `query` asks for, or `422 invalid_request`.
fn filter_of(query: &BreaksQuery) -> Result<ExplorerFilter, ApiError> {
    let principal = match query.principal.as_deref().filter(|text| !text.is_empty()) {
        None => None,
        Some(text) => {
            let (kind, id) = text.split_once(':').ok_or(ApiError::InvalidRequest)?;
            let id = parse_id(id)?;
            Some(match kind {
                "user" => Principal::User(UserId(id)),
                "group" => Principal::Group(GroupId(id)),
                _ => return Err(ApiError::InvalidRequest),
            })
        }
    };
    let mut kinds = Vec::new();
    for name in query
        .kind
        .as_deref()
        .unwrap_or_default()
        .split(',')
        .filter(|name| !name.is_empty())
    {
        let kind: NodeKind = serde_json::from_value(Value::String(name.to_owned()))
            .map_err(|_| ApiError::InvalidRequest)?;
        kinds.push(kind);
    }
    Ok(ExplorerFilter { principal, kinds })
}

/// Succeeds if `user` may use the explorer from `root`: `404` when they have no access to it,
/// `403` without `manage_access` on it.
async fn require_explorer(state: &AppState, user: UserId, root: NodeId) -> Result<(), ApiError> {
    require_manager(state.db(), user, root).await
}

/// One page of breaks from the records layer; `403` if the caller no longer manages `root`.
async fn page(
    state: &AppState,
    user: UserId,
    root: NodeId,
    filter: &ExplorerFilter,
    after: Option<&str>,
    limit: u32,
) -> Result<(Vec<Break>, Option<String>), ApiError> {
    if after.is_some_and(|text| !records::is_explorer_cursor(text)) {
        return Err(ApiError::InvalidRequest);
    }
    state
        .db()
        .breaks(user, root, filter, after, limit)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
        .ok_or(ApiError::Forbidden)
}

/// `GET /api/explorer/{root}`.
#[derive(Serialize)]
pub(crate) struct BreaksPage {
    items: Vec<BreakOut>,
    /// Passed as `after` for the next page; `None` on the last page.
    next: Option<String>,
}

/// `GET /api/explorer/{root}?principal=&kind=&after=&limit=`: one page of the breaks at and
/// below `root` that the caller manages access on, in the order of the tree. `principal` is
/// `user:<id>` or `group:<id>`; `kind` a comma-separated list of node kinds; `limit` 1 to 500
/// (100 if absent); `after` the `next` of the previous page (`422 invalid_request` for one
/// that is not). Needs `manage_access` on `root`.
pub(crate) async fn breaks(
    State(state): State<AppState>,
    signed: Signed,
    Path(root): Path<String>,
    query: Result<Query<BreaksQuery>, QueryRejection>,
) -> Result<Json<BreaksPage>, ApiError> {
    let root = node_id(&root)?;
    let Query(query) = query.map_err(|_| ApiError::BadRequest)?;
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(ApiError::InvalidRequest);
    }
    let filter = filter_of(&query)?;
    let user = signed.session.user.id;
    require_explorer(&state, user, root).await?;
    let (found, next) = page(&state, user, root, &filter, query.after.as_deref(), limit).await?;
    Ok(Json(BreaksPage {
        items: found.into_iter().map(BreakOut::from).collect(),
        next,
    }))
}

/// Query of `GET /api/explorer/{root}/counts`.
#[derive(Deserialize)]
pub(crate) struct CountsQuery {
    parent: Option<String>,
}

/// One row of `GET /api/explorer/{root}/counts`.
#[derive(Serialize)]
pub(crate) struct CountOut {
    id: String,
    count: i64,
}

/// `GET /api/explorer/{root}/counts?parent=`: for each child of `parent` (`root` if absent;
/// it must be at or below `root`, `422` otherwise) with breaks at or below it that the caller
/// manages access on, how many; children with none are left out. Needs `manage_access` on
/// `root`.
pub(crate) async fn counts(
    State(state): State<AppState>,
    signed: Signed,
    Path(root): Path<String>,
    query: Result<Query<CountsQuery>, QueryRejection>,
) -> Result<Json<Vec<CountOut>>, ApiError> {
    let root = node_id(&root)?;
    let Query(query) = query.map_err(|_| ApiError::BadRequest)?;
    let parent = query.parent.as_deref().map(node_id).transpose()?;
    let user = signed.session.user.id;
    require_explorer(&state, user, root).await?;
    let db = state.db();
    let parent = match parent {
        Some(parent) => {
            let below = db
                .is_at_or_below(parent, root)
                .await
                .map_err(|e| ApiError::unavailable(&e))?;
            if !below {
                return Err(ApiError::InvalidRequest);
            }
            parent
        }
        None => root,
    };
    let counts = db
        .break_counts(user, parent)
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
    Ok(Json(
        counts
            .into_iter()
            .map(|(id, count)| CountOut {
                id: id.0.to_string(),
                count,
            })
            .collect(),
    ))
}

/// `GET /api/explorer/{root}/export.csv?principal=&kind=`: every break the explorer shows
/// with this filter (every page), as CSV (`text/csv`, an attachment named
/// `inheritance.csv`): one row per assignment.
pub(crate) async fn export(
    State(state): State<AppState>,
    signed: Signed,
    Path(root): Path<String>,
    query: Result<Query<BreaksQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    let root = node_id(&root)?;
    let Query(query) = query.map_err(|_| ApiError::BadRequest)?;
    let filter = filter_of(&query)?;
    let user = signed.session.user.id;
    require_explorer(&state, user, root).await?;
    // Every break at once: paging would walk the scopes again for every page.
    let all = state
        .db()
        .all_breaks(user, root, &filter)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
        .ok_or(ApiError::Forbidden)?;
    let csv = records::breaks_csv(&all);
    Ok((
        [
            (CONTENT_TYPE, "text/csv; charset=utf-8"),
            (
                CONTENT_DISPOSITION,
                "attachment; filename=\"inheritance.csv\"",
            ),
        ],
        csv,
    )
        .into_response())
}

/// One person in a restore preview: their permissions now and after restoring.
#[derive(Serialize)]
pub(crate) struct PersonChange {
    user: UserOut,
    before: Perms,
    after: Perms,
}

/// One group in a restore preview whose members the caller may not see: the permissions its
/// role gives now and after restoring (`[]` for none).
#[derive(Serialize)]
pub(crate) struct GroupChange {
    group: PrincipalOut,
    before: Perms,
    after: Perms,
}

/// `GET /api/nodes/{id}/inheritance/preview`.
#[derive(Serialize)]
pub(crate) struct PreviewOut {
    node: CrumbOut,
    inherit_from: CrumbOut,
    people: Vec<PersonChange>,
    groups: Vec<GroupChange>,
    unchanged_breaks_below: i64,
}

/// `GET /api/nodes/{id}/inheritance/preview`: what restoring inheritance on the node would
/// change: the scope it would inherit from, every person whose permissions would change (by
/// their direct assignments and the groups whose members the caller may see), every other
/// group whose role would change (its members never named), and how many breaks below keep
/// their own assignments. Needs `manage_access` on the node and the scope above it; `400
/// is_site` for a site.
pub(crate) async fn preview(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
) -> Result<Json<PreviewOut>, ApiError> {
    let node = node_id(&id)?;
    let preview = state
        .db()
        .restore_preview(signed.session.user.id, node)
        .await?;
    let no_emails = HashMap::new();
    Ok(Json(PreviewOut {
        node: preview.node.into(),
        inherit_from: preview.inherit_from.into(),
        people: preview
            .people
            .iter()
            .map(|(user, before, after)| PersonChange {
                user: user.into(),
                before: *before,
                after: *after,
            })
            .collect(),
        groups: preview
            .groups
            .iter()
            .map(|(group, before, after)| GroupChange {
                group: PrincipalOut::of(group, &no_emails),
                before: *before,
                after: *after,
            })
            .collect(),
        unchanged_breaks_below: preview.unchanged_breaks_below,
    }))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn query(principal: Option<&str>, kind: Option<&str>) -> BreaksQuery {
        BreaksQuery {
            principal: principal.map(str::to_owned),
            kind: kind.map(str::to_owned),
            after: None,
            limit: None,
        }
    }

    #[test]
    fn filters_parse() {
        let id = uuid::Uuid::new_v4();
        let filter = filter_of(&query(
            Some(&format!("group:{id}")),
            Some("folder,document"),
        ))
        .unwrap();
        assert_eq!(filter.principal, Some(Principal::Group(GroupId(id))));
        assert_eq!(filter.kinds, vec![NodeKind::Folder, NodeKind::Document]);
        assert_eq!(
            filter_of(&query(None, None)).unwrap(),
            ExplorerFilter::default()
        );
        for bad in [
            query(Some("someone"), None),
            query(Some(&format!("robot:{id}")), None),
            query(Some("user:not-an-id"), None),
            query(None, Some("folder,shelf")),
        ] {
            assert_eq!(filter_of(&bad), Err(ApiError::InvalidRequest));
        }
    }
}
