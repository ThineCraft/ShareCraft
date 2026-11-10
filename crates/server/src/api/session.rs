//! Signing in and out, and who is signed in.

use std::time::Instant;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::http::header::SET_COOKIE;
use axum::response::{IntoResponse, Response};
use records::SignInError;
use serde::{Deserialize, Serialize};

use super::{ApiError, JsonBody};
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
}

/// `POST /api/session`: checks the email and password and sets the session cookie.
pub(crate) async fn sign_in(
    State(state): State<AppState>,
    JsonBody(credentials): JsonBody<Credentials>,
) -> Result<Response, ApiError> {
    let key = Throttle::key(&credentials.email);
    if !state.throttle().allows(&key, Instant::now()) {
        return Err(ApiError::TooManyAttempts);
    }
    match state
        .db()
        .sign_in(&credentials.email, &credentials.password)
        .await
    {
        Ok((_, token)) => {
            state.throttle().succeeded(&key);
            Ok((
                StatusCode::NO_CONTENT,
                [(SET_COOKIE, session_cookie(&token))],
            )
                .into_response())
        }
        Err(SignInError::Invalid) => {
            state.throttle().failed(&key, Instant::now());
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
    state
        .db()
        .end_session(signed.session.id)
        .await
        .map_err(|e| ApiError::unavailable(&e))?;
    Ok((StatusCode::NO_CONTENT, [(SET_COOKIE, cleared_cookie())]).into_response())
}

/// `GET /api/me`: the signed-in account.
pub(crate) async fn me(signed: Signed) -> Json<Me> {
    let user = signed.session.user;
    Json(Me {
        id: user.id.0.to_string(),
        email: user.email,
        name: user.name,
        is_admin: user.is_admin,
    })
}
