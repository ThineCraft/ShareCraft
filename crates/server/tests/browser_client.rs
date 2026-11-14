//! The browser's own session logic, `web_client::ClientInner`, against a real server: the
//! harness does only what the page does (send the join when the socket opens, hand every frame
//! to the client, send what it asks to send).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use collab_core::ids::{ClientId, DocId};
use common::{
    Site, Socket, StoppableServer, TIMEOUT, TestUser, new_doc, open_socket, spawn_server,
    wait_stored, wait_stored_in,
};
use futures_util::{SinkExt, StreamExt};
use revisions::RevisionStore;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;
use web_client::{ClientInner, ResetKind, Update};

/// One page: a socket plus the browser's client.
struct Page {
    client: ClientInner,
    doc: DocId,
    /// The signed-in account; its cookie goes with every socket the page opens.
    user: TestUser,
    socket: Option<Socket>,
    /// Every update the client produced, in order.
    updates: Vec<Update>,
}

impl Page {
    /// Signs in the account named `name`, makes sure it has a role on `doc`, opens the page
    /// on `site` and waits for the join to be answered.
    async fn open(site: &Site, doc: DocId, name: &str) -> Page {
        let user = site.user(name).await;
        site.give_access(doc, &user).await;
        Page::open_as(site, doc, user).await
    }

    /// Opens the page for `user` as they are, with whatever role the records give them.
    async fn open_as(site: &Site, doc: DocId, user: TestUser) -> Page {
        let mut page = Page {
            client: ClientInner::new(ClientId(Uuid::new_v4())),
            doc,
            user,
            socket: None,
            updates: Vec::new(),
        };
        page.connect(site).await;
        page
    }

    /// Opens a socket to `site` with the page's cookie, sends the client's join as the page
    /// does on `open`, and waits for the answer. Returns the update the answer produced.
    async fn connect(&mut self, site: &Site) -> Update {
        self.socket = Some(open_socket(site.addr, self.doc, &self.user).await);
        let join = self.client.join_message().unwrap();
        self.send(join).await;
        let before = self.updates.len();
        self.pump_until(|p| p.client.joined()).await;
        self.updates[before].clone()
    }

    /// The connection drops without a goodbye.
    fn disconnect(&mut self) {
        self.socket = None;
    }

    /// The text box replaced `removed` UTF-16 units at `start` with `inserted`.
    async fn edit(&mut self, start: usize, removed: usize, inserted: &str) {
        let message = self.client.local_replace(start, removed, inserted).unwrap();
        self.send(message).await;
    }

    /// Sends `message` if connected, as the page does; a change made while disconnected stays
    /// in the client and is resent after the next join.
    async fn send(&mut self, message: Option<String>) {
        if let (Some(socket), Some(message)) = (self.socket.as_mut(), message) {
            socket.send(Message::text(message)).await.unwrap();
        }
    }

    fn text(&self) -> String {
        self.client.text()
    }

    /// The latest `can_edit` the client reported: what the page gives the text box.
    fn can_edit(&self) -> Option<bool> {
        self.updates.iter().rev().find_map(|u| u.can_edit)
    }

    /// Hands frames to the client until `cond` holds. Fails after five seconds.
    async fn pump_until(&mut self, cond: impl Fn(&Page) -> bool) {
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        while !cond(self) {
            let socket = self.socket.as_mut().expect("not connected");
            let next = tokio::time::timeout_at(deadline, socket.next())
                .await
                .expect("timed out waiting for condition");
            match next {
                Some(Ok(Message::Text(frame))) => {
                    let update = self.client.receive(&frame).unwrap();
                    self.send(update.send.clone()).await;
                    self.updates.push(update);
                }
                Some(Ok(Message::Close(_)) | Err(_)) | None => {
                    panic!("socket closed while waiting")
                }
                Some(Ok(_)) => {}
            }
        }
    }
}

