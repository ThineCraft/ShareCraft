#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use collab_core::ids::{OpId, Seq};
use collab_text::{Component, TextOp};
use common::{TestClient, new_doc, spawn_server};
use protocol::{ClientMsg, Resume, RoomEpoch, ServerMsg};
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

#[tokio::test]
async fn two_clients_see_each_other() {
    let server = spawn_server().await;
    let doc = new_doc();
    let mut a = TestClient::connect(server.addr, doc, "Ann").await;
    let mut b = TestClient::connect(server.addr, doc, "Bob").await;
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
    let mut a = TestClient::connect(server.addr, doc, "A").await;
    let mut b = TestClient::connect(server.addr, doc, "B").await;
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
    let mut a = TestClient::connect(server.addr, doc, "A").await;
    let mut b = TestClient::connect(server.addr, doc, "B").await;
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
    let mut a = TestClient::connect(server.addr, doc, "A").await;
    let mut b = TestClient::connect(server.addr, doc, "B").await;
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
    let mut c = TestClient::connect_with_resume(server.addr, new_doc(), "C", Some(resume)).await;
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
async fn resume_into_a_new_room_gets_reset() {
    let server = spawn_server().await;
    let doc = new_doc();
    let mut a = TestClient::connect(server.addr, doc, "A").await;
    a.type_at(0, "x").await;
    a.pump_until(|c| c.seq() == Seq(1) && c.pending() == 0)
        .await;
    a.type_at(1, "y").await;
    a.pump_until(|c| c.seq() == Seq(2) && c.pending() == 0)
        .await;
    let old_epoch = a.epoch;
    a.disconnect();

    // A was alone, so its room ends once the server sees it leave. Join until a client lands
    // in a new room (a client that lands in the old one leaves again, which also ends it).
    let deadline = tokio::time::Instant::now() + common::TIMEOUT;
    let mut c = loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "old room never ended"
        );
        let mut c = TestClient::connect(server.addr, doc, "C").await;
        if c.epoch != old_epoch {
            break c;
        }
        c.close().await;
    };
    assert_eq!(c.text(), "");
    c.type_at(0, "12").await;
    c.pump_until(|c| c.seq() == Seq(1) && c.pending() == 0)
        .await;
    c.type_at(2, "34").await;
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
    assert_eq!(a.text(), "1234");
    assert_eq!(a.seq(), Seq(2));
}

#[tokio::test]
async fn cross_site_origin_is_refused() {
    let server = spawn_server().await;
    let status = handshake_status(server.addr, &[("Origin", "http://evil.example")]).await;
    assert_eq!(status, 403);
}

#[tokio::test]
async fn same_origin_is_accepted() {
    let server = spawn_server().await;
    let origin = format!("http://{}", server.addr);
    let status = handshake_status(server.addr, &[("Origin", &origin)]).await;
    assert_eq!(status, 101);
}

#[tokio::test]
async fn non_loopback_host_header_is_refused() {
    let server = spawn_server().await;
    let status = handshake_status(server.addr, &[("Host", "evil.example:8787")]).await;
    assert_eq!(status, 403);
}

/// Attempts a WebSocket handshake with extra headers and returns the HTTP status.
async fn handshake_status(addr: std::net::SocketAddr, headers: &[(&str, &str)]) -> u16 {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};
    let url = format!("ws://{addr}/ws/docs/{}?name=X", new_doc().0);
    let mut request = url.into_client_request().unwrap();
    for (name, value) in headers {
        request.headers_mut().insert(
            HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
    }
    let result = tokio::time::timeout(common::TIMEOUT, tokio_tungstenite::connect_async(request))
        .await
        .unwrap();
    match result {
        Ok((_, response)) => response.status().as_u16(),
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => response.status().as_u16(),
        Err(other) => panic!("unexpected error {other:?}"),
    }
}

#[tokio::test]
async fn invalid_op_is_refused_and_room_survives() {
    let server = spawn_server().await;
    let doc = new_doc();
    let mut a = TestClient::connect(server.addr, doc, "A").await;
    let mut b = TestClient::connect(server.addr, doc, "B").await;
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
    let mut a = TestClient::connect(server.addr, doc, "A").await;
    let mut b = TestClient::connect(server.addr, doc, "B").await;
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
    let mut a = TestClient::connect(server.addr, doc, "A").await;
    let mut b = TestClient::connect(server.addr, doc, "B").await;
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
    let url = format!("ws://{}/ws/docs/{}?name=X", server.addr, new_doc().0);
    let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
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
    let mut a = TestClient::connect(server.addr, doc, "A").await;
    let mut b = TestClient::connect(server.addr, doc, "B").await;
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

#[test]
fn non_loopback_address_is_refused() {
    assert!(server::config::parse_addr("0.0.0.0:8787").is_err());
    assert!(server::config::parse_addr("192.168.1.5:8787").is_err());
    assert!(server::config::parse_addr("127.0.0.1:8787").is_ok());
    assert!(server::config::parse_addr("[::1]:8787").is_ok());
    let built = server::Config::from_values(Some("0.0.0.0:8787"), None, None);
    assert!(built.is_err());
}
