//! Helpers for driving a real server over HTTP and WebSocket from tests.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    dead_code
)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use collab_core::client_state::{ClientState, Outgoing};
use collab_core::ids::{ClientId, DocId, OpId, Seq, UserId};
use collab_text::{TextDoc, TextOp};
use futures_util::{SinkExt, StreamExt};
use protocol::{ClientMsg, Participant, RemovedReason, Resume, RoomEpoch, SaveState, ServerMsg};
use records::test_support::TestDb;
use records::{Db, NewUser, Role};
use reqwest::header::{COOKIE, HeaderMap, HeaderValue, ORIGIN, SET_COOKIE};
use revisions::RevisionStore;
use server::{AppState, Checkpointer, Config};
use tempfile::TempDir;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::handshake::client::Request;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use uuid::Uuid;

/// How long any wait in a test may take before it fails.
pub const TIMEOUT: Duration = Duration::from_secs(5);

/// The email of the administrator the harness acts as when it grants access directly.
const HARNESS_ADMIN_EMAIL: &str = "harness-admin@test.invalid";

/// A running server as tests reach it: its address, its database, and the accounts the
/// harness made on it.
pub struct Site {
    pub addr: SocketAddr,
    records: Db,
    /// Accounts made by [`Site::user`] and [`Site::admin`], by name.
    accounts: Mutex<HashMap<String, Account>>,
}

/// An account the harness created, with the password it chose and keeps to itself.
#[derive(Clone)]
pub struct Account {
    id: UserId,
    email: String,
    password: String,
}

/// A signed-in account.
#[derive(Clone)]
pub struct TestUser {
    pub id: UserId,
    pub email: String,
    /// The `Cookie` header value: `sharecraft_session=<token>`.
    pub cookie: String,
}

impl std::fmt::Debug for TestUser {
    /// Leaves the cookie out: it holds the session token.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestUser")
            .field("id", &self.id)
            .field("email", &self.email)
            .finish_non_exhaustive()
    }
}

impl Site {
    fn new(addr: SocketAddr, records: Db) -> Site {
        Site {
            addr,
            records,
            accounts: Mutex::default(),
        }
    }

