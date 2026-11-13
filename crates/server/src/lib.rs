#![forbid(unsafe_code)]
//! The ShareCraft session server: one room per open document, reached over WebSocket, and a
//! JSON API for signing in.
//!
//! Each room owns its document's ordered change log, commits changes in arrival order and
//! sends every committed change to everyone in the room. Accounts and sessions live in
//! PostgreSQL (see the `records` crate).

mod api;
mod auth;
mod checkpoint;
pub mod cli;
pub mod config;
pub mod editors;
mod events;
mod file_room;
mod links;
mod members;
mod projection;
mod room;
pub mod tree;
mod ws;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use axum::Router;
use axum::extract::Request;
use axum::http::HeaderValue;
use axum::http::header::{
    CACHE_CONTROL, CONTENT_SECURITY_POLICY, REFERRER_POLICY, X_CONTENT_TYPE_OPTIONS,
};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::get;
use collab_core::ids::DocId;
use records::Db;
use revisions::RevisionStore;
use tokio::net::TcpListener;
use tower_http::services::ServeDir;

pub use checkpoint::{Checkpointer, library_path};
pub use config::{Config, ConfigError};
pub use links::sweep_once;
pub use projection::{Report, rebuild, reconcile};

use auth::Throttle;
use checkpoint::Timing;
use editors::Editors;
use events::EventSink;
use file_room::FileLink;
use room::{RoomLink, Rooms, Storage};

/// Shared state of a running server.
#[derive(Clone)]
pub struct AppState {
    shared: Arc<Shared>,
}

struct Shared {
    config: Config,
    rooms: Arc<Rooms>,
    checkpointer: Arc<dyn Checkpointer>,
    db: Db,
    editors: Editors,
    events: EventSink,
    throttle: Throttle,
    next_conn: AtomicU64,
    /// Held for the whole of each tree change, so changes are judged and written one at a time.
    tree_lock: tokio::sync::Mutex<()>,
    /// Set when PostgreSQL's tree could not be brought back in step with Git after a change;
    /// tree changes are refused until a reconcile of the whole tree succeeds.
    tree_out_of_step: AtomicBool,
    /// Take-backs of refused tree changes that could not be written to Git yet; written before
    /// the reconcile that brings the tree back in step.
    tree_undos: std::sync::Mutex<Vec<tree::PendingUndo>>,
}

impl AppState {
    /// State for a server with no rooms open yet, loading and saving documents through
    /// `checkpointer` and keeping accounts and sessions in `db`.
    #[must_use]
    pub fn new(config: Config, checkpointer: Arc<dyn Checkpointer>, db: Db) -> Self {
        let editors_dir = config.editors_dir.clone();
        Self {
            shared: Arc::new(Shared {
                config,
                rooms: Arc::new(Rooms::default()),
                checkpointer,
                events: EventSink::start(db.clone()),
                editors: Editors::load(&editors_dir),
                db,
                throttle: Throttle::default(),
                next_conn: AtomicU64::new(1),
                tree_lock: tokio::sync::Mutex::new(()),
                tree_out_of_step: AtomicBool::new(false),
                tree_undos: std::sync::Mutex::new(Vec::new()),
            }),
        }
    }

    /// The settings the server runs with.
    #[must_use]
    pub fn config(&self) -> &Config {
        &self.shared.config
    }

    /// The database of accounts, sessions and access grants.
    #[must_use]
    pub fn db(&self) -> &Db {
        &self.shared.db
    }

    /// The editors installed when the server started.
    #[must_use]
    pub fn editors(&self) -> &Editors {
        &self.shared.editors
    }

    fn checkpointer(&self) -> &Arc<dyn Checkpointer> {
        &self.shared.checkpointer
    }

    fn events(&self) -> &EventSink {
        &self.shared.events
    }

    fn throttle(&self) -> &Throttle {
        &self.shared.throttle
    }

    fn rooms(&self) -> &Rooms {
        &self.shared.rooms
    }

    fn tree_lock(&self) -> &tokio::sync::Mutex<()> {
        &self.shared.tree_lock
    }

    fn tree_out_of_step(&self) -> &AtomicBool {
        &self.shared.tree_out_of_step
    }

    fn tree_undos(&self) -> &std::sync::Mutex<Vec<tree::PendingUndo>> {
        &self.shared.tree_undos
    }

    fn next_conn(&self) -> u64 {
        self.shared.next_conn.fetch_add(1, Ordering::Relaxed)
    }

