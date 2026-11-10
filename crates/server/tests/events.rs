//! What happens inside a room is recorded as events (spine rule S7) without the room waiting
//! for the database, and owners can read a document's events.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use collab_core::ids::{DocId, Seq};
use common::{TestClient, new_doc, spawn_server, spawn_server_with, wait_events};
use records::{Event, NewEvent};
use reqwest::StatusCode;
use revisions::{
    Checkpoint, CheckpointError, GitError, Outcome, RevisionId, RevisionInfo, RevisionStore, Stored,
};
use serde_json::{Value, json};

fn kinds(events: &[Event]) -> Vec<&str> {
    events.iter().map(|e| e.kind.as_str()).collect()
}

fn count(events: &[Event], kind: &str) -> usize {
    events.iter().filter(|e| e.kind == kind).count()
}

/// Fails the first `failures` writes, then stores normally.
struct Flaky {
    inner: RevisionStore,
    failures: AtomicUsize,
    /// `describe` fails, so no revision can be recorded in the index.
    describe_fails: bool,
}

impl Flaky {
    fn store(failures: usize) -> impl FnOnce(&std::path::Path) -> Arc<dyn server::Checkpointer> {
        move |path| {
            Arc::new(Flaky {
                inner: RevisionStore::open(path).unwrap(),
                failures: AtomicUsize::new(failures),
                describe_fails: false,
            })
        }
    }

    fn unindexable() -> impl FnOnce(&std::path::Path) -> Arc<dyn server::Checkpointer> {
        |path| {
            Arc::new(Flaky {
                inner: RevisionStore::open(path).unwrap(),
                failures: AtomicUsize::new(0),
                describe_fails: true,
            })
        }
    }
}

impl server::Checkpointer for Flaky {
    fn write(&self, c: &Checkpoint<'_>) -> Result<Outcome, CheckpointError> {
        let left = self.failures.load(Ordering::SeqCst);
        if left > 0 {
            self.failures.store(left - 1, Ordering::SeqCst);
            return Err(CheckpointError::Git(GitError::new("disk on fire")));
        }
        self.inner.write(c)
    }

    fn load(&self, doc: DocId) -> Result<Option<Stored>, CheckpointError> {
        self.inner.load(doc)
    }

    fn describe(
        &self,
        doc: DocId,
        revision: &RevisionId,
    ) -> Result<Option<RevisionInfo>, CheckpointError> {
        if self.describe_fails {
            return Err(CheckpointError::Git(GitError::new("index on fire")));
        }
        self.inner.describe(doc, revision)
    }

    fn history(&self, doc: DocId) -> Result<Vec<RevisionInfo>, CheckpointError> {
        self.inner.history(doc)
    }

    fn documents(&self) -> Result<Vec<DocId>, GitError> {
        self.inner.documents()
    }
}

#[tokio::test]
async fn join_edit_save_and_leave_are_recorded_in_order() {
    let server = spawn_server().await;
    let doc = new_doc();
    let mut ann = TestClient::connect(&server, doc, "Ann").await;
    ann.type_at(0, "hello").await;
    ann.pump_until(|c| c.pending() == 0 && c.seq() == Seq(1))
        .await;
    let revision = ann.wait_saved(Seq(1)).await;
    ann.close().await;

    let events = wait_events(&server.db.db, doc, |e| count(e, "editing.left") == 1).await;
    let ours: Vec<&Event> = events
        .iter()
        .filter(|e| e.kind.starts_with("editing.") || e.kind.starts_with("revision."))
        .collect();
    let order: Vec<&str> = ours.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(order, ["editing.joined", "revision.saved", "editing.left"]);

    let ann_id = ann.user.id;
    assert_eq!(ours[0].actor, Some(ann_id));
    assert_eq!(ours[0].subject, Some(ann_id));
    assert_eq!(ours[0].detail, json!({ "can_edit": true }));
    assert_eq!(ours[1].actor, None);
    assert_eq!(ours[1].subject, None);
    assert_eq!(
        ours[1].detail,
        json!({ "revision": revision, "authors": [ann_id.0.to_string()] })
    );
    assert_eq!(ours[2].actor, Some(ann_id));
    assert_eq!(ours[2].subject, Some(ann_id));
    assert_eq!(ours[2].detail, json!({}));
}

