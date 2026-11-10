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
mod events;
mod projection;
mod room;
mod ws;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;
use axum::extract::Request;
use axum::http::HeaderValue;
use axum::http::header::{CONTENT_SECURITY_POLICY, REFERRER_POLICY, X_CONTENT_TYPE_OPTIONS};
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
pub use projection::{Report, rebuild, reconcile};

use auth::Throttle;
use checkpoint::Timing;
use events::EventSink;
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
    events: EventSink,
    throttle: Throttle,
    next_conn: AtomicU64,
}

impl AppState {
    /// State for a server with no rooms open yet, loading and saving documents through
    /// `checkpointer` and keeping accounts and sessions in `db`.
    #[must_use]
    pub fn new(config: Config, checkpointer: Arc<dyn Checkpointer>, db: Db) -> Self {
        Self {
            shared: Arc::new(Shared {
                config,
                rooms: Arc::new(Rooms::default()),
                checkpointer,
                events: EventSink::start(db.clone()),
                db,
                throttle: Throttle::default(),
                next_conn: AtomicU64::new(1),
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

    fn throttle(&self) -> &Throttle {
        &self.shared.throttle
    }

    fn rooms(&self) -> &Rooms {
        &self.shared.rooms
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
}

/// The HTTP routes:
/// - `GET /healthz` answers `ok`;
/// - `POST /api/session` signs in, `DELETE /api/session` signs out, `GET /api/me` names the
///   signed-in account;
/// - `GET /ws/docs/{doc_id}` opens a document session for a signed-in person with a role on
///   the document;
/// - `GET /api/docs/{id}/events` lists a document's latest events, for its owners;
/// - anything else is a static file from `web_dir`.
///
/// Every response carries the security headers (see [`security_headers`]); `/api` answers are
/// never cached.
pub fn router(state: AppState) -> Router {
    let files = ServeDir::new(&state.config().web_dir);
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .nest("/api", api::router())
        .route("/ws/docs/{doc_id}", get(ws::upgrade))
        .fallback_service(files)
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}

/// What a page may load: its own files only, no inline script or style, and WebAssembly.
const CONTENT_SECURITY_POLICY_VALUE: &str =
    "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; frame-ancestors 'none'";

/// Adds the security headers to every response: pages, API, WebSocket handshake and errors.
async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY_VALUE),
    );
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("same-origin"));
    response
}

/// Serves `config` on `listener` until the process ends, storing documents in the library
/// repository under `config.data_dir` (see [`library_path`]) and records in the database at
/// `config.database_url`, whose schema it brings up to date first. Before accepting
/// connections it reconciles the revisions recorded in the database with Git (see
/// [`reconcile`]) and prints one line with the counts. Once ready, it prints the address it
/// listens on and that it runs in development mode, then accepts connections.
///
/// # Errors
/// Returns an error if the listener is not on a loopback address, if the database cannot be
/// reached or migrated, if the library repository cannot be opened, created or listed, or if
/// accepting connections fails.
pub async fn serve(config: Config, listener: TcpListener) -> std::io::Result<()> {
    let addr = listener.local_addr()?;
    require_loopback(addr)?;
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
    println!("ShareCraft server listening on http://{addr}");
    println!("WARNING: development mode, local connections only");
    serve_state(AppState::new(config, store, db), listener).await
}

/// Serves `state` on `listener` until the process ends.
///
/// # Errors
/// Returns an error if the listener is not on a loopback address, or if accepting connections
/// fails.
pub async fn serve_state(state: AppState, listener: TcpListener) -> std::io::Result<()> {
    require_loopback(listener.local_addr()?)?;
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