    /// The command channel of `doc`'s room, opening it if needed; `name` is the document's
    /// name, which a newly opened room stores its checkpoints under.
    fn open_room(&self, doc: DocId, name: String) -> RoomLink {
        let config = &self.shared.config;
        let timing = Timing {
            idle: config.checkpoint_idle,
            max: config.checkpoint_max,
        };
        self.shared.rooms.open(
            doc,
            name,
            Storage {
                store: Arc::clone(&self.shared.checkpointer),
                db: Some(self.shared.db.clone()),
                events: self.shared.events.clone(),
            },
            timing,
            config.room_grace,
        )
    }

    /// The command channel of `doc`'s file session, opening it if needed; `name` is the
    /// document's name, which a newly opened session stores revisions under.
    fn open_file_room(&self, doc: DocId, name: String) -> FileLink {
        self.shared.rooms.open_file(
            doc,
            name,
            Storage {
                store: Arc::clone(&self.shared.checkpointer),
                db: Some(self.shared.db.clone()),
                events: self.shared.events.clone(),
            },
            self.shared.config.room_grace,
        )
    }
}

/// The HTTP routes:
/// - `GET /healthz` answers `ok`;
/// - `POST /api/session` signs in, `DELETE /api/session` signs out, `GET /api/me` names the
///   signed-in account;
/// - `GET /ws/docs/{doc_id}` opens a document session for a signed-in person with a role on
///   the document;
/// - `GET /api/docs/{id}/events` lists a document's latest events, for its owners;
/// - `/api/sites` and `/api/nodes/{id}…` list and change the content tree and who may access
///   each node (see the `api` module);
/// - `GET /api/editors` lists the installed editors, and `GET /editors/{id}/*` serves an
///   installed editor's files, for a signed-in caller;
/// - `GET /l/{token}` is a sharing link's landing page, and `/api/l/{token}…` what it reaches
///   (see the `links` module);
/// - anything else is a static file from `web_dir`.
///
/// Every response carries the security headers (see [`security_headers`]); `/api` answers are
/// never cached, and nothing under `/l/` or `/api/l/` is cached or sent as a referrer.
pub fn router(state: AppState) -> Router {
    let files = Router::new()
        .fallback_service(ServeDir::new(&state.config().web_dir))
        .layer(middleware::from_fn(no_cache));
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .nest("/api", api::router(state.config().max_file_bytes))
        .route("/ws/docs/{doc_id}", get(ws::upgrade))
        .route("/l/{token}", get(links::page))
        .merge(editors::router(state.clone()))
        .fallback_service(files)
        .layer(middleware::from_fn(links::link_headers))
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}

/// What a page may load: its own files only, no inline script or style, and WebAssembly. It may
/// frame its own pages (the frame page embeds an editor) and is framed by none.
const CONTENT_SECURITY_POLICY_VALUE: &str = "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; frame-src 'self'; frame-ancestors 'none'";

