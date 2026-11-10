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
}

impl Flaky {
    fn store(failures: usize) -> impl FnOnce(&std::path::Path) -> Arc<dyn server::Checkpointer> {
        move |path| {
            Arc::new(Flaky {
                inner: RevisionStore::open(path).unwrap(),
                failures: AtomicUsize::new(failures),
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

#[tokio::test]
async fn reconnecting_with_resume_records_no_new_join() {
    let server = spawn_server().await;
    let doc = new_doc();
    let mut ann = TestClient::connect(&server, doc, "Ann").await;
    ann.reconnect().await;
    ann.close().await;

    let events = wait_events(&server.db.db, doc, |e| count(e, "editing.left") >= 1).await;
    assert_eq!(count(&events, "editing.joined"), 1, "{:?}", kinds(&events));
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
