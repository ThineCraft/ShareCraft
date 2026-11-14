//! Site quotas and names over HTTP: a save, upload or copy over the site's quota is refused
//! before anything is committed, two uploads finishing together cannot take a site over it, the
//! live text room is never refused, a failed commit gives its reservation back, deleting and
//! purging always work; usage counts every version, in the bin and inline ones too. A name taken
//! is answered `409 name_taken {suggested}` until the person chooses to replace, keep both or
//! skip, and refused characters are replaced and reported.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use collab_core::ids::{DocId, NodeId, Seq};
use common::{
    FileClient, TestClient, TestServer, TestUser, new_doc, spawn_server, spawn_server_with,
};
use records::Editor;
use reqwest::{Method, StatusCode};
use revisions::{
    Author, BlobRef, Checkpoint, CheckpointError, GitError, Outcome, RevisionId, RevisionInfo,
    RevisionStore, Stored, TreeChange, TreeMove, TreeRecord,
};
use serde_json::{Map, Value, json};
use server::Checkpointer;
use uuid::Uuid;

const SEAT: &str = "x-sharecraft-seat";

/// A server with a site made by the administrator Root, and its library.
struct Team {
    server: TestServer,
    root: TestUser,
    site: String,
    library: String,
}

async fn team() -> Team {
    team_on(spawn_server().await).await
}

