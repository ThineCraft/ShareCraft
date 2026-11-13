//! The JSON API under `/api`. Every request must pass [`same_origin`], and any request other
//! than `GET` must also carry `Origin`. Errors are `{"error": "<code>"}`.

mod admin;
mod content;
mod docs;
mod explorer;
mod groups;
mod nodes;
mod roles;
mod session;
mod sites;

use std::fmt;

use axum::extract::{DefaultBodyLimit, FromRequest, Request};
use axum::http::header::{CACHE_CONTROL, ORIGIN};
use axum::http::{HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post, put};
use axum::{Json, Router};
use collab_core::ids::{DocId, NodeId, SessionId, UserId};
use records::{AccessError, AccountError};
use serde::de::DeserializeOwned;
use serde_json::json;
use uuid::Uuid;

use crate::AppState;
use crate::auth::same_origin;
use crate::room::AccessChange;

/// Largest request body accepted.
const MAX_BODY: usize = 16 * 1024;

/// The `/api` routes, to be nested under `/api`. Unknown paths answer `404 {"error":"not_found"}`
/// and pass the same guard as the others. File content (`max_file_bytes` at most per request) is
/// the only thing allowed a body larger than [`MAX_BODY`].
pub(crate) fn router(max_file_bytes: usize) -> Router<AppState> {
    Router::new()
        .route("/session", post(session::sign_in).delete(session::sign_out))
        .route("/me", get(session::me))
        .route("/editors", get(crate::editors::list))
        .route("/sites", get(sites::list).post(sites::create))
        .route("/sites/{id}/groups", get(sites::groups))
        .route("/roles", get(roles::list).post(roles::create))
        .route("/roles/{id}", patch(roles::change).delete(roles::remove))
        .route("/groups", get(groups::list).post(groups::create))
        .route("/groups/{id}", get(groups::show).delete(groups::remove))
        .route("/groups/{id}/members", post(groups::add_by_email))
        .route(
            "/groups/{id}/members/{user_id}",
            put(groups::add_member).delete(groups::remove_member),
        )
        .route(
            "/nodes/{id}",
            get(nodes::show).patch(nodes::change).delete(nodes::remove),
        )
        .route(
            "/nodes/{id}/children",
            get(nodes::children).post(nodes::create),
        )
        .route("/nodes/{id}/access", get(nodes::access).put(nodes::share))
        .route(
            "/nodes/{id}/access/{kind}/{principal_id}",
            delete(nodes::unshare),
        )
        .route("/nodes/{id}/inheritance", post(nodes::inheritance))
        .route("/nodes/{id}/inheritance/preview", get(explorer::preview))
        .route("/explorer/{root}", get(explorer::breaks))
        .route("/explorer/{root}/counts", get(explorer::counts))
        .route("/explorer/{root}/export.csv", get(explorer::export))
        .route("/docs", post(docs::create))
        .route(
            "/docs/upload",
            post(content::upload).layer(DefaultBodyLimit::max(max_file_bytes)),
        )
        .route("/docs/{id}", get(docs::open))
        .route(
            "/docs/{id}/content",
            get(content::download)
                .put(content::save)
                .layer(DefaultBodyLimit::max(max_file_bytes)),
        )
        .route("/docs/{id}/access", get(nodes::access).put(nodes::share))
        .route("/docs/{id}/events", get(docs::events))
        .route(
            "/docs/{id}/access/{kind}/{principal_id}",
            delete(nodes::unshare),
        )
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
    /// 409: a scope must keep someone who can manage access.
    LastManager,
    /// 403: the role has permissions the caller does not hold there.
    BeyondYourAccess,
    /// 409: the role is still assigned somewhere.
    RoleInUse,
    /// 400: a built-in role cannot be changed or deleted.
    BuiltinRole,
    /// 409: the group is still assigned somewhere.
    GroupInUse,
    /// 400: a site's groups cannot be deleted.
    SiteGroup,
    /// 409: another item in the same place already has that name (ignoring case).
    NameTaken,
    /// 409: a folder still holds items.
    NotEmpty,
    /// 400: a folder cannot be moved into itself or below itself.
    IntoItself,
    /// 400: that kind of item cannot go there, or cannot be moved or deleted.
    WrongPlace,
    /// 400: a site cannot be deleted, and cannot inherit.
    IsSite,
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
    /// 413: the body is larger than the route allows (16 KiB; file content, `max_file_bytes`).
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
            ApiError::LastManager => (StatusCode::CONFLICT, "last_manager"),
            ApiError::BeyondYourAccess => (StatusCode::FORBIDDEN, "beyond_your_access"),
            ApiError::RoleInUse => (StatusCode::CONFLICT, "role_in_use"),
            ApiError::BuiltinRole => (StatusCode::BAD_REQUEST, "builtin_role"),
            ApiError::GroupInUse => (StatusCode::CONFLICT, "group_in_use"),
            ApiError::SiteGroup => (StatusCode::BAD_REQUEST, "site_group"),
            ApiError::NameTaken => (StatusCode::CONFLICT, "name_taken"),
            ApiError::NotEmpty => (StatusCode::CONFLICT, "not_empty"),
            ApiError::IntoItself => (StatusCode::BAD_REQUEST, "into_itself"),
            ApiError::WrongPlace => (StatusCode::BAD_REQUEST, "wrong_place"),
            ApiError::IsSite => (StatusCode::BAD_REQUEST, "is_site"),
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
            AccessError::NoSuchGroup => ApiError::NotFound,
            // As an unknown role name was before roles could be defined.
            AccessError::NoSuchRole => ApiError::InvalidRequest,
            AccessError::Forbidden => ApiError::Forbidden,
            AccessError::LastManager => ApiError::LastManager,
            AccessError::BeyondYourAccess => ApiError::BeyondYourAccess,
            AccessError::RoleInUse => ApiError::RoleInUse,
            AccessError::BuiltinRole => ApiError::BuiltinRole,
            AccessError::GroupInUse => ApiError::GroupInUse,
            AccessError::SiteGroup => ApiError::SiteGroup,
            // Answered with their own codes once the link and request routes exist.
            AccessError::LinkGroup
            | AccessError::LinkRoleTooHigh
            | AccessError::AnyoneLinksOff
            | AccessError::ExpiryRequired
            | AccessError::ExpiryTooLong
            | AccessError::ExpiryInPast
            | AccessError::AlreadyDecided
            | AccessError::AlreadyHas
            | AccessError::MessageTooLong
            | AccessError::InvalidSetting => ApiError::InvalidRequest,
            AccessError::InvalidName => ApiError::InvalidName,
            // `POST /api/docs` answers this itself, naming the extension expected.
            AccessError::WrongExtension => ApiError::InvalidRequest,
            AccessError::NameTaken => ApiError::NameTaken,
            AccessError::WrongPlace => ApiError::WrongPlace,
            AccessError::IntoItself => ApiError::IntoItself,
            AccessError::NotEmpty => ApiError::NotEmpty,
            AccessError::IsSite => ApiError::IsSite,
            // The server picks a fresh random id, so a clash is not the caller's doing.
            AccessError::Exists | AccessError::Db(_) | AccessError::Unjudged(_) => {
                ApiError::unavailable(&e)
            }
            // The tree is being repaired; a later attempt can succeed.
            AccessError::OutOfStep => ApiError::Unavailable,
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

/// Runs `change` (a database write and the hook that tells the rooms about it, or a save) on a
/// task of its own and waits for it. A request whose caller goes away is dropped at its next
/// `.await`; the task is not, so a write that was stored always reaches the rooms. A task that
/// fails to finish answers `503`.
async fn to_completion<T: Send + 'static>(
    change: impl Future<Output = Result<T, ApiError>> + Send + 'static,
) -> Result<T, ApiError> {
    match tokio::spawn(change).await {
        Ok(result) => result,
        Err(e) => {
            eprintln!("sharecraft-server: a change did not finish: {e}");
            Err(ApiError::Unavailable)
        }
    }
}

