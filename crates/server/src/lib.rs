#![forbid(unsafe_code)]
//! The ShareCraft session server: one room per open document, reached over WebSocket, and a
//! JSON API for signing in.
//!
//! Each room owns its document's ordered change log, commits changes in arrival order and
//! sends every committed change to everyone in the room. Accounts and sessions live in
//! PostgreSQL (see the `records` crate).

mod api;
mod auth;
mod blobs;
mod brand;
mod checkpoint;
pub mod cli;
pub mod config;
pub mod deployment;
pub mod editors;
mod events;
mod file_room;
pub mod github;
pub mod icap;
pub mod libraries;
mod links;
mod looks;
mod members;
pub mod previews;
mod projection;
mod room;
mod scan;
pub mod secrets;
pub mod sites;
pub mod source_sync;
pub mod templates;
pub mod tree;
mod webhooks;
mod ws;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::header::{
    CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, LOCATION, REFERRER_POLICY,
    STRICT_TRANSPORT_SECURITY, X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use collab_core::ids::DocId;
use records::Db;
use revisions::{BlobStore, RevisionStore};
use tokio::net::TcpListener;
use tower_http::services::ServeDir;

use deployment::{Deployment, Production, Refusal, SameOrigin};

/// This build's version: what `cargo xtask package` names it (`0.1.0-6a18271`, from
/// `SHARECRAFT_BUILD` at compile time), or the crate's version marked `-dev` for any other build.
pub const VERSION: &str = match option_env!("SHARECRAFT_BUILD") {
    Some(version) => version,
    None => concat!(env!("CARGO_PKG_VERSION"), "-dev"),
};

pub use api::uploads::uploads_path;
pub use blobs::blobs_path;
pub use checkpoint::{Checkpointer, library_path};
pub use config::{Config, ConfigError};
pub use links::sweep_once;
pub use projection::{
    BlobReport, Report, UsageReport, rebuild, reconcile, reconcile_blobs, reconcile_usage,
};
pub use scan::scan_once;
pub use source_sync::sync_once;
pub use source_sync::tick_once as source_tick_once;

use api::uploads::Uploads;
use auth::{MAX_ADDRESS_FAILURES, MAX_LINK_MISSES, PasswordChecks, Throttle};
use blobs::Blobs;
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
    blobs: Arc<Blobs>,
    /// The upload sessions' part files and running hashes.
    uploads: Arc<Uploads>,
    /// The malware scan worker's turns and the contents it is scanning.
    scanner: scan::Scanner,
    /// The turns for making thumbnails.
    thumbnailer: previews::Thumbnailer,
    db: Db,
    editors: Editors,
    events: EventSink,
    /// Failed sign-ins per email.
    throttle: Throttle,
    /// Failed sign-ins per client address.
    address_throttle: Throttle,
    /// Unknown anyone-link tokens asked about per client address.
    link_throttle: Throttle,
    /// The password checks that may run at once.
    password_checks: PasswordChecks,
    next_conn: AtomicU64,
    /// Held for the whole of each tree change, so changes are judged and written one at a time.
    tree_lock: tokio::sync::Mutex<()>,
    /// Set when PostgreSQL's tree could not be brought back in step with Git after a change;
    /// tree changes are refused until a reconcile of the whole tree succeeds.
    tree_out_of_step: AtomicBool,
    /// Take-backs of refused tree changes that could not be written to Git yet; written before
    /// the reconcile that brings the tree back in step.
    tree_undos: std::sync::Mutex<Vec<tree::PendingUndo>>,
    /// Seals and opens the secrets connected libraries use.
    secrets: Arc<secrets::SecretBox>,
    /// The client for the configured GitHub API address.
    github: Arc<github::GithubClient>,
    /// The sync worker's wakes, turns and back-off.
    sources: source_sync::Sources,
}

