//! Signing in and out, and who is signed in.

use std::time::Instant;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::http::header::SET_COOKIE;
use axum::response::{IntoResponse, Response};
use records::SignInError;
use serde::{Deserialize, Serialize};

use super::{ApiError, JsonBody, after_sign_out, to_completion};
use crate::AppState;
use crate::auth::{Signed, Throttle, cleared_cookie, session_cookie};

/// The body of `POST /api/session`. No `Debug`: it holds a password.
#[derive(Deserialize)]
pub(crate) struct Credentials {
    email: String,
    password: String,
}

/// `GET /api/me`.
#[derive(Serialize)]
pub(crate) struct Me {
    id: String,
    email: String,
    name: String,
    is_admin: bool,
    /// How many open requests for access the person may decide, for a badge.
    open_requests: i64,
}

/// `POST /api/session`: checks the email and password and sets the session cookie.
pub(crate) async fn sign_in(
    State(state): State<AppState>,
    JsonBody(credentials): JsonBody<Credentials>,
) -> Result<Response, ApiError> {
    let key = Throttle::key(&credentials.email);
    // Held until this attempt ends, also if the request is cancelled mid-check.
    let Some(attempt) = state.throttle().try_begin(&key, Instant::now()) else {
        return Err(ApiError::TooManyAttempts);
    };
    match state
        .db()
        .sign_in(&credentials.email, &credentials.password)
        .await
    {
        Ok((_, token)) => {
            attempt.succeeded();
            Ok((
                StatusCode::NO_CONTENT,
                [(SET_COOKIE, session_cookie(&token))],
            )
                .into_response())
        }
        Err(SignInError::Invalid) => {
            attempt.failed(Instant::now());
            Err(ApiError::InvalidCredentials)
        }
        Err(SignInError::Db(e)) => Err(ApiError::unavailable(&e)),
    }
}

/// `DELETE /api/session`: ends the caller's session and clears the cookie.
pub(crate) async fn sign_out(
    State(state): State<AppState>,
    signed: Signed,
) -> Result<Response, ApiError> {
    let session = signed.session.id;
    // On its own task, so that the session's connections close even if the request is
    // cancelled once the session has ended.
    to_completion(async move {
        state
            .db()
            .end_session(session)
            .await
            .map_err(|e| ApiError::unavailable(&e))?;
        after_sign_out(&state, session);
        Ok(())
    })
    .await?;
    Ok((StatusCode::NO_CONTENT, [(SET_COOKIE, cleared_cookie())]).into_response())
}

/// `GET /api/me`: the signed-in account, and how many open requests for access it may decide.
pub(crate) async fn me(
    State(state): State<AppState>,
    signed: Signed,
) -> Result<Json<Me>, ApiError> {
    let user = signed.session.user;
    let open_requests = state
        .db()
        .open_request_count(user.id)
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
    Ok(Json(Me {
        id: user.id.0.to_string(),
        email: user.email,
        name: user.name,
        is_admin: user.is_admin,
        open_requests,
    }))
}
