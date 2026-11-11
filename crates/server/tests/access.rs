//! Who may open a document's session, under which name, and with which rights.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use collab_core::ids::{ClientId, OpId, Seq};
use collab_text::TextOp;
use common::{
    TestClient, handshake_status, new_doc, open_socket_at, spawn_server, spawn_server_config,
    ws_request,
};
use futures_util::{SinkExt, StreamExt};
use protocol::{ClientMsg, ServerMsg};
use records::{Editor, Role};
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

/// `/ws/docs/<doc>`.
fn ws_path(doc: collab_core::ids::DocId) -> String {
    format!("/ws/docs/{}", doc.0)
}

/// The `can_edit` of the last `Joined` or `Reset` a client received.
fn joined_can_edit(c: &TestClient) -> Option<bool> {
    c.received.iter().rev().find_map(|m| match m {
        ServerMsg::Joined { can_edit, .. } | ServerMsg::Reset { can_edit, .. } => Some(*can_edit),
        _ => None,
    })
}

#[tokio::test]
async fn socket_without_session_is_401() {
    let server = spawn_server().await;
    let ann = server.user("Ann").await;
    let doc = new_doc();
    server.give_access(doc, &ann).await;
    let status = handshake_status(ws_request(server.addr, &ws_path(doc), None)).await;
    assert_eq!(status, 401);
    // A cookie that is not a live session is no better.
    let forged = format!("sharecraft_session={}", "ab".repeat(32));
    let status = handshake_status(ws_request(server.addr, &ws_path(doc), Some(&forged))).await;
    assert_eq!(status, 401);
}

#[tokio::test]
async fn socket_for_unknown_document_is_404() {
    let server = spawn_server().await;
    let ann = server.user("Ann").await;
    let request = ws_request(server.addr, &ws_path(new_doc()), Some(&ann.cookie));
    assert_eq!(handshake_status(request).await, 404);
}

#[tokio::test]
async fn socket_without_a_role_is_404() {
    let server = spawn_server().await;
    let ann = server.user("Ann").await;
    let bob = server.user("Bob").await;
    let doc = new_doc();
    server.give_access(doc, &ann).await;
    let request = ws_request(server.addr, &ws_path(doc), Some(&bob.cookie));
    assert_eq!(handshake_status(request).await, 404);
    // The owner gets in, so the document is there.
    let request = ws_request(server.addr, &ws_path(doc), Some(&ann.cookie));
    assert_eq!(handshake_status(request).await, 101);
}

#[tokio::test]
async fn participants_carry_account_names_and_ids() {
    let server = spawn_server().await;
    let doc = new_doc();
    let mut a = TestClient::connect(&server, doc, "Ann").await;
    let b = TestClient::connect(&server, doc, "Bob").await;
    a.pump_until(|c| c.people().len() == 2).await;
    let mut seen: Vec<_> = a
        .participants()
        .iter()
        .map(|p| (p.client, p.user, p.name.clone(), p.can_edit))
        .collect();
    seen.sort_by(|x, y| x.2.cmp(&y.2));
    assert_eq!(
        seen,
        vec![
            (a.client(), a.user.id, "Ann".to_owned(), true),
            (b.client(), b.user.id, "Bob".to_owned(), true),
        ]
    );
    assert_eq!(joined_can_edit(&a), Some(true));
}

#[tokio::test]
async fn name_query_parameter_is_ignored() {
    let server = spawn_server().await;
    let ann = server.user("Ann").await;
    let doc = new_doc();
    server.give_access(doc, &ann).await;
    let path = format!("{}?name=Mallory", ws_path(doc));
    let mut socket = open_socket_at(server.addr, &path, &ann.cookie).await;
    let join = ClientMsg::<TextOp>::Join {
        client: ClientId(Uuid::new_v4()),
        resume: None,
    };
    socket
        .send(Message::text(serde_json::to_string(&join).unwrap()))
        .await
        .unwrap();
    let frame = tokio::time::timeout(common::TIMEOUT, socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let msg: ServerMsg<TextOp> = serde_json::from_str(frame.to_text().unwrap()).unwrap();
    let ServerMsg::Joined { participants, .. } = msg else {
        panic!("expected joined, got {msg:?}");
    };
    let names: Vec<_> = participants.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, vec!["Ann"]);
    assert_eq!(participants[0].user, ann.id);
}

