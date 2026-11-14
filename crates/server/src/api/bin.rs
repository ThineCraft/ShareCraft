//! The recycle bin: a site's entries in each stage, restoring an entry, and removing one from
//! a stage (from the first to the second; from the second, purged).
//!
//! - `GET /api/sites/{id}/bin?stage=first|second&after=&limit=` lists one page of the stage.
//! - `POST /api/bin/{id}/restore` restores an entry where it was.
//! - `DELETE /api/bin/{id}` moves a first-stage entry to the second stage, or purges a
//!   second-stage one.

use std::fmt::Write as _;

use axum::Json;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use records::{BinCursor, BinEntry, BinStage, Editor, NodeKind};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use super::nodes::{NodeOut, node_id, rfc3339};
use super::{ApiError, to_completion};
use crate::AppState;
use crate::auth::Signed;
use crate::tree::{self, TreeOp};

/// Entries listed when the request names no limit.
const DEFAULT_LIMIT: u32 = 100;
/// Most entries listed at once.
const MAX_LIMIT: u32 = 500;

/// Query of `GET /api/sites/{id}/bin`.
#[derive(Deserialize)]
pub(crate) struct BinQuery {
    /// `first` (the default) or `second`.
    stage: Option<BinStage>,
    after: Option<String>,
    limit: Option<u32>,
}

/// Who deleted an entry.
#[derive(Serialize)]
struct PersonOut {
    id: String,
    name: String,
}

/// One entry of a bin.
#[derive(Serialize)]
struct EntryOut {
    id: String,
    kind: NodeKind,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    editor: Option<Editor>,
    /// The names of the nodes it was in, from the site down.
    path: Vec<String>,
    /// Bytes of the stored versions of the files deleted with it.
    size: i64,
    /// `null` when the person is not known.
    deleted_by: Option<PersonOut>,
    deleted_at: Option<String>,
    /// Whole days, rounded up, until it is purged; 0 when that is due.
    days_left: i64,
    stage: BinStage,
}

impl EntryOut {
    fn of(entry: BinEntry, now: OffsetDateTime) -> EntryOut {
        let left = (entry.purge_at - now).whole_seconds().max(0);
        EntryOut {
            id: entry.id.0.to_string(),
            kind: entry.kind,
            name: entry.name,
            editor: entry.editor,
            path: entry.path,
            size: entry.size,
            deleted_by: entry.deleted_by.map(|by| PersonOut {
                id: by.id.0.to_string(),
                name: by.name,
            }),
            deleted_at: rfc3339(Some(entry.deleted_at)),
            days_left: (left + 86_399) / 86_400,
            stage: entry.stage,
        }
    }
}

/// `GET /api/sites/{id}/bin`.
#[derive(Serialize)]
pub(crate) struct BinPage {
    items: Vec<EntryOut>,
    /// Passed as `after` for the next page; `None` on the last page.
    next: Option<String>,
}

/// `GET /api/sites/{id}/bin?stage=&after=&limit=`: one page of a stage of the site's recycle
/// bin, newest deletion first. The first stage lists the entries the caller could restore; the
/// second, only to people with `manage_site` on the site (`403` for others), every entry.
/// `limit` is 1 to 500 (100 if absent); `after` is the `next` of the previous page. `404` for
/// anything but a live site.
pub(crate) async fn list(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
    query: Result<Query<BinQuery>, QueryRejection>,
) -> Result<Json<BinPage>, ApiError> {
    let site = node_id(&id)?;
    let Query(query) = query.map_err(|_| ApiError::InvalidRequest)?;
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(ApiError::InvalidRequest);
    }
    let after = match query.after.as_deref() {
        Some(text) => Some(decode_cursor(text).ok_or(ApiError::InvalidRequest)?),
        None => None,
    };
    let stage = query.stage.unwrap_or(BinStage::First);
    let (entries, next) = state
        .db()
        .bin(signed.session.user.id, site, stage, after.as_ref(), limit)
        .await?;
    let now = OffsetDateTime::now_utc();
    Ok(Json(BinPage {
        items: entries
            .into_iter()
            .map(|entry| EntryOut::of(entry, now))
            .collect(),
        next: next.as_ref().map(encode_cursor),
    }))
}

/// A restored item renamed because its name was taken meanwhile.
#[derive(Serialize)]
struct RenamedOut {
    id: String,
    from: String,
    to: String,
}

/// `POST /api/bin/{id}/restore`.
#[derive(Serialize)]
pub(crate) struct Restored {
    /// The entry, where it now is.
    node: NodeOut,
    /// The item that had to be renamed (the entry, or a folder above it restored with it), if
    /// any.
    renamed: Option<RenamedOut>,
}

/// `POST /api/bin/{id}/restore`: restores the entry, with everything deleted with it, where it
/// was (see [`tree::apply`]). `200 {node, renamed}`; `404` unless the caller sees the entry;
/// `403` without the permission; `409 parent_in_bin` when a folder above it is in the bin and
/// the caller may not restore it.
pub(crate) async fn restore(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
) -> Result<Json<Restored>, ApiError> {
    let id = node_id(&id)?;
    let actor = signed.session.user;
    let applied = to_completion(async move {
        let applied = tree::apply(&state, &actor, TreeOp::Restore { id }).await?;
        let renamed = match &applied.renamed {
            Some((node, from)) => {
                let now = state
                    .db()
                    .node(*node)
                    .await
                    .map_err(|e| ApiError::unavailable(&e))?
                    .ok_or(ApiError::NotFound)?;
                Some(RenamedOut {
                    id: node.0.to_string(),
                    from: from.clone(),
                    to: now.name,
                })
            }
            None => None,
        };
        Ok((applied.node, renamed))
    })
    .await?;
    Ok(Json(Restored {
        node: NodeOut::from(applied.0),
        renamed: applied.1,
    }))
}

/// `DELETE /api/bin/{id}`: moves a first-stage entry to the second stage (needs what restoring
/// it needs), or purges a second-stage entry, with everything deleted with it (site managers).
/// `204`; `404` unless the caller sees the entry.
pub(crate) async fn remove(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let id = node_id(&id)?;
    let actor = signed.session.user;
    to_completion(async move {
        let entry = state
            .db()
            .bin_state(id)
            .await
            .map_err(|e| ApiError::unavailable(&e))?
            .ok_or(ApiError::NotFound)?;
        let op = match entry.stage {
            BinStage::First => TreeOp::ToSecondStage { id },
            BinStage::Second => TreeOp::Purge { id },
        };
        tree::apply(&state, &actor, op).await?;
        Ok(())
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// A cursor as the opaque text a page passes back: its JSON, in hexadecimal.
fn encode_cursor(cursor: &BinCursor) -> String {
    let json = serde_json::to_vec(cursor).unwrap_or_default();
    let mut text = String::with_capacity(json.len() * 2);
    for byte in json {
        // Writing to a string cannot fail.
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// The cursor `text` holds; `None` if it holds none.
fn decode_cursor(text: &str) -> Option<BinCursor> {
    if !text.len().is_multiple_of(2) || !text.is_ascii() {
        return None;
    }
    let bytes = (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(text.get(at..at + 2)?, 16).ok())
        .collect::<Option<Vec<u8>>>()?;
    serde_json::from_slice(&bytes).ok()
}