impl AppState {
    /// State for a server with no rooms open yet, loading and saving documents through
    /// `checkpointer`, their bytes in the content store `blobs`, and keeping accounts and
    /// sessions in `db`. The secret key is opened (or made) in `config.data_dir`, and the
    /// GitHub client is set up for `config.github_api`.
    ///
    /// # Errors
    /// If the secret key file cannot be read or made, or the GitHub client cannot be set up.
    pub fn new(
        config: Config,
        checkpointer: Arc<dyn Checkpointer>,
        blobs: BlobStore,
        db: Db,
    ) -> std::io::Result<Self> {
        let editors_dir = config.editors_dir.clone();
        let uploads_dir = uploads_path(&config.data_dir);
        let secrets = secrets::SecretBox::open_or_create(&config.data_dir)
            .map_err(|e| std::io::Error::other(format!("cannot open the secret key: {e}")))?;
        let github = github::GithubClient::new(&config.github_api)
            .map_err(|e| std::io::Error::other(format!("cannot set up the GitHub client: {e}")))?;
        Ok(Self {
            shared: Arc::new(Shared {
                config,
                rooms: Arc::new(Rooms::default()),
                checkpointer,
                blobs: Arc::new(Blobs::new(blobs)),
                uploads: Arc::new(Uploads::new(uploads_dir)),
                scanner: scan::Scanner::default(),
                thumbnailer: previews::Thumbnailer::default(),
                events: EventSink::start(db.clone()),
                editors: Editors::load(&editors_dir),
                db,
                throttle: Throttle::default(),
                address_throttle: Throttle::new(MAX_ADDRESS_FAILURES),
                link_throttle: Throttle::new(MAX_LINK_MISSES),
                password_checks: PasswordChecks::default(),
                next_conn: AtomicU64::new(1),
                tree_lock: tokio::sync::Mutex::new(()),
                tree_out_of_step: AtomicBool::new(false),
                tree_undos: std::sync::Mutex::new(Vec::new()),
                secrets: Arc::new(secrets),
                github: Arc::new(github),
                sources: source_sync::Sources::default(),
            }),
        })
    }

    /// The sync worker of connected libraries: [`source_sync::Sources::wake`] asks it to sync a
    /// library at once.
    #[must_use]
    pub fn sources(&self) -> &source_sync::Sources {
        &self.shared.sources
    }

    /// Seals and opens the secrets of connected libraries.
    #[must_use]
    pub fn secrets(&self) -> &secrets::SecretBox {
        &self.shared.secrets
    }

    /// The client for the configured GitHub API address.
    #[must_use]
    pub fn github(&self) -> &github::GithubClient {
        &self.shared.github
    }

    /// The settings the server runs with.
    #[must_use]
    pub fn config(&self) -> &Config {
        &self.shared.config
    }

