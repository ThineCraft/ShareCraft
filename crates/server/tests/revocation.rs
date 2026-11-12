//! Access that ends while someone is connected takes effect at once (spine rule S5): the person
//! is removed from the live session, or their rights change without reconnecting.

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

use collab_core::ids::{DocId, OpId, Seq, SessionId, UserId};
use collab_text::TextOp;
use common::{
    SHORT_GRACE, Site, TestClient, TestServer, TestUser, handshake_status, new_doc, spawn_server,
    spawn_server_config, spawn_server_with_grace, wait_out_grace, wait_stored, ws_request,
};
use protocol::{ClientMsg, RemovedReason, ServerMsg};
use records::{Principal, SessionToken};
use reqwest::StatusCode;
use revisions::{
    Checkpoint, CheckpointError, GitError, Outcome, RevisionId, RevisionInfo, RevisionStore,
    Stored, TreeChange, TreeRecord,
};
use server::Checkpointer;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

/// How soon a person must be out of a session once their access ended.
const PROMPTLY: Duration = Duration::from_secs(1);

/// `owner` removes `user`'s grant on `doc` through the API.
async fn revoke(site: &Site, owner: &TestUser, doc: DocId, user: UserId) {
    let response = site
        .http(Some(owner))
        .delete(site.url(&format!("/api/docs/{}/access/user/{}", doc.0, user.0)))
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

/// Two owners change Bob's role at the same moment, over and over. Whichever change is stored
/// last, the room must end up agreeing with the database.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_role_changes_end_as_the_database_says() {
    let server = spawn_server().await;
    let doc = new_doc();
    let mut ann = TestClient::connect(&server, doc, "Ann").await;
    let mut bob = TestClient::connect(&server, doc, "Bob").await;
    let bob_id = bob.user.id;
    for round in 0..30 {
        let (first, second) = if round % 2 == 0 {
            ("editor", "viewer")
        } else {
            ("viewer", "editor")
        };
        tokio::join!(
            share(&server, &ann.user, doc, &bob.user, first),
            share(&server, &ann.user, doc, &bob.user, second),
        );
        // Both changes reached the room before the API answered; Ann's change comes after
        // them, so once Bob has it the room has dealt with both.
        let end = ann.text().chars().count();
        ann.type_at(end, "x").await;
        ann.pump_until(|c| c.pending() == 0).await;
        let seq = ann.seq();
        bob.pump_until(|c| c.seq() == seq).await;

        let stored = server.db.db.doc_access(bob_id, doc).await.unwrap().unwrap();
        assert_eq!(bob.can_edit, stored.can_edit(), "round {round}");
        let listed = ann
            .participants()
            .iter()
            .find(|p| p.user == bob_id)
            .map(|p| p.can_edit);
        assert_eq!(listed, Some(stored.can_edit()), "round {round}");
    }
    assert!(bob.removed.is_none() && !bob.closed);
}

/// The `Access` messages `c` has received.
fn access_messages(c: &TestClient) -> usize {
    c.received
        .iter()
        .filter(|m| matches!(m, ServerMsg::Access { .. }))
        .count()
}

/// A role change that leaves editing as it was is still told, so the page can show or take
/// away sharing: an editor made owner, then the owner made editor again.
#[tokio::test]
async fn role_changes_that_keep_editing_are_told() {
    let server = spawn_server().await;
    let doc = new_doc();
    let ann = TestClient::connect(&server, doc, "Ann").await;
    let mut bob = TestClient::connect(&server, doc, "Bob").await;
    assert!(bob.can_edit);

    share(&server, &ann.user, doc, &bob.user, "owner").await;
    tokio::time::timeout(PROMPTLY, bob.pump_until(|c| access_messages(c) == 1))
        .await
        .expect("promotion not told within one second");
    share(&server, &ann.user, doc, &bob.user, "editor").await;
    tokio::time::timeout(PROMPTLY, bob.pump_until(|c| access_messages(c) == 2))
        .await
        .expect("demotion not told within one second");
    let told: Vec<_> = bob
        .received
        .iter()
        .filter(|m| matches!(m, ServerMsg::Access { .. }))
        .collect();
    assert_eq!(
        told,
        vec![
            &ServerMsg::Access { can_edit: true },
            &ServerMsg::Access { can_edit: true }
        ]
    );
    assert!(bob.can_edit && bob.removed.is_none() && !bob.closed);
    assert_eq!(joins(&bob), 1);
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
    // Editor to owner: they may still edit, and nothing ever said otherwise.
    assert!(
        !r.received
            .iter()
            .any(|m| matches!(m, ServerMsg::Access { can_edit: false })),
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

/// What `doc` holds in the tests below; no frame sent to someone removed may contain it.
const SECRET: &str = "the launch moves to Tuesday";

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

    fn write_tree(&self, change: &TreeChange<'_>) -> Result<RevisionId, CheckpointError> {
        self.inner.write_tree(change)
    }

    fn read_tree(&self) -> Result<Vec<TreeRecord>, CheckpointError> {
        self.inner.read_tree()
    }

    fn read_tree_record(&self, id: Uuid) -> Result<Option<TreeRecord>, CheckpointError> {
        self.inner.read_tree_record(id)
    }
}

/// Opens the gate when dropped, so a failing test does not leave a load blocked.
struct ReleaseOnDrop(Arc<HeldLoad>);

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.release();
    }
}