/// Makes browsers ask again before using a static file, so an upgrade never runs a stale script.
async fn no_cache(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

/// Adds the security headers to every response: pages, API, WebSocket handshake and errors.
/// Each is added only where the answer does not carry its own: an editor's
/// Content-Security-Policy, a download's sandbox and a link page's `no-referrer` stay.
async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    for (name, value) in [
        (CONTENT_SECURITY_POLICY, CONTENT_SECURITY_POLICY_VALUE),
        (X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (REFERRER_POLICY, "same-origin"),
    ] {
        if !headers.contains_key(&name) {
            headers.insert(name, HeaderValue::from_static(value));
        }
    }
    response
}

/// Serves `config` on `listener` until the process ends: checks that the listener is on a
/// loopback address, makes the server's state with [`prepare`], prints the address it listens
/// on and that it runs in development mode, then accepts connections.
///
/// # Errors
/// Returns an error if the listener is not on a loopback address, if [`prepare`] fails, or if
/// accepting connections fails.
pub async fn serve(config: Config, listener: TcpListener) -> std::io::Result<()> {
    let addr = listener.local_addr()?;
    require_loopback(addr)?;
    let state = prepare(config).await?;
    println!("ShareCraft server listening on http://{addr}");
    println!("WARNING: development mode, local connections only");
    serve_state(state, listener).await
}

/// The state of a server about to accept connections, storing documents in the library
/// repository under `config.data_dir` (see [`library_path`]) and records in the database at
/// `config.database_url`, whose schema it brings up to date first. It moves tree records written
/// before the sub-folders into them (see [`tree::move_records`]), reconciles the content
/// tree in the database with Git (see [`tree::reconcile`]), adopts documents recorded before
/// the tree (see [`tree::adopt_flat_documents`]), reconciles the revisions recorded in the
/// database with Git (see [`reconcile`]), and prints one line with the counts of each.
///
/// # Errors
/// Returns an error if the database cannot be reached or migrated, if the library repository
/// cannot be opened, created or listed, or if the tree cannot be moved or read or the adoption fails (a
/// missing administrator is only reported: the documents wait for a start with one).
pub async fn prepare(config: Config) -> std::io::Result<AppState> {
    let db = Db::connect(config.database_url.expose())
        .await
        .map_err(|e| std::io::Error::other(format!("cannot connect to the database: {e}")))?;
    db.migrate()
        .await
        .map_err(|e| std::io::Error::other(format!("cannot migrate the database: {e}")))?;
    let path = library_path(&config.data_dir);
    let opened = tokio::task::spawn_blocking(move || {
        RevisionStore::open(&path)
            .map_err(|e| std::io::Error::other(format!("cannot open {}: {e}", path.display())))
    })
    .await
    .map_err(std::io::Error::other)??;
    let store: Arc<dyn Checkpointer> = Arc::new(opened);
    let moved = tree::move_records(Arc::clone(&store)).await.map_err(|e| {
        std::io::Error::other(format!(
            "cannot move the tree records into sub-folders: {e}"
        ))
    })?;
    let tree_report = tree::reconcile(Arc::clone(&store), &db)
        .await
        .map_err(|e| std::io::Error::other(format!("cannot reconcile the content tree: {e}")))?;
    match tree::adopt_flat_documents(Arc::clone(&store), &db).await {
        Ok(Some(adopted)) => {
            println!("Reconciled the tree with Git: {moved}; {tree_report}; {adopted}");
        }
        Ok(None) => {
            println!("Reconciled the tree with Git: {moved}; {tree_report}; nothing to adopt");
        }
        // Nothing was written; the documents wait, unseen, for a start with an administrator.
        Err(e @ tree::TreeError::NoAdministrator) => {
            println!("Reconciled the tree with Git: {moved}; {tree_report}; nothing adopted");
            eprintln!("sharecraft-server: existing documents not adopted: {e}");
        }
        Err(e) => {
            return Err(std::io::Error::other(format!(
                "cannot adopt the existing documents: {e}"
            )));
        }
    }
    for (node, error) in &tree_report.failed {
        eprintln!("sharecraft-server: node {node} of the content tree not reconciled: {error}");
    }
    let report = reconcile(Arc::clone(&store), &db)
        .await
        .map_err(|e| std::io::Error::other(format!("cannot list the documents in Git: {e}")))?;
    println!("Reconciled revisions with Git: {report}");
    for (doc, error) in &report.failed {
        eprintln!(
            "sharecraft-server: revisions of document {} not reconciled: {error}",
            doc.0
        );
    }
    Ok(AppState::new(config, store, db))
}

/// Serves `state` on `listener` until the process ends, with the expiry sweeper (see
/// [`sweep_once`]) running every minute meanwhile; it stops when serving does.
///
/// # Errors
/// Returns an error if the listener is not on a loopback address, or if accepting connections
/// fails.
pub async fn serve_state(state: AppState, listener: TcpListener) -> std::io::Result<()> {
    require_loopback(listener.local_addr()?)?;
    let _sweeper = links::start_sweeper(state.clone());
    axum::serve(listener, router(state)).await
}

/// The server only ever listens on a loopback address.
fn require_loopback(addr: std::net::SocketAddr) -> std::io::Result<()> {
    if addr.ip().is_loopback() {
        return Ok(());
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!("refusing to serve on {addr}: only loopback addresses are allowed"),
    ))
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    // `serve` applies this to the listener's address. A real non-loopback listener is not
    // bound in tests: on Windows that raises a firewall prompt.
    #[test]
    fn non_loopback_listener_is_refused() {
        let err = require_loopback("0.0.0.0:8787".parse().unwrap()).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(require_loopback("192.168.1.5:80".parse().unwrap()).is_err());
        assert!(require_loopback("127.0.0.1:0".parse().unwrap()).is_ok());
        assert!(require_loopback("[::1]:0".parse().unwrap()).is_ok());
    }
}
