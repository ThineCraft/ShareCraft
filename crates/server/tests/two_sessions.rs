#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use collab_core::ids::{DocId, OpId, Seq};
use collab_text::{Component, TextOp};
use common::{
    SHORT_GRACE, TestClient, new_doc, spawn_server, spawn_server_config, spawn_server_with,
    wait_out_grace, wait_stored,
};
use protocol::{ClientMsg, Resume, RoomEpoch, SaveState, ServerMsg};
use revisions::{Checkpoint, CheckpointError, Outcome, RevisionStore, Stored};
use server::Checkpointer;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

#[tokio::test]
async fn two_clients_see_each_other() {
    let server = spawn_server().await;
    let doc = new_doc();
    let mut a = TestClient::connect(&server, doc, "Ann").await;
    let mut b = TestClient::connect(&server, doc, "Bob").await;
    a.pump_until(|c| c.people().len() == 2).await;
    b.pump_until(|c| c.people().len() == 2).await;
    let mut names = a.people();
    names.sort();
    assert_eq!(names, vec!["Ann".to_owned(), "Bob".to_owned()]);
}

#[tokio::test]
async fn concurrent_typing_converges() {
    let server = spawn_server().await;
    let doc = new_doc();
    let mut a = TestClient::connect(&server, doc, "A").await;
    let mut b = TestClient::connect(&server, doc, "B").await;
    a.type_at(0, "hello ").await;
    b.type_at(0, "world").await;
    a.pump_until(|c| c.pending() == 0 && c.seq() == Seq(2))
        .await;
    b.pump_until(|c| c.pending() == 0 && c.seq() == Seq(2))
        .await;
    assert_eq!(a.text(), b.text());
    assert!(a.text().contains("hello "), "{}", a.text());
    assert!(a.text().contains("world"), "{}", a.text());
}

#[tokio::test]
async fn astral_text_round_trips() {
    let server = spawn_server().await;
    let doc = new_doc();
    let mut a = TestClient::connect(&server, doc, "A").await;
    let mut b = TestClient::connect(&server, doc, "B").await;
    a.type_at(0, "😀").await;
    b.pump_until(|c| c.seq() == Seq(1)).await;
    assert_eq!(b.text(), "😀");
    // A following edit after the emoji proves positions are counted in chars.
    b.type_at(1, "!").await;
    a.pump_until(|c| c.seq() == Seq(2)).await;
    assert_eq!(a.text(), "😀!");
}

#[tokio::test]
async fn reconnect_resends_without_duplicating() {
    let server = spawn_server().await;
    let doc = new_doc();
    let mut a = TestClient::connect(&server, doc, "A").await;
    let mut b = TestClient::connect(&server, doc, "B").await;
    a.type_at(0, "abc").await;
    // Once B sees the change the server has committed it; A has not read its confirmation.
    b.pump_until(|c| c.text() == "abc").await;
    assert_eq!(a.pending(), 1);
    a.disconnect();

    // The catch-up confirms the change; the resend that follows is dropped by the server.
    // `TestClient` fails on any entry that is not the next one, so a duplicate would panic.
    a.reconnect().await;
    a.pump_until(|c| c.pending() == 0).await;
    assert_eq!(a.text(), "abc");
    assert_eq!(b.text(), "abc");
    assert_eq!(a.seq(), Seq(1));

    // A further change lands at seq 2, so the resend was not committed a second time, and it
    // is the next thing A receives, so no duplicate entry was delivered in between.
    b.type_at(3, "!").await;
    a.pump_until(|c| c.seq() == Seq(2)).await;
    b.pump_until(|c| c.seq() == Seq(2) && c.pending() == 0)
        .await;
    assert_eq!(a.text(), "abc!");
    assert_eq!(b.text(), "abc!");
    assert_eq!(committed_seqs(&a), vec![1, 2]);
    assert_eq!(committed_seqs(&b), vec![1, 2]);
}