    /// How the server is reached: development or production mode.
    #[must_use]
    pub fn deployment(&self) -> &Deployment {
        &self.shared.config.deployment
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

    fn blobs(&self) -> &Arc<Blobs> {
        &self.shared.blobs
    }

    fn uploads(&self) -> &Arc<Uploads> {
        &self.shared.uploads
    }

    fn scanner(&self) -> &scan::Scanner {
        &self.shared.scanner
    }

    fn thumbnailer(&self) -> &previews::Thumbnailer {
        &self.shared.thumbnailer
    }

    /// Takes every turn for making thumbnails until the answer is dropped, as long-running
    /// decodes would. For tests.
    #[doc(hidden)]
    pub async fn hold_thumbnail_turns(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        self.shared.thumbnailer.hold_turns().await
    }

    /// Sets how long a thumbnail request waits for a turn. For tests.
    #[doc(hidden)]
    pub fn set_thumbnail_wait(&self, wait: std::time::Duration) {
        self.shared.thumbnailer.set_wait(wait);
    }

    fn events(&self) -> &EventSink {
        &self.shared.events
    }

    fn throttle(&self) -> &Throttle {
        &self.shared.throttle
    }

    fn address_throttle(&self) -> &Throttle {
        &self.shared.address_throttle
    }

    fn link_throttle(&self) -> &Throttle {
        &self.shared.link_throttle
    }

    fn password_checks(&self) -> &PasswordChecks {
        &self.shared.password_checks
    }

    /// Takes every password-check turn until the answer is dropped, as a flood of sign-ins
    /// would. For tests.
    #[doc(hidden)]
    pub async fn hold_password_checks(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        self.shared.password_checks.hold_all().await
    }

    /// Sets how long a sign-in waits for a password-check turn. For tests.
    #[doc(hidden)]
    pub fn set_password_wait(&self, wait: std::time::Duration) {
        self.shared.password_checks.set_wait(wait);
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
                blobs: Arc::clone(&self.shared.blobs),
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
                blobs: Arc::clone(&self.shared.blobs),
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
/// - `POST /hooks/github/{library}` takes GitHub's webhook deliveries for a connected library
///   (see the `webhooks` module); it is outside `/api`, as GitHub sends no `Origin`;
/// - anything else is a static file from `web_dir`.
///
/// Every request first passes [`source`], which works out where it came from and, in production
/// mode, refuses what did not come through a trusted proxy over HTTPS. Every response it lets
/// through carries the security headers (see [`security_headers`]); `/api` answers are never
/// cached, and nothing under `/l/` or `/api/l/` is cached or sent as a referrer.
pub fn router(state: AppState) -> Router {
    let files = Router::new()
        .fallback_service(ServeDir::new(&state.config().web_dir))
        .layer(middleware::from_fn(no_cache));
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .nest("/api", api::router())
        .route("/ws/docs/{doc_id}", get(ws::upgrade))
        .route("/l/{token}", get(links::page))
        .route(
            "/hooks/github/{library}",
            post(webhooks::github).layer(DefaultBodyLimit::max(webhooks::MAX_BODY)),
        )
        .merge(editors::router(state.clone()))
        .fallback_service(files)
        .layer(middleware::from_fn(links::link_headers))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            security_headers,
        ))
        .layer(middleware::from_fn_with_state(state.clone(), source))
        .with_state(state)
}

/// Works out where a request came from ([`deployment::Source`]) and whether it passes the origin
/// check ([`SameOrigin`]), and stores both in the request for the routes and guards.
///
/// In production mode it answers itself, before any route, for a request that did not come
/// from a trusted proxy (`403`, except `GET /healthz` from loopback) and for one the proxy
/// received over plain HTTP: a page `GET` or `HEAD` is redirected (`308`) to the public
/// address, anything else is `403 https_required`.
async fn source(State(state): State<AppState>, mut request: Request, next: Next) -> Response {
    let deployment = state.deployment();
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0);
    let health = request.method() == Method::GET && request.uri().path() == "/healthz";
    let source = match deployment.source_of(peer, request.headers(), health) {
        Ok(source) => source,
        Err(Refusal::Untrusted) => return untrusted_answer(),
    };
    if let Some(production) = deployment.production()
        && !source.https
        && !health
    {
        return plain_http_answer(production, &request);
    }
    let same_origin = deployment.same_origin(request.headers(), &source);
    let link_lookup = request.uri().path().starts_with("/api/l/");
    let hook = request.uri().path().starts_with("/hooks/");
    let address = auth::Throttle::address_key(source.client_ip);
    request.extensions_mut().insert(SameOrigin(same_origin));
    request.extensions_mut().insert(source);
    if !link_lookup && !hook {
        return next.run(request).await;
    }
    // Guessing anyone-link tokens or webhook signatures: an address with 60 answers of
    // `link_not_found`, or of a webhook `404` or `401`, in the last 15 minutes is refused until
    // the oldest is 15 minutes old. A link used as meant, or a delivery taken, is never counted.
    let throttle = state.link_throttle();
    if throttle.blocked(&address, std::time::Instant::now()) {
        let mut refused = api::ApiError::TooManyAttempts.into_response();
        let headers = refused.headers_mut();
        headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
        headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
        add_headers(headers, deployment.production().map(|p| p.hsts_seconds));
        return refused;
    }
    let response = next.run(request).await;
    let failed = response.status() == StatusCode::NOT_FOUND
        || (hook && response.status() == StatusCode::UNAUTHORIZED);
    if failed {
        throttle.note_failure(&address, std::time::Instant::now());
    }
    response
}