#[tokio::test]
async fn viewer_receives_changes_but_cannot_edit() {
    let server = spawn_server().await;
    let doc = new_doc();
    let mut ann = TestClient::connect(&server, doc, "Ann").await;
    let vic = server.user("Vic").await;
    server
        .db
        .db
        .set_access(ann.user.id, doc, vic.id, Some(Role::Viewer))
        .await
        .unwrap();
    let mut v = TestClient::connect_as(&server, doc, &vic).await;
    assert_eq!(joined_can_edit(&v), Some(false));
    let vic_client = v.client();
    ann.pump_until(|c| {
        c.participants()
            .iter()
            .any(|p| p.client == vic_client && p.user == vic.id && !p.can_edit)
    })
    .await;

    // A raw `Submit` from the viewer is refused and changes nothing.
    let refused = OpId(Uuid::new_v4());
    v.send(&ClientMsg::Submit {
        op_id: refused,
        base: Seq(0),
        op: TextOp::replace(0, 0, 0, "vandal").unwrap(),
    })
    .await;
    v.pump_until(|c| {
        c.received.iter().any(|m| {
            matches!(m, ServerMsg::Refused { op_id, reason } if *op_id == refused && reason == "read_only")
        })
    })
    .await;

    // The editor's typing reaches the viewer, and the editor's text has nothing of the viewer's.
    ann.type_at(0, "hello").await;
    ann.pump_until(|c| c.pending() == 0 && c.seq() == Seq(1))
        .await;
    v.pump_until(|c| c.seq() == Seq(1)).await;
    assert_eq!(v.text(), "hello");
    assert_eq!(ann.text(), "hello");

    // The viewer may still show a cursor.
    v.send(&ClientMsg::Cursor { at: Some(3) }).await;
    ann.pump_until(|c| {
        c.participants()
            .iter()
            .any(|p| p.client == vic_client && p.cursor == Some(3))
    })
    .await;
    assert_eq!(ann.text(), "hello");
    assert_eq!(ann.seq(), Seq(1));
}

#[tokio::test]
async fn checkpoint_credits_accounts_and_uses_the_document_name() {
    let server = spawn_server().await;
    let ann = server.user("Ann").await;
    let bob = server.user("Bob").await;
    let doc = new_doc();
    let db = &server.db.db;
    server
        .create_document(doc, "Plan.txt", Editor::Text, &ann)
        .await;
    db.set_access(ann.id, doc, bob.id, Some(Role::Editor))
        .await
        .unwrap();
    let mut a = TestClient::connect_as(&server, doc, &ann).await;
    let mut b = TestClient::connect_as(&server, doc, &bob).await;
    a.type_at(0, "abc").await;
    a.pump_until(|c| c.pending() == 0 && c.seq() == Seq(1))
        .await;
    b.pump_until(|c| c.seq() == Seq(1)).await;
    b.type_at(3, "def").await;
    b.pump_until(|c| c.pending() == 0 && c.seq() == Seq(2))
        .await;
    let rev = b.wait_saved(Seq(2)).await;

    let stored = server.store().read(doc, None).unwrap().unwrap();
    assert_eq!(stored.meta.file_name, "Plan.txt");
    let contributors: Vec<_> = stored
        .meta
        .contributors
        .iter()
        .map(|p| (p.id.clone(), p.name.clone()))
        .collect();
    assert_eq!(
        contributors,
        vec![
            (ann.id.0.to_string(), "Ann".to_owned()),
            (bob.id.0.to_string(), "Bob".to_owned()),
        ]
    );

    let repo = gix::open(server.library()).unwrap();
    let commit = repo
        .find_commit(gix::ObjectId::from_hex(rev.as_bytes()).unwrap())
        .unwrap();
    let author = commit.author().unwrap();
    assert_eq!(author.name, "Ann");
    assert_eq!(
        author.email.to_string(),
        format!("{}@sharecraft.invalid", ann.id.0)
    );
    let message = commit.message_raw().unwrap().to_string();
    assert!(message.starts_with("Checkpoint Plan.txt"), "{message}");
    assert!(
        message.contains(&format!(
            "Co-authored-by: Bob <{}@sharecraft.invalid>",
            bob.id.0
        )),
        "{message}"
    );
}

