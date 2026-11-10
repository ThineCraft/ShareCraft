//! Access that ends while someone is connected takes effect at once (spine rule S5): the person
//! is removed from the live session, or their rights change without reconnecting.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::time::Duration;

use collab_core::ids::{DocId, OpId, Seq, SessionId, UserId};
use collab_text::TextOp;
use common::{
    Site, TestClient, TestServer, TestUser, handshake_status, new_doc, spawn_server,
    spawn_server_config, spawn_server_with_state, wait_stored, ws_request,
};
use protocol::{ClientMsg, RemovedReason, ServerMsg};
use records::{Role, SessionToken};
use reqwest::StatusCode;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

/// How soon a person must be out of a session once their access ended.
const PROMPTLY: Duration = Duration::from_secs(1);

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

/// Waits at most [`PROMPTLY`] for `c` to be removed, and returns why.
async fn removed_promptly(c: &mut TestClient) -> RemovedReason {
    tokio::time::timeout(PROMPTLY, c.pump_until(|c| c.removed.is_some()))
        .await
        .expect("not removed within one second");
    c.removed.unwrap()
}

/// Waits until `c`'s people list no longer has `user` in it.
async fn wait_gone(c: &mut TestClient, user: UserId) {
    c.pump_until(|c| c.participants().iter().all(|p| p.user != user))
        .await;
}

/// How many times `c` joined (`Joined` or `Reset`): one means it never reconnected.
fn joins(c: &TestClient) -> usize {
    c.received
        .iter()
        .filter(|m| matches!(m, ServerMsg::Joined { .. } | ServerMsg::Reset { .. }))
        .count()
}

/// The sign-in session `user`'s cookie holds.
async fn session_of(server: &TestServer, user: &TestUser) -> SessionId {
    let token = user.cookie.strip_prefix("sharecraft_session=").unwrap();
    let token = SessionToken::parse(token).unwrap();
    server.db.db.session(&token).await.unwrap().unwrap().id
}

/// Sends a change typed at the start of the text, straight to the socket.
async fn submit_raw(c: &mut TestClient, base: Seq, text: &str) -> OpId {
    let op_id = OpId(Uuid::new_v4());
    let msg = ClientMsg::Submit {
        op_id,
        base,
        op: TextOp::replace(c.text().chars().count(), 0, 0, text).unwrap(),
    };
    c.send_raw_lossy(Message::text(serde_json::to_string(&msg).unwrap()))
        .await;
    op_id
}

#[tokio::test]
async fn revoked_editor_is_removed_within_one_second() {
    let server = spawn_server().await;
    let doc = new_doc();
    let mut ann = TestClient::connect(&server, doc, "Ann").await;
    let mut bob = TestClient::connect(&server, doc, "Bob").await;
    ann.pump_until(|c| c.people().len() == 2).await;

    revoke(&server, &ann.user, doc, bob.user.id).await;
    assert_eq!(
        removed_promptly(&mut bob).await,
        RemovedReason::AccessRevoked
    );
    bob.wait_closed().await;
    wait_gone(&mut ann, bob.user.id).await;
    assert_eq!(ann.people(), vec!["Ann".to_owned()]);

    let path = format!("/ws/docs/{}", doc.0);
    let again = ws_request(server.addr, &path, Some(&bob.user.cookie));
    assert_eq!(handshake_status(again).await, 404);
}