fn committed_seqs(c: &TestClient) -> Vec<u64> {
    c.received
        .iter()
        .filter_map(|m| match m {
            ServerMsg::Committed { entry } => Some(entry.seq.0),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn unknown_since_gets_reset() {
    let server = spawn_server().await;
    let resume = Resume {
        epoch: RoomEpoch(Uuid::new_v4()),
        seq: Seq(40),
    };
    let mut c = TestClient::connect_with_resume(&server, new_doc(), "C", Some(resume)).await;
    c.pump_until(|c| c.joined).await;
    assert!(
        matches!(
            c.received.first(),
            Some(ServerMsg::Reset { seq: Seq(0), snapshot, revision: None, .. }) if snapshot.is_empty()
        ),
        "{:?}",
        c.received
    );
}

#[tokio::test]
async fn solo_disconnect_resumes_without_losing_typing() {
    let server = spawn_server().await;
    let doc = new_doc();
    let mut a = TestClient::connect(&server, doc, "A").await;
    a.type_at(0, "abc").await;
    a.pump_until(|c| c.seq() == Seq(1) && c.pending() == 0)
        .await;
    a.disconnect();
    // The room has seen A leave once it saves, which an empty room does at once.
    wait_stored(&server, doc, "abc").await;

    // Typed while disconnected.
    a.type_at(3, "def").await;
    a.type_at(6, "!").await;
    assert_eq!(a.pending(), 2);
    let before = a.received.len();
    a.reconnect().await;
    assert!(
        matches!(a.received.get(before), Some(ServerMsg::Joined { .. })),
        "{:?}",
        &a.received[before..]
    );
    a.pump_until(|c| c.pending() == 0 && c.seq() == Seq(3))
        .await;
    assert_eq!(a.text(), "abcdef!");

    let c = TestClient::connect(&server, doc, "C").await;
    assert_eq!(c.text(), "abcdef!");
}

#[tokio::test]
async fn room_ends_after_grace_period() {
    let server = spawn_server_config(|c| c.room_grace = SHORT_GRACE).await;
    let doc = new_doc();
    let mut a = TestClient::connect(&server, doc, "A").await;
    a.type_at(0, "abc").await;
    a.pump_until(|c| c.seq() == Seq(1) && c.pending() == 0)
        .await;
    a.disconnect();
    wait_stored(&server, doc, "abc").await;
    wait_out_grace().await;

    // The room that knew A's epoch has ended; the new one loaded the saved text.
    let before = a.received.len();
    a.reconnect().await;
    assert!(
        matches!(a.received.get(before), Some(ServerMsg::Reset { .. })),
        "{:?}",
        &a.received[before..]
    );
    assert_eq!(a.text(), "abc");
}

#[tokio::test]
async fn resume_into_a_new_room_gets_reset() {
    let server = spawn_server_config(|c| c.room_grace = SHORT_GRACE).await;
    let doc = new_doc();
    let mut a = TestClient::connect(&server, doc, "A").await;
    a.type_at(0, "x").await;
    a.pump_until(|c| c.seq() == Seq(1) && c.pending() == 0)
        .await;
    a.type_at(1, "y").await;
    a.pump_until(|c| c.seq() == Seq(2) && c.pending() == 0)
        .await;
    let old_epoch = a.epoch;
    a.disconnect();

    // A was alone, so its room saves once the server sees it leave and ends after the grace
    // period.
    wait_stored(&server, doc, "xy").await;
    wait_out_grace().await;
    let mut c = TestClient::connect(&server, doc, "C").await;
    assert_ne!(c.epoch, old_epoch, "old room never ended");
    // The old room saved A's text before ending; the new one loads it.
    assert_eq!(c.text(), "xy");
    c.type_at(2, "12").await;
    c.pump_until(|c| c.seq() == Seq(1) && c.pending() == 0)
        .await;
    c.type_at(4, "34").await;
    c.pump_until(|c| c.seq() == Seq(2) && c.pending() == 0)
        .await;

    // A's position (seq 2) exists in the new room too, but belongs to the old one.
    let before = a.received.len();
    a.reconnect().await;
    assert!(
        matches!(a.received.get(before), Some(ServerMsg::Reset { .. })),
        "{:?}",
        &a.received[before..]
    );
    assert_eq!(a.text(), c.text());
    assert_eq!(a.text(), "xy1234");
    assert_eq!(a.seq(), Seq(2));
}

#[tokio::test]
async fn cross_site_origin_is_refused() {
    let server = spawn_server().await;
    let status = handshake_status(&server, &[("Origin", "http://evil.example")]).await;
    assert_eq!(status, 403);
}

#[tokio::test]
async fn same_origin_is_accepted() {
    let server = spawn_server().await;
    let origin = format!("http://{}", server.addr);
    let status = handshake_status(&server, &[("Origin", &origin)]).await;
    assert_eq!(status, 101);
}

#[tokio::test]
async fn non_loopback_host_header_is_refused() {
    let server = spawn_server().await;
    let status = handshake_status(&server, &[("Host", "evil.example:8787")]).await;
    assert_eq!(status, 403);
}

/// Attempts a WebSocket handshake to a document a signed-in person may open, with their cookie
/// and the extra headers, and returns the HTTP status.
async fn handshake_status(server: &common::TestServer, headers: &[(&str, &str)]) -> u16 {
    use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};
    let ann = server.user("Ann").await;
    let doc = new_doc();
    server.give_access(doc, &ann).await;
    let path = format!("/ws/docs/{}", doc.0);
    let mut request = common::ws_request(server.addr, &path, Some(&ann.cookie));
    for (name, value) in headers {
        request.headers_mut().insert(
            HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
    }
    common::handshake_status(request).await
}

#[tokio::test]
async fn invalid_op_is_refused_and_room_survives() {
    let server = spawn_server().await;
    let doc = new_doc();
    let mut a = TestClient::connect(&server, doc, "A").await;
    let mut b = TestClient::connect(&server, doc, "B").await;
    let bad = OpId(Uuid::new_v4());
    // Expects a five-character document; the document is empty.
    a.send(&ClientMsg::Submit {
        op_id: bad,
        base: Seq(0),
        op: TextOp::from(vec![Component::Retain(5), Component::Insert("x".into())]),
    })
    .await;
    a.pump_until(|c| {
        c.received
            .iter()
            .any(|m| matches!(m, ServerMsg::Refused { op_id, .. } if *op_id == bad))
    })
    .await;

    b.type_at(0, "fine").await;
    b.pump_until(|c| c.pending() == 0 && c.seq() == Seq(1))
        .await;
    a.pump_until(|c| c.seq() == Seq(1)).await;
    assert_eq!(a.text(), "fine");
    assert_eq!(b.text(), "fine");
}

#[tokio::test]
async fn garbage_frame_closes_only_that_connection() {
    let server = spawn_server().await;
    let doc = new_doc();
    let mut a = TestClient::connect(&server, doc, "A").await;
    let mut b = TestClient::connect(&server, doc, "B").await;
    a.send_raw(Message::text("not json")).await;
    a.wait_closed().await;

    b.type_at(0, "still here").await;
    b.pump_until(|c| c.pending() == 0 && c.seq() == Seq(1))
        .await;
    assert_eq!(b.text(), "still here");
    b.pump_until(|c| c.people() == vec!["B".to_owned()]).await;
}

#[tokio::test]
async fn oversized_frame_closes_only_that_connection() {
    let server = spawn_server().await;
    let doc = new_doc();
    let mut a = TestClient::connect(&server, doc, "A").await;
    let mut b = TestClient::connect(&server, doc, "B").await;
    let big = "x".repeat(1024 * 1024 + 1);
    // The server may reset the connection while the frame is still being written.
    let socket_send = tokio::time::timeout(common::TIMEOUT, a.send_raw_lossy(Message::text(big)));
    assert!(socket_send.await.is_ok(), "send hung");
    a.wait_closed().await;

    b.type_at(0, "ok").await;
    b.pump_until(|c| c.pending() == 0 && c.seq() == Seq(1))
        .await;
}

#[tokio::test]
async fn message_before_join_closes_connection() {
    let server = spawn_server().await;
    let ann = server.user("Ann").await;
    let doc = new_doc();
    server.give_access(doc, &ann).await;
    let mut socket = common::open_socket(server.addr, doc, &ann).await;
    let cursor = serde_json::to_string(&ClientMsg::<TextOp>::Cursor { at: Some(1) }).unwrap();
    futures_util::SinkExt::send(&mut socket, Message::text(cursor))
        .await
        .unwrap();
    let closed = tokio::time::timeout(common::TIMEOUT, async {
        loop {
            match futures_util::StreamExt::next(&mut socket).await {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                Some(Ok(_)) => {}
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "connection stayed open");
}

#[tokio::test]
async fn cursor_is_relayed_and_oversized_cursor_dropped() {
    let server = spawn_server().await;
    let doc = new_doc();
    let mut a = TestClient::connect(&server, doc, "A").await;
    let mut b = TestClient::connect(&server, doc, "B").await;
    let a_id = a.client();
    a.send(&ClientMsg::Cursor {
        at: Some(10_000_001),
    })
    .await;
    a.send(&ClientMsg::Cursor { at: Some(7) }).await;
    b.pump_until(|c| {
        c.participants()
            .iter()
            .any(|p| p.client == a_id && p.cursor.is_some())
    })
    .await;
    let seen = b
        .participants()
        .iter()
        .find(|p| p.client == a_id)
        .and_then(|p| p.cursor);
    assert_eq!(seen, Some(7));
}

#[tokio::test]
async fn bad_doc_id_is_rejected() {
    let server = spawn_server().await;
    let url = format!("ws://{}/ws/docs/not-a-uuid", server.addr);
    let err = tokio_tungstenite::connect_async(url).await.unwrap_err();
    match err {
        tokio_tungstenite::tungstenite::Error::Http(resp) => {
            assert_eq!(resp.status().as_u16(), 400);
        }
        other => panic!("unexpected error {other:?}"),
    }
}

#[tokio::test]
async fn healthz_answers_ok() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let server = spawn_server().await;
    let mut stream = tokio::net::TcpStream::connect(server.addr).await.unwrap();
    stream
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut body = String::new();
    tokio::time::timeout(common::TIMEOUT, stream.read_to_string(&mut body))
        .await
        .unwrap()
        .unwrap();
    assert!(body.starts_with("HTTP/1.1 200"), "{body}");
    assert!(body.ends_with("ok"), "{body}");
}

// ---- Checkpoints ----

fn stored_text(stored: &Stored) -> String {
    String::from_utf8(stored.bytes.clone()).unwrap()
}

fn open_store(path: &std::path::Path) -> RevisionStore {
    RevisionStore::open(path).unwrap()
}

#[tokio::test]
async fn idle_session_is_checkpointed() {
    let server = spawn_server().await;
    let doc = new_doc();
    let mut a = TestClient::connect(&server, doc, "Ann").await;
    let mut b = TestClient::connect(&server, doc, "Bob").await;
    a.type_at(0, "hello ").await;
    b.type_at(0, "world").await;
    a.pump_until(|c| c.pending() == 0 && c.seq() == Seq(2))
        .await;
    b.pump_until(|c| c.pending() == 0 && c.seq() == Seq(2))
        .await;

    let rev_a = a.wait_saved(Seq(2)).await;
    let rev_b = b.wait_saved(Seq(2)).await;
    assert_eq!(rev_a, rev_b);

    let stored = server.store().read(doc, None).unwrap().unwrap();
    assert_eq!(stored.revision.to_string(), rev_a);
    assert_eq!(stored_text(&stored), a.text());
    assert_eq!(stored.meta.file_name, format!("{}.txt", doc.0));
    let mut names: Vec<_> = stored
        .meta
        .contributors
        .iter()
        .map(|p| p.name.clone())
        .collect();
    names.sort();
    assert_eq!(names, vec!["Ann".to_owned(), "Bob".to_owned()]);
    let mut ids: Vec<_> = stored
        .meta
        .contributors
        .iter()
        .map(|p| p.id.clone())
        .collect();
    ids.sort();
    let mut expected = vec![a.user.id.0.to_string(), b.user.id.0.to_string()];
    expected.sort();
    assert_eq!(ids, expected);
}

/// Two clients type, the room saves, both leave; returns the saved text and revision.
async fn type_save_and_leave(server: &common::TestServer, doc: DocId) -> (String, String) {
    let mut a = TestClient::connect(server, doc, "Ann").await;
    let mut b = TestClient::connect(server, doc, "Bob").await;
    a.type_at(0, "abc").await;
    a.pump_until(|c| c.pending() == 0 && c.seq() == Seq(1))
        .await;
    b.pump_until(|c| c.seq() == Seq(1)).await;
    b.type_at(3, "def").await;
    b.pump_until(|c| c.pending() == 0 && c.seq() == Seq(2))
        .await;
    let rev = b.wait_saved(Seq(2)).await;
    let text = b.text();
    a.close().await;
    b.close().await;
    (text, rev)
}

#[tokio::test]
async fn reopening_loads_the_saved_text() {
    let server = spawn_server_config(|c| c.room_grace = SHORT_GRACE).await;
    let doc = new_doc();
    let (text, rev) = type_save_and_leave(&server, doc).await;
    assert_eq!(text, "abcdef");
    // The room ends, so the next join opens a new one that loads the saved revision.
    wait_out_grace().await;

    let c = TestClient::connect(&server, doc, "Cy").await;
    assert_eq!(c.text(), "abcdef");
    assert!(
        matches!(
            c.received.first(),
            Some(ServerMsg::Joined { snapshot: Some(s), revision: Some(r), .. })
                if s == "abcdef" && *r == rev
        ),
        "{:?}",
        c.received
    );
}

#[tokio::test]
async fn typing_resumes_from_saved_revision() {
    let server = spawn_server_config(|c| c.room_grace = SHORT_GRACE).await;
    let doc = new_doc();
    let (_, first) = type_save_and_leave(&server, doc).await;
    wait_out_grace().await;

    let mut c = TestClient::connect(&server, doc, "Cy").await;
    assert_eq!(c.joined_revision(), Some(first.clone()));
    let end = c.text().chars().count();
    c.type_at(end, "!").await;
    c.pump_until(|c| c.pending() == 0 && c.seq() == Seq(1))
        .await;
    let second = c.wait_saved(Seq(1)).await;
    assert_ne!(second, first);

    let commits = server.doc_commits(doc);
    assert_eq!(commits.first(), Some(&second));
    assert!(commits.contains(&first), "{commits:?}");
    let stored = server.store().read(doc, None).unwrap().unwrap();
    assert_eq!(stored_text(&stored), "abcdef!");
}

/// Fails the first `failures` writes, then stores normally.
struct Flaky {
    inner: RevisionStore,
    failures: AtomicUsize,
}

impl Checkpointer for Flaky {
    fn write(&self, c: &Checkpoint<'_>) -> Result<Outcome, CheckpointError> {
        let left = self.failures.load(Ordering::SeqCst);
        if left > 0 {
            self.failures.store(left - 1, Ordering::SeqCst);
            return Err(CheckpointError::Git(revisions::GitError::new(
                "disk on fire",
            )));
        }
        self.inner.write(c)
    }

    fn load(&self, doc: DocId) -> Result<Option<Stored>, CheckpointError> {
        self.inner.load(doc)
    }
}

#[tokio::test]
async fn failed_checkpoint_is_retried_and_reported() {
    let server = spawn_server_with(|path| {
        Arc::new(Flaky {
            inner: open_store(path),
            failures: AtomicUsize::new(1),
        })
    })
    .await;
    let doc = new_doc();
    let mut a = TestClient::connect(&server, doc, "Ann").await;
    let mut b = TestClient::connect(&server, doc, "Bob").await;
    a.type_at(0, "one").await;
    a.pump_until(|c| c.pending() == 0 && c.seq() == Seq(1))
        .await;
    let failed = |c: &TestClient| c.saves().iter().any(|s| s.0 == SaveState::Failed);
    a.pump_until(failed).await;

    // Typed between the failure and the retry.
    a.type_at(3, " two").await;
    a.pump_until(|c| c.pending() == 0 && c.seq() == Seq(2))
        .await;
    let rev = a.wait_saved(Seq(2)).await;
    b.wait_saved(Seq(2)).await;

    for c in [&a, &b] {
        let states: Vec<_> = c.saves().iter().map(|s| s.0).collect();
        let failed_at = states.iter().position(|s| *s == SaveState::Failed);
        let saved_at = states.iter().position(|s| *s == SaveState::Saved);
        assert!(failed_at.is_some() && failed_at < saved_at, "{states:?}");
    }
    let first_saved = a
        .saves()
        .into_iter()
        .find(|s| s.0 == SaveState::Saved)
        .unwrap();
    assert_eq!(
        first_saved.2,
        Seq(2),
        "first save after the failure covers the retry"
    );
    let stored = server.store().read(doc, None).unwrap().unwrap();
    assert_eq!(stored.revision.to_string(), rev);
    assert_eq!(stored_text(&stored), "one two");
}

#[tokio::test]
async fn final_checkpoint_outlasts_grace_period() {
    // Three failures with a 100 ms idle time: attempts at about 0, 100, 300 and 700 ms, so the
    // fourth, which succeeds, comes after the 300 ms grace period.
    let server = common::spawn_server_with_grace(
        |path| {
            Arc::new(Flaky {
                inner: open_store(path),
                failures: AtomicUsize::new(3),
            })
        },
        SHORT_GRACE,
    )
    .await;
    let doc = new_doc();
    let mut a = TestClient::connect(&server, doc, "Ann").await;
    a.type_at(0, "keep").await;
    a.pump_until(|c| c.pending() == 0 && c.seq() == Seq(1))
        .await;
    a.close().await;
    wait_stored(&server, doc, "keep").await;
    wait_out_grace().await;
    let b = TestClient::connect(&server, doc, "Bob").await;
    assert_eq!(b.text(), "keep");
}

#[tokio::test]
async fn no_commit_per_change() {
    let server = spawn_server().await;
    let doc = new_doc();
    let mut a = TestClient::connect(&server, doc, "Ann").await;
    for n in 0..50u64 {
        a.type_at(usize::try_from(n).unwrap(), "x").await;
        a.pump_until(|c| c.pending() == 0 && c.seq() == Seq(n + 1))
            .await;
    }
    a.wait_saved(Seq(50)).await;
    assert_eq!(server.doc_commits(doc).len(), 1);
    let stored = server.store().read(doc, None).unwrap().unwrap();
    assert_eq!(stored_text(&stored), "x".repeat(50));
}

#[tokio::test]
async fn brief_disconnect_after_checkpoint_resumes_without_reset() {
    let server = spawn_server().await;
    let doc = new_doc();
    let mut a = TestClient::connect(&server, doc, "A").await;
    let mut b = TestClient::connect(&server, doc, "B").await;
    a.type_at(0, "abc").await;
    // A has not read the confirmation, so it still resumes from seq 0.
    b.pump_until(|c| c.text() == "abc").await;
    b.wait_saved(Seq(1)).await;
    assert_eq!(a.pending(), 1);
    a.disconnect();

    let before = a.received.len();
    a.reconnect().await;
    assert!(
        matches!(a.received.get(before), Some(ServerMsg::Joined { .. })),
        "{:?}",
        &a.received[before..]
    );
    a.pump_until(|c| c.pending() == 0).await;
    assert_eq!(a.text(), "abc");
}

/// Every load fails; counts loads and writes.
#[derive(Default)]
struct BrokenLoad {
    loads: AtomicUsize,
    writes: AtomicUsize,
}

impl Checkpointer for BrokenLoad {
    fn write(&self, _c: &Checkpoint<'_>) -> Result<Outcome, CheckpointError> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        Err(CheckpointError::Git(revisions::GitError::new(
            "not expected",
        )))
    }

    fn load(&self, _doc: DocId) -> Result<Option<Stored>, CheckpointError> {
        self.loads.fetch_add(1, Ordering::SeqCst);
        Err(CheckpointError::Git(revisions::GitError::new("unreadable")))
    }
}

#[tokio::test]
async fn load_failure_refuses_join_and_does_not_overwrite() {
    let broken = Arc::new(BrokenLoad::default());
    let shared = Arc::clone(&broken);
    let server = spawn_server_with(move |_| shared).await;
    let doc = new_doc();
    for attempt in 1..=2 {
        let mut c = TestClient::connect_with_resume(&server, doc, "C", None).await;
        c.wait_closed().await;
        assert!(!c.joined, "{:?}", c.received);
        assert!(
            !c.received
                .iter()
                .any(|m| matches!(m, ServerMsg::Joined { .. } | ServerMsg::Reset { .. })),
            "{:?}",
            c.received
        );
        // The failed room is gone, so the next join tries the load again.
        assert_eq!(broken.loads.load(Ordering::SeqCst), attempt);
    }
    assert_eq!(broken.writes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn awkward_display_name_does_not_block_saving() {
    let server = spawn_server().await;
    let doc = new_doc();
    // The least name the account rules allow: one letter, wrapped in everything Git trims
    // from the ends of a signature name.
    let awkward = r#""';:\, x ,\:;'""#;
    let mut a = TestClient::connect(&server, doc, awkward).await;
    assert_eq!(a.people(), vec![awkward.to_owned()]);
    a.type_at(0, "hi").await;
    a.pump_until(|c| c.pending() == 0 && c.seq() == Seq(1))
        .await;
    a.wait_saved(Seq(1)).await;
    assert!(
        !a.saves().iter().any(|s| s.0 == SaveState::Failed),
        "{:?}",
        a.saves()
    );
    let stored = server.store().read(doc, None).unwrap().unwrap();
    let names: Vec<_> = stored
        .meta
        .contributors
        .iter()
        .map(|p| p.name.as_str())
        .collect();
    assert_eq!(names, vec![awkward]);
}

/// Holds every write until released, and reports when one starts.
struct Gated {
    inner: RevisionStore,
    released: Mutex<bool>,
    open: Condvar,
    started: tokio::sync::mpsc::UnboundedSender<()>,
}

impl Gated {
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.open.notify_all();
    }
}

impl Checkpointer for Gated {
    fn write(&self, c: &Checkpoint<'_>) -> Result<Outcome, CheckpointError> {
        let _ = self.started.send(());
        let mut released = self.released.lock().unwrap();
        while !*released {
            released = self.open.wait(released).unwrap();
        }
        drop(released);
        self.inner.write(c)
    }

    fn load(&self, doc: DocId) -> Result<Option<Stored>, CheckpointError> {
        self.inner.load(doc)
    }
}

/// Opens the gate when dropped, so a failing test does not leave a write blocked.
struct ReleaseOnDrop(Arc<Gated>);

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.release();
    }
}

#[tokio::test]
async fn rejoin_during_final_checkpoint_sees_latest_text() {
    let (started_tx, mut started) = tokio::sync::mpsc::unbounded_channel();
    let slot: Arc<Mutex<Option<Arc<Gated>>>> = Arc::default();
    let keep = Arc::clone(&slot);
    let server = spawn_server_with(move |path| {
        let gated = Arc::new(Gated {
            inner: open_store(path),
            released: Mutex::new(false),
            open: Condvar::new(),
            started: started_tx,
        });
        *keep.lock().unwrap() = Some(Arc::clone(&gated));
        gated
    })
    .await;
    let gate = ReleaseOnDrop(slot.lock().unwrap().clone().unwrap());
    let doc = new_doc();

    let mut a = TestClient::connect(&server, doc, "A").await;
    a.type_at(0, "hello").await;
    a.pump_until(|c| c.pending() == 0 && c.seq() == Seq(1))
        .await;
    a.close().await;
    // The last participant has left and the room's checkpoint is held mid-write.
    tokio::time::timeout(common::TIMEOUT, started.recv())
        .await
        .expect("no checkpoint started")
        .unwrap();

    let mut b = TestClient::connect(&server, doc, "B").await;
    assert_eq!(b.text(), "hello");
    gate.0.release();
    b.wait_saved(Seq(1)).await;
    b.close().await;

    let c = TestClient::connect(&server, doc, "C").await;
    assert_eq!(c.text(), "hello");
    let stored = server.store().read(doc, None).unwrap().unwrap();
    assert_eq!(stored_text(&stored), "hello");
}

#[test]
fn non_loopback_address_is_refused() {
    assert!(server::config::parse_addr("0.0.0.0:8787").is_err());
    assert!(server::config::parse_addr("192.168.1.5:8787").is_err());
    assert!(server::config::parse_addr("127.0.0.1:8787").is_ok());
    assert!(server::config::parse_addr("[::1]:8787").is_ok());
    let built =
        server::Config::from_values(Some("0.0.0.0:8787"), None, None, Some("postgres://unused"));
    assert!(built.is_err());
}
