//! The JSON API under `/api`. Every request must pass [`same_origin`], and any request other
//! than `GET` must also carry `Origin`. Errors are `{"error": "<code>"}`.

mod admin;
mod docs;
mod session;

use std::fmt;

use axum::extract::{DefaultBodyLimit, FromRequest, Request};
use axum::http::header::ORIGIN;
use axum::http::{Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use collab_core::ids::{DocId, SessionId, UserId};
use records::{AccessError, AccountError};
use serde::de::DeserializeOwned;
use serde_json::json;
use uuid::Uuid;

use crate::AppState;
use crate::auth::same_origin;
use crate::room::Command;

/// Largest request body accepted.
const MAX_BODY: usize = 16 * 1024;

/// The `/api` routes, to be nested under `/api`. Unknown paths answer `404 {"error":"not_found"}`
/// and pass the same guard as the others.
pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/session", post(session::sign_in).delete(session::sign_out))
        .route("/me", get(session::me))
        .route("/docs", get(docs::list).post(docs::create))
        .route("/docs/{id}", get(docs::open))
        .route("/docs/{id}/access", get(docs::access).put(docs::share))
        .route("/docs/{id}/access/{user_id}", delete(docs::unshare))
        .route("/admin/users", get(admin::users).post(admin::create))
        .route("/admin/users/{id}/disable", post(admin::disable))
        .route("/admin/users/{id}/enable", post(admin::enable))
        .fallback(|| async { ApiError::NotFound })
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .layer(middleware::from_fn(require_same_origin))
}

/// Refuses requests from other origins, and requests that change something without `Origin`.
async fn require_same_origin(request: Request, next: Next) -> Response {
    let headers = request.headers();
    let origin_needed = request.method() != Method::GET;
    if !same_origin(headers) || (origin_needed && !headers.contains_key(ORIGIN)) {
        return ApiError::Forbidden.into_response();
    }
    next.run(request).await
}

/// An API failure, answered as `{"error": "<code>"}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApiError {
    /// 400: the body is not the JSON expected.
    BadRequest,
    /// 401: no valid session.
    NotSignedIn,
    /// 401: wrong email or password.
    InvalidCredentials,
    /// 403: the request failed the same-origin check.
    Forbidden,
    /// 404: no such API route, document or account, or none the caller may see.
    NotFound,
    /// 404: no account has the email given.
    NoSuchUser,
    /// 409: the email is already in use.
    EmailTaken,
    /// 409: a document must keep one owner.
    LastOwner,
    /// 409: one administrator must stay enabled.
    LastAdmin,
    /// 422: a well-formed request with a value that makes no sense, such as an unknown role or
    /// an id that is not an id.
    InvalidRequest,
    /// 422: not an acceptable document or person name.
    InvalidName,
    /// 422: not an acceptable email address.
    InvalidEmail,
    /// 422: the password is too short or too long.
    WeakPassword,
    /// 413: the body is larger than 16 KiB.
    TooLarge,
    /// 415: the body is not `application/json`.
    NotJson,
    /// 429: too many failed sign-ins for this email.
    TooManyAttempts,
    /// 503: the database failed.
    Unavailable,
}

impl ApiError {
    /// A database failure: logs `cause` to standard error and answers `503`.
    ///
    /// `cause` must not contain the database URL; [`records::DbError`] does not.
    pub(crate) fn unavailable(cause: &dyn fmt::Display) -> ApiError {
        eprintln!("sharecraft-server: database failure: {cause}");
        ApiError::Unavailable
    }