#[tokio::test]
async fn revoked_editors_committed_text_is_kept_and_credited() {
    // No checkpoint while anyone is connected: the one after the removal is the first.
    let server = spawn_server_config(|c| c.checkpoint_idle = Duration::from_secs(600)).await;
    let doc = new_doc();
    let mut ann = TestClient::connect(&server, doc, "Ann").await;
    let mut bob = TestClient::connect(&server, doc, "Bob").await;
    bob.type_at(0, "bob was here").await;
    bob.pump_until(|c| c.pending() == 0 && c.seq() == Seq(1))
        .await;
    ann.pump_until(|c| c.seq() == Seq(1)).await;

    revoke(&server, &ann.user, doc, bob.user.id).await;
    assert_eq!(
        removed_promptly(&mut bob).await,
        RemovedReason::AccessRevoked
    );
    ann.type_at(12, ".").await;
    ann.pump_until(|c| c.pending() == 0 && c.seq() == Seq(2))
        .await;
    assert_eq!(ann.text(), "bob was here.");
    // Ann leaves; the empty room saves at once.
    ann.close().await;
    wait_stored(&server, doc, "bob was here.").await;

    let stored = server.store().read(doc, None).unwrap().unwrap();
    let mut credited: Vec<_> = stored
        .meta
        .contributors
        .iter()
        .map(|p| p.id.clone())
        .collect();
    credited.sort();
    let mut expected = vec![ann.user.id.0.to_string(), bob.user.id.0.to_string()];
    expected.sort();
    assert_eq!(credited, expected);
}

#[tokio::test]
async fn change_sent_after_removal_is_not_committed() {
    let server = spawn_server().await;
    let doc = new_doc();
    let mut ann = TestClient::connect(&server, doc, "Ann").await;
    let mut bob = TestClient::connect(&server, doc, "Bob").await;
    ann.type_at(0, "abc").await;
    ann.pump_until(|c| c.pending() == 0 && c.seq() == Seq(1))
        .await;
    bob.pump_until(|c| c.seq() == Seq(1)).await;

    // The room was told before the API answered, so Bob's change reaches it afterwards, if it
    // gets through at all.
    revoke(&server, &ann.user, doc, bob.user.id).await;
    let late = submit_raw(&mut bob, Seq(1), "zzz").await;
    if !bob.closed {
        assert_eq!(
            removed_promptly(&mut bob).await,
            RemovedReason::AccessRevoked
        );
    }

    ann.type_at(3, "!").await;
    ann.pump_until(|c| c.pending() == 0 && c.seq() == Seq(2))
        .await;
    assert_eq!(ann.text(), "abc!");
    let bobs_commit = bob
        .received
        .iter()
        .any(|m| matches!(m, ServerMsg::Committed { entry } if entry.op_id == late));
    assert!(!bobs_commit, "{:?}", bob.received);
    ann.close().await;
    wait_stored(&server, doc, "abc!").await;
}

#[tokio::test]
async fn demoted_editor_becomes_read_only_without_reconnecting() {
    let server = spawn_server().await;
    let doc = new_doc();
    let mut ann = TestClient::connect(&server, doc, "Ann").await;
    let mut bob = TestClient::connect(&server, doc, "Bob").await;
    assert!(bob.can_edit);

    share(&server, &ann.user, doc, &bob.user, "viewer").await;
    tokio::time::timeout(PROMPTLY, bob.pump_until(|c| !c.can_edit))
        .await
        .expect("not told within one second");
    assert!(
        bob.received
            .contains(&ServerMsg::Access { can_edit: false })
    );
    let bob_id = bob.user.id;
    ann.pump_until(|c| {
        c.participants()
            .iter()
            .any(|p| p.user == bob_id && !p.can_edit)
    })
    .await;

    let refused = submit_raw(&mut bob, Seq(0), "vandal").await;
    bob.pump_until(|c| {
        c.received.iter().any(|m| {
            matches!(m, ServerMsg::Refused { op_id, reason } if *op_id == refused && reason == "read_only")
        })
    })
    .await;
    assert_eq!(joins(&bob), 1);
    assert!(bob.removed.is_none() && !bob.closed);
    assert_eq!(ann.seq(), Seq(0));
}

