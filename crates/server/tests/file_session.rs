//! File sessions over real sockets: a file document is opened in an editor by one person at a
//! time. Whoever may edit and joins while nobody is editing gets the one editing seat; the
//! others watch. Seats, joins and removal are tested here; saves are tested inside the room
//! (`file_room.rs`), as no test-only route is added to the server.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use collab_core::ids::{ClientId, DocId, UserId};
use collab_text::TextOp;
use common::{
    FileClient, SHORT_GRACE, Site, TestServer, TestUser, new_doc, open_socket, spawn_server,
    spawn_server_with_grace,
};
use futures_util::{SinkExt, StreamExt};
use protocol::{ClientMsg, FileClientMsg, FileServerMsg, RemovedReason, SeatState};
use reqwest::StatusCode;
use revisions::{
    Checkpoint, CheckpointError, GitError, Outcome, RevisionId, RevisionInfo, RevisionStore, Stored,
};
use server::Checkpointer;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

/// How soon a person must be out of a session once their access ended.
const PROMPTLY: Duration = Duration::from_secs(1);

/// `owner` gives `user` the role `role` on `doc` through the API.
async fn share(site: &Site, owner: &TestUser, doc: DocId, user: &TestUser, role: &str) {
    let response = site
        .http(Some(owner))
        .put(site.url(&format!("/api/docs/{}/access", doc.0)))
        .json(&serde_json::json!({ "email": user.email, "role": role }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

/// `owner` removes `user`'s grant on `doc` through the API.
async fn revoke(site: &Site, owner: &TestUser, doc: DocId, user: UserId) {
    let response = site
        .http(Some(owner))
        .delete(site.url(&format!("/api/docs/{}/access/{}", doc.0, user.0)))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

/// A file document owned by Ann, with Bob as an editor. Returns the document, Ann and Bob.
async fn shared_file(server: &TestServer) -> (DocId, TestUser, TestUser) {
    let doc = new_doc();
    let ann = server.user("Ann").await;
    let bob = server.user("Bob").await;
    server.create_file_document(doc, &ann).await;
    share(server, &ann, doc, &bob, "editor").await;
    (doc, ann, bob)
}

/// Whether `seat` is held by someone else, namely `user`.
fn taken_by(seat: Option<&SeatState>, user: UserId) -> bool {
    matches!(seat, Some(SeatState::Taken { by }) if by.user == user)
}

/// Whether `token` is 32 bytes written as lowercase hexadecimal.
fn is_token(token: &str) -> bool {
    token.len() == 64
        && token
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

#[tokio::test]
async fn first_editor_gets_the_seat() {
    let server = spawn_server().await;
    let (doc, ann, _) = shared_file(&server).await;
    let a = FileClient::connect_as(&server, doc, &ann).await;
    assert!(a.can_edit);
    let token = a.token().expect("the first editor holds the seat");
    assert!(is_token(&token), "not a 32-byte hex token");
    assert!(matches!(
        a.received.first(),
        Some(FileServerMsg::Joined { revision: None, participants, .. })
            if participants.len() == 1 && participants[0].user == ann.id
    ));
}

#[tokio::test]
async fn second_editor_does_not_get_the_seat() {
    let server = spawn_server().await;
    let (doc, ann, bob) = shared_file(&server).await;
    let mut a = FileClient::connect_as(&server, doc, &ann).await;
    let b = FileClient::connect_as(&server, doc, &bob).await;
    assert!(b.can_edit);
    assert!(taken_by(b.seat.as_ref(), ann.id), "{:?}", b.seat);
    a.pump_until(|c| c.participants.len() == 2).await;
    assert!(a.token().is_some(), "Ann keeps the seat");
    assert!(
        a.seat_messages().is_empty(),
        "the seat did not change hands"
    );
}

#[tokio::test]
async fn viewer_never_gets_the_seat() {
    let server = spawn_server().await;
    let (doc, ann, _) = shared_file(&server).await;
    let vic = server.user("Vic").await;
    share(&server, &ann, doc, &vic, "viewer").await;
    // Vic is first, and the seat is free; he still does not get it.
    let mut v = FileClient::connect_as(&server, doc, &vic).await;
    assert!(!v.can_edit);
    assert_eq!(v.seat, Some(SeatState::Free));

    let a = FileClient::connect_as(&server, doc, &ann).await;
    assert!(a.token().is_some());
    v.pump_until(|c| taken_by(c.seat.as_ref(), ann.id)).await;
    assert!(
        !v.received.iter().any(|m| matches!(
            m,
            FileServerMsg::Joined {
                seat: SeatState::Yours { .. },
                ..
            } | FileServerMsg::Seat {
                seat: SeatState::Yours { .. }
            }
        )),
        "{:?}",
        v.received
    );
}

#[tokio::test]
async fn seat_is_freed_when_the_holder_leaves_and_not_handed_on() {
    let server = spawn_server().await;
    let (doc, ann, bob) = shared_file(&server).await;
    let mut a = FileClient::connect_as(&server, doc, &ann).await;
    let mut b = FileClient::connect_as(&server, doc, &bob).await;
    assert!(taken_by(b.seat.as_ref(), ann.id));

    a.close().await;
    b.pump_until(|c| c.participants.len() == 1).await;
    b.pump_until(|c| c.seat == Some(SeatState::Free)).await;
    assert_eq!(b.seat_messages(), vec![SeatState::Free]);
    assert!(b.token().is_none(), "nobody is given the seat");
}

#[tokio::test]
async fn rejoining_takes_a_free_seat_with_a_new_token() {
    let server = spawn_server().await;
    let (doc, ann, bob) = shared_file(&server).await;
    let mut a = FileClient::connect_as(&server, doc, &ann).await;
    let first = a.token().unwrap();

    // The holder joins again: a new token.
    a.rejoin().await;
    let second = a.token().expect("the seat was free when Ann joined again");
    assert_ne!(first, second);

    // Bob watches until Ann leaves, and takes the free seat by joining again.
    let mut b = FileClient::connect_as(&server, doc, &bob).await;
    assert!(taken_by(b.seat.as_ref(), ann.id));
    a.close().await;
    b.pump_until(|c| c.seat == Some(SeatState::Free)).await;
    b.rejoin().await;
    let bobs = b.token().expect("Bob joined again while the seat was free");
    assert!(is_token(&bobs));
    assert_ne!(bobs, first);
    assert_ne!(bobs, second);
}

#[tokio::test]
async fn demoted_holder_loses_the_seat() {
    let server = spawn_server().await;
    let (doc, ann, bob) = shared_file(&server).await;
    let mut b = FileClient::connect_as(&server, doc, &bob).await;
    assert!(b.token().is_some());
    let mut a = FileClient::connect_as(&server, doc, &ann).await;
    assert!(taken_by(a.seat.as_ref(), bob.id));

    share(&server, &ann, doc, &bob, "viewer").await;
    tokio::time::timeout(
        PROMPTLY,
        b.pump_until(|c| !c.can_edit && c.seat == Some(SeatState::Free)),
    )
    .await
    .expect("not told within one second");
    assert!(
        b.received
            .contains(&FileServerMsg::Access { can_edit: false })
    );
    a.pump_until(|c| c.seat == Some(SeatState::Free)).await;
    assert!(a.token().is_none(), "nobody is given the seat");
    assert!(b.removed.is_none() && !b.closed);

    // Made an editor again, Bob does not get the seat back without joining again.
    share(&server, &ann, doc, &bob, "editor").await;
    b.pump_until(|c| c.can_edit).await;
    assert_eq!(b.seat, Some(SeatState::Free));
}

#[tokio::test]
async fn removed_holder_loses_the_seat() {
    let server = spawn_server().await;
    let (doc, ann, bob) = shared_file(&server).await;
    let mut b = FileClient::connect_as(&server, doc, &bob).await;
    assert!(b.token().is_some());
    let mut a = FileClient::connect_as(&server, doc, &ann).await;

    revoke(&server, &ann, doc, bob.id).await;
    tokio::time::timeout(PROMPTLY, b.pump_until(|c| c.removed.is_some()))
        .await
        .expect("not removed within one second");
    b.wait_closed().await;
    assert_eq!(
        b.received.last(),
        Some(&FileServerMsg::Removed {
            reason: RemovedReason::AccessRevoked
        }),
        "Removed is the last frame"
    );
    a.pump_until(|c| c.seat == Some(SeatState::Free)).await;
    a.pump_until(|c| c.participants.iter().all(|p| p.user != bob.id))
        .await;
}

#[tokio::test]
async fn signing_out_closes_the_file_session() {
    let server = spawn_server().await;
    let (doc, ann, bob) = shared_file(&server).await;
    let mut b = FileClient::connect_as(&server, doc, &bob).await;
    let mut a = FileClient::connect_as(&server, doc, &ann).await;
    assert!(b.token().is_some());

    let response = server
        .http(Some(&bob))
        .delete(server.url("/api/session"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    tokio::time::timeout(PROMPTLY, b.pump_until(|c| c.removed.is_some()))
        .await
        .expect("not removed within one second");
    assert_eq!(b.removed, Some(RemovedReason::SignedOut));
    b.wait_closed().await;
    a.pump_until(|c| c.seat == Some(SeatState::Free)).await;
}

/// Reads `socket` until the server closes it, and returns the text frames it sent first.
async fn texts_until_closed(socket: &mut common::Socket) -> Vec<String> {
    let mut texts = Vec::new();
    let closed = tokio::time::timeout(common::TIMEOUT, async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Text(text))) => texts.push(text.to_string()),
                Some(Ok(Message::Close(_)) | Err(_)) | None => return,
                Some(Ok(_)) => {}
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "the server did not close the socket");
    texts
}

#[tokio::test]
async fn text_client_on_a_file_document_is_closed() {
    let server = spawn_server().await;
    let (doc, ann, _) = shared_file(&server).await;
    let mut socket = open_socket(server.addr, doc, &ann).await;
    let join = ClientMsg::<TextOp>::Join {
        client: ClientId(Uuid::new_v4()),
        resume: None,
    };
    socket
        .send(Message::text(serde_json::to_string(&join).unwrap()))
        .await
        .unwrap();
    assert_eq!(texts_until_closed(&mut socket).await, Vec::<String>::new());
}

#[tokio::test]
async fn file_client_on_a_text_document_is_closed() {
    let server = spawn_server().await;
    let doc = new_doc();
    let ann = server.user("Ann").await;
    server.give_access(doc, &ann).await;
    let mut socket = open_socket(server.addr, doc, &ann).await;
    let join = FileClientMsg::Join {
        client: ClientId(Uuid::new_v4()),
    };
    socket
        .send(Message::text(serde_json::to_string(&join).unwrap()))
        .await
        .unwrap();
    assert_eq!(texts_until_closed(&mut socket).await, Vec::<String>::new());
}

/// Holds the next load of a document, once armed, until released, and reports when it starts.
/// A join that opens a room waits in that room while its load is held: after the check before
/// the upgrade, and before the room accepts it.
struct HeldLoad {
    inner: RevisionStore,
    armed: AtomicBool,
    released: Mutex<bool>,
    open: Condvar,
    started: tokio::sync::mpsc::UnboundedSender<()>,
}

impl HeldLoad {
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.open.notify_all();
    }
}

impl Checkpointer for HeldLoad {
    fn write(&self, c: &Checkpoint<'_>) -> Result<Outcome, CheckpointError> {
        self.inner.write(c)
    }

    fn load(&self, doc: DocId) -> Result<Option<Stored>, CheckpointError> {
        if self.armed.swap(false, Ordering::SeqCst) {
            let _ = self.started.send(());
            let mut released = self.released.lock().unwrap();
            while !*released {
                released = self.open.wait(released).unwrap();
            }
        }
        self.inner.load(doc)
    }

    fn load_at(
        &self,
        doc: DocId,
        revision: &RevisionId,
    ) -> Result<Option<Stored>, CheckpointError> {
        self.inner.load_at(doc, revision)
    }

    fn describe(
        &self,
        doc: DocId,
        revision: &RevisionId,
    ) -> Result<Option<RevisionInfo>, CheckpointError> {
        self.inner.describe(doc, revision)
    }

    fn history(&self, doc: DocId) -> Result<Vec<RevisionInfo>, CheckpointError> {
        self.inner.history(doc)
    }

    fn documents(&self) -> Result<Vec<DocId>, GitError> {
        self.inner.documents()
    }
}

/// Opens the gate when dropped, so a failing test does not leave a load blocked.
struct ReleaseOnDrop(Arc<HeldLoad>);

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.release();
    }
}

/// Bob's grant is removed through the API while his join waits in a file session still
/// loading: the API's message reaches the room before he is in it, so only the check made
/// after his join was accepted can catch it. He must get `Removed` and nothing else: no
/// `Joined`, and above all no seat token.
#[tokio::test]
async fn join_racing_a_removal_gets_only_removed() {
    let (started_tx, mut started) = tokio::sync::mpsc::unbounded_channel();
    let slot: Arc<Mutex<Option<Arc<HeldLoad>>>> = Arc::default();
    let keep = Arc::clone(&slot);
    let server = spawn_server_with_grace(
        move |path| {
            let held = Arc::new(HeldLoad {
                inner: RevisionStore::open(path).unwrap(),
                armed: AtomicBool::new(true),
                released: Mutex::new(false),
                open: Condvar::new(),
                started: started_tx,
            });
            *keep.lock().unwrap() = Some(Arc::clone(&held));
            held
        },
        SHORT_GRACE,
    )
    .await;
    let held = ReleaseOnDrop(slot.lock().unwrap().clone().unwrap());
    let (doc, ann, bob) = shared_file(&server).await;
    let vic = server.user("Vic").await;
    share(&server, &ann, doc, &vic, "viewer").await;

    // Vic, a viewer, waits in the loading session too, and watches.
    let mut v = FileClient::open_as(&server, doc, &vic).await;
    let mut b = FileClient::open_as(&server, doc, &bob).await;
    tokio::time::timeout(common::TIMEOUT, started.recv())
        .await
        .expect("no load started")
        .unwrap();
    revoke(&server, &ann, doc, bob.id).await;
    held.0.release();

    b.pump_until(|c| c.removed.is_some()).await;
    b.wait_closed().await;
    assert_eq!(
        b.received,
        vec![FileServerMsg::Removed {
            reason: RemovedReason::AccessRevoked
        }]
    );

    // Nobody saw Bob come and go, nor take the seat. Ann joining marks the end of what Vic
    // was sent.
    v.pump_until(|c| c.joined).await;
    let _a = FileClient::connect_as(&server, doc, &ann).await;
    v.pump_until(|c| taken_by(c.seat.as_ref(), ann.id)).await;
    assert_eq!(v.seat_messages().len(), 1, "{:?}", v.received);
    assert!(
        v.received.iter().all(|m| match m {
            FileServerMsg::Joined { participants, .. } | FileServerMsg::People { participants } =>
                participants.iter().all(|p| p.user != bob.id),
            _ => true,
        }),
        "{:?}",
        v.received
    );
    let events = common::wait_events(&server.db.db, doc, |e| {
        e.iter().any(|e| e.kind == "seat.taken")
    })
    .await;
    let bobs: Vec<&str> = events
        .iter()
        .filter(|e| e.subject == Some(bob.id) && e.kind != "access.granted")
        .map(|e| e.kind.as_str())
        .collect();
    assert_eq!(bobs, vec!["access.revoked", "editing.removed"]);
}