/// `owner` gives `user` the role `role` on `doc` through the API, as the Share panel does.
async fn share(site: &Site, owner: &TestUser, doc: DocId, user: &TestUser, role: &str) {
    let response = site
        .http(Some(owner))
        .put(site.url(&format!("/api/docs/{}/access", doc.0)))
        .json(&serde_json::json!({ "email": user.email, "role": role }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::NO_CONTENT);
}

/// The UTF-16 offset just after the first `needle` in `text`.
fn utf16_after(text: &str, needle: &str) -> usize {
    let at = text.find(needle).unwrap() + needle.len();
    text[..at].encode_utf16().count()
}

#[tokio::test]
async fn browser_clients_converge_on_concurrent_typing_with_emoji() {
    let server = spawn_server().await;
    let doc = new_doc();
    let mut a = Page::open(&server, doc, "Ann").await;
    let mut b = Page::open(&server, doc, "Bob").await;
    a.edit(0, 0, "a😀b").await;
    b.edit(0, 0, "hi").await;
    a.pump_until(|p| p.client.pending() == 0 && p.text().contains("hi"))
        .await;
    b.pump_until(|p| p.client.pending() == 0 && p.text().contains("😀"))
        .await;
    assert_eq!(a.text(), b.text());
    assert_eq!(a.text().chars().count(), 5, "{}", a.text());

    // An edit right after the emoji, at a UTF-16 offset, lands after it for both.
    let after = utf16_after(&b.text(), "😀");
    b.edit(after, 0, "!").await;
    b.pump_until(|p| p.client.pending() == 0).await;
    a.pump_until(|p| p.text().contains("😀!")).await;
    assert_eq!(a.text(), b.text());
}

#[tokio::test]
async fn browser_solo_disconnect_resumes_without_losing_typing() {
    let server = spawn_server().await;
    let doc = new_doc();
    let mut a = Page::open(&server, doc, "Ann").await;
    a.edit(0, 0, "abc").await;
    a.pump_until(|p| p.client.pending() == 0).await;
    a.disconnect();
    // The room has seen the page leave once it saves, which an empty room does at once.
    wait_stored(&server, doc, "abc").await;

    // Typed while disconnected.
    a.edit(3, 0, "def").await;
    a.edit(6, 0, "!").await;
    assert_eq!(a.client.pending(), 2);
    let answer = a.connect(&server).await;
    assert_eq!(answer.reset, None, "{answer:?}");
    assert_eq!(answer.dropped, 0);
    a.pump_until(|p| p.client.pending() == 0).await;
    assert_eq!(a.text(), "abcdef!");

    let b = Page::open(&server, doc, "Bob").await;
    assert_eq!(b.text(), "abcdef!");
}

#[tokio::test]
async fn browser_reports_text_lost_in_a_server_restart() {
    let data_dir = tempfile::tempdir().unwrap();
    // Both servers use one database schema, as one installation does across a restart.
    let db = records::test_support::TestDb::create().await;
    let library = server::library_path(data_dir.path());
    // Nothing is saved while someone is connected; an empty room still saves at once.
    let slow = |c: &mut server::Config| c.checkpoint_idle = std::time::Duration::from_secs(600);
    let mut first = StoppableServer::start(data_dir.path(), &db, slow);
    let doc = new_doc();
    let mut a = Page::open(&first, doc, "Ann").await;
    a.edit(0, 0, "saved").await;
    a.pump_until(|p| p.client.pending() == 0).await;
    a.disconnect();
    wait_stored_in(&library, doc, "saved").await;

    // Back in the same room: this text is confirmed but never saved.
    let answer = a.connect(&first).await;
    assert_eq!(answer.reset, None, "{answer:?}");
    a.edit(5, 0, " and lost").await;
    a.pump_until(|p| p.client.pending() == 0).await;
    assert_eq!(a.text(), "saved and lost");

    first.stop();
    a.disconnect();
    let second = StoppableServer::start(data_dir.path(), &db, slow);
    let answer = a.connect(&second).await;
    assert_eq!(
        answer.reset,
        Some(ResetKind::ReopenedLostText),
        "{answer:?}"
    );
    assert_eq!(
        serde_json::to_value(&answer).unwrap()["reset"],
        "reopened_lost_text"
    );
    assert_eq!(answer.dropped, 0);
    let stored = RevisionStore::open(&library)
        .unwrap()
        .read(doc, None)
        .unwrap()
        .unwrap();
    assert_eq!(common::bytes_in(&library, &stored), b"saved");
    assert_eq!(a.text(), "saved");
}

#[tokio::test]
async fn browser_viewer_is_read_only() {
    let server = spawn_server().await;
    let doc = new_doc();
    let mut ann = Page::open(&server, doc, "Ann").await;
    ann.edit(0, 0, "hello").await;
    ann.pump_until(|p| p.client.pending() == 0).await;
    let bob = server.user("Bob").await;
    share(&server, &ann.user, doc, &bob, "viewer").await;

    let mut bob = Page::open_as(&server, doc, bob).await;
    // The page gives its text box the rights the join reported, and shows the text.
    assert_eq!(bob.can_edit(), Some(false));
    assert_eq!(bob.text(), "hello");
    // The people list says who may not edit, so the page can say "(read only)".
    let people = serde_json::to_value(bob.updates[0].people.as_ref().unwrap()).unwrap();
    let flags: Vec<(String, bool)> = people
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                p["name"].as_str().unwrap().to_owned(),
                p["can_edit"].as_bool().unwrap(),
            )
        })
        .collect();
    assert!(flags.contains(&("Bob".to_owned(), false)), "{flags:?}");
    assert!(flags.contains(&("Ann".to_owned(), true)), "{flags:?}");

    // Even if a change got out, the server refuses it and the text stays as it was.
    bob.edit(5, 0, "!").await;
    bob.pump_until(|p| {
        p.updates
            .iter()
            .any(|u| u.refused.as_deref() == Some("read_only"))
    })
    .await;
    ann.edit(5, 0, "?").await;
    ann.pump_until(|p| p.client.pending() == 0).await;
    assert_eq!(ann.text(), "hello?");
}