#[tokio::test]
async fn promoted_viewer_can_type_without_reconnecting() {
    let server = spawn_server().await;
    let doc = new_doc();
    let mut ann = TestClient::connect(&server, doc, "Ann").await;
    let vic = server.user("Vic").await;
    share(&server, &ann.user, doc, &vic, "viewer").await;
    let mut v = TestClient::connect_as(&server, doc, &vic).await;
    assert!(!v.can_edit);

    share(&server, &ann.user, doc, &vic, "editor").await;
    tokio::time::timeout(PROMPTLY, v.pump_until(|c| c.can_edit))
        .await
        .expect("not told within one second");
    assert!(v.received.contains(&ServerMsg::Access { can_edit: true }));

    v.type_at(0, "hi").await;
    v.pump_until(|c| c.pending() == 0 && c.seq() == Seq(1))
        .await;
    ann.pump_until(|c| c.seq() == Seq(1)).await;
    assert_eq!(ann.text(), "hi");
    assert_eq!(joins(&v), 1);
    assert!(v.removed.is_none() && !v.closed);
}

#[tokio::test]
async fn revoking_one_person_leaves_the_others_typing() {
    let server = spawn_server().await;
    let doc = new_doc();
    let mut ann = TestClient::connect(&server, doc, "Ann").await;
    let mut bob = TestClient::connect(&server, doc, "Bob").await;
    let mut cat = TestClient::connect(&server, doc, "Cat").await;
    ann.pump_until(|c| c.people().len() == 3).await;

    revoke(&server, &ann.user, doc, bob.user.id).await;
    assert_eq!(
        removed_promptly(&mut bob).await,
        RemovedReason::AccessRevoked
    );
    bob.wait_closed().await;

    ann.type_at(0, "left ").await;
    cat.type_at(0, "right").await;
    ann.pump_until(|c| c.pending() == 0 && c.seq() == Seq(2))
        .await;
    cat.pump_until(|c| c.pending() == 0 && c.seq() == Seq(2))
        .await;
    assert_eq!(ann.text(), cat.text());
    assert!(ann.text().contains("left ") && ann.text().contains("right"));
    wait_gone(&mut cat, bob.user.id).await;
    let mut names = cat.people();
    names.sort();
    assert_eq!(names, vec!["Ann".to_owned(), "Cat".to_owned()]);
    assert!(ann.removed.is_none() && cat.removed.is_none());
}

#[tokio::test]
async fn both_tabs_of_a_revoked_person_close() {
    let server = spawn_server().await;
    let doc = new_doc();
    let mut ann = TestClient::connect(&server, doc, "Ann").await;
    let mut one = TestClient::connect(&server, doc, "Bob").await;
    let mut two = TestClient::connect_as(&server, doc, &one.user.clone()).await;
    ann.pump_until(|c| c.people().len() == 3).await;

    revoke(&server, &ann.user, doc, one.user.id).await;
    assert_eq!(
        removed_promptly(&mut one).await,
        RemovedReason::AccessRevoked
    );
    assert_eq!(
        removed_promptly(&mut two).await,
        RemovedReason::AccessRevoked
    );
    one.wait_closed().await;
    two.wait_closed().await;
    wait_gone(&mut ann, one.user.id).await;
}