    fn status_and_code(self) -> (StatusCode, &'static str) {
        match self {
            ApiError::BadRequest => (StatusCode::BAD_REQUEST, "bad_request"),
            ApiError::NotSignedIn => (StatusCode::UNAUTHORIZED, "not_signed_in"),
            ApiError::InvalidCredentials => (StatusCode::UNAUTHORIZED, "invalid_credentials"),
            ApiError::Forbidden => (StatusCode::FORBIDDEN, "forbidden"),
            ApiError::NotFound => (StatusCode::NOT_FOUND, "not_found"),
            ApiError::NoSuchUser => (StatusCode::NOT_FOUND, "no_such_user"),
            ApiError::EmailTaken => (StatusCode::CONFLICT, "email_taken"),
            ApiError::LastOwner => (StatusCode::CONFLICT, "last_owner"),
            ApiError::LastAdmin => (StatusCode::CONFLICT, "last_admin"),
            ApiError::InvalidRequest => (StatusCode::UNPROCESSABLE_ENTITY, "invalid_request"),
            ApiError::InvalidName => (StatusCode::UNPROCESSABLE_ENTITY, "invalid_name"),
            ApiError::InvalidEmail => (StatusCode::UNPROCESSABLE_ENTITY, "invalid_email"),
            ApiError::WeakPassword => (StatusCode::UNPROCESSABLE_ENTITY, "weak_password"),
            ApiError::TooLarge => (StatusCode::PAYLOAD_TOO_LARGE, "too_large"),
            ApiError::NotJson => (StatusCode::UNSUPPORTED_MEDIA_TYPE, "not_json"),
            ApiError::TooManyAttempts => (StatusCode::TOO_MANY_REQUESTS, "too_many_attempts"),
            ApiError::Unavailable => (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
        }
    }
}

impl From<AccessError> for ApiError {
    fn from(e: AccessError) -> Self {
        match e {
            AccessError::NotFound => ApiError::NotFound,
            AccessError::NoSuchUser => ApiError::NoSuchUser,
            AccessError::Forbidden => ApiError::Forbidden,
            AccessError::LastOwner => ApiError::LastOwner,
            AccessError::InvalidName => ApiError::InvalidName,
            // The server picks a fresh random id, so a clash is not the caller's doing.
            AccessError::Exists | AccessError::Db(_) => ApiError::unavailable(&e),
        }
    }
}

impl From<AccountError> for ApiError {
    fn from(e: AccountError) -> Self {
        match e {
            AccountError::EmailTaken => ApiError::EmailTaken,
            AccountError::InvalidEmail => ApiError::InvalidEmail,
            AccountError::InvalidName => ApiError::InvalidName,
            AccountError::WeakPassword => ApiError::WeakPassword,
            AccountError::NotFound => ApiError::NotFound,
            AccountError::Forbidden => ApiError::Forbidden,
            AccountError::LastAdmin => ApiError::LastAdmin,
            AccountError::Db(_) => ApiError::unavailable(&e),
        }
    }
}

/// An id from a path, or `422 invalid_request` when it is not one.
fn parse_id(text: &str) -> Result<Uuid, ApiError> {
    Uuid::parse_str(text).map_err(|_| ApiError::InvalidRequest)
}

/// Called after `user`'s access to `doc` was granted, changed or removed, once the change is
/// stored. Reads their role afresh, so an administrator who loses a grant stays an owner, and
/// tells the document's room: connections that lost access are removed, the others get their
/// new rights. If the role cannot be read, their connections are removed, as access can no
/// longer be confirmed.
async fn after_access_change(state: &AppState, doc: DocId, user: UserId) {
    let role = match state.db().role(user, doc).await {
        Ok(role) => role,
        Err(e) => {
            eprintln!("sharecraft-server: database failure: {e}");
            None
        }
    };
    state
        .rooms()
        .send_to(doc, Command::SetAccess { user, role })
        .await;
}

/// Called after `user`'s account was disabled: their connections to every document are removed.
async fn after_account_disabled(state: &AppState, user: UserId) {
    state
        .rooms()
        .send_all(|| Command::RemoveUser { user })
        .await;
}

/// Called after `session` was ended by its owner signing out: that session's connections are
/// removed; the same person's other sign-ins stay.
async fn after_sign_out(state: &AppState, session: SessionId) {
    state
        .rooms()
        .send_all(|| Command::EndSession { session })
        .await;
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code) = self.status_and_code();
        (status, Json(json!({ "error": code }))).into_response()
    }
}

/// A JSON request body, refused with an [`ApiError`] when it is not `application/json`, is
/// larger than 16 KiB or does not parse.
pub(crate) struct JsonBody<T>(pub T);

impl<T, S> FromRequest<S> for JsonBody<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Self, ApiError> {
        match Json::<T>::from_request(request, state).await {
            Ok(Json(value)) => Ok(JsonBody(value)),
            Err(rejection) => Err(match rejection.status() {
                StatusCode::PAYLOAD_TOO_LARGE => ApiError::TooLarge,
                StatusCode::UNSUPPORTED_MEDIA_TYPE => ApiError::NotJson,
                _ => ApiError::BadRequest,
            }),
        }
    }
}