#[tokio::test]
async fn browser_demotion_arrives_without_reconnecting() {
    let server = spawn_server().await;
    let doc = new_doc();
    let ann = Page::open(&server, doc, "Ann").await;
    let mut bob = Page::open(&server, doc, "Bob").await;
    assert_eq!(bob.can_edit(), Some(true));
    let joins_before = bob.updates.iter().filter(|u| u.can_edit.is_some()).count();
    assert_eq!(joins_before, 1);

    share(&server, &ann.user, doc, &bob.user, "viewer").await;
    bob.pump_until(|p| p.can_edit() == Some(false)).await;
    // The same socket and the same client carried the news: no second join was made.
    assert!(bob.socket.is_some());
    assert!(bob.client.joined());
    let last = bob.updates.last().unwrap();
    assert_eq!(last.can_edit, Some(false));
    assert!(last.removed.is_none());

    share(&server, &ann.user, doc, &bob.user, "editor").await;
    bob.pump_until(|p| p.can_edit() == Some(true)).await;
    bob.edit(0, 0, "back").await;
    bob.pump_until(|p| p.client.pending() == 0).await;
    assert_eq!(bob.text(), "back");
}

#[tokio::test]
async fn browser_removed_client_stops_and_reports_why() {
    let server = spawn_server().await;
    let doc = new_doc();
    let ann = Page::open(&server, doc, "Ann").await;
    let mut bob = Page::open(&server, doc, "Bob").await;
    bob.edit(0, 0, "mine").await;
    bob.pump_until(|p| p.client.pending() == 0).await;

    let response = server
        .http(Some(&ann.user))
        .delete(server.url(&format!(
            "/api/docs/{}/access/user/{}",
            doc.0, bob.user.id.0
        )))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::NO_CONTENT);
    bob.pump_until(|p| p.updates.iter().any(|u| u.removed.is_some()))
        .await;
    let last = bob.updates.last().unwrap();
    assert_eq!(last.removed.as_deref(), Some("access_revoked"));
    assert_eq!(
        serde_json::to_value(last).unwrap()["removed"],
        "access_revoked"
    );
    // The page must not join again, and further typing is refused locally.
    assert_eq!(bob.client.join_message().unwrap(), None);
    assert!(bob.client.local_replace(0, 0, "x").is_err());
}