    /// `http://<addr>`, the origin of pages this server serves.
    pub fn origin(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// `http://<addr><path>`.
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    /// The account `<slug of name>@test.invalid`, named `name`, created on first use, freshly
    /// signed in.
    pub async fn user(&self, name: &str) -> TestUser {
        self.signed_in(name, false).await
    }

    /// Like [`Site::user`], for an administrator account.
    pub async fn admin(&self, name: &str) -> TestUser {
        self.signed_in(name, true).await
    }

    /// A client that sends this server's `Origin` on every request, and `as_user`'s cookie.
    pub fn http(&self, as_user: Option<&TestUser>) -> reqwest::Client {
        let mut headers = HeaderMap::new();
        headers.insert(ORIGIN, HeaderValue::from_str(&self.origin()).unwrap());
        if let Some(user) = as_user {
            headers.insert(COOKIE, HeaderValue::from_str(&user.cookie).unwrap());
        }
        reqwest::Client::builder()
            .default_headers(headers)
            .timeout(TIMEOUT)
            .build()
            .unwrap()
    }

    /// Makes sure `user` has a role on `doc`, writing the records directly: creates the
    /// document, named `<doc id>.txt` and owned by `user`, if it has no record; otherwise grants
    /// `user` Editor if they have no role.
    pub async fn give_access(&self, doc: DocId, user: &TestUser) {
        let db = &self.records;
        if db.document(doc).await.unwrap().is_none() {
            db.create_document(user.id, doc, &format!("{}.txt", doc.0))
                .await
                .unwrap();
        } else if db.role(user.id, doc).await.unwrap().is_none() {
            let admin = self.harness_admin().await;
            db.set_access(admin, doc, user.id, Some(Role::Editor))
                .await
                .unwrap();
        }
    }

    /// The administrator the harness grants access as, created on first use. It never signs
    /// in, so it is never anyone's participant.
    async fn harness_admin(&self) -> UserId {
        let db = &self.records;
        if let Some(user) = db.user_by_email(HARNESS_ADMIN_EMAIL).await.unwrap() {
            return user.id;
        }
        let password = Uuid::new_v4().simple().to_string();
        db.create_user(
            None,
            NewUser {
                email: HARNESS_ADMIN_EMAIL,
                name: "Harness Admin",
                password: &password,
                is_admin: true,
            },
        )
        .await
        .unwrap()
        .id
    }

    async fn signed_in(&self, name: &str, is_admin: bool) -> TestUser {
        let account = self.account(name, is_admin).await;
        let response = self
            .http(None)
            .post(self.url("/api/session"))
            .json(&serde_json::json!({
                "email": account.email,
                "password": account.password,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::NO_CONTENT);
        TestUser {
            id: account.id,
            email: account.email,
            cookie: session_cookie(response.headers()).expect("sign-in set no session cookie"),
        }
    }

    async fn account(&self, name: &str, is_admin: bool) -> Account {
        let email = {
            let accounts = self.accounts.lock().unwrap();
            if let Some(account) = accounts.get(name) {
                return account.clone();
            }
            unused_email(name, &accounts)
        };
        let password = Uuid::new_v4().simple().to_string();
        let user = self
            .records
            .create_user(
                None,
                NewUser {
                    email: &email,
                    name,
                    password: &password,
                    is_admin,
                },
            )
            .await
            .unwrap();
        let account = Account {
            id: user.id,
            email,
            password,
        };
        self.accounts
            .lock()
            .unwrap()
            .insert(name.to_owned(), account.clone());
        account
    }
}

/// A server running in this test's runtime, with its own data directory and database schema.
pub struct TestServer {
    site: Site,
    pub data_dir: TempDir,
    pub db: TestDb,
}

impl Deref for TestServer {
    type Target = Site;

    fn deref(&self) -> &Site {
        &self.site
    }
}

impl TestServer {
    /// The library repository the server stores documents in.
    pub fn library(&self) -> PathBuf {
        server::library_path(self.data_dir.path())
    }

    /// Opens the server's library repository.
    pub fn store(&self) -> RevisionStore {
        RevisionStore::open(&self.library()).unwrap()
    }

    /// The commits that changed `doc`, newest first.
    pub fn doc_commits(&self, doc: DocId) -> Vec<String> {
        let repo = gix::open(self.library()).unwrap();
        let tip = repo
            .find_reference(&format!("refs/sharecraft/docs/{}", doc.0))
            .unwrap()
            .id();
        repo.rev_walk([tip])
            .all()
            .unwrap()
            .map(|info| info.unwrap().id.to_string())
            .collect()
    }
}

/// `<slug>@test.invalid` for `name`, with a number added if another name has that slug.
fn unused_email(name: &str, accounts: &HashMap<String, Account>) -> String {
    let mut slug = String::new();
    for c in name.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = match slug.trim_end_matches('-') {
        "" => "user",
        trimmed => trimmed,
    };
    let taken = |email: &str| accounts.values().any(|a| a.email == email);
    let mut email = format!("{slug}@test.invalid");
    let mut n = 2;
    while taken(&email) {
        email = format!("{slug}-{n}@test.invalid");
        n += 1;
    }
    email
}

/// The `sharecraft_session=<token>` pair from a response's `Set-Cookie`, if it sets one.
pub fn session_cookie(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(|v| v.split(';').next())
        .map(str::trim)
        .find(|pair| pair.starts_with("sharecraft_session="))
        .map(str::to_owned)
}

fn config_at(data_dir: &Path, db: &TestDb) -> Config {
    Config {
        addr: "127.0.0.1:0".parse().unwrap(),
        data_dir: data_dir.to_path_buf(),
        web_dir: data_dir.join("web"),
        checkpoint_idle: Duration::from_millis(100),
        checkpoint_max: Duration::from_secs(30),
        room_grace: LONG_GRACE,
        database_url: server::config::DatabaseUrl::new(db.url()),
    }
}

/// A grace period no test waits out: an empty room stays open for the whole test.
pub const LONG_GRACE: Duration = Duration::from_secs(60);
/// A grace period tests wait out (see [`wait_out_grace`]) when they need a room to end.
pub const SHORT_GRACE: Duration = Duration::from_millis(300);

/// Waits until a room that became empty with [`SHORT_GRACE`] has certainly ended.
pub async fn wait_out_grace() {
    tokio::time::sleep(SHORT_GRACE * 3).await;
}

/// Runs a server, panicking with its error if it stops, for example because it could not reach
/// or migrate the database after the listener was bound. The panic is printed with the test's
/// output, and the test's next request fails after [`TIMEOUT`] instead of hanging.
async fn fail_loudly(serving: impl std::future::Future<Output = std::io::Result<()>>) {
    if let Err(e) = serving.await {
        panic!("the test server stopped: {e}");
    }
}

/// Starts a server on an ephemeral loopback port with its own data directory and database
/// schema; empty rooms stay open for [`LONG_GRACE`].
pub async fn spawn_server() -> TestServer {
    spawn_server_config(|_| {}).await
}

/// Starts a server like [`spawn_server`], with its settings adjusted by `edit`.
pub async fn spawn_server_config(edit: impl FnOnce(&mut Config)) -> TestServer {
    let data_dir = tempfile::tempdir().unwrap();
    let db = TestDb::create().await;
    let mut config = config_at(data_dir.path(), &db);
    edit(&mut config);
    let listener = tokio::net::TcpListener::bind(config.addr).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(fail_loudly(server::serve(config, listener)));
    TestServer {
        site: Site::new(addr, db.db.clone()),
        data_dir,
        db,
    }
}

/// A server on its own runtime. Stopping it ends every connection, room and task at once, as
/// stopping the server process would, and leaves its data directory for a new server.
pub struct StoppableServer {
    site: Site,
    runtime: Option<tokio::runtime::Runtime>,
}

impl Deref for StoppableServer {
    type Target = Site;

    fn deref(&self) -> &Site {
        &self.site
    }
}

impl StoppableServer {
    /// Starts a server on an ephemeral loopback port storing documents under `data_dir` and
    /// records in `db`'s schema, with its settings adjusted by `edit`.
    pub fn start(data_dir: &Path, db: &TestDb, edit: impl FnOnce(&mut Config)) -> StoppableServer {
        let mut config = config_at(data_dir, db);
        edit(&mut config);
        let listener = std::net::TcpListener::bind(config.addr).unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        runtime.spawn(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            fail_loudly(server::serve(config, listener)).await;
        });
        StoppableServer {
            site: Site::new(addr, db.db.clone()),
            runtime: Some(runtime),
        }
    }

    /// Stops the server without saving anything further.
    pub fn stop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

impl Drop for StoppableServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Waits until the stored text of `doc` in `server`'s library is `text`.
pub async fn wait_stored(server: &TestServer, doc: DocId, text: &str) {
    wait_stored_in(&server.library(), doc, text).await;
}

/// Waits until the stored text of `doc` in the library repository at `library` is `text`.
pub async fn wait_stored_in(library: &Path, doc: DocId, text: &str) {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    let store = RevisionStore::open(library).unwrap();
    loop {
        let stored = store.read(doc, None).unwrap();
        if stored.is_some_and(|s| s.bytes == text.as_bytes()) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{text:?} was never stored"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Starts a server like [`spawn_server`], and returns its state too, for the test hooks on
/// [`AppState`].
pub async fn spawn_server_with_state() -> (TestServer, AppState) {
    let data_dir = tempfile::tempdir().unwrap();
    let db = TestDb::create().await;
    let config = config_at(data_dir.path(), &db);
    let store = RevisionStore::open(&server::library_path(data_dir.path())).unwrap();
    let listener = tokio::net::TcpListener::bind(config.addr).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = AppState::new(config, Arc::new(store), db.db.clone());
    tokio::spawn(fail_loudly(server::serve_state(state.clone(), listener)));
    let server = TestServer {
        site: Site::new(addr, db.db.clone()),
        data_dir,
        db,
    };
    (server, state)
}

/// Starts a server whose rooms load and save through the checkpointer `make` builds from the
/// library repository path.
pub async fn spawn_server_with(make: impl FnOnce(&Path) -> Arc<dyn Checkpointer>) -> TestServer {
    spawn_server_with_grace(make, LONG_GRACE).await
}

/// Like [`spawn_server_with`], with empty rooms staying open for `grace`.
pub async fn spawn_server_with_grace(
    make: impl FnOnce(&Path) -> Arc<dyn Checkpointer>,
    grace: Duration,
) -> TestServer {
    let data_dir = tempfile::tempdir().unwrap();
    let db = TestDb::create().await;
    let mut config = config_at(data_dir.path(), &db);
    config.room_grace = grace;
    let checkpointer = make(&server::library_path(data_dir.path()));
    let listener = tokio::net::TcpListener::bind(config.addr).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = AppState::new(config, checkpointer, db.db.clone());
    tokio::spawn(fail_loudly(server::serve_state(state, listener)));
    TestServer {
        site: Site::new(addr, db.db.clone()),
        data_dir,
        db,
    }
}

pub type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// A WebSocket handshake request for `ws://<addr><path>` from a page this server served
/// (same-origin `Origin`), carrying `cookie` if given.
pub fn ws_request(addr: SocketAddr, path: &str, cookie: Option<&str>) -> Request {
    let mut request = format!("ws://{addr}{path}").into_client_request().unwrap();
    let headers = request.headers_mut();
    headers.insert(
        ORIGIN,
        HeaderValue::from_str(&format!("http://{addr}")).unwrap(),
    );
    if let Some(cookie) = cookie {
        headers.insert(COOKIE, HeaderValue::from_str(cookie).unwrap());
    }
    request
}

/// Opens a socket at `path` with `cookie`; fails the test unless the handshake succeeds.
pub async fn open_socket_at(addr: SocketAddr, path: &str, cookie: &str) -> Socket {
    let request = ws_request(addr, path, Some(cookie));
    let (socket, _) = tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(request))
        .await
        .expect("connect timed out")
        .unwrap();
    socket
}

/// Opens a socket to `doc` as `user`.
pub async fn open_socket(addr: SocketAddr, doc: DocId, user: &TestUser) -> Socket {
    open_socket_at(addr, &format!("/ws/docs/{}", doc.0), &user.cookie).await
}

/// The HTTP status a WebSocket handshake `request` is answered with: 101 when it upgrades.
pub async fn handshake_status(request: Request) -> u16 {
    let result = tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(request))
        .await
        .expect("handshake timed out");
    match result {
        Ok((_, response)) => response.status().as_u16(),
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => response.status().as_u16(),
        Err(other) => panic!("unexpected error {other:?}"),
    }
}

/// One browser session: a socket plus the client half of the protocol.
pub struct TestClient {
    addr: SocketAddr,
    doc: DocId,
    /// The signed-in account this session belongs to.
    pub user: TestUser,
    socket: Option<Socket>,
    state: ClientState<TextDoc>,
    people: Vec<Participant>,
    /// Every message received, in order.
    pub received: Vec<ServerMsg<TextOp>>,
    /// Set when `Joined` or `Reset` arrives for the current socket.
    pub joined: bool,
    /// Set when the server closed the current socket.
    pub closed: bool,
    /// The room lifetime from the last `Joined` or `Reset`.
    pub epoch: Option<RoomEpoch>,
    /// Why the server removed this session, once it has. A page that is removed does not
    /// reconnect, and neither does this client: [`TestClient::reconnect`] refuses.
    pub removed: Option<RemovedReason>,
    /// Whether this connection may change the text, from the last `Joined`, `Reset` or
    /// `Access`.
    pub can_edit: bool,
}

impl TestClient {
    /// Signs in the account named `name`, makes sure it has a role on `doc` (see
    /// [`Site::give_access`]), opens a socket and joins for the first time.
    pub async fn connect(site: &Site, doc: DocId, name: &str) -> TestClient {
        let mut c = TestClient::connect_with_resume(site, doc, name, None).await;
        c.pump_until(|c| c.joined).await;
        c
    }

    /// Opens a socket to `doc` as `user` and joins for the first time. No record or grant is
    /// made.
    pub async fn connect_as(site: &Site, doc: DocId, user: &TestUser) -> TestClient {
        let mut c = TestClient::open_as(site.addr, doc, user.clone(), None).await;
        c.pump_until(|c| c.joined).await;
        c
    }

    /// Like [`TestClient::connect`], sending `Join` with the given `resume` and not waiting
    /// for a reply.
    pub async fn connect_with_resume(
        site: &Site,
        doc: DocId,
        name: &str,
        resume: Option<Resume>,
    ) -> TestClient {
        let user = site.user(name).await;
        site.give_access(doc, &user).await;
        TestClient::open_as(site.addr, doc, user, resume).await
    }

    /// Opens a socket as `user` and sends `Join` with `resume`, without waiting for a reply.
    async fn open_as(
        addr: SocketAddr,
        doc: DocId,
        user: TestUser,
        resume: Option<Resume>,
    ) -> TestClient {
        let client = ClientId(Uuid::new_v4());
        let socket = open_socket(addr, doc, &user).await;
        let mut c = TestClient {
            addr,
            doc,
            user,
            socket: Some(socket),
            state: ClientState::new(client, TextDoc::default(), Seq(0)),
            people: Vec::new(),
            received: Vec::new(),
            joined: false,
            closed: false,
            epoch: None,
            removed: None,
            can_edit: false,
        };
        c.send(&ClientMsg::Join { client, resume }).await;
        c
    }

    pub fn client(&self) -> ClientId {
        self.state.client()
    }

    pub fn text(&self) -> String {
        self.state.doc().as_str().to_owned()
    }

    pub fn seq(&self) -> Seq {
        self.state.seq()
    }

    pub fn pending(&self) -> usize {
        self.state.pending()
    }

    pub fn in_flight(&self) -> Option<Outgoing<TextOp>> {
        self.state.in_flight()
    }

    pub fn people(&self) -> Vec<String> {
        self.people.iter().map(|p| p.name.clone()).collect()
    }

    pub fn participants(&self) -> &[Participant] {
        &self.people
    }

    /// Inserts `s` at character position `pos` and sends it if nothing is in flight. While
    /// disconnected the change is kept, as a page keeps it, and sent after reconnecting.
    pub async fn type_at(&mut self, pos: usize, s: &str) {
        let op = TextOp::replace(self.state.doc().len_chars(), pos, 0, s).unwrap();
        if let Some(out) = self.state.local(OpId(Uuid::new_v4()), op).unwrap()
            && self.socket.is_some()
        {
            self.submit(&out).await;
        }
    }

    pub async fn submit(&mut self, out: &Outgoing<TextOp>) {
        self.send(&ClientMsg::Submit {
            op_id: out.op_id,
            base: out.base,
            op: out.op.clone(),
        })
        .await;
    }

    pub async fn send(&mut self, msg: &ClientMsg<TextOp>) {
        let json = serde_json::to_string(msg).unwrap();
        self.send_raw(Message::text(json)).await;
    }

    pub async fn send_raw(&mut self, msg: Message) {
        let socket = self.socket.as_mut().expect("not connected");
        socket.send(msg).await.unwrap();
    }

    /// Sends a frame the server may refuse mid-write; a failed send marks the socket closed.
    pub async fn send_raw_lossy(&mut self, msg: Message) {
        let socket = self.socket.as_mut().expect("not connected");
        if socket.send(msg).await.is_err() {
            self.closed = true;
        }
    }

    /// Reads and handles messages until `cond` holds. Fails after five seconds, or if the
    /// socket closes while `cond` is still false.
    pub async fn pump_until(&mut self, cond: impl Fn(&TestClient) -> bool) {
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        while !cond(self) {
            assert!(!self.closed, "socket closed while waiting");
            let socket = self.socket.as_mut().expect("not connected");
            let next = tokio::time::timeout_at(deadline, socket.next())
                .await
                .expect("timed out waiting for condition");
            match next {
                Some(Ok(Message::Text(text))) => {
                    let msg: ServerMsg<TextOp> = serde_json::from_str(&text).unwrap();
                    self.handle(msg).await;
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => self.closed = true,
                Some(Ok(_)) => {}
            }
        }
    }

    /// The `Save` messages received, in order.
    pub fn saves(&self) -> Vec<(SaveState, Option<String>, Seq)> {
        self.received
            .iter()
            .filter_map(|m| match m {
                ServerMsg::Save {
                    state,
                    revision,
                    through,
                } => Some((*state, revision.clone(), *through)),
                _ => None,
            })
            .collect()
    }

    /// The revision of the latest `Save{Saved}` covering `through`, if one has arrived.
    pub fn saved_revision(&self, through: Seq) -> Option<String> {
        self.saves()
            .into_iter()
            .rev()
            .find_map(|(state, revision, t)| {
                (state == SaveState::Saved && t >= through)
                    .then_some(revision)
                    .flatten()
            })
    }

    /// Waits for a `Save{Saved}` covering `through` and returns its revision.
    pub async fn wait_saved(&mut self, through: Seq) -> String {
        self.pump_until(|c| c.saved_revision(through).is_some())
            .await;
        self.saved_revision(through).unwrap()
    }

    /// The revision named by the last `Joined` or `Reset`.
    pub fn joined_revision(&self) -> Option<String> {
        self.received.iter().rev().find_map(|m| match m {
            ServerMsg::Joined { revision, .. } | ServerMsg::Reset { revision, .. } => {
                Some(revision.clone())
            }
            _ => None,
        })?
    }

    /// Waits until the server closes this socket.
    pub async fn wait_closed(&mut self) {
        self.pump_until(|c| c.closed).await;
    }

    async fn handle(&mut self, msg: ServerMsg<TextOp>) {
        self.received.push(msg.clone());
        match msg {
            ServerMsg::Joined {
                epoch,
                seq,
                snapshot,
                participants,
                can_edit,
                ..
            } => {
                if let Some(text) = snapshot {
                    self.state.reset(TextDoc::new(text), seq);
                }
                self.people = participants;
                self.epoch = Some(epoch);
                self.can_edit = can_edit;
                self.joined = true;
            }
            ServerMsg::Reset {
                epoch,
                seq,
                snapshot,
                can_edit,
                ..
            } => {
                self.state.reset(TextDoc::new(snapshot), seq);
                self.epoch = Some(epoch);
                self.can_edit = can_edit;
                self.joined = true;
            }
            ServerMsg::Committed { entry } => {
                // Every entry must be the next one; a duplicate or gap fails the test.
                let applied = self
                    .state
                    .committed(&entry)
                    .unwrap_or_else(|e| panic!("bad committed entry {entry:?}: {e}"));
                if let Some(out) = applied.send_next {
                    self.submit(&out).await;
                }
            }
            ServerMsg::People { participants } => self.people = participants,
            ServerMsg::Access { can_edit } => self.can_edit = can_edit,
            ServerMsg::Removed { reason } => self.removed = Some(reason),
            ServerMsg::Save { .. } | ServerMsg::Refused { .. } => {}
        }
    }

    /// Closes the socket politely and waits for the server to finish the close.
    pub async fn close(&mut self) {
        let Some(mut socket) = self.socket.take() else {
            return;
        };
        let _ = socket.close(None).await;
        let drained = tokio::time::timeout(TIMEOUT, async {
            while let Some(Ok(_)) = socket.next().await {}
        })
        .await;
        assert!(drained.is_ok(), "server did not finish closing");
        self.joined = false;
    }

    /// Drops the socket without reading anything further or closing politely.
    pub fn disconnect(&mut self) {
        self.socket = None;
        self.joined = false;
    }

    /// Opens a new socket, joins with the room epoch and last applied sequence, and waits for
    /// `Joined` or `Reset`. After `Joined` the change in flight is resent (the server drops it
    /// if it was already committed); after `Reset` it has been discarded.
    pub async fn reconnect(&mut self) {
        assert!(
            self.removed.is_none(),
            "a removed session does not reconnect ({:?})",
            self.removed
        );
        self.socket = Some(open_socket(self.addr, self.doc, &self.user).await);
        self.joined = false;
        self.closed = false;
        let client = self.state.client();
        let resume = self.epoch.map(|epoch| Resume {
            epoch,
            seq: self.state.seq(),
        });
        let in_flight = self.state.in_flight();
        let before = self.received.len();
        self.send(&ClientMsg::Join { client, resume }).await;
        self.pump_until(|c| c.joined).await;
        let resumed = matches!(self.received.get(before), Some(ServerMsg::Joined { .. }));
        if resumed && let Some(out) = in_flight {
            self.submit(&out).await;
        }
    }
}

/// A fresh document identifier.
pub fn new_doc() -> DocId {
    DocId(Uuid::new_v4())
}

/// Polls `db` for `doc`'s events until `done` holds for them or [`TIMEOUT`] passes (the writer
/// that records them is asynchronous), and returns them.
pub async fn wait_events(
    db: &Db,
    doc: DocId,
    done: impl Fn(&[records::Event]) -> bool,
) -> Vec<records::Event> {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        let events = db.events_for(doc).await.unwrap();
        if done(&events) {
            return events;
        }
        let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
        assert!(
            tokio::time::Instant::now() < deadline,
            "the expected events were not recorded; recorded: {kinds:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
