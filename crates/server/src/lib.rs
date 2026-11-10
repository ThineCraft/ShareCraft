#![forbid(unsafe_code)]
//! The ShareCraft session server: one room per open document, reached over WebSocket.
//!
//! Each room owns its document's ordered change log, commits changes in arrival order and
//! sends every committed change to everyone in the room.

pub mod config;
mod room;
mod ws;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;
use axum::routing::get;
use collab_core::ids::DocId;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tower_http::services::ServeDir;

pub use config::{Config, ConfigError};

use room::{Command, Initial, Rooms};

/// Shared state of a running server.
#[derive(Clone)]
pub struct AppState {
    shared: Arc<Shared>,
}

struct Shared {
    config: Config,
    rooms: Arc<Rooms>,
    next_conn: AtomicU64,
}

impl AppState {
    /// State for a server with no rooms open yet.
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self {
            shared: Arc::new(Shared {
                config,
                rooms: Arc::new(Rooms::default()),
                next_conn: AtomicU64::new(1),
            }),
        }
    }

    /// The settings the server runs with.
    #[must_use]
    pub fn config(&self) -> &Config {
        &self.shared.config
    }

    fn next_conn(&self) -> u64 {
        self.shared.next_conn.fetch_add(1, Ordering::Relaxed)
    }

    fn open_room(&self, doc: DocId) -> mpsc::Sender<Command> {
        self.shared.rooms.open(doc, load_initial)
    }
}

/// The state a newly opened room starts from. Every room starts empty until documents are
/// loaded from the revision store.
fn load_initial(_doc: DocId) -> Initial {
    Initial::default()
}

/// The HTTP routes:
/// - `GET /healthz` answers `ok`;
/// - `GET /ws/docs/{doc_id}?name=<display name>` opens a document session;
/// - anything else is a static file from `web_dir`.
pub fn router(state: AppState) -> Router {
    let files = ServeDir::new(&state.config().web_dir);
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/ws/docs/{doc_id}", get(ws::upgrade))
        .fallback_service(files)
        .with_state(state)
}

/// Serves `config` on `listener` until the process ends.
///
/// # Errors
/// Returns an error if accepting connections fails.
pub async fn serve(config: Config, listener: TcpListener) -> std::io::Result<()> {
    axum::serve(listener, router(AppState::new(config))).await
}