/// Asserts that for each person the presence events alternate, starting with a join, and end
/// balanced: every `editing.joined` is followed by exactly one `editing.left` or
/// `editing.removed` before the next join.
fn assert_presence_pairs_up(events: &[Event]) {
    let mut people: Vec<_> = events.iter().filter_map(|e| e.subject).collect();
    people.sort_by_key(|u| u.0);
    people.dedup();
    for person in people {
        let mut inside = false;
        for e in events.iter().filter(|e| e.subject == Some(person)) {
            match e.kind.as_str() {
                "editing.joined" => {
                    assert!(!inside, "joined twice in a row: {:?}", kinds(events));
                    inside = true;
                }
                "editing.left" | "editing.removed" => {
                    assert!(inside, "left without joining: {:?}", kinds(events));
                    inside = false;
                }
                _ => {}
            }
        }
        assert!(!inside, "never left: {:?}", kinds(events));
    }
}

/// Waits for exactly `joined` and `left` presence events on `doc`, lets any more arrive, and
/// checks the counts still hold.
async fn settled_presence(
    server: &common::TestServer,
    doc: DocId,
    joined: usize,
    left: usize,
) -> Vec<Event> {
    wait_events(&server.db.db, doc, |e| {
        count(e, "editing.joined") == joined && count(e, "editing.left") == left
    })
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let events = server.db.db.events_for(doc).await.unwrap();
    assert_eq!(
        count(&events, "editing.joined"),
        joined,
        "{:?}",
        kinds(&events)
    );
    assert_eq!(count(&events, "editing.left"), left, "{:?}", kinds(&events));
    events
}

#[tokio::test]
async fn joins_and_leaves_pair_up_across_reconnects() {
    let server = spawn_server().await;

    // A new connection replaces the old one while it is still open: no extra events.
    let doc = new_doc();
    let mut ann = TestClient::connect(&server, doc, "Ann").await;
    let old = ann.reconnect_keeping_old().await;
    ann.close().await;
    drop(old);
    let events = settled_presence(&server, doc, 1, 1).await;
    assert_presence_pairs_up(&events);

    // The old connection ended first, and the client resumes: left, then joined again.
    let doc = new_doc();
    let mut bob = TestClient::connect(&server, doc, "Bob").await;
    bob.disconnect();
    wait_events(&server.db.db, doc, |e| count(e, "editing.left") == 1).await;
    bob.reconnect().await;
    assert!(
        bob.received
            .iter()
            .all(|m| !matches!(m, protocol::ServerMsg::Reset { .. })),
        "the resume was meant to be honoured"
    );
    bob.close().await;
    let events = settled_presence(&server, doc, 2, 2).await;
    assert_presence_pairs_up(&events);

    // The old connection ended first, and the client's position is from another lifetime of
    // the room, so it gets a Reset: left, then joined again.
    let doc = new_doc();
    let mut cy = TestClient::connect(&server, doc, "Cy").await;
    cy.disconnect();
    wait_events(&server.db.db, doc, |e| count(e, "editing.left") == 1).await;
    cy.epoch = Some(protocol::RoomEpoch(uuid::Uuid::new_v4()));
    cy.reconnect().await;
    assert!(
        cy.received
            .iter()
            .any(|m| matches!(m, protocol::ServerMsg::Reset { .. })),
        "the resume was meant to be refused"
    );
    cy.close().await;
    let events = settled_presence(&server, doc, 2, 2).await;
    assert_presence_pairs_up(&events);
}

