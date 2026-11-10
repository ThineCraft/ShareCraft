//! The JSON API under `/api`. Every request must pass [`same_origin`], and any request other
//! than `GET` must also carry `Origin`. Errors are `{"error": "<code>"}`.

mod admin;
mod docs;
mod session;

use std::fmt;

use axum::extract::{DefaultBodyLimit, FromRequest, Request};
use axum::http::header::{CACHE_CONTROL, ORIGIN};
use axum::http::{HeaderValue, Method, StatusCode};
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
        .route("/docs/{id}/events", get(docs::events))
        .route("/docs/{id}/access/{user_id}", delete(docs::unshare))
        .route("/admin/users", get(admin::users).post(admin::create))
        .route("/admin/users/{id}/disable", post(admin::disable))
        .route("/admin/users/{id}/enable", post(admin::enable))
        .fallback(|| async { ApiError::NotFound })
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .layer(middleware::from_fn(require_same_origin))
        .layer(middleware::from_fn(no_store))
}

/// Keeps every `/api` answer, errors included, out of caches.
async fn no_store(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
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
///
/// The read and the message to the room happen under the document's access lock, so that of
/// two changes made at once the room hears the later reading last.
async fn after_access_change(state: &AppState, doc: DocId, user: UserId) {
    let rooms = state.rooms();
    let _in_order = rooms.order_access(doc).await;
    let role = match state.db().role(user, doc).await {
        Ok(role) => role,
        Err(e) => {
            eprintln!("sharecraft-server: database failure: {e}");
            None
        }
    };
    rooms.send_to(doc, Command::SetAccess { user, role });
}

/// Called after `user`'s account was disabled: their connections to every document are removed.
/// Removal is final, so it needs no ordering with other access changes.
fn after_account_disabled(state: &AppState, user: UserId) {
    state.rooms().send_all(|| Command::RemoveUser { user });
}

/// Called after `session` was ended by its owner signing out: that session's connections are
/// removed; the same person's other sign-ins stay.
fn after_sign_out(state: &AppState, session: SessionId) {
    state.rooms().send_all(|| Command::EndSession { session });
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

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use collab_core::ids::ClientId;
    use collab_text::TextOp;
    use protocol::ServerMsg;
    use records::test_support::TestDb;
    use records::{NewUser, Role};
    use revisions::RevisionStore;
    use tokio::sync::{mpsc, oneshot};

    use super::*;
    use crate::config::{Config, DatabaseUrl};
    use crate::room::{JoinReply, OUTBOUND_CAPACITY, Outbound};

    async fn account(db: &records::Db, name: &str) -> UserId {
        db.create_user(
            None,
            NewUser {
                email: &format!("{name}@test.invalid"),
                name,
                password: "a long enough password",
                is_admin: false,
            },
        )
        .await
        .unwrap()
        .id
    }

    /// Two changes to Bob's role are stored one after the other, but the hook of the first
    /// is slower: it has read the role and not yet told the room when the second change is
    /// stored and its hook runs. The room must end with the role stored last.
    #[tokio::test]
    async fn access_changes_reach_the_room_in_database_order() {
        let test = TestDb::create().await;
        let db = test.db.clone();
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            addr: "127.0.0.1:0".parse().unwrap(),
            data_dir: dir.path().to_path_buf(),
            web_dir: dir.path().join("web"),
            checkpoint_idle: Duration::from_secs(600),
            checkpoint_max: Duration::from_secs(600),
            room_grace: Duration::from_secs(60),
            database_url: DatabaseUrl::new(test.url()),
        };
        let store = RevisionStore::open(&crate::library_path(dir.path())).unwrap();
        let state = AppState::new(config, Arc::new(store), db.clone());
        let ann = account(&db, "Ann").await;
        let bob = account(&db, "Bob").await;
        let doc = DocId(Uuid::new_v4());
        db.create_document(ann, doc, "Plan.txt").await.unwrap();
        db.set_access(ann, doc, bob, Some(Role::Viewer))
            .await
            .unwrap();

        // Bob is in the room as a viewer.
        let room = state.open_room(doc, "Plan.txt".into());
        let client = ClientId(Uuid::new_v4());
        let (tx, mut out) = mpsc::channel(OUTBOUND_CAPACITY);
        let (accepted, reply) = oneshot::channel();
        room.commands
            .send(Command::Join {
                conn: 1,
                client,
                user: bob,
                session: SessionId(Uuid::new_v4()),
                name: "Bob".into(),
                role: Role::Viewer,
                resume: None,
                tx,
                accepted,
            })
            .await
            .unwrap();
        assert_eq!(reply.await.unwrap(), JoinReply::Accepted);

        // The first change, made the way `after_access_change` makes it, paused between
        // reading the role and telling the room.
        let first = state.rooms().order_access(doc).await;
        db.set_access(ann, doc, bob, Some(Role::Editor))
            .await
            .unwrap();
        let first_read = db.role(bob, doc).await.unwrap();
        assert_eq!(first_read, Some(Role::Editor));

        // The second change is stored and its hook runs meanwhile.
        db.set_access(ann, doc, bob, Some(Role::Viewer))
            .await
            .unwrap();
        let mut second = tokio::spawn({
            let state = state.clone();
            async move { after_access_change(&state, doc, bob).await }
        });
        // Long enough for the second hook to finish if nothing holds it back.
        let finished_early = tokio::time::timeout(Duration::from_millis(500), &mut second)
            .await
            .is_ok();

        // The first hook carries on.
        state.rooms().send_to(
            doc,
            Command::SetAccess {
                user: bob,
                role: first_read,
            },
        );
        drop(first);
        if !finished_early {
            second.await.unwrap();
        }

        // A cursor move after both changes marks where they end in Bob's frames.
        room.commands
            .send(Command::Cursor {
                conn: 1,
                client,
                at: Some(1),
            })
            .await
            .unwrap();
        let mut can_edit = None;
        let mut listed = None;
        loop {
            let frames = match tokio::time::timeout(Duration::from_secs(5), out.recv())
                .await
                .unwrap()
                .unwrap()
            {
                Outbound::One(f) => vec![f],
                Outbound::Many(fs) => fs,
            };
            let mut marked = false;
            for frame in frames {
                match serde_json::from_str::<ServerMsg<TextOp>>(frame.as_str()).unwrap() {
                    ServerMsg::Joined { can_edit: c, .. } | ServerMsg::Access { can_edit: c } => {
                        can_edit = Some(c);
                    }
                    ServerMsg::People { participants } => {
                        if let Some(p) = participants.iter().find(|p| p.client == client) {
                            listed = Some(p.can_edit);
                            marked |= p.cursor == Some(1);
                        }
                    }
                    _ => {}
                }
            }
            if marked {
                break;
            }
        }
        let stored = db.role(bob, doc).await.unwrap().unwrap();
        assert_eq!(stored, Role::Viewer);
        assert_eq!(
            can_edit,
            Some(stored.can_edit()),
            "Bob's rights in the room"
        );
        assert_eq!(listed, Some(stored.can_edit()), "Bob in the people list");
        assert!(!finished_early, "the later change was not held back");
    }
}
