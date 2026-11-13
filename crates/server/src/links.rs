//! What a sharing link's address reaches, and the sweeper that revokes expired links.
//!
//! - `GET /l/{token}` serves the landing page (`link.html`) whatever the token; the page asks
//!   `GET /api/l/{token}` what the link is.
//! - `POST /api/l/{token}/redeem` opens a signed-in link for the signed-in caller.
//! - `GET /api/l/{token}/children/{node}`, `…/content/{node}` and `…/text/{node}` are the
//!   anonymous routes: they take an anyone link's token only, check it on every request
//!   ([`records::Db::anyone_link`]), and never read or set the session cookie.
//!
//! Every answer under `/l/` and `/api/l/` carries `Cache-Control: no-store` and
//! `Referrer-Policy: no-referrer` (see [`link_headers`]). A link that does not work, for
//! whatever reason, is `404 link_not_found`.

use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, Request, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, REFERRER_POLICY, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use collab_core::ids::{DocId, NodeId};
use records::{Document, Editor, LinkKind, LinkToken, Node, NodeKind};
use serde::Serialize;
use serde_json::json;
use time::OffsetDateTime;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::AppState;
use crate::api::content::{blocking, file_answer};
use crate::api::nodes::{ChildrenPage, ChildrenQuery, node_id, rfc3339};
use crate::api::{ApiError, recheck_below, recheck_person, to_completion};
use crate::auth::{MaybeSigned, Signed};

/// How often the sweeper revokes expired links.
const SWEEP_EVERY: Duration = Duration::from_secs(60);

/// The routes under `/api/l`, to be nested there in the API's router (so that they pass its
/// same-origin guard). Unknown paths below it answer `404 link_not_found`.
pub(crate) fn api_router() -> Router<AppState> {
    Router::new()
        .route("/{token}", get(what))
        .route("/{token}/redeem", post(redeem))
        .route("/{token}/children/{node}", get(children))
        .route("/{token}/content/{node}", get(content))
        .route("/{token}/text/{node}", get(text))
        .fallback(|| async { ApiError::LinkNotFound })
}

/// Whether `path` is one a link's address reaches: a landing page or an `/api/l/` route.
fn is_link_path(path: &str) -> bool {
    path.starts_with("/l/") || path.starts_with("/api/l/")
}

/// Keeps every answer under `/l/` and `/api/l/`, errors included, out of caches and out of
/// `Referer` headers (the token is in the address), and lets none of them set a cookie. Applied
/// inside [`crate::security_headers`], which then leaves these headers as they are.
pub(crate) async fn link_headers(request: Request, next: Next) -> Response {
    let link = is_link_path(request.uri().path());
    let mut response = next.run(request).await;
    if link {
        let headers = response.headers_mut();
        headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
        headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
        headers.remove(SET_COOKIE);
    }
    response
}