async fn team_on(server: TestServer) -> Team {
    let root = server.admin("Root").await;
    let mut t = Team {
        server,
        root,
        site: String::new(),
        library: String::new(),
    };
    let (status, site) = t
        .call(
            &t.root,
            Method::POST,
            "/api/sites",
            Some(json!({ "name": "Team" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    t.site = site["id"].as_str().unwrap().to_owned();
    let (_, page) = t
        .call(
            &t.root,
            Method::GET,
            &format!("/api/nodes/{}/children", t.site),
            None,
        )
        .await;
    t.library = page["items"][0]["id"].as_str().unwrap().to_owned();
    t
}

impl Team {
    async fn call(
        &self,
        user: &TestUser,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        call(&self.server, user, method, path, body).await
    }

    /// Sets the site's quota, as Root.
    async fn set_quota(&self, bytes: Option<u64>) {
        let (status, body) = self
            .call(
                &self.root,
                Method::PUT,
                &format!("/api/sites/{}/quota", self.site),
                Some(json!({ "bytes": bytes })),
            )
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    }

    /// `(used, reserved)` of the site.
    async fn usage(&self) -> (u64, u64) {
        usage_of(&self.server, node_id(&self.site)).await
    }

    /// Uploads `bytes` as `name` into `parent` the one-request way, as Root.
    async fn upload(&self, parent: &str, name: &str, bytes: &[u8]) -> (StatusCode, Value) {
        upload_as(&self.server, &self.root, parent, name, bytes, None).await
    }

    /// Uploads `bytes` as `name` into the library, which must work; the new file's id.
    async fn file(&self, name: &str, bytes: &[u8]) -> String {
        let (status, body) = self.upload(&self.library, name, bytes).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        body["id"].as_str().unwrap().to_owned()
    }

    /// Starts and sends a whole session as Root; the start's answer.
    async fn session(&self, body: Value, bytes: &[u8]) -> Value {
        session_as(&self.server, &self.root, body, bytes).await
    }

    /// Finishes the session `id` as Root, with `on_conflict` if given.
    async fn finish(&self, id: &str, on_conflict: Option<&str>) -> (StatusCode, Value) {
        let path = match on_conflict {
            Some(choice) => format!("/api/uploads/{id}/finish?on_conflict={choice}"),
            None => format!("/api/uploads/{id}/finish"),
        };
        self.call(&self.root, Method::POST, &path, None).await
    }

    /// The names in the library, sorted.
    async fn names(&self) -> Vec<String> {
        let (status, page) = self
            .call(
                &self.root,
                Method::GET,
                &format!("/api/nodes/{}/children", self.library),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{page}");
        let mut names: Vec<String> = page["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["name"].as_str().unwrap().to_owned())
            .collect();
        names.sort();
        names
    }

    /// The number of commits on `main`.
    fn commits(&self) -> usize {
        let repo = gix::open(self.server.library()).unwrap();
        let tip = repo.find_reference("refs/heads/main").unwrap();
        repo.rev_walk([tip.id()]).all().unwrap().count()
    }

    /// The current revision of `doc` in Git.
    fn head(&self, doc: &str) -> Option<String> {
        self.server
            .store()
            .read(doc_id(doc), None)
            .unwrap()
            .map(|stored| stored.revision.to_string())
    }

    /// The current bytes of `doc`, as Root downloads them.
    async fn download(&self, doc: &str) -> Vec<u8> {
        let response = self
            .server
            .http(Some(&self.root))
            .get(self.server.url(&format!("/api/docs/{doc}/content")))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response.bytes().await.unwrap().to_vec()
    }

    async fn delete(&self, node: &str) -> StatusCode {
        self.call(
            &self.root,
            Method::DELETE,
            &format!("/api/nodes/{node}"),
            None,
        )
        .await
        .0
    }

    async fn unbin(&self, node: &str) -> StatusCode {
        self.call(
            &self.root,
            Method::DELETE,
            &format!("/api/bin/{node}"),
            None,
        )
        .await
        .0
    }

    async fn copy(&self, node: &str) -> (StatusCode, Value) {
        self.call(
            &self.root,
            Method::POST,
            &format!("/api/nodes/{node}/copy"),
            Some(json!({ "parent": self.library })),
        )
        .await
    }
}

async fn call(
    server: &TestServer,
    user: &TestUser,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = server.http(Some(user)).request(method, server.url(path));
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.unwrap();
    let status = response.status();
    let bytes = response.bytes().await.unwrap();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, body)
}

/// `(used, reserved)` of `site`, as the records hold them.
async fn usage_of(server: &TestServer, site: NodeId) -> (u64, u64) {
    let usage = server.records_db().site_usage(site).await.unwrap();
    (usage.used, usage.reserved)
}

async fn upload_as(
    server: &TestServer,
    user: &TestUser,
    parent: &str,
    name: &str,
    bytes: &[u8],
    on_conflict: Option<&str>,
) -> (StatusCode, Value) {
    let mut query = vec![("parent", parent), ("name", name)];
    if let Some(choice) = on_conflict {
        query.push(("on_conflict", choice));
    }
    let response = server
        .http(Some(user))
        .post(server.url("/api/docs/upload"))
        .query(&query)
        .body(bytes.to_vec())
        .send()
        .await
        .unwrap();
    let status = response.status();
    (status, response.json().await.unwrap())
}

/// Starts a session with `body` (to which the size and SHA-256 of `bytes` are added) as
/// `user`, and sends all of `bytes`; the start's answer.
async fn session_as(server: &TestServer, user: &TestUser, mut body: Value, bytes: &[u8]) -> Value {
    body["size"] = json!(bytes.len());
    body["sha256"] = json!(BlobRef::of(bytes).hex());
    let (status, started) = call(server, user, Method::POST, "/api/uploads", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{started}");
    let id = started["id"].as_str().unwrap();
    let response = server
        .http(Some(user))
        .put(server.url(&format!("/api/uploads/{id}")))
        .header(
            reqwest::header::CONTENT_RANGE,
            format!("bytes 0-{}/{}", bytes.len() - 1, bytes.len()),
        )
        .body(bytes.to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    started
}

fn uuid(text: &str) -> Uuid {
    text.parse().unwrap()
}

fn doc_id(text: &str) -> DocId {
    DocId(uuid(text))
}

fn node_id(text: &str) -> NodeId {
    NodeId(uuid(text))
}

/// Over the quota, an upload (in one request or a session), a copy and a save are refused with
/// `507 quota_exceeded` before anything is committed, and hold nothing reserved.
#[tokio::test]
async fn quota_refuses_before_committing() {
    let t = team().await;
    let first = t.file("A.pdf", &[1u8; 50]).await;
    t.set_quota(Some(100)).await;
    assert_eq!(t.usage().await, (50, 0));

    let commits = t.commits();
    let (status, body) = t.upload(&t.library, "B.pdf", &[2u8; 60]).await;
    assert_eq!(status, StatusCode::INSUFFICIENT_STORAGE, "{body}");
    assert_eq!(body["error"], "quota_exceeded");
    let (status, body) = t
        .call(
            &t.root,
            Method::POST,
            "/api/uploads",
            Some(json!({
                "parent": t.library,
                "name": "C.pdf",
                "size": 60,
                "sha256": BlobRef::of(&[3u8; 60]).hex(),
            })),
        )
        .await;
    assert_eq!(status, StatusCode::INSUFFICIENT_STORAGE, "{body}");
    assert_eq!(body["error"], "quota_exceeded");

    // A copy fits once, then not again.
    let (status, body) = t.copy(&first).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(t.usage().await, (100, 0));
    let commits_after_copy = t.commits();
    assert_eq!(commits_after_copy, commits + 2);
    let (status, body) = t.copy(&first).await;
    assert_eq!(status, StatusCode::INSUFFICIENT_STORAGE, "{body}");
    assert_eq!(body["error"], "quota_exceeded");

    // An editor's save.
    let seat = FileClient::connect_as(&t.server, doc_id(&first), &t.root).await;
    let response = t
        .server
        .http(Some(&t.root))
        .put(t.server.url(&format!("/api/docs/{first}/content")))
        .query(&[("base", t.head(&first).unwrap())])
        .header(SEAT, seat.token().unwrap())
        .body(vec![9u8; 10])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::INSUFFICIENT_STORAGE);

    assert_eq!(t.commits(), commits_after_copy);
    assert_eq!(t.usage().await, (100, 0));
    assert_eq!(t.names().await, ["A (2).pdf", "A.pdf"]);
    assert_eq!(t.download(&first).await, [1u8; 50]);
}

/// Two uploads that started before the quota was set, finishing at the same moment, cannot take
/// the site over it together: one is stored, the other refused and still open.
#[tokio::test]
async fn concurrent_finish_respects_quota() {
    let t = team().await;
    let a = t
        .session(json!({ "parent": t.library, "name": "A.pdf" }), &[1u8; 60])
        .await;
    let b = t
        .session(json!({ "parent": t.library, "name": "B.pdf" }), &[2u8; 60])
        .await;
    assert_eq!(t.usage().await, (0, 120));
    t.set_quota(Some(100)).await;

    let (a_id, b_id) = (a["id"].as_str().unwrap(), b["id"].as_str().unwrap());
    let (first, second) = tokio::join!(t.finish(a_id, None), t.finish(b_id, None));
    let mut statuses = [first.0, second.0];
    statuses.sort();
    assert_eq!(
        statuses,
        [StatusCode::CREATED, StatusCode::INSUFFICIENT_STORAGE],
        "{first:?} {second:?}"
    );
    assert_eq!(t.names().await.len(), 1);
    // The refused one keeps its reservation until it is abandoned.
    assert_eq!(t.usage().await, (60, 60));
    let refused = if first.0 == StatusCode::CREATED {
        b_id
    } else {
        a_id
    };
    let (status, _) = t
        .call(
            &t.root,
            Method::DELETE,
            &format!("/api/uploads/{refused}"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(t.usage().await, (60, 0));
}

/// The live text room's checkpoints are written whatever the quota, and counted.
#[tokio::test]
async fn live_checkpoints_never_refused_for_quota() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let ann = server.user("Ann").await;
    let doc = new_doc();
    server
        .create_document(doc, "Notes.txt", Editor::Text, &ann)
        .await;
    let site = server
        .records_db()
        .node(doc.into())
        .await
        .unwrap()
        .unwrap()
        .site;
    let (status, body) = call(
        &server,
        &root,
        Method::PUT,
        &format!("/api/sites/{}/quota", site.0),
        Some(json!({ "bytes": 1 })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    let mut a = TestClient::connect_as(&server, doc, &ann).await;
    a.type_at(0, "over the quota, and kept").await;
    a.pump_until(|c| c.pending() == 0 && c.seq() == Seq(1))
        .await;
    a.wait_saved(Seq(1)).await;
    let (used, reserved) = usage_of(&server, site).await;
    assert_eq!(used, 24);
    assert_eq!(reserved, 0);
}

/// A store whose commits of file content fail while `failing` is set.
struct Flaky(RevisionStore, Arc<AtomicBool>);

impl Flaky {
    fn refuse(&self) -> Result<(), CheckpointError> {
        if self.1.load(Ordering::SeqCst) {
            return Err(CheckpointError::Git(GitError::new("disk on fire")));
        }
        Ok(())
    }
}

impl Checkpointer for Flaky {
    fn write(&self, c: &Checkpoint<'_>) -> Result<Outcome, CheckpointError> {
        self.refuse()?;
        self.0.checkpoint(c)
    }

    fn load(&self, doc: DocId) -> Result<Option<Stored>, CheckpointError> {
        self.0.read(doc, None)
    }

    fn load_at(
        &self,
        doc: DocId,
        revision: &RevisionId,
    ) -> Result<Option<Stored>, CheckpointError> {
        self.0.read(doc, Some(revision))
    }

    fn describe(
        &self,
        doc: DocId,
        revision: &RevisionId,
    ) -> Result<Option<RevisionInfo>, CheckpointError> {
        self.0.describe(doc, revision)
    }

    fn history(&self, doc: DocId) -> Result<Vec<RevisionInfo>, CheckpointError> {
        self.0.history(doc)
    }

    fn documents(&self) -> Result<Vec<DocId>, GitError> {
        self.0.documents()
    }

    fn write_tree(&self, change: &TreeChange<'_>) -> Result<RevisionId, CheckpointError> {
        self.0.write_tree(change)
    }

    fn read_tree(&self) -> Result<Vec<TreeRecord>, CheckpointError> {
        self.0.read_tree()
    }

    fn read_tree_record(&self, id: Uuid) -> Result<Option<TreeRecord>, CheckpointError> {
        self.0.read_tree_record(id)
    }

    fn move_tree_records(&self, author: &Author) -> Result<TreeMove, CheckpointError> {
        self.0.move_tree_records(author)
    }

    fn write_new(
        &self,
        author: &Author,
        message: &str,
        checkpoints: &[Checkpoint<'_>],
    ) -> Result<RevisionId, CheckpointError> {
        self.refuse()?;
        self.0.checkpoint_new(author, message, checkpoints)
    }
}

/// An upload or copy whose version cannot be committed gives its reservation back and counts
/// nothing.
#[tokio::test]
async fn failed_commit_releases_the_reservation() {
    let failing = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&failing);
    let server = spawn_server_with(move |path: &Path| {
        Arc::new(Flaky(RevisionStore::open(path).unwrap(), flag)) as Arc<dyn Checkpointer>
    })
    .await;
    let t = team_on(server).await;
    t.set_quota(Some(1000)).await;
    let first = t.file("A.pdf", &[1u8; 100]).await;
    assert_eq!(t.usage().await, (100, 0));

    failing.store(true, Ordering::SeqCst);
    let (status, body) = t.upload(&t.library, "B.pdf", &[2u8; 200]).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(t.usage().await, (100, 0));

    let (status, body) = t.copy(&first).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(t.usage().await, (100, 0));

    let started = t
        .session(json!({ "parent": t.library, "name": "C.pdf" }), &[3u8; 300])
        .await;
    assert_eq!(t.usage().await, (100, 300));
    let (status, body) = t.finish(started["id"].as_str().unwrap(), None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(t.usage().await, (100, 0));

    failing.store(false, Ordering::SeqCst);
    t.file("D.pdf", &[4u8; 900]).await;
    assert_eq!(t.usage().await, (1000, 0));
}

/// Over its quota a site can still delete, restore, move to the second stage and purge; a
/// purge gives the space back.
#[tokio::test]
async fn deleting_and_purging_always_allowed_over_quota() {
    let t = team().await;
    let big = t.file("Big.pdf", &[1u8; 100]).await;
    t.file("Small.pdf", &[2u8; 50]).await;
    t.set_quota(Some(10)).await;
    assert_eq!(t.usage().await, (150, 0));

    assert_eq!(t.delete(&big).await, StatusCode::NO_CONTENT);
    let (status, body) = t
        .call(
            &t.root,
            Method::POST,
            &format!("/api/bin/{big}/restore"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(t.delete(&big).await, StatusCode::NO_CONTENT);
    // In either stage of the bin it still counts.
    assert_eq!(t.unbin(&big).await, StatusCode::NO_CONTENT);
    assert_eq!(t.usage().await, (150, 0));
    assert_eq!(t.unbin(&big).await, StatusCode::NO_CONTENT);
    assert_eq!(t.usage().await, (50, 0));
}

/// Usage is the sum of every version of every item in the site: new versions, items in the bin,
/// and old versions whose bytes are inline in Git, as a rebuild reads them; a purge takes its
/// item's versions off.
#[tokio::test]
async fn usage_counts_versions_bins_and_inline() {
    let server = spawn_server().await;
    let ann = server.user("Ann").await;
    let home = server.home_of(&ann).await;
    let site = server.records_db().node(home).await.unwrap().unwrap().site;
    let home = home.0.to_string();

    // An old file, its bytes inline in Git, written behind the server's back.
    let old = new_doc();
    server.create_file_document(old, &ann).await;
    let name = format!("{}.xlsx", old.0);
    server
        .store()
        .checkpoint_inline(
            &Checkpoint {
                doc: old,
                file_name: &name,
                blob: BlobRef::of(b""),
                base: None,
                authors: &[Author {
                    id: "someone".into(),
                    name: "Someone".into(),
                }],
                fields: &Map::new(),
            },
            &[7u8; 1000],
        )
        .unwrap();

    let (status, body) = upload_as(&server, &ann, &home, "V.pdf", &[1u8; 100], None).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let v = body["id"].as_str().unwrap().to_owned();
    let base = server
        .store()
        .read(doc_id(&v), None)
        .unwrap()
        .unwrap()
        .revision
        .to_string();
    let started = session_as(
        &server,
        &ann,
        json!({ "doc": v, "base": base }),
        &[2u8; 150],
    )
    .await;
    let (status, body) = call(
        &server,
        &ann,
        Method::POST,
        &format!("/api/uploads/{}/finish", started["id"].as_str().unwrap()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(usage_of(&server, site).await, (250, 0));

    let (status, body) = upload_as(&server, &ann, &home, "Binned.pdf", &[3u8; 30], None).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let binned = body["id"].as_str().unwrap().to_owned();
    let (status, _) = call(
        &server,
        &ann,
        Method::DELETE,
        &format!("/api/nodes/{binned}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(usage_of(&server, site).await, (280, 0));

    // The rebuild reads the inline version's size from Git, as on the upgrade, when no
    // version's size was kept. (A start records the inline version first.)
    let db = server.records_db();
    server::reconcile(Arc::new(server.store()), &db)
        .await
        .unwrap();
    sqlx::query("update revisions set size = null")
        .execute(db.test_pool())
        .await
        .unwrap();
    db.flag_usage_rebuild().await.unwrap();
    let state = server.app_state().await;
    let report = server::reconcile_usage(&state).await;
    assert!(report.rebuilt, "{report}");
    assert_eq!(usage_of(&server, site).await, (1280, 0));
    assert!(!db.usage_rebuild_flagged().await.unwrap());

    // Purged, the binned file counts no more.
    let admin = server.admin("Root").await;
    for _ in 0..2 {
        let (status, body) = call(
            &server,
            &admin,
            Method::DELETE,
            &format!("/api/bin/{binned}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    }
    assert_eq!(usage_of(&server, site).await, (1250, 0));
}

/// A name taken in the destination: `409 name_taken {suggested}` without a choice; keep both
/// takes the suggested name, skip creates nothing and ends the session, replace saves a new
/// version of the file there (refused while someone edits it). The one-request upload takes the
/// same choices.
#[tokio::test]
async fn upload_name_choices() {
    let t = team().await;
    let plan = t.file("Plan.pdf", b"first").await;

    let s = t
        .session(
            json!({ "parent": t.library, "name": "plan.pdf" }),
            b"second",
        )
        .await;
    let id = s["id"].as_str().unwrap();
    let (status, body) = t.finish(id, None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "name_taken");
    assert_eq!(body["suggested"], "plan (2).pdf");
    let (status, body) = t.finish(id, Some("keep_both")).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["name"], "plan (2).pdf");

    let s = t
        .session(json!({ "parent": t.library, "name": "Plan.pdf" }), b"third")
        .await;
    let id = s["id"].as_str().unwrap();
    let (status, body) = t.finish(id, Some("skip")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["skipped"], true);
    let (status, _) = t
        .call(&t.root, Method::GET, &format!("/api/uploads/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(t.names().await, ["Plan.pdf", "plan (2).pdf"]);

    // Chosen when it starts: a new version of the file there.
    let s = t
        .session(
            json!({ "parent": t.library, "name": "Plan.pdf", "on_conflict": "replace" }),
            b"fourth",
        )
        .await;
    // While someone holds the editing seat, it is refused and stays open.
    let mut seat = FileClient::connect_as(&t.server, doc_id(&plan), &t.root).await;
    let id = s["id"].as_str().unwrap();
    let (status, body) = t.finish(id, None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "being_edited");
    seat.close().await;
    let (status, body) = t.finish(id, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["changed"], true);
    assert_eq!(t.download(&plan).await, b"fourth");
    assert_eq!(t.names().await, ["Plan.pdf", "plan (2).pdf"]);

    // The one-request upload.
    let (status, body) = t.upload(&t.library, "PLAN.pdf", b"fifth").await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "name_taken");
    assert_eq!(body["suggested"], "PLAN (3).pdf");
    let (status, body) = upload_as(
        &t.server,
        &t.root,
        &t.library,
        "PLAN.pdf",
        b"fifth",
        Some("skip"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["skipped"], true);
    let (status, body) = upload_as(
        &t.server,
        &t.root,
        &t.library,
        "PLAN.pdf",
        b"fifth",
        Some("keep_both"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["name"], "PLAN (3).pdf");
    let (status, body) = upload_as(
        &t.server,
        &t.root,
        &t.library,
        "Plan.pdf",
        b"sixth",
        Some("replace"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(t.download(&plan).await, b"sixth");
    assert_eq!(t.usage().await.1, 0);
}

/// Refused characters in an uploaded file's name become `_`, and the answers say so.
#[tokio::test]
async fn refused_characters_cleaned_and_reported() {
    let t = team().await;
    let started = t
        .session(
            json!({ "parent": t.library, "name": " Q3: plan?.pdf " }),
            b"bytes",
        )
        .await;
    assert_eq!(started["name"], "Q3_ plan_.pdf");
    assert_eq!(started["renamed"]["from"], "Q3: plan?.pdf");
    assert_eq!(started["renamed"]["to"], "Q3_ plan_.pdf");
    let (status, body) = t.finish(started["id"].as_str().unwrap(), None).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["name"], "Q3_ plan_.pdf");
    // Said once, when the session started under that name.
    assert!(body["renamed"].is_null());

    let (status, body) = t.upload(&t.library, "a|b<c>.pdf", b"more").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["name"], "a_b_c_.pdf");
    assert_eq!(body["renamed"]["from"], "a|b<c>.pdf");
    assert_eq!(body["renamed"]["to"], "a_b_c_.pdf");

    // A clean name says nothing.
    let (status, body) = t.upload(&t.library, "Clean.pdf", b"clean").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert!(body["renamed"].is_null());
    assert_eq!(
        t.names().await,
        ["Clean.pdf", "Q3_ plan_.pdf", "a_b_c_.pdf"]
    );
}

/// Administrators set a site's quota; the site answers its usage and quota to people who manage
/// it, with a warning from 90%, and to nobody else.
#[tokio::test]
async fn site_quota_set_and_shown() {
    let t = team().await;
    let ann = t.server.user("Ann").await;
    let (status, body) = t
        .call(
            &t.root,
            Method::PUT,
            &format!("/api/nodes/{}/access", t.site),
            Some(json!({ "email": ann.email, "role": "viewer" })),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    let (status, _) = t
        .call(
            &ann,
            Method::PUT,
            &format!("/api/sites/{}/quota", t.site),
            Some(json!({ "bytes": 5 })),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = t
        .call(
            &t.root,
            Method::PUT,
            &format!("/api/sites/{}/quota", t.library),
            Some(json!({ "bytes": 5 })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    t.file("A.pdf", &[1u8; 95]).await;
    t.set_quota(Some(100)).await;
    let (status, site) = t
        .call(
            &t.root,
            Method::GET,
            &format!("/api/sites/{}", t.site),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{site}");
    assert_eq!(site["name"], "Team");
    assert_eq!(site["used"], 95);
    assert_eq!(site["quota"], 100);
    assert_eq!(site["quota_warning"], true);

    let (status, seen) = t
        .call(&ann, Method::GET, &format!("/api/sites/{}", t.site), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{seen}");
    assert_eq!(seen["name"], "Team");
    assert!(seen.get("used").is_none());
    assert!(seen.get("quota").is_none());

    t.set_quota(None).await;
    let (_, site) = t
        .call(
            &t.root,
            Method::GET,
            &format!("/api/sites/{}", t.site),
            None,
        )
        .await;
    assert!(site["quota"].is_null());
    assert_eq!(site["quota_warning"], false);
    let events: i64 =
        sqlx::query_scalar("select count(*) from events where kind = 'quota.changed'")
            .fetch_one(t.server.records_db().test_pool())
            .await
            .unwrap();
    assert_eq!(events, 2);
}

/// A second site made by Root; `(site, library)`.
async fn other_site(t: &Team, name: &str) -> (String, String) {
    let (status, site) = t
        .call(
            &t.root,
            Method::POST,
            "/api/sites",
            Some(json!({ "name": name })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{site}");
    let site = site["id"].as_str().unwrap().to_owned();
    let (_, page) = t
        .call(
            &t.root,
            Method::GET,
            &format!("/api/nodes/{site}/children"),
            None,
        )
        .await;
    let library = page["items"][0]["id"].as_str().unwrap().to_owned();
    (site, library)
}

/// Sets the quota of `site`, as Root.
async fn set_quota_of(t: &Team, site: &str, bytes: Option<u64>) {
    let (status, body) = t
        .call(
            &t.root,
            Method::PUT,
            &format!("/api/sites/{site}/quota"),
            Some(json!({ "bytes": bytes })),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
}

/// Moves `node` into `parent`, as Root.
async fn move_to(t: &Team, node: &str, parent: &str) -> (StatusCode, Value) {
    t.call(
        &t.root,
        Method::PATCH,
        &format!("/api/nodes/{node}"),
        Some(json!({ "parent": parent })),
    )
    .await
}

/// A file moved into another site takes its versions' bytes there, and is refused with
/// `507 quota_exceeded` when that site has no room for them; nothing moves then.
#[tokio::test]
async fn move_into_full_site_refused() {
    let t = team().await;
    let (other, other_library) = other_site(&t, "Other").await;
    set_quota_of(&t, &other, Some(50)).await;
    let big = t.file("Big.pdf", &[1u8; 100]).await;
    let small = t.file("Small.pdf", &[2u8; 10]).await;

    let (status, body) = move_to(&t, &big, &other_library).await;
    assert_eq!(status, StatusCode::INSUFFICIENT_STORAGE, "{body}");
    assert_eq!(body["error"], "quota_exceeded");
    assert_eq!(t.names().await, ["Big.pdf", "Small.pdf"]);
    assert_eq!(t.usage().await, (110, 0));
    assert_eq!(usage_of(&t.server, node_id(&other)).await, (0, 0));

    let (status, body) = move_to(&t, &small, &other_library).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(t.usage().await, (100, 0));
    assert_eq!(usage_of(&t.server, node_id(&other)).await, (10, 0));
}

/// An upload of a new version whose file moved to another site while it was under way is
/// judged against that site's quota when it finishes: refused there (the session left open,
/// still reserved where it started), and once there is room, stored and counted there.
#[tokio::test]
async fn upload_follows_its_file_to_another_site() {
    let t = team().await;
    let (other, other_library) = other_site(&t, "Other").await;
    let file = t.file("F.pdf", &[1u8; 10]).await;
    let started = t
        .session(json!({ "doc": file, "base": t.head(&file) }), &[2u8; 60])
        .await;
    let id = started["id"].as_str().unwrap();
    set_quota_of(&t, &other, Some(50)).await;
    let (status, body) = move_to(&t, &file, &other_library).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body) = t.finish(id, None).await;
    assert_eq!(status, StatusCode::INSUFFICIENT_STORAGE, "{body}");
    let (status, _) = t
        .call(&t.root, Method::GET, &format!("/api/uploads/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(t.usage().await, (0, 60));
    assert_eq!(usage_of(&t.server, node_id(&other)).await, (10, 0));

    set_quota_of(&t, &other, None).await;
    let (status, body) = t.finish(id, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(t.usage().await, (0, 0));
    assert_eq!(usage_of(&t.server, node_id(&other)).await, (70, 0));
}

/// A body that sends a little and then nothing more: only an answer given before reading it
/// can arrive.
fn endless_body() -> reqwest::Body {
    let start = futures_util::stream::once(async { Ok::<_, std::io::Error>(vec![1u8; 4]) });
    reqwest::Body::wrap_stream(futures_util::StreamExt::chain(
        start,
        futures_util::stream::pending(),
    ))
}

/// A body that cannot fit is refused before it is read: by its declared length, or one byte
/// when it declares none.
#[tokio::test]
async fn over_quota_refused_before_the_body() {
    let t = team().await;
    let file = t.file("Full.pdf", &[1u8; 10]).await;
    t.set_quota(Some(10)).await;
    let answer = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        t.server
            .http(Some(&t.root))
            .post(t.server.url("/api/docs/upload"))
            .query(&[("parent", t.library.as_str()), ("name", "More.pdf")])
            .body(endless_body())
            .send(),
    )
    .await
    .expect("the upload was not answered before its body was read")
    .unwrap();
    assert_eq!(answer.status(), StatusCode::INSUFFICIENT_STORAGE);

    let seat = FileClient::connect_as(&t.server, doc_id(&file), &t.root).await;
    let answer = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        t.server
            .http(Some(&t.root))
            .put(t.server.url(&format!("/api/docs/{file}/content")))
            .query(&[("base", t.head(&file).unwrap())])
            .header(SEAT, seat.token().unwrap())
            .body(endless_body())
            .send(),
    )
    .await
    .expect("the save was not answered before its body was read")
    .unwrap();
    assert_eq!(answer.status(), StatusCode::INSUFFICIENT_STORAGE);
}

/// Replacing an item the person cannot see answers as if nothing could be replaced there,
/// whatever kind of item it is.
#[tokio::test]
async fn replace_of_a_hidden_item_answers_not_found() {
    let t = team().await;
    let ann = t.server.user("Ann").await;
    let (status, body) = t
        .call(
            &t.root,
            Method::PUT,
            &format!("/api/nodes/{}/access", t.library),
            Some(json!({ "email": ann.email, "role": "editor" })),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, folder) = t
        .call(
            &t.root,
            Method::POST,
            &format!("/api/nodes/{}/children", t.library),
            Some(json!({ "kind": "folder", "name": "Secret.pdf" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{folder}");
    let file = t.file("Hidden.pdf", b"hidden").await;
    for node in [folder["id"].as_str().unwrap(), file.as_str()] {
        let (status, body) = t
            .call(
                &t.root,
                Method::POST,
                &format!("/api/nodes/{node}/inheritance"),
                Some(json!({ "inherit": false })),
            )
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
        let (status, body) = t
            .call(
                &t.root,
                Method::DELETE,
                &format!("/api/nodes/{node}/access/user/{}", ann.id.0),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    }

    for name in ["Secret.pdf", "Hidden.pdf"] {
        let started = session_as(
            &t.server,
            &ann,
            json!({ "parent": t.library, "name": name, "on_conflict": "replace" }),
            b"mine",
        )
        .await;
        let (status, body) = call(
            &t.server,
            &ann,
            Method::POST,
            &format!("/api/uploads/{}/finish", started["id"].as_str().unwrap()),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{name}: {body}");
    }
}

/// Names Windows reserves for devices get `_` after their first part, and the answer says so.
#[tokio::test]
async fn reserved_windows_names_cleaned() {
    let t = team().await;
    for (asked, cleaned) in [
        ("con.pdf", "con_.pdf"),
        ("LPT1.backup.pdf", "LPT1_.backup.pdf"),
        ("Nul .pdf", "Nul_.pdf"),
    ] {
        let (status, body) = t.upload(&t.library, asked, b"device").await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["name"], cleaned);
        assert_eq!(body["renamed"]["from"], asked);
    }
    let (status, body) = t.upload(&t.library, "Console.pdf", b"not a device").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert!(body["renamed"].is_null());
}