/// The answer to a request that did not come from a trusted proxy, in production mode. It
/// carries no HSTS: it never came over HTTPS.
fn untrusted_answer() -> Response {
    (
        StatusCode::FORBIDDEN,
        [
            (CONTENT_TYPE, "text/plain; charset=utf-8"),
            (X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        "This ShareCraft server answers only through its reverse proxy, at its public address.\n",
    )
        .into_response()
}

/// The answer to a request the proxy received over plain HTTP: pages are redirected to the same
/// path at the public address, everything else is refused.
fn plain_http_answer(production: &Production, request: &Request) -> Response {
    let path = request.uri().path();
    let page = matches!(*request.method(), Method::GET | Method::HEAD)
        && !path.starts_with("/api/")
        && !path.starts_with("/ws/");
    let mut response = if page {
        let target = request.uri().path_and_query().map_or("/", |pq| pq.as_str());
        let location = format!("{}{target}", production.public.origin());
        match HeaderValue::from_str(&location) {
            Ok(location) => {
                (StatusCode::PERMANENT_REDIRECT, [(LOCATION, location)]).into_response()
            }
            Err(_) => api::ApiError::HttpsRequired.into_response(),
        }
    } else {
        api::ApiError::HttpsRequired.into_response()
    };
    add_security_headers(response.headers_mut(), production.hsts_seconds);
    response
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
/// In production mode it also adds `Strict-Transport-Security`.
async fn security_headers(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let hsts = state.deployment().production().map(|p| p.hsts_seconds);
    add_headers(response.headers_mut(), hsts);
    response
}

/// The security headers of a production answer made before the routes.
fn add_security_headers(headers: &mut HeaderMap, hsts_seconds: u64) {
    add_headers(headers, Some(hsts_seconds));
}

/// Adds the security headers `headers` lacks, and HSTS with `hsts_seconds` when given.
fn add_headers(headers: &mut HeaderMap, hsts_seconds: Option<u64>) {
    if let Some(seconds) = hsts_seconds
        && let Ok(value) = HeaderValue::from_str(&format!("max-age={seconds}"))
    {
        headers.insert(STRICT_TRANSPORT_SECURITY, value);
    }
    for (name, value) in [
        (CONTENT_SECURITY_POLICY, CONTENT_SECURITY_POLICY_VALUE),
        (X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (REFERRER_POLICY, "same-origin"),
    ] {
        if !headers.contains_key(&name) {
            headers.insert(name, HeaderValue::from_static(value));
        }
    }
}

/// Serves `config` on `listener` until the process ends: in development mode checks that the
/// listener is on a loopback address, makes the server's state with [`prepare`], prints the
/// address it listens on and its mode, then accepts connections.
///
/// # Errors
/// Returns an error if, in development mode, the listener is not on a loopback address, if
/// [`prepare`] fails, or if accepting connections fails.
pub async fn serve(config: Config, listener: TcpListener) -> std::io::Result<()> {
    let addr = listener.local_addr()?;
    require_loopback(&config.deployment, addr)?;
    let state = prepare(config).await?;
    match state.deployment().production() {
        None => {
            println!("ShareCraft {VERSION} listening on http://{addr}");
            println!("WARNING: development mode, local connections only");
        }
        Some(production) => {
            println!("ShareCraft {VERSION} listening on {addr}");
            println!(
                "Production mode: public address {}, trusted proxies {}",
                production.public, production.proxies
            );
        }
    }
    serve_state(state, listener).await
}

/// The state of a server about to accept connections, storing documents in the library
/// repository under `config.data_dir` (see [`library_path`]), their bytes in the content store
/// there (see [`blobs_path`]), and records in the database at `config.database_url`, whose
/// schema it brings up to date first. It moves tree records written before the sub-folders
/// into them (see [`tree::move_records`]), reconciles the content tree in the database with Git
/// (see [`tree::reconcile`]), adopts documents recorded before the tree (see
/// [`tree::adopt_flat_documents`]), reconciles the revisions recorded in the database with Git
/// (see [`reconcile`]), rebuilds the content rows from Git when a flag asks for it (see
/// [`reconcile_blobs`]), drops interrupted quota reservations and rebuilds the sites' usage
/// when a flag asks for it (see [`reconcile_usage`]), and prints one line with the counts of
/// each.
///
/// # Errors
/// Returns an error if the database cannot be reached or migrated, if the library repository
/// or the content store cannot be opened, created or listed, or if the tree cannot be moved or
/// read or the adoption fails (a missing administrator is only reported: the documents wait for
/// a start with one).
pub async fn prepare(config: Config) -> std::io::Result<AppState> {
    let db = Db::connect(config.database_url.expose())
        .await
        .map_err(|e| std::io::Error::other(format!("cannot connect to the database: {e}")))?;
    db.migrate()
        .await
        .map_err(|e| std::io::Error::other(format!("cannot migrate the database: {e}")))?;
    let path = library_path(&config.data_dir);
    let blobs_dir = blobs_path(&config.data_dir);
    let (opened, blobs) = tokio::task::spawn_blocking(move || {
        let store = RevisionStore::open(&path)
            .map_err(|e| std::io::Error::other(format!("cannot open {}: {e}", path.display())))?;
        let blobs = BlobStore::open(&blobs_dir).map_err(|e| {
            std::io::Error::other(format!("cannot open {}: {e}", blobs_dir.display()))
        })?;
        Ok::<_, std::io::Error>((store, blobs))
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
    let sites_report = sites::reconcile(Arc::clone(&store), &db)
        .await
        .map_err(|e| std::io::Error::other(format!("cannot reconcile the site records: {e}")))?;
    println!("Reconciled the site and library records with Git: {sites_report}");
    for (id, error) in &sites_report.failed {
        eprintln!("sharecraft-server: site or library record {id} not reconciled: {error}");
    }
    let report = reconcile(Arc::clone(&store), &db)
        .await
        .map_err(|e| std::io::Error::other(format!("cannot list the documents in Git: {e}")))?;
    println!("Reconciled revisions with Git: {report}");
    if !report.failed.is_empty() {
        // A version that could not be listed may name content that has no row yet: nothing is
        // erased until the content rows are rebuilt from Git without a failure.
        blobs::flag_dirty(&db).await;
    }
    for (doc, error) in &report.failed {
        eprintln!(
            "sharecraft-server: revisions of document {} not reconciled: {error}",
            doc.0
        );
    }
    let state = AppState::new(config, store, blobs, db)?;
    // No sync runs before the server starts: one left `syncing` was cut short.
    let stale = source_sync::recover_stale_syncs(&state, time::OffsetDateTime::now_utc()).await;
    if stale > 0 {
        println!("Connected libraries whose sync was cut short, to sync again: {stale}");
    }
    let blob_report = reconcile_blobs(&state, time::OffsetDateTime::now_utc())
        .await
        .map_err(|e| std::io::Error::other(format!("cannot list the documents in Git: {e}")))?;
    println!("Reconciled stored content with Git: {blob_report}");
    for (doc, error) in &blob_report.failed {
        eprintln!(
            "sharecraft-server: content of document {} not reconciled: {error}",
            doc.0
        );
    }
    let usage_report = reconcile_usage(&state).await;
    println!("Reconciled the sites' usage: {usage_report}");
    for (doc, error) in &usage_report.failed {
        eprintln!(
            "sharecraft-server: sizes of the versions of document {} not read: {error}",
            doc.0
        );
    }
    Ok(state)
}

/// Serves `state` on `listener` until the process ends, with the expiry sweeper (see
/// [`sweep_once`]) running every minute, the malware scan worker (see [`scan_once`]) and the
/// sync worker of connected libraries (see [`source_tick_once`]) meanwhile; they stop when
/// serving does.
///
/// # Errors
/// Returns an error if, in development mode, the listener is not on a loopback address, or if
/// accepting connections fails.
pub async fn serve_state(state: AppState, listener: TcpListener) -> std::io::Result<()> {
    require_loopback(state.deployment(), listener.local_addr()?)?;
    let _sweeper = links::start_sweeper(state.clone());
    let _scanner = scan::start_worker(state.clone());
    // `prepare` has put syncs cut short by an earlier process back to `connecting` already.
    let _syncer = source_sync::start_worker(state.clone());
    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
}

/// In development mode the server only ever listens on a loopback address. In production mode
/// any address is allowed: [`source`] refuses every peer that is not a trusted proxy.
fn require_loopback(deployment: &Deployment, addr: SocketAddr) -> std::io::Result<()> {
    if deployment.is_production() || addr.ip().is_loopback() {
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
        let dev = Deployment::Development;
        let err = require_loopback(&dev, "0.0.0.0:8787".parse().unwrap()).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(require_loopback(&dev, "192.168.1.5:80".parse().unwrap()).is_err());
        assert!(require_loopback(&dev, "127.0.0.1:0".parse().unwrap()).is_ok());
        assert!(require_loopback(&dev, "[::1]:0".parse().unwrap()).is_ok());
        // Production mode may listen anywhere: only trusted proxies are answered.
        let prod =
            Deployment::production_for_tests("https://share.example.com", "10.0.0.1").unwrap();
        assert!(require_loopback(&prod, "0.0.0.0:8787".parse().unwrap()).is_ok());
    }
}