/// Called once a change that can alter access at or below `node` is stored: an assignment,
/// a break or restore of inheritance on `node`, or a move of `node`. Rechecks (see
/// [`recheck`]) every open document at or below `node`; if it cannot be told whether a
/// document is below `node`, it is checked anyway.
///
/// Callers run it inside [`to_completion`], so a cancelled request still revokes.
pub(crate) async fn recheck_below(state: &AppState, node: NodeId) {
    let db = state.db();
    let mut below = Vec::new();
    for doc in state.rooms().open_documents() {
        match db.is_at_or_below(doc.into(), node).await {
            Ok(true) => below.push(doc),
            Ok(false) => {}
            Err(e) => {
                eprintln!("sharecraft-server: database failure: {e}");
                below.push(doc);
            }
        }
    }
    recheck(state, below, None).await;
}

/// Called once a change of `user`'s memberships is stored: rechecks (see [`recheck`]) `user`
/// in every open document, wherever they are connected.
///
/// Callers run it inside [`to_completion`], so a cancelled request still revokes.
pub(crate) async fn recheck_person(state: &AppState, user: UserId) {
    let open = state.rooms().open_documents();
    recheck(state, open, Some(user)).await;
}

/// Called once a change of a role's permissions is stored: the role may be held anywhere, so
/// everyone in every open document is rechecked (see [`recheck`]).
///
/// Callers run it inside [`to_completion`], so a cancelled request still revokes.
pub(crate) async fn recheck_everyone(state: &AppState) {
    let open = state.rooms().open_documents();
    recheck(state, open, None).await;
}

