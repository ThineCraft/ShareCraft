//! Signing in and out, and who is signed in.

use std::net::{IpAddr, Ipv4Addr};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::StatusCode;
use axum::http::header::{RETRY_AFTER, SET_COOKIE};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use records::{NewEvent, SignInError};
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;

use super::{ApiError, JsonBody, after_sign_out, to_completion};
use crate::AppState;
use crate::auth::{PASSWORD_WAIT, Signed, Throttle, WINDOW, cleared_cookie, session_cookie};
use crate::deployment::Source;

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
///
/// Before the password is checked, in this order: the client address may not have 30 failed
/// sign-ins in the last 15 minutes, the email may not have 5, and a password-check turn must
/// come free within 10 seconds. A refusal by either limit is `429 too_many_attempts` with
/// `Retry-After` (and `retry_after` in the body) in seconds, whether or not the account exists;
/// no turn is `503 busy`. The failure event of a known account is recorded after the answer is
/// decided, so the answer takes the same work for an unknown email and a wrong password.
pub(crate) async fn sign_in(
    State(state): State<AppState>,
    source: Option<Extension<Source>>,
    JsonBody(credentials): JsonBody<Credentials>,
) -> Result<Response, ApiError> {
    let now = Instant::now();
    let client = source.map_or(IpAddr::V4(Ipv4Addr::LOCALHOST), |s| s.0.client_ip);
    let address = Throttle::address_key(client);
    // Each attempt is held until it ends, also if the request is cancelled mid-check.
    let Some(by_address) = state.address_throttle().try_begin(&address, now) else {
        return Ok(too_many_attempts(
            state.address_throttle().retry_after(&address, now),
        ));
    };
    let key = Throttle::key(&credentials.email);
    let Some(attempt) = state.throttle().try_begin(&key, now) else {
        return Ok(too_many_attempts(state.throttle().retry_after(&key, now)));
    };
    let Some(_turn) = state.password_checks().turn().await else {
        return Ok(busy());
    };
    match state
        .db()
        .sign_in_unrecorded(&credentials.email, &credentials.password)
        .await
    {
        Ok((_, token)) => {
            attempt.succeeded();
            // A right password does not wipe the address's count: one account of one's own
            // must not reset the guesses at others'.
            drop(by_address);
            Ok((
                StatusCode::NO_CONTENT,
                [(SET_COOKIE, session_cookie(state.deployment(), &token))],
            )
                .into_response())
        }
        Err(failure) => match failure.error {
            SignInError::Invalid => {
                let now = Instant::now();
                let account_locked = attempt.failed(now);
                let address_locked = by_address.failed(now);
                let subject = failure.event.as_ref().and_then(|event| event.subject);
                if let Some(event) = failure.event {
                    state.events().emit(*event);
                }
                if account_locked {
                    record_lock(&state, "account", subject, client);
                }
                if address_locked {
                    record_lock(&state, "address", None, client);
                }
                Err(ApiError::InvalidCredentials)
            }
            SignInError::Db(e) => Err(ApiError::unavailable(&e)),
        },
    }
}

/// `429 too_many_attempts`, saying in whole seconds how long to wait.
fn too_many_attempts(wait: Duration) -> Response {
    let seconds = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
    (
        StatusCode::TOO_MANY_REQUESTS,
        [(RETRY_AFTER, seconds.to_string())],
        Json(json!({ "error": "too_many_attempts", "retry_after": seconds })),
    )
        .into_response()
}

/// `503 busy`: every password check is taken.
fn busy() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(RETRY_AFTER, PASSWORD_WAIT.as_secs().to_string())],
        Json(json!({ "error": "busy" })),
    )
        .into_response()
}

/// Records `sign_in.locked`, once, when an account's or an address's failures reach the limit.
fn record_lock(
    state: &AppState,
    by: &str,
    subject: Option<collab_core::ids::UserId>,
    client: IpAddr,
) {
    let until = OffsetDateTime::now_utc() + WINDOW;
    let until = until
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();
    state.events().emit(NewEvent {
        kind: "sign_in.locked",
        actor: None,
        subject,
        document: None,
        node: None,
        detail: json!({ "by": by, "until": until, "address": client.to_string() }),
    });
}

/// `DELETE /api/session`: ends the caller's session and clears the cookie.
pub(crate) async fn sign_out(
    State(state): State<AppState>,
    signed: Signed,
) -> Result<Response, ApiError> {
    let session = signed.session.id;
    let cleared = cleared_cookie(state.deployment());
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
    Ok((StatusCode::NO_CONTENT, [(SET_COOKIE, cleared)]).into_response())
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
