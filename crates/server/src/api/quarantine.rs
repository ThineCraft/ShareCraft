//! The organisation's quarantine, for administrators only: content the malware scanner found
//! infected, or holds (a failed scan, or over its size limit), with the documents whose
//! current version names it. Administrators release it (it becomes available, recorded with
//! who and why) or delete it (every document whose current version names it is purged,
//! skipping the recycle bin, and the content erased once nothing names it). They can also
//! queue the content stored before scanning for the scanner.

use axum::Json;
use axum::extract::{Path, State};
use records::{NewEvent, QuarantineEntry, RELEASE_REASON, ScanState};
use revisions::parse_hex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::{ApiError, JsonBody, to_completion};
use crate::AppState;
use crate::auth::Signed;
use crate::room::AccessChange;
use crate::tree;

/// One document of `GET /api/admin/quarantine`.
#[derive(Serialize)]
pub(crate) struct DocumentOut {
    id: String,
    name: String,
    /// From the site down, names joined with ` / `.
    path: String,
    /// Whether it is in the recycle bin.
    deleted: bool,
}

/// One content of `GET /api/admin/quarantine`.
#[derive(Serialize)]
pub(crate) struct EntryOut {
    sha256: String,
    /// `infected`, `failed` or `too_large`.
    scan: ScanState,
    /// The threat's name, when infected.
    threat: Option<String>,
    size: u64,
    /// When the scanner answered, RFC 3339.
    scanned_at: Option<String>,
    documents: Vec<DocumentOut>,
}

impl From<QuarantineEntry> for EntryOut {
    fn from(entry: QuarantineEntry) -> Self {
        EntryOut {
            sha256: entry.sha256,
            scan: entry.state,
            threat: entry.threat,
            size: entry.size,
            scanned_at: entry.scanned_at.and_then(rfc3339),
            documents: entry
                .documents
                .into_iter()
                .map(|doc| DocumentOut {
                    id: doc.id.0.to_string(),
                    name: doc.name,
                    path: doc.path,
                    deleted: doc.deleted,
                })
                .collect(),
        }
    }
}

fn rfc3339(at: OffsetDateTime) -> Option<String> {
    at.format(&Rfc3339).ok()
}

/// `403` unless the caller is an administrator.
fn require_admin(signed: &Signed) -> Result<(), ApiError> {
    if signed.session.user.is_admin {
        Ok(())
    } else {
        Err(ApiError::Forbidden)
    }
}

/// A content's SHA-256 from a path: `404` when it is not 64 hexadecimal digits.
fn sha256_of(text: &str) -> Result<[u8; 32], ApiError> {
    parse_hex(&text.to_ascii_lowercase()).ok_or(ApiError::NotFound)
}

/// `GET /api/admin/quarantine`: every content that is infected, or held by a failed scan or
/// the size limit, that some document's current version names, with those documents and their
/// paths; the most recently scanned first. Administrators only (`403`).
pub(crate) async fn list(
    State(state): State<AppState>,
    signed: Signed,
) -> Result<Json<Vec<EntryOut>>, ApiError> {
    require_admin(&signed)?;
    let entries = state
        .db()
        .quarantine_list()
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
    Ok(Json(entries.into_iter().map(EntryOut::from).collect()))
}

/// The body of `POST /api/admin/quarantine/{sha256}/release`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReleaseBody {
    /// Why it is let through: 3 to 500 characters once trimmed.
    reason: String,
}

/// `POST /api/admin/quarantine/{sha256}/release {reason}`: lets the content through: it
/// becomes available everywhere it is named, recorded as `content.released {sha256, reason,
/// from}` with the administrator. Administrators only (`403`); `422 invalid_request` for a
/// reason shorter than 3 or longer than 500 characters; `404` for content that is not held.
pub(crate) async fn release(
    State(state): State<AppState>,
    signed: Signed,
    Path(sha256): Path<String>,
    JsonBody(body): JsonBody<ReleaseBody>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&signed)?;
    let hash = sha256_of(&sha256)?;
    if !RELEASE_REASON.contains(&body.reason.trim().chars().count()) {
        return Err(ApiError::InvalidRequest);
    }
    let released = state
        .db()
        .release(&hash, signed.session.user.id, &body.reason)
        .await?;
    if !released {
        return Err(ApiError::NotFound);
    }
    Ok(Json(json!({
        "sha256": revisions::hex(&hash),
        "scan": ScanState::Released,
    })))
}

/// `POST /api/admin/quarantine/{sha256}/delete`: purges every document whose current version
/// names the content (live or in the recycle bin), skipping the bin, then erases the content
/// once nothing names it (an older version of another document may still name it). Recorded
/// as `content.deleted {sha256, documents}` with the administrator. Answers `{sha256,
/// documents, erased}`. Administrators only (`403`); `404` for content that is not infected or
/// held.
pub(crate) async fn delete(
    State(state): State<AppState>,
    signed: Signed,
    Path(sha256): Path<String>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&signed)?;
    let hash = sha256_of(&sha256)?;
    let db = state.db();
    let held = db
        .availability(&hash)
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
    if !matches!(
        held,
        Some(ScanState::Infected | ScanState::Failed | ScanState::TooLarge)
    ) {
        return Err(ApiError::NotFound);
    }
    let actor = signed.session.user;
    to_completion(async move {
        let db = state.db();
        let documents = db
            .documents_naming(&hash)
            .await
            .map_err(|e| ApiError::unavailable(&e))?;
        let mut purged: u64 = 0;
        for document in documents {
            if tree::purge_quarantined(&state, &actor, document.id).await? {
                purged += 1;
                // Removal is final, so it needs no ordering with other access changes.
                state.rooms().send_to(document.id, AccessChange::Deleted);
            }
        }
        let erased = match db.blob_refs_flagged().await {
            Ok(false) => match state.blobs().erase_if_unreferenced(db, &hash).await {
                Ok(_) => !state.blobs().store().contains(&hash),
                Err(e) => {
                    eprintln!("sharecraft-server: cannot erase quarantined content: {e}");
                    false
                }
            },
            Ok(true) => false,
            Err(e) => return Err(ApiError::unavailable(&e)),
        };
        db.append_event(&NewEvent {
            kind: "content.deleted",
            actor: Some(actor.id),
            subject: None,
            document: None,
            node: None,
            detail: json!({ "sha256": revisions::hex(&hash), "documents": purged }),
        })
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
        Ok(Json(json!({
            "sha256": revisions::hex(&hash),
            "documents": purged,
            "erased": erased,
        })))
    })
    .await
}

/// `POST /api/admin/scan-existing`: queues for the scanner every content stored before
/// scanning (or while no scanner was configured), which stays available meanwhile. Answers
/// `{queued}`. Administrators only (`403`).
pub(crate) async fn scan_existing(
    State(state): State<AppState>,
    signed: Signed,
) -> Result<Json<Value>, ApiError> {
    require_admin(&signed)?;
    let queued = state
        .db()
        .queue_unscanned()
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
    state.scanner().wake();
    Ok(Json(json!({ "queued": queued })))
}