/// For each of `docs`, one at a time in id order, reads the access of each person connected
/// to it (only of `only`, when given) afresh (so an administrator who loses a grant keeps every
/// permission) and tells the room: connections that lost access are removed, the others get
/// their new rights. If access cannot be read, that person's connections are removed, as it
/// can no longer be confirmed.
///
/// Each document's readings and messages happen under its access lock, so that of two changes
/// made at once the room hears the later reading last. One lock is held at a time.
async fn recheck(state: &AppState, mut docs: Vec<DocId>, only: Option<UserId>) {
    let rooms = state.rooms();
    let db = state.db();
    docs.sort_by_key(|doc| doc.0);
    docs.dedup();
    for doc in docs {
        let _in_order = rooms.order_access(doc).await;
        for user in rooms.connected_users(doc).await {
            if only.is_some_and(|only| only != user) {
                continue;
            }
            let access = match db.effective_access(user, doc.into()).await {
                Ok(access) => access,
                Err(e) => {
                    eprintln!("sharecraft-server: database failure: {e}");
                    None
                }
            };
            rooms.send_to(doc, AccessChange::SetAccess { user, role: access });
        }
    }
}

/// Called after `user`'s account was disabled: their connections to every document are removed.
/// Removal is final, so it needs no ordering with other access changes.
fn after_account_disabled(state: &AppState, user: UserId) {
    state.rooms().send_all(|| AccessChange::RemoveUser { user });
}

