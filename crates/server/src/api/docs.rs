//! Documents: create, list, open, and who may access them.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use collab_core::ids::{DocId, UserId};
use records::Role;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use super::{ApiError, JsonBody, after_access_change, parse_id};
use crate::AppState;
use crate::auth::Signed;

/// The name a document gets when the request gives none.
const DEFAULT_NAME: &str = "Untitled";

/// An RFC 3339 timestamp, or `None` when there is no time (or it cannot be written).
fn rfc3339(time: Option<OffsetDateTime>) -> Option<String> {
    time.and_then(|time| time.format(&Rfc3339).ok())
}

/// One row of `GET /api/docs`.
#[derive(Serialize)]
pub(crate) struct DocSummary {
    id: String,
    name: String,
    role: Role,
    revised_at: Option<String>,
}

/// `GET /api/docs`: the documents the caller can open.
pub(crate) async fn list(
    State(state): State<AppState>,
    signed: Signed,
) -> Result<Json<Vec<DocSummary>>, ApiError> {
    let documents = state
        .db()
        .documents_for(signed.session.user.id)
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
    Ok(Json(
        documents
            .into_iter()
            .map(|(document, role)| DocSummary {
                id: document.id.0.to_string(),
                name: document.name,
                role,
                revised_at: rfc3339(document.revised_at),
            })
            .collect(),
    ))
}

/// The body of `POST /api/docs`.
#[derive(Deserialize)]
pub(crate) struct NewDoc {
    name: Option<String>,
}

/// `POST /api/docs`: creates a document owned by the caller.
pub(crate) async fn create(
    State(state): State<AppState>,
    signed: Signed,
    JsonBody(new): JsonBody<NewDoc>,
) -> Result<Response, ApiError> {
    let id = DocId(Uuid::new_v4());
    let name = new.name.as_deref().unwrap_or(DEFAULT_NAME);
    let document = state
        .db()
        .create_document(signed.session.user.id, id, name)
        .await?;
    let body = Json(serde_json::json!({
        "id": document.id.0.to_string(),
        "name": document.name,
        "role": Role::Owner,
    }));
    Ok((StatusCode::CREATED, body).into_response())
}

/// `GET /api/docs/{id}`.
#[derive(Serialize)]
pub(crate) struct DocDetail {
    id: String,
    name: String,
    role: Role,
    revision: Option<String>,
}

/// `GET /api/docs/{id}`: the document, if the caller has a role on it.
pub(crate) async fn open(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
) -> Result<Json<DocDetail>, ApiError> {
    let doc = DocId(parse_id(&id)?);
    let db = state.db();
    let role = db
        .role(signed.session.user.id, doc)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
        .ok_or(ApiError::NotFound)?;
    let document = db
        .document(doc)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(DocDetail {
        id: document.id.0.to_string(),
        name: document.name,
        role,
        revision: document.revision,
    }))
}

/// A person in `GET /api/docs/{id}/access`.
#[derive(Serialize)]
pub(crate) struct Person {
    id: String,
    email: String,
    name: String,
}

/// One row of `GET /api/docs/{id}/access`.
#[derive(Serialize)]
pub(crate) struct AccessRow {
    user: Person,
    role: Role,
}

/// `GET /api/docs/{id}/access`: who has been granted access. Owners only.
pub(crate) async fn access(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
) -> Result<Json<Vec<AccessRow>>, ApiError> {
    let doc = DocId(parse_id(&id)?);
    let grants = state.db().grants(signed.session.user.id, doc).await?;
    Ok(Json(
        grants
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
    ))
}

/// The body of `PUT /api/docs/{id}/access`.
#[derive(Deserialize)]
pub(crate) struct Share {
    email: String,
    role: String,
}

/// `PUT /api/docs/{id}/access`: gives the person with this email a role.
pub(crate) async fn share(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
    JsonBody(share): JsonBody<Share>,
) -> Result<StatusCode, ApiError> {
    let doc = DocId(parse_id(&id)?);
    let role: Role =
        serde_json::from_value(Value::String(share.role)).map_err(|_| ApiError::InvalidRequest)?;
    let actor = signed.session.user.id;
    let db = state.db();
    // The email is looked up only for someone who may manage access, so that nobody else can
    // learn which emails have accounts. The records layer decides again when it makes the change.
    let held = db
        .role(actor, doc)
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
    match held {
        None => return Err(ApiError::NotFound),
        Some(held) if !held.can_manage() => return Err(ApiError::Forbidden),
        Some(_) => {}
    }
    let user = db
        .user_by_email(&share.email)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
        .ok_or(ApiError::NoSuchUser)?;
    db.set_access(actor, doc, user.id, Some(role)).await?;
    after_access_change(&state, doc, user.id);
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /api/docs/{id}/access/{user_id}`: removes a person's grant.
pub(crate) async fn unshare(
    State(state): State<AppState>,
    signed: Signed,
    Path((id, user_id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let doc = DocId(parse_id(&id)?);
    let user = UserId(parse_id(&user_id)?);
    state
        .db()
        .set_access(signed.session.user.id, doc, user, None)
        .await?;
    after_access_change(&state, doc, user);
    Ok(StatusCode::NO_CONTENT)
}