#[tokio::test]
async fn a_stale_save_records_one_conflict_with_the_current_revision() {
    let server = spawn_server().await;
    let doc = new_doc();
    let mut ann = TestClient::connect(&server, doc, "Ann").await;
    ann.type_at(0, "abc").await;
    ann.pump_until(|c| c.pending() == 0 && c.seq() == Seq(1))
        .await;
    let saved: RevisionId = ann.wait_saved(Seq(1)).await.parse().unwrap();

    // Another writer saves the document behind the room's back.
    let other = server
        .store()
        .checkpoint(&Checkpoint {
            doc,
            file_name: "other.txt",
            bytes: b"other text",
            base: Some(&saved),
            authors: &[revisions::Author {
                id: "someone-else".into(),
                name: "Someone Else".into(),
            }],
            fields: &serde_json::Map::new(),
        })
        .unwrap();
    let Outcome::Written(other) = other else {
        panic!("expected a new revision");
    };
    ann.type_at(3, "!").await;
    ann.pump_until(|c| c.pending() == 0 && c.seq() == Seq(2))
        .await;

    let events = wait_events(&server.db.db, doc, |e| count(e, "revision.conflict") == 1).await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let later = server.db.db.events_for(doc).await.unwrap();
    assert_eq!(count(&later, "revision.conflict"), 1);
    let conflict = events
        .iter()
        .find(|e| e.kind == "revision.conflict")
        .unwrap();
    assert_eq!(conflict.detail, json!({ "current": other.to_string() }));
}

#[tokio::test]
async fn a_revision_that_cannot_be_indexed_records_an_index_failure() {
    let server = spawn_server_with(Flaky::unindexable()).await;
    let doc = new_doc();
    let mut ann = TestClient::connect(&server, doc, "Ann").await;
    ann.type_at(0, "abc").await;
    ann.pump_until(|c| c.pending() == 0 && c.seq() == Seq(1))
        .await;
    ann.wait_saved(Seq(1)).await;

    let events = wait_events(&server.db.db, doc, |e| {
        count(e, "revision.index_failed") == 1
    })
    .await;
    let failed = events
        .iter()
        .find(|e| e.kind == "revision.index_failed")
        .unwrap();
    assert_eq!(failed.actor, None);
    assert_eq!(failed.subject, None);
    let error = failed.detail["error"].as_str().unwrap();
    assert!(error.contains("index on fire"), "{error}");
}

#[tokio::test]
async fn removal_is_recorded_after_the_revocation_that_caused_it() {
    let server = spawn_server().await;
    let doc = new_doc();
    let mut ann = TestClient::connect(&server, doc, "Ann").await;
    let mut bob = TestClient::connect(&server, doc, "Bob").await;
    ann.pump_until(|c| c.people().len() == 2).await;

    let response = server
        .http(Some(&ann.user))
        .delete(server.url(&format!("/api/docs/{}/access/{}", doc.0, bob.user.id.0)))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    bob.pump_until(|c| c.removed.is_some()).await;

    let events = wait_events(&server.db.db, doc, |e| count(e, "editing.removed") == 1).await;
    let revoked = events
        .iter()
        .position(|e| e.kind == "access.revoked")
        .unwrap();
    let removed = events
        .iter()
        .position(|e| e.kind == "editing.removed")
        .unwrap();
    assert!(revoked < removed, "{:?}", kinds(&events));
    assert!(events[revoked].id < events[removed].id);
    assert_eq!(events[removed].actor, None);
    assert_eq!(events[removed].subject, Some(bob.user.id));
    assert_eq!(
        events[removed].detail,
        json!({ "reason": "access_revoked" })
    );
    // Being removed is not also leaving.
    assert!(
        !events
            .iter()
            .any(|e| e.kind == "editing.left" && e.subject == Some(bob.user.id))
    );
}