/// Called after `session` was ended by its owner signing out: that session's connections are
/// removed; the same person's other sign-ins stay.
fn after_sign_out(state: &AppState, session: SessionId) {
    state
        .rooms()
        .send_all(|| AccessChange::EndSession { session });
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

    use crate::members::builtin_access;
    use collab_core::ids::{ClientId, DocId};
    use collab_text::TextOp;
    use protocol::ServerMsg;
    use records::test_support::TestDb;
    use records::{Editor, NewUser, NodeKind, Principal};
    use revisions::RevisionStore;
    use tokio::sync::{mpsc, oneshot};

    use super::*;
    use crate::config::{Config, DatabaseUrl};
    use crate::room::{Command, JoinReply, OUTBOUND_CAPACITY, Outbound};
    use crate::tree::{self, TreeOp};

    async fn account(db: &records::Db, name: &str) -> UserId {
        user(db, name, false).await.id
    }

    async fn user(db: &records::Db, name: &str, is_admin: bool) -> records::User {
        db.create_user(
            None,
            NewUser {
                email: &format!("{name}@test.invalid"),
                name,
                password: "a long enough password",
                is_admin,
            },
        )
        .await
        .unwrap()
    }

    /// A text document named Plan.txt in the library of a new site, which an administrator
    /// made; `owner` owns the document.
    async fn placed_document(state: &AppState, owner: UserId) -> DocId {
        let db = state.db();
        let root = user(db, "Root", true).await;
        let site = tree::apply(
            state,
            &root,
            TreeOp::Create {
                parent: None,
                kind: NodeKind::Site,
                name: "Site".into(),
                id: NodeId(Uuid::new_v4()),
            },
        )
        .await
        .unwrap();
        let (children, _) = db
            .children(root.id, site.id, None, 10)
            .await
            .unwrap()
            .unwrap();
        let library = children[0].node.id;
        let doc = DocId(Uuid::new_v4());
        tree::create_document(state, &root, library, doc, "Plan.txt", Editor::Text, None)
            .await
            .unwrap();
        db.set_access(root.id, doc, Principal::User(owner), Some("owner"))
            .await
            .unwrap();
        doc
    }

    /// A server's state on `test`'s database, storing documents under `dir`; no listener.
    fn state_for(test: &TestDb, dir: &std::path::Path) -> AppState {
        let config = Config {
            addr: "127.0.0.1:0".parse().unwrap(),
            data_dir: dir.to_path_buf(),
            web_dir: dir.join("web"),
            checkpoint_idle: Duration::from_secs(600),
            checkpoint_max: Duration::from_secs(600),
            room_grace: Duration::from_secs(60),
            write_timeout: Duration::from_secs(10),
            database_url: DatabaseUrl::new(test.url()),
            editors_dir: dir.join("editors"),
            max_file_bytes: 64 << 20,
        };
        let store = RevisionStore::open(&crate::library_path(dir)).unwrap();
        AppState::new(config, Arc::new(store), test.db.clone())
    }

    /// The owner's request to remove Bob's grant is cancelled (its future dropped) after the
    /// grant is deleted but while its hook waits to tell the room. Bob must still be removed.
    #[tokio::test]
    async fn cancelled_revoke_still_removes_the_person() {
        let test = TestDb::create().await;
        let db = test.db.clone();
        let dir = tempfile::tempdir().unwrap();
        let state = state_for(&test, dir.path());
        let ann = account(&db, "Ann").await;
        let bob = account(&db, "Bob").await;
        let doc = placed_document(&state, ann).await;
        db.set_access(ann, doc, Principal::User(bob), Some("editor"))
            .await
            .unwrap();
        let room = state.open_room(doc, "Plan.txt".into());
        let (tx, mut out) = mpsc::channel(OUTBOUND_CAPACITY);
        let (accepted, reply) = oneshot::channel();
        room.commands
            .send(Command::Join {
                conn: 1,
                client: ClientId(Uuid::new_v4()),
                user: bob,
                session: SessionId(Uuid::new_v4()),
                name: "Bob".into(),
                role: builtin_access("editor"),
                resume: None,
                tx,
                accepted,
            })
            .await
            .unwrap();
        assert_eq!(reply.await.unwrap(), JoinReply::Accepted);

        // Held so that the hook waits after the grant is deleted.
        let held = state.rooms().order_access(doc).await;
        let mut request = Box::pin(nodes::change_access(
            state.clone(),
            ann,
            doc.into(),
            Principal::User(bob),
            None,
        ));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            tokio::select! {
                _ = &mut request => panic!("the request finished while the hook could not run"),
                () = tokio::time::sleep(Duration::from_millis(20)) => {}
            }
            if db.doc_access(bob, doc).await.unwrap().is_none() {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the grant was never deleted"
            );
        }
        // The caller goes away.
        drop(request);
        drop(held);

        let removed = encode_removed(protocol::RemovedReason::AccessRevoked);
        loop {
            let next = tokio::time::timeout(Duration::from_secs(5), out.recv())
                .await
                .expect("Bob was not removed");
            let frames = match next {
                Some(Outbound::One(f)) => vec![f],
                Some(Outbound::Many(fs)) => fs,
                None => panic!("Bob's connection ended without Removed"),
            };
            if frames.iter().any(|f| f.as_str() == removed) {
                break;
            }
        }
    }

    fn encode_removed(reason: protocol::RemovedReason) -> String {
        serde_json::to_string(&ServerMsg::<TextOp>::Removed { reason }).unwrap()
    }

    /// Two changes to Bob's role are stored one after the other, but the hook of the first
    /// is slower: it has read the role and not yet told the room when the second change is
    /// stored and its hook runs. The room must end with the role stored last.
    #[tokio::test]
    async fn access_changes_reach_the_room_in_database_order() {
        let test = TestDb::create().await;
        let db = test.db.clone();
        let dir = tempfile::tempdir().unwrap();
        let state = state_for(&test, dir.path());
        let ann = account(&db, "Ann").await;
        let bob = account(&db, "Bob").await;
        let doc = placed_document(&state, ann).await;
        db.set_access(ann, doc, Principal::User(bob), Some("viewer"))
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
                role: builtin_access("viewer"),
                resume: None,
                tx,
                accepted,
            })
            .await
            .unwrap();
        assert_eq!(reply.await.unwrap(), JoinReply::Accepted);

        // The first change, made the way `recheck_below` makes it, paused between
        // reading the role and telling the room.
        let first = state.rooms().order_access(doc).await;
        db.set_access(ann, doc, Principal::User(bob), Some("editor"))
            .await
            .unwrap();
        let first_read = db.doc_access(bob, doc).await.unwrap();
        assert_eq!(first_read, Some(builtin_access("editor")));

        // The second change is stored and its hook runs meanwhile.
        db.set_access(ann, doc, Principal::User(bob), Some("viewer"))
            .await
            .unwrap();
        let mut second = tokio::spawn({
            let state = state.clone();
            async move { recheck_below(&state, doc.into()).await }
        });
        // Long enough for the second hook to finish if nothing holds it back.
        let finished_early = tokio::time::timeout(Duration::from_millis(500), &mut second)
            .await
            .is_ok();

        // The first hook carries on.
        state.rooms().send_to(
            doc,
            AccessChange::SetAccess {
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
        let stored = db.doc_access(bob, doc).await.unwrap().unwrap();
        assert_eq!(stored, builtin_access("viewer"));
        assert_eq!(
            can_edit,
            Some(stored.can_edit()),
            "Bob's rights in the room"
        );
        assert_eq!(listed, Some(stored.can_edit()), "Bob in the people list");
        assert!(!finished_early, "the later change was not held back");
    }
}
