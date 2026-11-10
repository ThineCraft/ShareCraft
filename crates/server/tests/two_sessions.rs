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
use protocol::{ClientMsg, ServerMsg};
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

    a.reconnect().await;
    a.pump_until(|c| c.pending() == 0).await;
    assert_eq!(a.text(), "abc");
    assert_eq!(a.seq(), Seq(1));
    // The resend is answered with the existing entry again, not a new one.
    a.pump_until(|c| {
        c.received
            .iter()
            .filter(|m| matches!(m, ServerMsg::Committed { entry } if entry.seq == Seq(1)))
            .count()
            == 2
    })
    .await;

    // A further change lands at seq 2, so the resend was not committed a second time.
    b.type_at(3, "!").await;
    a.pump_until(|c| c.seq() == Seq(2)).await;
    b.pump_until(|c| c.seq() == Seq(2) && c.pending() == 0)
        .await;
    assert_eq!(a.text(), "abc!");
    assert_eq!(b.text(), "abc!");
}

#[tokio::test]
async fn unknown_since_gets_reset() {
    let server = spawn_server().await;
    let mut c = TestClient::connect_with_since(server.addr, new_doc(), "C", Some(Seq(40))).await;
    c.pump_until(|c| c.joined).await;
    assert!(
        matches!(
            c.received.first(),
            Some(ServerMsg::Reset { seq: Seq(0), snapshot, revision: None }) if snapshot.is_empty()
        ),
        "{:?}",
        c.received
    );
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
