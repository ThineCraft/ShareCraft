//! Helpers for driving a real server over WebSocket from tests.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    dead_code
)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use collab_core::client_state::{ClientState, Outgoing};
use collab_core::ids::{ClientId, DocId, OpId, Seq};
use collab_text::{TextDoc, TextOp};
use futures_util::{SinkExt, StreamExt};
use protocol::{ClientMsg, Participant, Resume, RoomEpoch, SaveState, ServerMsg};
use revisions::RevisionStore;
use server::{AppState, Checkpointer, Config};
use tempfile::TempDir;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use uuid::Uuid;

/// How long any wait in a test may take before it fails.
pub const TIMEOUT: Duration = Duration::from_secs(5);

/// A server running in this test's runtime.
pub struct TestServer {
    pub addr: SocketAddr,
    pub data_dir: TempDir,
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
        let repo = git2::Repository::open_bare(self.library()).unwrap();
        let mut walk = repo.revwalk().unwrap();
        walk.push_ref(&format!("refs/sharecraft/docs/{}", doc.0))
            .unwrap();
        walk.map(|oid| oid.unwrap().to_string()).collect()
    }
}

fn test_config(data_dir: &TempDir) -> Config {
    config_at(data_dir.path())
}

fn config_at(data_dir: &Path) -> Config {
    Config {
        addr: "127.0.0.1:0".parse().unwrap(),
        data_dir: data_dir.to_path_buf(),
        web_dir: data_dir.join("web"),
        checkpoint_idle: Duration::from_millis(100),
        checkpoint_max: Duration::from_secs(30),
        room_grace: LONG_GRACE,
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

/// Starts a server on an ephemeral loopback port with its own data directory; empty rooms
/// stay open for [`LONG_GRACE`].
pub async fn spawn_server() -> TestServer {
    spawn_server_config(|_| {}).await
}

/// Starts a server like [`spawn_server`], with its settings adjusted by `edit`.
pub async fn spawn_server_config(edit: impl FnOnce(&mut Config)) -> TestServer {
    let data_dir = tempfile::tempdir().unwrap();
    let mut config = test_config(&data_dir);
    edit(&mut config);
    let listener = tokio::net::TcpListener::bind(config.addr).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(server::serve(config, listener));
    TestServer { addr, data_dir }
}

/// A server on its own runtime. Stopping it ends every connection, room and task at once, as
/// stopping the server process would, and leaves its data directory for a new server.
pub struct StoppableServer {
    pub addr: SocketAddr,
    runtime: Option<tokio::runtime::Runtime>,
}

impl StoppableServer {
    /// Starts a server on an ephemeral loopback port storing documents under `data_dir`, with
    /// its settings adjusted by `edit`.
    pub fn start(data_dir: &Path, edit: impl FnOnce(&mut Config)) -> StoppableServer {
        let mut config = config_at(data_dir);
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
            server::serve(config, listener).await
        });
        StoppableServer {
            addr,
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
    let mut config = test_config(&data_dir);
    config.room_grace = grace;
    let checkpointer = make(&server::library_path(data_dir.path()));
    let listener = tokio::net::TcpListener::bind(config.addr).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = AppState::new(config, checkpointer);
    tokio::spawn(server::serve_state(state, listener));
    TestServer { addr, data_dir }
}

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// One browser session: a socket plus the client half of the protocol.
pub struct TestClient {
    addr: SocketAddr,
    doc: DocId,
    name: String,
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
}

async fn open(addr: SocketAddr, doc: DocId, name: &str) -> Socket {
    let url = format!("ws://{addr}/ws/docs/{}?name={name}", doc.0);
    let (socket, _) = tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(url))
        .await
        .expect("connect timed out")
        .unwrap();
    socket
}

impl TestClient {
    /// Opens a socket to `doc` and joins it for the first time.
    pub async fn connect(addr: SocketAddr, doc: DocId, name: &str) -> TestClient {
        let mut c = TestClient::connect_with_resume(addr, doc, name, None).await;
        c.pump_until(|c| c.joined).await;
        c
    }

    /// Opens a socket and sends `Join` with the given `resume`, without waiting for a reply.
    pub async fn connect_with_resume(
        addr: SocketAddr,
        doc: DocId,
        name: &str,
        resume: Option<Resume>,
    ) -> TestClient {
        let client = ClientId(Uuid::new_v4());
        let mut c = TestClient {
            addr,
            doc,
            name: name.to_owned(),
            socket: Some(open(addr, doc, name).await),
            state: ClientState::new(client, TextDoc::default(), Seq(0)),
            people: Vec::new(),
            received: Vec::new(),
            joined: false,
            closed: false,
            epoch: None,
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
                ..
            } => {
                if let Some(text) = snapshot {
                    self.state.reset(TextDoc::new(text), seq);
                }
                self.people = participants;
                self.epoch = Some(epoch);
                self.joined = true;
            }
            ServerMsg::Reset {
                epoch,
                seq,
                snapshot,
                ..
            } => {
                self.state.reset(TextDoc::new(snapshot), seq);
                self.epoch = Some(epoch);
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
        self.socket = Some(open(self.addr, self.doc, &self.name).await);
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