#[tokio::test]
async fn repeated_save_failures_record_one_event() {
    let server = spawn_server_with(Flaky::store(3)).await;
    let doc = new_doc();
    let mut ann = TestClient::connect(&server, doc, "Ann").await;
    ann.type_at(0, "abc").await;
    ann.pump_until(|c| c.pending() == 0 && c.seq() == Seq(1))
        .await;
    ann.wait_saved(Seq(1)).await;

    let events = wait_events(&server.db.db, doc, |e| count(e, "revision.saved") == 1).await;
    assert_eq!(
        count(&events, "revision.save_failed"),
        1,
        "{:?}",
        kinds(&events)
    );
    let failed_at = events
        .iter()
        .position(|e| e.kind == "revision.save_failed")
        .unwrap();
    let saved_at = events
        .iter()
        .position(|e| e.kind == "revision.saved")
        .unwrap();
    assert!(failed_at < saved_at);
    let error = events[failed_at].detail["error"].as_str().unwrap();
    assert!(error.contains("disk on fire"), "{error}");
}

#[tokio::test]
async fn events_hold_no_document_text() {
    let marker = "zq-private-marker-7731";
    let server = spawn_server_with(Flaky::store(1)).await;
    let doc = new_doc();
    let mut ann = TestClient::connect(&server, doc, "Ann").await;
    ann.type_at(0, marker).await;
    ann.pump_until(|c| c.pending() == 0 && c.seq() == Seq(1))
        .await;
    ann.wait_saved(Seq(1)).await;
    ann.close().await;

    let events = wait_events(&server.db.db, doc, |e| {
        count(e, "editing.left") == 1 && count(e, "revision.saved") == 1
    })
    .await;
    assert!(count(&events, "revision.save_failed") == 1);
    for event in &events {
        let text = format!("{event:?}");
        assert!(!text.contains(marker), "{text}");
    }
    let api = server
        .http(Some(&ann.user))
        .get(server.url(&format!("/api/docs/{}/events", doc.0)))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!api.contains(marker));
}

#[tokio::test]
async fn only_owners_read_a_documents_events() {
    let server = spawn_server().await;
    let doc = new_doc();
    let ann = server.user("Ann").await;
    let bob = server.user("Bob").await;
    let cy = server.user("Cy").await;
    server.give_access(doc, &ann).await;
    server.give_access(doc, &bob).await;
    let url = server.url(&format!("/api/docs/{}/events", doc.0));

    let owner = server.http(Some(&ann)).get(&url).send().await.unwrap();
    assert_eq!(owner.status(), StatusCode::OK);
    let rows: Vec<Value> = owner.json().await.unwrap();
    assert!(!rows.is_empty());
    let row = &rows[0];
    assert_eq!(row["kind"], "document.created");
    assert_eq!(row["actor"], ann.id.0.to_string());
    assert!(row["subject"].is_null());
    assert!(row["id"].is_i64());
    assert!(row["at"].as_str().unwrap().contains('T'));
    assert!(row["detail"].is_object());

    let editor = server.http(Some(&bob)).get(&url).send().await.unwrap();
    assert_eq!(editor.status(), StatusCode::FORBIDDEN);
    let stranger = server.http(Some(&cy)).get(&url).send().await.unwrap();
    assert_eq!(stranger.status(), StatusCode::NOT_FOUND);
    let missing = server
        .http(Some(&ann))
        .get(server.url(&format!("/api/docs/{}/events", new_doc().0)))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn events_route_returns_at_most_200() {
    let server = spawn_server().await;
    let doc = new_doc();
    let ann = server.user("Ann").await;
    server.give_access(doc, &ann).await;
    for n in 0..205 {
        server
            .db
            .db
            .append_event(&NewEvent {
                kind: "editing.joined",
                actor: Some(ann.id),
                subject: Some(ann.id),
                document: Some(doc),
                detail: json!({ "n": n }),
            })
            .await
            .unwrap();
    }

    let rows: Vec<Value> = server
        .http(Some(&ann))
        .get(server.url(&format!("/api/docs/{}/events", doc.0)))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(rows.len(), 200);
    // The document.created event and the first five appended are the oldest, so left out.
    assert_eq!(rows[0]["detail"], json!({ "n": 5 }));
    assert_eq!(rows[199]["detail"], json!({ "n": 204 }));
    let ids: Vec<i64> = rows.iter().map(|r| r["id"].as_i64().unwrap()).collect();
    assert!(ids.windows(2).all(|w| w[0] < w[1]));
}