/// `GET /l/{token}`: the landing page, `link.html` from the web folder, whatever the token (the
/// page asks `GET /api/l/{token}`). Changes nothing: a visit, a prefetch or a link preview
/// never joins anyone to a link. `404` if the page is not installed.
pub(crate) async fn page(State(state): State<AppState>) -> Response {
    let path = state.config().web_dir.join("link.html");
    match tokio::task::spawn_blocking(move || std::fs::read(path)).await {
        Ok(Ok(bytes)) => ([(CONTENT_TYPE, "text/html; charset=utf-8")], bytes).into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

/// The token in a link's address, or `404 link_not_found`.
fn token_of(text: &str) -> Result<LinkToken, ApiError> {
    LinkToken::parse(text).ok_or(ApiError::LinkNotFound)
}

/// A node id in an anonymous route's address, or `404 link_not_found`: such a route says
/// nothing but that the link does not reach it.
fn reached_id(text: &str) -> Result<NodeId, ApiError> {
    node_id(text).map_err(|_| ApiError::LinkNotFound)
}

/// The item an anyone link shares, as its landing page shows it.
#[derive(Serialize)]
struct ItemOut {
    id: String,
    kind: NodeKind,
    name: String,
    /// For a file, the editor that opens it (`text` for a plain-text document).
    #[serde(skip_serializing_if = "Option::is_none")]
    editor: Option<Editor>,
    /// For a file that has been saved, when it was last saved (RFC 3339); absent for one never
    /// saved, which has nothing to download.
    #[serde(skip_serializing_if = "Option::is_none")]
    revised_at: Option<String>,
}

/// The document of `node` when it is a file.
async fn document_of(state: &AppState, node: &Node) -> Result<Option<Document>, ApiError> {
    if node.kind != NodeKind::Document {
        return Ok(None);
    }
    state
        .db()
        .document(DocId(node.id.0))
        .await
        .map_err(|e| ApiError::unavailable(&e))
}

/// The editor of `node` when it is a file.
async fn editor_of(state: &AppState, node: &Node) -> Result<Option<Editor>, ApiError> {
    Ok(document_of(state, node)
        .await?
        .map(|document| document.editor))
}

/// `GET /api/l/{token}`: what the link is. For an anyone link, `{kind: "anyone", role, item}`,
/// without reading the session cookie. For a signed-in link, `{kind: "signed_in", role}` to a
/// signed-in caller, so that the page can offer "Open with <role> access", and `401
/// not_signed_in` to anyone else, so that it can send them to sign in and back; nothing about
/// the item. `404 link_not_found` for a link that does not work.
async fn what(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(token): Path<String>,
) -> Result<Response, ApiError> {
    let token = token_of(&token)?;
    let db = state.db();
    let (kind, role) = db
        .link_kind(&token)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
        .ok_or(ApiError::LinkNotFound)?;
    match kind {
        LinkKind::Anyone => {
            let grant = db
                .anyone_link(&token, None)
                .await
                .map_err(|e| ApiError::unavailable(&e))?
                .ok_or(ApiError::LinkNotFound)?;
            let document = document_of(&state, &grant.node).await?;
            let item = ItemOut {
                id: grant.node.id.0.to_string(),
                kind: grant.node.kind,
                editor: document.as_ref().map(|document| document.editor),
                revised_at: rfc3339(document.and_then(|document| document.revised_at)),
                name: grant.node.name,
            };
            Ok(Json(json!({ "kind": kind, "role": role, "item": item })).into_response())
        }
        LinkKind::SignedIn => match MaybeSigned::of(&headers, &state).await? {
            MaybeSigned(Some(_)) => Ok(Json(json!({ "kind": kind, "role": role })).into_response()),
            MaybeSigned(None) => Err(ApiError::NotSignedIn),
        },
    }
}

/// The page that opens `node`, as the web pages address it.
fn page_for(node: &Node, editor: Option<Editor>) -> String {
    let id = node.id.0;
    match node.kind {
        NodeKind::Site => format!("/site.html?id={id}"),
        NodeKind::Library | NodeKind::Folder => format!("/library.html?id={id}"),
        NodeKind::Document if editor == Some(Editor::Text) => format!("/doc.html#{id}"),
        NodeKind::Document => format!("/file.html?doc={id}"),
    }
}

/// `POST /api/l/{token}/redeem`: the signed-in caller opens the signed-in link: they join it
/// (unless their access to its item already covers its role), their open sessions are
/// rechecked (they may now edit), and the answer is `{node, url}`, the item and the page that
/// opens it. Same origin only, like every change. `404 link_not_found` for a link that does not
/// work, including an anyone link.
async fn redeem(
    State(state): State<AppState>,
    signed: Signed,
    Path(token): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let token = token_of(&token)?;
    let user = signed.session.user.id;
    let node = {
        let state = state.clone();
        to_completion(async move {
            let node = state.db().redeem_signed_in(user, &token).await?;
            recheck_person(&state, user).await;
            Ok(node)
        })
        .await?
    };
    let item = state
        .db()
        .node(node)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
        .ok_or(ApiError::LinkNotFound)?;
    let editor = editor_of(&state, &item).await?;
    Ok(Json(json!({
        "node": node.0.to_string(),
        "url": page_for(&item, editor),
    })))
}

/// `GET /api/l/{token}/children/{node}?after=&limit=`: one page of a folder's children that the
/// anyone link reaches, each with Viewer access, in the order of the signed-in listing.
async fn children(
    State(state): State<AppState>,
    Path((token, node)): Path<(String, String)>,
    query: Result<Query<ChildrenQuery>, QueryRejection>,
) -> Result<Json<ChildrenPage>, ApiError> {
    let token = token_of(&token)?;
    let node = reached_id(&node)?;
    let Query(query) = query.map_err(|_| ApiError::BadRequest)?;
    let (after, limit) = query.page()?;
    let (listed, next) = state
        .db()
        .anyone_children(&token, node, after.as_ref(), limit)
        .await
        .map_err(|e| ApiError::unavailable(&e))?
        .ok_or(ApiError::LinkNotFound)?;
    Ok(Json(ChildrenPage::of(listed, next.as_ref())))
}

/// The document `node` if the anyone link with `token` reaches it now, with its name and
/// editor; `404 link_not_found` otherwise, and for a node that is not a file.
async fn reached_document(
    state: &AppState,
    token: &LinkToken,
    node: NodeId,
) -> Result<records::Document, ApiError> {
    let db = state.db();
    let grant = db
        .anyone_link(token, Some(node))
        .await
        .map_err(|e| ApiError::unavailable(&e))?
        .ok_or(ApiError::LinkNotFound)?;
    if grant.node.kind != NodeKind::Document {
        return Err(ApiError::LinkNotFound);
    }
    db.document(DocId(node.0))
        .await
        .map_err(|e| ApiError::unavailable(&e))?
        .ok_or(ApiError::LinkNotFound)
}

/// `GET /api/l/{token}/content/{node}`: a file's bytes as its last saved revision, with the
/// headers of every download (an `octet-stream` attachment, `nosniff`, a sandbox policy). `204`
/// if it has never been saved.
async fn content(
    State(state): State<AppState>,
    Path((token, node)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let token = token_of(&token)?;
    let node = reached_id(&node)?;
    let document = reached_document(&state, &token, node).await?;
    let doc = document.id;
    match blocking(&state, move |store| store.load(doc)).await? {
        Some(stored) => file_answer(&document.name, stored),
        None => Ok(StatusCode::NO_CONTENT.into_response()),
    }
}

/// `GET /api/l/{token}/text/{node}`: a plain-text document's text as of its last saved
/// revision (unsaved live edits are not shown): `{id, name, text, revision}`, `revision` `null`
/// and `text` empty if it has never been saved. `404 link_not_found` for any other file.
async fn text(
    State(state): State<AppState>,
    Path((token, node)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let token = token_of(&token)?;
    let node = reached_id(&node)?;
    let document = reached_document(&state, &token, node).await?;
    if document.editor != Editor::Text {
        return Err(ApiError::LinkNotFound);
    }
    let doc = document.id;
    let stored = blocking(&state, move |store| store.load(doc)).await?;
    let (text, revision) = match stored {
        Some(stored) => (
            String::from_utf8_lossy(&stored.bytes).into_owned(),
            Some(stored.revision.to_string()),
        ),
        None => (String::new(), None),
    };
    Ok(Json(json!({
        "id": doc.0.to_string(),
        "name": document.name,
        "text": text,
        "revision": revision,
    })))
}

/// Revokes, as the server and for the reason `expired`, every live link whose expiry is at or
/// before `now`, one link per transaction exactly as a manager would revoke it, rechecking the
/// open documents below each signed-in link's assignments before the next; then forgets the
/// requests for access submitted more than a day before `now`. Logs one line if it revoked
/// anything, and answers how many links it revoked. A database failure is logged and skipped:
/// the next sweep tries again.
pub async fn sweep_once(state: &AppState, now: OffsetDateTime) -> usize {
    let db = state.db();
    let expired = match db.expired_links(now).await {
        Ok(expired) => expired,
        Err(e) => {
            eprintln!("sharecraft-server: database failure: {e}");
            Vec::new()
        }
    };
    let mut revoked = 0;
    for link in expired {
        let state = state.clone();
        // Database failures are logged on the way to an `ApiError`.
        let done = to_completion(async move {
            let touched = state.db().revoke_link(None, link, "expired").await?;
            let Some(touched) = touched else {
                return Ok(false);
            };
            for node in touched {
                recheck_below(&state, node).await;
            }
            Ok(true)
        })
        .await;
        if done == Ok(true) {
            revoked += 1;
        }
    }
    if let Err(e) = db.forget_old_attempts(now).await {
        eprintln!("sharecraft-server: database failure: {e}");
    }
    if revoked > 0 {
        println!("sharecraft-server: revoked {revoked} expired link(s)");
    }
    revoked
}

/// The running expiry sweeper; dropping it stops it.
pub(crate) struct Sweeper(JoinHandle<()>);

impl Drop for Sweeper {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Starts the expiry sweeper for `state`: [`sweep_once`] now and every [`SWEEP_EVERY`] after.
pub(crate) fn start_sweeper(state: AppState) -> Sweeper {
    Sweeper(tokio::spawn(async move {
        let mut every = tokio::time::interval(SWEEP_EVERY);
        every.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            every.tick().await;
            sweep_once(&state, OffsetDateTime::now_utc()).await;
        }
    }))
}