#[tokio::test]
async fn sign_out_closes_that_sessions_sockets_only() {
    let server = spawn_server().await;
    let doc = new_doc();
    let mut ann = TestClient::connect(&server, doc, "Ann").await;
    // One account signed in twice: two sessions.
    let mut here = TestClient::connect(&server, doc, "Bob").await;
    let mut there = TestClient::connect(&server, doc, "Bob").await;
    assert_eq!(here.user.id, there.user.id);
    assert_ne!(here.user.cookie, there.user.cookie);
    ann.pump_until(|c| c.people().len() == 3).await;

    let response = server
        .http(Some(&here.user))
        .delete(server.url("/api/session"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(removed_promptly(&mut here).await, RemovedReason::SignedOut);
    here.wait_closed().await;

    let gone = here.client();
    ann.pump_until(|c| c.participants().iter().all(|p| p.client != gone))
        .await;
    there.type_at(0, "still here").await;
    there
        .pump_until(|c| c.pending() == 0 && c.seq() == Seq(1))
        .await;
    ann.pump_until(|c| c.seq() == Seq(1)).await;
    assert_eq!(ann.text(), "still here");
    assert!(there.removed.is_none() && !there.closed);
}

#[tokio::test]
async fn disabled_account_is_removed_from_every_document() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let first = new_doc();
    let second = new_doc();
    let mut ann_first = TestClient::connect(&server, first, "Ann").await;
    let mut ann_second = TestClient::connect(&server, second, "Ann").await;
    let mut bob_first = TestClient::connect(&server, first, "Bob").await;
    let mut bob_second = TestClient::connect(&server, second, "Bob").await;
    let bob = bob_first.user.id;

    let response = server
        .http(Some(&root))
        .post(server.url(&format!("/api/admin/users/{}/disable", bob.0)))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    for c in [&mut bob_first, &mut bob_second] {
        assert_eq!(removed_promptly(c).await, RemovedReason::AccountDisabled);
        c.wait_closed().await;
    }
    wait_gone(&mut ann_first, bob).await;
    wait_gone(&mut ann_second, bob).await;
    assert!(ann_first.removed.is_none() && ann_second.removed.is_none());
}

#[tokio::test]
async fn expired_session_closes_the_socket() {
    let server = spawn_server().await;
    let doc = new_doc();
    let mut ann = TestClient::connect(&server, doc, "Ann").await;
    let bob = server.user("Bob").await;
    server.give_access(doc, &bob).await;
    // Bob's sign-in runs out two seconds from now.
    sqlx::query(
        "update sessions set expires_at = now() + interval '2 seconds' \
         where user_id = $1 and ended_at is null",
    )
    .bind(bob.id.0)
    .execute(server.db.db.test_pool())
    .await
    .unwrap();
    let mut b = TestClient::connect_as(&server, doc, &bob).await;
    ann.pump_until(|c| c.people().len() == 2).await;

    b.pump_until(|c| c.removed.is_some()).await;
    assert_eq!(b.removed, Some(RemovedReason::SignedOut));
    b.wait_closed().await;
    wait_gone(&mut ann, bob.id).await;
}

#[tokio::test]
async fn admin_keeps_access_when_their_grant_is_removed() {
    let server = spawn_server().await;
    let doc = new_doc();
    let mut ann = TestClient::connect(&server, doc, "Ann").await;
    let root = server.admin("Root").await;
    share(&server, &ann.user, doc, &root, "editor").await;
    let mut r = TestClient::connect_as(&server, doc, &root).await;
    assert!(r.can_edit);

    revoke(&server, &ann.user, doc, root.id).await;
    // Ann's change reaches the room after the access change, so once the administrator sees
    // it, the room has dealt with the change.
    ann.type_at(0, "still yours").await;
    r.pump_until(|c| c.seq() == Seq(1)).await;
    assert_eq!(r.text(), "still yours");
    assert!(r.removed.is_none() && !r.closed && r.can_edit);
    assert!(
        !r.received
            .iter()
            .any(|m| matches!(m, ServerMsg::Access { .. })),
        "{:?}",
        r.received
    );
    r.type_at(11, "!").await;
    r.pump_until(|c| c.pending() == 0 && c.seq() == Seq(2))
        .await;
}

/// The story `sequencing.html` asks to see on every build: two people type at once and
/// converge, one drops and comes back without losing what they typed meanwhile, one has their
/// access revoked and is removed, and the checkpoint read back from Git holds the converged
/// text.
#[tokio::test]
async fn standing_proof() {
    let server = spawn_server().await;
    let doc = new_doc();
    let mut ann = TestClient::connect(&server, doc, "Ann").await;
    let mut bob = TestClient::connect(&server, doc, "Bob").await;
    let mut cat = TestClient::connect(&server, doc, "Cat").await;
    ann.pump_until(|c| c.people().len() == 3).await;

    // Ann and Bob type at the same moment, and both end up with the same text.
    ann.type_at(0, "Hello ").await;
    bob.type_at(0, "world").await;
    ann.pump_until(|c| c.pending() == 0 && c.seq() == Seq(2))
        .await;
    bob.pump_until(|c| c.pending() == 0 && c.seq() == Seq(2))
        .await;
    assert_eq!(ann.text(), bob.text());

    // Bob's connection drops. He keeps typing; Ann does too. When he comes back his typing
    // is sent and nothing is lost.
    bob.disconnect();
    let end = bob.text().chars().count();
    bob.type_at(end, "!").await;
    ann.type_at(0, "> ").await;
    ann.pump_until(|c| c.pending() == 0 && c.seq() == Seq(3))
        .await;
    bob.reconnect().await;
    bob.pump_until(|c| c.pending() == 0 && c.seq() == Seq(4))
        .await;
    ann.pump_until(|c| c.seq() == Seq(4)).await;
    let text = ann.text();
    assert_eq!(bob.text(), text);
    assert!(text.starts_with("> ") && text.ends_with('!'), "{text}");
    assert!(text.contains("Hello ") && text.contains("world"), "{text}");

    // Ann revokes Cat's access; Cat is out within a second and the others see her go.
    revoke(&server, &ann.user, doc, cat.user.id).await;
    assert_eq!(
        removed_promptly(&mut cat).await,
        RemovedReason::AccessRevoked
    );
    cat.wait_closed().await;
    wait_gone(&mut ann, cat.user.id).await;
    wait_gone(&mut bob, cat.user.id).await;

    // The checkpoint read back from Git holds the converged text.
    ann.wait_saved(Seq(4)).await;
    let stored = server.store().read(doc, None).unwrap().unwrap();
    assert_eq!(String::from_utf8(stored.bytes).unwrap(), text);
}

#[tokio::test]
async fn access_ended_during_join_is_caught() {
    let (server, state) = spawn_server_with_state().await;
    let doc = new_doc();
    let mut ann = TestClient::connect(&server, doc, "Ann").await;
    let mut bob = TestClient::connect(&server, doc, "Bob").await;
    let mut cat = TestClient::connect(&server, doc, "Cat").await;
    let mut dan = TestClient::connect(&server, doc, "Dan").await;
    ann.pump_until(|c| c.people().len() == 4).await;
    let db = &server.db.db;

    // Each change lands in the records while the person's join is on its way: the API's
    // message to the room went out before they were in it, so the room was never told.
    // Bob's grant is deleted; Cat signs out; Dan becomes a viewer.
    db.set_access(ann.user.id, doc, bob.user.id, None)
        .await
        .unwrap();
    let cat_session = session_of(&server, &cat.user).await;
    db.end_session(cat_session).await.unwrap();
    db.set_access(ann.user.id, doc, dan.user.id, Some(Role::Viewer))
        .await
        .unwrap();
    // The room still has them as they were.
    ann.type_at(0, "x").await;
    for c in [&mut bob, &mut cat, &mut dan] {
        c.pump_until(|c| c.seq() == Seq(1)).await;
        assert!(c.removed.is_none() && c.can_edit);
    }

    // The check each connection makes once the room has accepted its join catches all three.
    let bob_session = session_of(&server, &bob.user).await;
    assert!(
        state
            .recheck_after_join_for_test(doc, bob.user.id, bob_session, Role::Editor)
            .await
    );
    assert_eq!(
        removed_promptly(&mut bob).await,
        RemovedReason::AccessRevoked
    );

    assert!(
        state
            .recheck_after_join_for_test(doc, cat.user.id, cat_session, Role::Editor)
            .await
    );
    assert_eq!(removed_promptly(&mut cat).await, RemovedReason::SignedOut);

    let dan_session = session_of(&server, &dan.user).await;
    assert!(
        state
            .recheck_after_join_for_test(doc, dan.user.id, dan_session, Role::Editor)
            .await
    );
    tokio::time::timeout(PROMPTLY, dan.pump_until(|c| !c.can_edit))
        .await
        .expect("not told within one second");
    assert!(dan.removed.is_none() && !dan.closed);

    wait_gone(&mut ann, bob.user.id).await;
    wait_gone(&mut ann, cat.user.id).await;
    let dan_id = dan.user.id;
    ann.pump_until(|c| {
        c.participants()
            .iter()
            .any(|p| p.user == dan_id && !p.can_edit)
    })
    .await;
}
