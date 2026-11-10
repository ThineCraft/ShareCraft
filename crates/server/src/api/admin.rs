//! Managing people. Administrators only.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use collab_core::ids::UserId;
use records::NewUser;
use serde::{Deserialize, Serialize};

use super::{ApiError, JsonBody, after_account_disabled, parse_id};
use crate::AppState;
use crate::auth::Signed;

/// Refuses everyone but administrators, before any other work. The records layer checks again.
fn require_admin(signed: &Signed) -> Result<(), ApiError> {
    if signed.session.user.is_admin {
        Ok(())
    } else {
        Err(ApiError::Forbidden)
    }
}

/// One row of `GET /api/admin/users`.
#[derive(Serialize)]
pub(crate) struct Person {
    id: String,
    email: String,
    name: String,
    is_admin: bool,
    disabled: bool,
}

/// `GET /api/admin/users`: every account.
pub(crate) async fn users(
    State(state): State<AppState>,
    signed: Signed,
) -> Result<Json<Vec<Person>>, ApiError> {
    require_admin(&signed)?;
    let users = state
        .db()
        .users()
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
    Ok(Json(
        users
            .into_iter()
            .map(|user| Person {
                id: user.id.0.to_string(),
                email: user.email,
                name: user.name,
                is_admin: user.is_admin,
                disabled: user.disabled,
            })
            .collect(),
    ))
}

/// The body of `POST /api/admin/users`. No `Debug`: it holds a password.
#[derive(Deserialize)]
pub(crate) struct NewPerson {
    email: String,
    name: String,
    password: String,
    #[serde(default)]
    is_admin: bool,
}

/// `POST /api/admin/users`: creates an account.
pub(crate) async fn create(
    State(state): State<AppState>,
    signed: Signed,
    JsonBody(new): JsonBody<NewPerson>,
) -> Result<Response, ApiError> {
    require_admin(&signed)?;
    let user = state
        .db()
        .create_user(
            Some(signed.session.user.id),
            NewUser {
                email: &new.email,
                name: &new.name,
                password: &new.password,
                is_admin: new.is_admin,
            },
        )
        .await?;
    let body = Json(serde_json::json!({ "id": user.id.0.to_string() }));
    Ok((StatusCode::CREATED, body).into_response())
}

/// `POST /api/admin/users/{id}/disable`: disables an account and ends its sessions.
pub(crate) async fn disable(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    require_admin(&signed)?;
    let user = UserId(parse_id(&id)?);
    state
        .db()
        .set_disabled(signed.session.user.id, user, true)
        .await?;
    after_account_disabled(&state, user).await;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/admin/users/{id}/enable`: re-enables an account.
pub(crate) async fn enable(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    require_admin(&signed)?;
    let user = UserId(parse_id(&id)?);
    state
        .db()
        .set_disabled(signed.session.user.id, user, false)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