/// A server whose loads can be held, with a document Ann created holding [`SECRET`]. Its room
/// has ended, and the load of the next room opened for it is held until released. Returns
/// Ann's account too.
async fn held_document() -> (
    TestServer,
    ReleaseOnDrop,
    tokio::sync::mpsc::UnboundedReceiver<()>,
    DocId,
    TestUser,
) {
    let (started_tx, started) = tokio::sync::mpsc::unbounded_channel();
    let slot: Arc<Mutex<Option<Arc<HeldLoad>>>> = Arc::default();
    let keep = Arc::clone(&slot);
    let server = spawn_server_with_grace(
        move |path| {
            let held = Arc::new(HeldLoad {
                inner: RevisionStore::open(path).unwrap(),
                armed: AtomicBool::new(false),
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
    let doc = new_doc();
    let mut ann = TestClient::connect(&server, doc, "Ann").await;
    ann.type_at(0, SECRET).await;
    ann.pump_until(|c| c.pending() == 0 && c.seq() == Seq(1))
        .await;
    ann.close().await;
    wait_stored(&server, doc, SECRET).await;
    wait_out_grace().await;
    held.0.armed.store(true, Ordering::SeqCst);
    let owner = ann.user.clone();
    (server, held, started, doc, owner)
}

/// Waits until the held load has started: the room is open and every join waits in it.
async fn load_started(started: &mut tokio::sync::mpsc::UnboundedReceiver<()>) {
    tokio::time::timeout(common::TIMEOUT, started.recv())
        .await
        .expect("no load started")
        .unwrap();
}

/// Waits for `c` to be removed and closed, and checks that `Removed { reason }` is the only
/// thing it was sent: nothing of the document reached it.
async fn removed_before_seeing_anything(c: &mut TestClient, reason: RemovedReason) {
    c.pump_until(|c| c.removed.is_some()).await;
    assert_eq!(c.received, vec![ServerMsg::Removed { reason }]);
    c.wait_closed().await;
    assert_eq!(c.received, vec![ServerMsg::Removed { reason }]);
}

/// Bob's grant is removed through the API while his join waits in a room still loading: the
/// API's message reaches the room before he is in it, so only the check made after his join
/// was accepted can catch it. He must get `Removed` and nothing of the text.
#[tokio::test]
async fn revoked_while_joining_gets_no_text() {
    let (server, held, mut started, doc, ann) = held_document().await;
    let mut bob = TestClient::connect_with_resume(&server, doc, "Bob", None).await;
    load_started(&mut started).await;

    revoke(&server, &ann, doc, bob.user.id).await;
    held.0.release();
    removed_before_seeing_anything(&mut bob, RemovedReason::AccessRevoked).await;

    let path = format!("/ws/docs/{}", doc.0);
    let again = ws_request(server.addr, &path, Some(&bob.user.cookie));
    assert_eq!(handshake_status(again).await, 404);
}

/// Changes land in the records while four joins wait in a room still loading, and no room is
/// told of them: Bob's grant is deleted, Cat signs out, Dan becomes a viewer, Eve's account is
/// disabled. The check each connection makes once the room has accepted its join catches all
/// four.
#[tokio::test]
async fn access_ended_during_join_is_caught() {
    let (server, held, mut started, doc, ann) = held_document().await;
    let root = server.admin("Root").await;
    let mut bob = TestClient::connect_with_resume(&server, doc, "Bob", None).await;
    let mut cat = TestClient::connect_with_resume(&server, doc, "Cat", None).await;
    let mut dan = TestClient::connect_with_resume(&server, doc, "Dan", None).await;
    let mut eve = TestClient::connect_with_resume(&server, doc, "Eve", None).await;
    load_started(&mut started).await;

    let db = &server.db.db;
    db.set_access(ann.id, doc, Principal::User(bob.user.id), None)
        .await
        .unwrap();
    let cat_session = session_of(&server, &cat.user).await;
    db.end_session(cat_session).await.unwrap();
    db.set_access(ann.id, doc, Principal::User(dan.user.id), Some("viewer"))
        .await
        .unwrap();
    db.set_disabled(root.id, eve.user.id, true).await.unwrap();
    held.0.release();

    removed_before_seeing_anything(&mut bob, RemovedReason::AccessRevoked).await;
    removed_before_seeing_anything(&mut cat, RemovedReason::SignedOut).await;
    removed_before_seeing_anything(&mut eve, RemovedReason::AccountDisabled).await;

    // Dan stays, read-only, without reconnecting.
    tokio::time::timeout(PROMPTLY, dan.pump_until(|c| c.joined && !c.can_edit))
        .await
        .expect("not told within one second");
    assert_eq!(dan.text(), SECRET);
    let dan_id = dan.user.id;
    dan.pump_until(|c| {
        c.participants()
            .iter()
            .any(|p| p.user == dan_id && !p.can_edit)
    })
    .await;
    assert!(dan.removed.is_none() && !dan.closed);
}
