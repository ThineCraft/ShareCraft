//! Helpers for driving a real server over WebSocket from tests.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    dead_code
)]

use std::net::SocketAddr;
use std::time::Duration;

use collab_core::client_state::{ClientState, Outgoing};
use collab_core::ids::{ClientId, DocId, OpId, Seq};
use collab_text::{TextDoc, TextOp};
use futures_util::{SinkExt, StreamExt};
use protocol::{ClientMsg, Participant, ServerMsg};
use server::Config;
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

/// Starts a server on an ephemeral loopback port with its own data directory.
pub async fn spawn_server() -> TestServer {
    let data_dir = tempfile::tempdir().unwrap();
    let config = Config {
        addr: "127.0.0.1:0".parse().unwrap(),
        data_dir: data_dir.path().to_path_buf(),
        web_dir: data_dir.path().join("web"),
        checkpoint_idle: Duration::from_millis(100),
        checkpoint_max: Duration::from_secs(30),
    };
    let listener = tokio::net::TcpListener::bind(config.addr).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(server::serve(config, listener));
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
        let mut c = TestClient::connect_with_since(addr, doc, name, None).await;
        c.pump_until(|c| c.joined).await;
        c
    }

    /// Opens a socket and sends `Join` with the given `since`, without waiting for a reply.
    pub async fn connect_with_since(
        addr: SocketAddr,
        doc: DocId,
        name: &str,
        since: Option<Seq>,
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
        };
        c.send(&ClientMsg::Join { client, since }).await;
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

    /// Inserts `s` at character position `pos` and sends it if nothing is in flight.
    pub async fn type_at(&mut self, pos: usize, s: &str) {
        let op = TextOp::replace(self.state.doc().len_chars(), pos, 0, s).unwrap();
        if let Some(out) = self.state.local(OpId(Uuid::new_v4()), op).unwrap() {
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

    /// Waits until the server closes this socket.
    pub async fn wait_closed(&mut self) {
        self.pump_until(|c| c.closed).await;
    }

    async fn handle(&mut self, msg: ServerMsg<TextOp>) {
        self.received.push(msg.clone());
        match msg {
            ServerMsg::Joined {
                seq,
                snapshot,
                participants,
                ..
            } => {
                if let Some(text) = snapshot {
                    self.state.reset(TextDoc::new(text), seq);
                }
                self.people = participants;
                self.joined = true;
            }
            ServerMsg::Reset { seq, snapshot, .. } => {
                self.state.reset(TextDoc::new(snapshot), seq);
                self.joined = true;
            }
            ServerMsg::Committed { entry } => {
                // A resent confirmation of something already applied is ignored.
                if entry.seq <= self.state.seq() {
                    return;
                }
                let applied = self.state.committed(&entry).unwrap();
                if let Some(out) = applied.send_next {
                    self.submit(&out).await;
                }
            }
            ServerMsg::People { participants } => self.people = participants,
            ServerMsg::Save { .. } | ServerMsg::Refused { .. } => {}
        }
    }

    /// Drops the socket without reading anything further or closing politely.
    pub fn disconnect(&mut self) {
        self.socket = None;
        self.joined = false;
    }

    /// Opens a new socket, joins with the last applied sequence, resends the change in
    /// flight and waits for `Joined` or `Reset`.
    pub async fn reconnect(&mut self) {
        self.socket = Some(open(self.addr, self.doc, &self.name).await);
        self.joined = false;
        self.closed = false;
        let client = self.state.client();
        let since = Some(self.state.seq());
        self.send(&ClientMsg::Join { client, since }).await;
        if let Some(out) = self.state.in_flight() {
            self.submit(&out).await;
        }
        self.pump_until(|c| c.joined).await;
    }
}

/// A fresh document identifier.
pub fn new_doc() -> DocId {
    DocId(Uuid::new_v4())
}