#[tokio::test]
async fn same_account_in_two_tabs_is_two_participants_one_author() {
    let server = spawn_server().await;
    let doc = new_doc();
    let mut one = TestClient::connect(&server, doc, "Ann").await;
    let mut two = TestClient::connect(&server, doc, "Ann").await;
    assert_eq!(one.user.id, two.user.id);
    one.pump_until(|c| c.people().len() == 2).await;
    let clients: Vec<_> = one.participants().iter().map(|p| p.client).collect();
    assert!(clients.contains(&one.client()) && clients.contains(&two.client()));
    assert!(one.participants().iter().all(|p| p.user == one.user.id));

    one.type_at(0, "abc").await;
    one.pump_until(|c| c.pending() == 0 && c.seq() == Seq(1))
        .await;
    two.pump_until(|c| c.seq() == Seq(1)).await;
    two.type_at(3, "def").await;
    two.pump_until(|c| c.pending() == 0 && c.seq() == Seq(2))
        .await;
    two.wait_saved(Seq(2)).await;

    let stored = server.store().read(doc, None).unwrap().unwrap();
    let ids: Vec<_> = stored
        .meta
        .contributors
        .iter()
        .map(|p| p.id.clone())
        .collect();
    assert_eq!(ids, vec![one.user.id.0.to_string()]);
}

#[tokio::test]
async fn joined_reports_saved_through() {
    // Nothing is saved while someone is connected.
    let server =
        spawn_server_config(|c| c.checkpoint_idle = std::time::Duration::from_secs(600)).await;
    let doc = new_doc();
    let mut a = TestClient::connect(&server, doc, "Ann").await;
    a.type_at(0, "abc").await;
    a.pump_until(|c| c.pending() == 0 && c.seq() == Seq(1))
        .await;

    let b = TestClient::connect(&server, doc, "Bob").await;
    assert!(
        matches!(
            b.received.first(),
            Some(ServerMsg::Joined {
                seq: Seq(1),
                saved_through: Seq(0),
                ..
            })
        ),
        "{:?}",
        b.received
    );

    // Someone joining after a checkpoint finds everything saved.
    let server = spawn_server().await;
    let doc = new_doc();
    let mut a = TestClient::connect(&server, doc, "Ann").await;
    a.type_at(0, "abc").await;
    a.pump_until(|c| c.pending() == 0 && c.seq() == Seq(1))
        .await;
    a.wait_saved(Seq(1)).await;
    let b = TestClient::connect(&server, doc, "Bob").await;
    assert!(
        matches!(
            b.received.first(),
            Some(ServerMsg::Joined {
                seq: Seq(1),
                saved_through: Seq(1),
                ..
            })
        ),
        "{:?}",
        b.received
    );
}

#[tokio::test]
async fn join_with_another_persons_client_id_is_refused() {
    let server = spawn_server().await;
    let doc = new_doc();
    let mut ann = TestClient::connect(&server, doc, "Ann").await;
    ann.type_at(0, "abc").await;
    ann.pump_until(|c| c.pending() == 0 && c.seq() == Seq(1))
        .await;
    let bob = server.user("Bob").await;
    server
        .db
        .db
        .set_access(ann.user.id, doc, bob.id, Some(Role::Viewer))
        .await
        .unwrap();

    // Bob's page claims Ann's client id, which everyone sees in the people list.
    let mut socket = common::open_socket(server.addr, doc, &bob).await;
    let join = ClientMsg::<TextOp>::Join {
        client: ann.client(),
        resume: None,
    };
    socket
        .send(Message::text(serde_json::to_string(&join).unwrap()))
        .await
        .unwrap();
    let mut got = Vec::new();
    let closed = tokio::time::timeout(common::TIMEOUT, async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Text(text))) => got.push(text.to_string()),
                Some(Ok(Message::Close(_)) | Err(_)) | None => return,
                Some(Ok(_)) => {}
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "Bob's socket stayed open");
    assert!(got.is_empty(), "{got:?}");

    // Ann is still connected and still typing.
    ann.type_at(3, "def").await;
    ann.pump_until(|c| c.pending() == 0 && c.seq() == Seq(2))
        .await;
    assert_eq!(ann.text(), "abcdef");
    assert!(!ann.closed);
    let bob_seen = ann.received.iter().any(|m| match m {
        ServerMsg::People { participants } | ServerMsg::Joined { participants, .. } => {
            participants.iter().any(|p| p.user == bob.id)
        }
        _ => false,
    });
    assert!(!bob_seen, "{:?}", ann.received);
    assert_eq!(
        ann.participants()
            .iter()
            .map(|p| (p.client, p.user))
            .collect::<Vec<_>>(),
        vec![(ann.client(), ann.user.id)]
    );
}
