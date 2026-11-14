//! Erasing purged content, copying files and folders, and bulk operations over HTTP: purged
//! content is erased only when nothing names it any more (another file, a copy, an old version
//! of another file, a save under way); a copy makes new items with the same bytes and the
//! destination's access; a bulk request answers each item in order. Link tokens are never
//! printed.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use collab_core::ids::{DocId, NodeId};
use common::{TestServer, TestUser, spawn_server, spawn_server_with};
use reqwest::{Method, StatusCode};
use revisions::{
    Author, BlobRef, Checkpoint, CheckpointError, GitError, Outcome, RevisionId, RevisionInfo,
    RevisionStore, Stored, TreeChange, TreeMove, TreeRecord,
};
use serde_json::{Map, Value, json};
use server::Checkpointer;
use uuid::Uuid;

/// A server with a site made by the administrator Root, and its library.
struct World {
    server: TestServer,
    root: TestUser,
    library: String,
}

async fn world() -> World {
    world_on(spawn_server().await).await
}

async fn world_on(server: TestServer) -> World {
    let root = server.admin("Root").await;
    let w = World {
        server,
        root,
        library: String::new(),
    };
    let (status, site) = w
        .call(
            &w.root,
            Method::POST,
            "/api/sites",
            Some(json!({ "name": "Team" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let site = site["id"].as_str().unwrap().to_owned();
    let (_, page) = w
        .call(
            &w.root,
            Method::GET,
            &format!("/api/nodes/{site}/children"),
            None,
        )
        .await;
    let library = page["items"][0]["id"].as_str().unwrap().to_owned();
    World { library, ..w }
}

impl World {
    async fn call(
        &self,
        user: &TestUser,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut request = self
            .server
            .http(Some(user))
            .request(method, self.server.url(path));
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

    async fn folder(&self, parent: &str, name: &str) -> String {
        let (status, body) = self
            .call(
                &self.root,
                Method::POST,
                &format!("/api/nodes/{parent}/children"),
                Some(json!({ "kind": "folder", "name": name })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        body["id"].as_str().unwrap().to_owned()
    }

    /// Uploads `bytes` as the file `name` in `parent`, as Root.
    async fn upload(&self, parent: &str, name: &str, bytes: &[u8]) -> String {
        let response = self
            .server
            .http(Some(&self.root))
            .post(self.server.url("/api/docs/upload"))
            .query(&[("parent", parent), ("name", name)])
            .body(bytes.to_vec())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let body: Value = response.json().await.unwrap();
        body["id"].as_str().unwrap().to_owned()
    }

    /// The current revision of `doc` in Git.
    fn head(&self, doc: &str) -> Option<String> {
        self.server
            .store()
            .read(doc_id(doc), None)
            .unwrap()
            .map(|stored| stored.revision.to_string())
    }

    /// Starts an upload session for a new version of `doc` holding `bytes` and sends them all;
    /// the session's id.
    async fn send_version(&self, doc: &str, bytes: &[u8]) -> String {
        let (status, session) = self
            .call(
                &self.root,
                Method::POST,
                "/api/uploads",
                Some(json!({
                    "doc": doc,
                    "size": bytes.len(),
                    "sha256": BlobRef::of(bytes).hex(),
                    "base": self.head(doc),
                })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{session}");
        let id = session["id"].as_str().unwrap().to_owned();
        let response = self
            .server
            .http(Some(&self.root))
            .put(self.server.url(&format!("/api/uploads/{id}")))
            .header(
                reqwest::header::CONTENT_RANGE,
                format!("bytes 0-{}/{}", bytes.len() - 1, bytes.len()),
            )
            .body(bytes.to_vec())
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success(), "{}", response.status());
        id
    }

    /// Finishes the upload session `id`.
    async fn finish(&self, id: &str) -> (StatusCode, Value) {
        self.call(
            &self.root,
            Method::POST,
            &format!("/api/uploads/{id}/finish"),
            None,
        )
        .await
    }

    /// Saves `bytes` as a new version of `doc`.
    async fn new_version(&self, doc: &str, bytes: &[u8]) {
        let id = self.send_version(doc, bytes).await;
        let (status, body) = self.finish(&id).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    /// The current bytes of `doc`, as `user` downloads them.
    async fn download(&self, user: &TestUser, doc: &str) -> (StatusCode, Vec<u8>) {
        let response = self
            .server
            .http(Some(user))
            .get(self.server.url(&format!("/api/docs/{doc}/content")))
            .send()
            .await
            .unwrap();
        let status = response.status();
        (status, response.bytes().await.unwrap().to_vec())
    }

    async fn delete(&self, node: &str) {
        let (status, body) = self
            .call(
                &self.root,
                Method::DELETE,
                &format!("/api/nodes/{node}"),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    }

    /// Deletes `node` and purges it as Root: to the bin, to its second stage, then out.
    async fn purge(&self, node: &str) {
        self.delete(node).await;
        for _ in 0..2 {
            let (status, body) = self
                .call(
                    &self.root,
                    Method::DELETE,
                    &format!("/api/bin/{node}"),
                    None,
                )
                .await;
            assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
        }
    }

    async fn copy(
        &self,
        user: &TestUser,
        node: &str,
        parent: &str,
        name: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut body = json!({ "parent": parent });
        if let Some(name) = name {
            body["name"] = json!(name);
        }
        self.call(
            user,
            Method::POST,
            &format!("/api/nodes/{node}/copy"),
            Some(body),
        )
        .await
    }

    async fn grant(&self, node: &str, user: &TestUser, role: &str) {
        let (status, body) = self
            .call(
                &self.root,
                Method::PUT,
                &format!("/api/nodes/{node}/access"),
                Some(json!({ "email": user.email, "role": role })),
            )
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    }

    /// The children of `node` Root sees, as (name, id).
    async fn children(&self, node: &str) -> Vec<(String, String)> {
        let (status, page) = self
            .call(
                &self.root,
                Method::GET,
                &format!("/api/nodes/{node}/children"),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{page}");
        page["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| {
                (
                    item["name"].as_str().unwrap().to_owned(),
                    item["id"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    }

    /// Whether the content store holds `bytes`.
    fn stored(&self, bytes: &[u8]) -> bool {
        self.server.blobs().contains(&BlobRef::of(bytes).sha256)
    }

    /// How many files the content store holds.
    fn stored_files(&self) -> usize {
        self.server.blobs().list().unwrap().len()
    }

    /// The number of commits on `main`.
    fn commits(&self) -> usize {
        let repo = gix::open(self.server.library()).unwrap();
        let tip = repo.find_reference("refs/heads/main").unwrap();
        repo.rev_walk([tip.id()]).all().unwrap().count()
    }

    /// The `blob_refs` rows naming versions of `doc`.
    async fn refs_of(&self, doc: &str) -> i64 {
        sqlx::query_scalar("select count(*) from blob_refs where doc_id = $1")
            .bind(uuid(doc))
            .fetch_one(self.server.records_db().test_pool())
            .await
            .unwrap()
    }

    /// The detail of the `node.purged` event of `node`.
    async fn purged_detail(&self, node: &str) -> Value {
        sqlx::query_scalar("select detail from events where kind = 'node.purged' and node_id = $1")
            .bind(uuid(node))
            .fetch_one(self.server.records_db().test_pool())
            .await
            .unwrap()
    }
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

/// Purged content is erased only once nothing names it: not while another file, a copy or an
/// old version of another file names the same bytes. Each purge says how many bytes it freed.
#[tokio::test]
async fn erasure_keeps_shared_bytes() {
    let w = world().await;
    let shared: &[u8] = b"the same bytes in three files";
    let old: &[u8] = b"an old version of C, the latest of D";
    let unique: &[u8] = b"bytes only E holds";
    let a = w.upload(&w.library, "A.xlsx", shared).await;
    let b = w.upload(&w.library, "B.xlsx", shared).await;
    let c = w.upload(&w.library, "C.xlsx", old).await;
    w.new_version(&c, b"C moved on").await;
    let d = w.upload(&w.library, "D.xlsx", old).await;
    let e = w.upload(&w.library, "E.xlsx", unique).await;
    let dest = w.folder(&w.library, "Copies").await;
    let (status, copied) = w.copy(&w.root, &a, &dest, None).await;
    assert_eq!(status, StatusCode::CREATED, "{copied}");
    let copy = copied["node"]["id"].as_str().unwrap().to_owned();

    // Another file and a copy still name the bytes of A.
    w.purge(&a).await;
    assert_eq!(w.refs_of(&a).await, 0);
    assert!(w.stored(shared));
    assert_eq!(w.purged_detail(&a).await["bytes_freed"], 0);
    w.purge(&b).await;
    assert!(w.stored(shared));
    // An old version of C names the bytes of D.
    w.purge(&d).await;
    assert!(w.stored(old));
    assert_eq!(w.purged_detail(&d).await["bytes_freed"], 0);
    let (status, bytes) = w.download(&w.root, &copy).await;
    assert_eq!((status, bytes.as_slice()), (StatusCode::OK, shared));

    // Nothing else names the bytes of E, nor, once the copy goes, those of A.
    w.purge(&e).await;
    assert!(!w.stored(unique));
    let detail = w.purged_detail(&e).await;
    assert_eq!(detail["bytes_freed"], unique.len());
    assert_eq!(detail["bytes"], unique.len());
    w.purge(&dest).await;
    assert!(!w.stored(shared));
    assert_eq!(w.purged_detail(&dest).await["bytes_freed"], shared.len());
    assert!(w.stored(old));
    assert!(w.stored(b"C moved on"));
}

/// Purging deletes the rows naming the purged files' versions, and closes the requests for
/// access still open on them.
#[tokio::test]
async fn purge_deletes_blob_refs_and_closes_requests() {
    let w = world().await;
    let bob = w.server.user("Bob").await;
    let doc = w.upload(&w.library, "Asked.xlsx", b"asked about").await;
    w.new_version(&doc, b"asked about, again").await;
    assert_eq!(w.refs_of(&doc).await, 2);
    let (status, body) = w
        .call(
            &bob,
            Method::POST,
            &format!("/api/nodes/{doc}/requests"),
            Some(json!({ "message": "may I?" })),
        )
        .await;
    assert!(status.is_success(), "{status} {body}");
    w.delete(&doc).await;
    // Deleting closed it; it is opened again as if it had been made after.
    let pool = w.server.records_db().test_pool().clone();
    sqlx::query(
        "update access_requests set decided_at = null, decided_by = null, decision = null \
         where node_id = $1",
    )
    .bind(uuid(&doc))
    .execute(&pool)
    .await
    .unwrap();
    for _ in 0..2 {
        let (status, _) = w
            .call(&w.root, Method::DELETE, &format!("/api/bin/{doc}"), None)
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }
    assert_eq!(w.refs_of(&doc).await, 0);
    let open: i64 = sqlx::query_scalar(
        "select count(*) from access_requests where node_id = $1 and decided_at is null",
    )
    .bind(uuid(&doc))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(open, 0);
    let closed: Option<String> =
        sqlx::query_scalar("select decision from access_requests where node_id = $1")
            .bind(uuid(&doc))
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(closed.as_deref(), Some("closed"));
    assert!(!w.stored(b"asked about"));
    assert!(!w.stored(b"asked about, again"));
}

/// Holds the checkpoints of one file name (or the loads of one document, by id), once armed,
/// until let go; and fails every commit of several new documents while `fail_new` is set.
#[derive(Default)]
struct Gate {
    armed: AtomicBool,
    fail_new: AtomicBool,
    name: Mutex<String>,
    /// Whether a checkpoint is waiting, and whether it may go on.
    state: Mutex<(bool, bool)>,
    changed: Condvar,
}

impl Gate {
    fn arm(&self, name: &str) {
        name.clone_into(&mut self.name.lock().unwrap());
        self.armed.store(true, Ordering::SeqCst);
    }

    fn pass(&self, name: &str) {
        if !self.armed.load(Ordering::SeqCst) || *self.name.lock().unwrap() != name {
            return;
        }
        let mut state = self.state.lock().unwrap();
        state.0 = true;
        self.changed.notify_all();
        while !state.1 {
            state = self.changed.wait(state).unwrap();
        }
    }

    fn waiting(&self) -> bool {
        self.state.lock().unwrap().0
    }

    fn open(&self) {
        self.state.lock().unwrap().1 = true;
        self.changed.notify_all();
    }
}

/// A store whose checkpoints wait at a [`Gate`] before they are written.
struct Gated(RevisionStore, Arc<Gate>);

impl Checkpointer for Gated {
    fn write(&self, c: &Checkpoint<'_>) -> Result<Outcome, CheckpointError> {
        self.1.pass(c.file_name);
        self.0.checkpoint(c)
    }

    fn load(&self, doc: DocId) -> Result<Option<Stored>, CheckpointError> {
        self.1.pass(&doc.0.to_string());
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
        if self.1.fail_new.load(Ordering::SeqCst) {
            return Err(CheckpointError::Git(GitError::new("disk on fire")));
        }
        self.0.checkpoint_new(author, message, checkpoints)
    }
}

/// A world whose store waits at, or fails through, `gate`.
async fn gated_world(gate: &Arc<Gate>) -> World {
    let held = Arc::clone(gate);
    let server = spawn_server_with(move |path: &Path| {
        Arc::new(Gated(RevisionStore::open(path).unwrap(), held)) as Arc<dyn Checkpointer>
    })
    .await;
    world_on(server).await
}

/// A copy whose versions cannot be committed after its items were is taken back: the answer
/// is `503`, the copies are tombstones (in no list and no bin), no `node.copied` event tells
/// of them, and no claim is left.
#[tokio::test]
async fn failed_versions_commit_takes_the_copy_back() {
    let gate = Arc::new(Gate::default());
    let w = gated_world(&gate).await;
    let plans = w.folder(&w.library, "Plans").await;
    w.upload(&plans, "A.xlsx", b"to copy, in vain").await;
    let dest = w.folder(&w.library, "Dest").await;
    gate.fail_new.store(true, Ordering::SeqCst);

    let (status, body) = w.copy(&w.root, &plans, &dest, None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(!body.to_string().contains("disk on fire"));
    assert_eq!(w.children(&dest).await.len(), 0);
    for stage in ["first", "second"] {
        let site = w
            .server
            .records_db()
            .node(node_id(&dest))
            .await
            .unwrap()
            .unwrap()
            .site;
        let (status, page) = w
            .call(
                &w.root,
                Method::GET,
                &format!("/api/sites/{}/bin?stage={stage}", site.0),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(page["items"].as_array().unwrap().len(), 0, "{page}");
    }
    let pool = w.server.records_db().test_pool().clone();
    let copied: i64 = sqlx::query_scalar("select count(*) from events where kind = 'node.copied'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(copied, 0);
    let rows: Vec<(Uuid, bool)> =
        sqlx::query_as("select id, purged_at is not null from nodes where parent_id = $1")
            .bind(uuid(&dest))
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].1, "the copy is a tombstone");
    let record = w
        .server
        .store()
        .read_tree_record(rows[0].0)
        .unwrap()
        .unwrap();
    assert!(record.purged.is_some());
    let pending: i64 = sqlx::query_scalar("select count(*) from blob_refs where pending")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(pending, 0);

    // Once the store works again, the same copy goes through.
    gate.fail_new.store(false, Ordering::SeqCst);
    let (status, body) = w.copy(&w.root, &plans, &dest, None).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["node"]["name"], "Plans");
}

/// Plans, which Bob may view, with Inner (Bob may view it too) and Secret (Bob may not) in it,
/// and Dest, where Bob may add.
struct Shared {
    plans: String,
    inner: String,
    secret: String,
    dest: String,
    bob: TestUser,
}

async fn shared(w: &World) -> Shared {
    let bob = w.server.user("Bob").await;
    let plans = w.folder(&w.library, "Plans").await;
    w.upload(&plans, "Top.xlsx", b"top").await;
    let inner = w.folder(&plans, "Inner").await;
    w.upload(&inner, "In.xlsx", b"inner").await;
    let secret = w.folder(&plans, "Secret").await;
    w.upload(&secret, "S.xlsx", b"secret").await;
    let dest = w.folder(&w.library, "Dest").await;
    w.grant(&plans, &bob, "viewer").await;
    w.grant(&dest, &bob, "editor").await;
    w.unshare(&secret, &bob).await;
    Shared {
        plans,
        inner,
        secret,
        dest,
        bob,
    }
}

impl World {
    /// Removes `user`'s own assignment on `node` (breaking its inheritance first).
    async fn unshare(&self, node: &str, user: &TestUser) {
        let (status, body) = self
            .call(
                &self.root,
                Method::DELETE,
                &format!("/api/nodes/{node}/access/user/{}", user.id.0),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    }
}

/// A folder's copy leaves out what the copier may not view.
#[tokio::test]
async fn folder_copy_skips_what_you_cannot_view() {
    let w = world().await;
    let s = shared(&w).await;
    let (status, _) = w
        .call(
            &s.bob,
            Method::GET,
            &format!("/api/nodes/{}", s.secret),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, copied) = w.copy(&s.bob, &s.plans, &s.dest, None).await;
    assert_eq!(status, StatusCode::CREATED, "{copied}");
    let copy = copied["node"]["id"].as_str().unwrap().to_owned();
    let names: Vec<String> = w
        .children(&copy)
        .await
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    assert_eq!(names, ["Inner", "Top.xlsx"]);
    assert_eq!(
        copied_items(&w, &copy).await,
        4,
        "Plans, Inner, In.xlsx and Top.xlsx"
    );
}

/// The `items` the `node.copied` event of `node` counts.
async fn copied_items(w: &World, node: &str) -> i64 {
    let detail: Value =
        sqlx::query_scalar("select detail from events where kind = 'node.copied' and node_id = $1")
            .bind(uuid(node))
            .fetch_one(w.server.records_db().test_pool())
            .await
            .unwrap();
    detail["items"].as_i64().unwrap()
}

/// View of an item inside a folder, lost while the folder's copy is being planned, is judged
/// again when the copy is applied: the copy is refused and makes nothing.
#[tokio::test]
async fn view_lost_mid_copy_is_honoured() {
    let gate = Arc::new(Gate::default());
    let w = gated_world(&gate).await;
    let s = shared(&w).await;
    let inside = w.children(&s.inner).await[0].1.clone();
    let commits = w.commits();
    gate.arm(&inside);
    let copying = {
        let http = w.server.http(Some(&s.bob));
        let url = w.server.url(&format!("/api/nodes/{}/copy", s.plans));
        let body = json!({ "parent": s.dest });
        tokio::spawn(async move { http.post(url).json(&body).send().await.unwrap().status() })
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !gate.waiting() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the copy never loaded In.xlsx"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    w.unshare(&s.inner, &s.bob).await;
    gate.open();
    let status = copying.await.unwrap();
    assert!(
        matches!(status, StatusCode::NOT_FOUND | StatusCode::FORBIDDEN),
        "{status}"
    );
    assert_eq!(w.children(&s.dest).await.len(), 0);
    let pending: i64 = sqlx::query_scalar("select count(*) from blob_refs where pending")
        .fetch_one(w.server.records_db().test_pool())
        .await
        .unwrap();
    assert_eq!(pending, 0);
    // The refused change was taken back in Git; nothing new is left there.
    let tree = w.server.store().read_tree().unwrap();
    assert!(
        tree.iter()
            .all(|r| r.parent != Some(uuid(&s.dest)) || r.purged.is_some())
    );
    assert!(w.commits() >= commits);
}

/// The sweeper walks the content store's files at most hourly; content with a row nothing names
/// is erased on every sweep.
#[tokio::test]
async fn sweep_walks_the_store_hourly() {
    let w = world().await;
    let now = time::OffsetDateTime::now_utc();
    let first = w.server.blob(b"loose, swept on the first walk");
    let state = w.server.state().await;
    server::sweep_once(state, now + time::Duration::days(2)).await;
    assert!(!w.stored(b"loose, swept on the first walk"));

    let second = w.server.blob(b"loose, waits for the next walk");
    server::sweep_once(
        state,
        now + time::Duration::days(2) + time::Duration::minutes(10),
    )
    .await;
    assert!(
        w.stored(b"loose, waits for the next walk"),
        "walked again within the hour"
    );
    // Content with a row is found without a walk.
    let db = w.server.records_db();
    let withdrawn = w.server.blob(b"claimed, then withdrawn");
    let doc = DocId(Uuid::new_v4());
    let claim = db.claim_blob(withdrawn, doc).await.unwrap();
    db.settle_claim(claim, None).await.unwrap();
    server::sweep_once(
        state,
        now + time::Duration::days(2) + time::Duration::minutes(20),
    )
    .await;
    assert!(!w.stored(b"claimed, then withdrawn"));
    assert!(w.stored(b"loose, waits for the next walk"));

    server::sweep_once(
        state,
        now + time::Duration::days(2) + time::Duration::hours(2),
    )
    .await;
    assert!(!w.stored(b"loose, waits for the next walk"));
    let _ = (first, second);
}

/// A bulk copy or restore says when an item's name had to change.
#[tokio::test]
async fn bulk_results_say_renamed() {
    let w = world().await;
    let a = w.upload(&w.library, "A.xlsx", b"renamed in bulk").await;
    let (_, results) = w
        .call(
            &w.root,
            Method::POST,
            "/api/bulk",
            Some(json!({ "op": "copy", "items": [a], "parent": w.library })),
        )
        .await;
    assert_eq!(
        results[0]["renamed"],
        json!({ "from": "A.xlsx", "to": "A (2).xlsx" }),
        "{results}"
    );
    w.delete(&a).await;
    let copy = results[0]["new_id"].as_str().unwrap().to_owned();
    let (status, _) = w
        .call(
            &w.root,
            Method::PATCH,
            &format!("/api/nodes/{copy}"),
            Some(json!({ "name": "A.xlsx" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (_, results) = w
        .call(
            &w.root,
            Method::POST,
            "/api/bulk",
            Some(json!({ "op": "restore", "items": [a] })),
        )
        .await;
    assert_eq!(results[0]["ok"], true, "{results}");
    assert_eq!(
        results[0]["renamed"],
        json!({ "from": "A.xlsx", "to": "A (2).xlsx" }),
        "{results}"
    );
}

/// A save of the same bytes as a file being purged, held between its claim and its commit,
/// keeps those bytes: the purge erases nothing, and the save then names them.
#[tokio::test]
async fn purge_racing_a_save_of_the_same_bytes() {
    let gate = Arc::new(Gate::default());
    let held = Arc::clone(&gate);
    let server = spawn_server_with(move |path: &Path| {
        Arc::new(Gated(RevisionStore::open(path).unwrap(), held)) as Arc<dyn Checkpointer>
    })
    .await;
    let w = world_on(server).await;
    let same: &[u8] = b"bytes saved while purged";
    let a = w.upload(&w.library, "A.xlsx", same).await;
    let b = w.upload(&w.library, "B.xlsx", b"B before").await;
    w.delete(&a).await;

    // B's new version, the same bytes as A, is claimed and waits before its commit.
    let session = w.send_version(&b, same).await;
    gate.arm("B.xlsx");
    let finishing = {
        let http = w.server.http(Some(&w.root));
        let url = w.server.url(&format!("/api/uploads/{session}/finish"));
        tokio::spawn(async move { http.post(url).send().await.unwrap().status() })
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !gate.waiting() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the save never reached its commit"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    for _ in 0..2 {
        let (status, _) = w
            .call(&w.root, Method::DELETE, &format!("/api/bin/{a}"), None)
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }
    assert_eq!(w.refs_of(&a).await, 0);
    assert!(
        w.stored(same),
        "the purge erased bytes a save was about to name"
    );
    assert_eq!(w.purged_detail(&a).await["bytes_freed"], 0);

    gate.open();
    assert_eq!(finishing.await.unwrap(), StatusCode::OK);
    assert!(w.stored(same));
    let (status, bytes) = w.download(&w.root, &b).await;
    assert_eq!((status, bytes.as_slice()), (StatusCode::OK, same));
    // A sweep a day on erases nothing a version names.
    server_sweep(&w, 2).await;
    assert!(w.stored(same));
}

/// Runs the sweeper as of `days` days from now.
async fn server_sweep(w: &World, days: i64) {
    let state = w.server.state().await;
    server::sweep_once(
        state,
        time::OffsetDateTime::now_utc() + time::Duration::days(days),
    )
    .await;
}

/// Copying a file or a folder makes new items with new ids holding the same bytes, stores no
/// new content, takes the destination's access (no permissions or links of the original), and
/// costs two commits for a folder. A name taken there gets ` (2)`, and refused characters
/// become `_`; the answer says so.
#[tokio::test]
async fn copy_file_and_folder() {
    let w = world().await;
    let bob = w.server.user("Bob").await;
    let plans = w.folder(&w.library, "Plans").await;
    let a = w.upload(&plans, "A.xlsx", b"bytes of A").await;
    w.new_version(&a, b"bytes of A, again").await;
    let sub = w.folder(&plans, "Sub").await;
    let b = w.upload(&sub, "B.xlsx", b"bytes of B").await;
    // Plans has permissions of its own: Bob may view it. A has a link.
    w.grant(&plans, &bob, "viewer").await;
    let (status, _) = w
        .call(
            &w.root,
            Method::POST,
            &format!("/api/nodes/{a}/links"),
            Some(json!({ "kind": "signed_in", "role": "viewer" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let dest = w.folder(&w.library, "Archive").await;

    let commits = w.commits();
    let files = w.stored_files();
    let (status, copied) = w.copy(&w.root, &plans, &dest, None).await;
    assert_eq!(status, StatusCode::CREATED, "{copied}");
    assert_eq!(
        w.commits(),
        commits + 2,
        "one tree commit, one for the versions"
    );
    assert_eq!(w.stored_files(), files, "no new content");
    assert_eq!(copied["node"]["name"], "Plans");
    assert_eq!(copied["node"]["parent"], dest.as_str());
    assert!(copied["renamed"].is_null());
    let plans_copy = copied["node"]["id"].as_str().unwrap().to_owned();
    assert_ne!(plans_copy, plans);

    let inside = w.children(&plans_copy).await;
    let names: Vec<&str> = inside.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["Sub", "A.xlsx"]);
    let a_copy = inside[1].1.clone();
    let sub_copy = inside[0].1.clone();
    assert_ne!(a_copy, a);
    let b_copy = w.children(&sub_copy).await[0].1.clone();
    assert_ne!(b_copy, b);
    let (status, bytes) = w.download(&w.root, &a_copy).await;
    assert_eq!(
        (status, bytes.as_slice()),
        (StatusCode::OK, &b"bytes of A, again"[..])
    );
    let (_, bytes) = w.download(&w.root, &b_copy).await;
    assert_eq!(bytes, b"bytes of B");
    // The latest version only, credited to who copied it.
    let history = w.server.store().history(doc_id(&a_copy)).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].authors[0].id, w.root.id.0.to_string());
    assert_eq!(w.refs_of(&a_copy).await, 1);

    // The destination's access: Bob, who may view Plans, sees nothing of the copy.
    let db = w.server.records_db();
    assert!(
        db.node(node_id(&plans))
            .await
            .unwrap()
            .unwrap()
            .unique_scope
    );
    assert!(
        !db.node(node_id(&plans_copy))
            .await
            .unwrap()
            .unwrap()
            .unique_scope
    );
    let (status, _) = w
        .call(&bob, Method::GET, &format!("/api/nodes/{plans_copy}"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = w.download(&bob, &a_copy).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, links) = w
        .call(
            &w.root,
            Method::GET,
            &format!("/api/nodes/{a_copy}/links"),
            None,
        )
        .await;
    assert_eq!(links.as_array().unwrap().len(), 0, "{links}");

    // Events name the source.
    let from: Vec<(Uuid, Value)> =
        sqlx::query_as("select node_id, detail from events where kind = 'node.copied' order by id")
            .fetch_all(db.test_pool())
            .await
            .unwrap();
    assert!(
        from.iter()
            .any(|(n, d)| *n == uuid(&plans_copy) && d["from"] == plans.as_str())
    );
    assert!(
        from.iter()
            .any(|(n, d)| *n == uuid(&a_copy) && d["from"] == a.as_str())
    );

    // A file copied where its name is taken keeps both; a refused character becomes `_`.
    let commits = w.commits();
    let (status, copied) = w.copy(&w.root, &a, &plans, None).await;
    assert_eq!(status, StatusCode::CREATED, "{copied}");
    assert_eq!(w.commits(), commits + 2);
    assert_eq!(copied["node"]["name"], "A (2).xlsx");
    assert_eq!(copied["renamed"]["from"], "A.xlsx");
    assert_eq!(copied["renamed"]["to"], "A (2).xlsx");
    let (status, copied) = w.copy(&w.root, &a, &plans, Some("x:y.xlsx")).await;
    assert_eq!(status, StatusCode::CREATED, "{copied}");
    assert_eq!(copied["node"]["name"], "x_y.xlsx");
    assert_eq!(copied["renamed"]["from"], "x:y.xlsx");
    assert_eq!(w.stored_files(), files, "still no new content");
    // Nothing is copied into itself.
    let (status, body) = w.copy(&w.root, &plans, &sub, None).await;
    assert_eq!(
        (status, body),
        (StatusCode::BAD_REQUEST, json!({ "error": "into_itself" }))
    );
}

/// Copying a file whose latest version was saved before the content store writes its bytes to
/// the store, and the copy's version points at them.
#[tokio::test]
async fn copy_of_an_old_inline_file_writes_its_blob() {
    let w = world().await;
    let ann = w.server.user("Ann").await;
    let doc = DocId(Uuid::new_v4());
    w.server.create_file_document(doc, &ann).await;
    let name = format!("{}.xlsx", doc.0);
    let old: &[u8] = b"bytes inline in Git, from before the content store";
    let author = Author {
        id: "someone-else".into(),
        name: "Someone Else".into(),
    };
    w.server
        .store()
        .checkpoint_inline(
            &Checkpoint {
                doc,
                file_name: &name,
                blob: BlobRef::of(b""),
                base: None,
                authors: &[author],
                fields: &Map::new(),
            },
            old,
        )
        .unwrap();
    assert!(!w.stored(old));

    let home = w.server.home_of(&ann).await.0.to_string();
    let (status, copied) = w
        .call(
            &ann,
            Method::POST,
            &format!("/api/nodes/{}/copy", doc.0),
            Some(json!({ "parent": home, "name": "Copy.xlsx" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{copied}");
    let copy = copied["node"]["id"].as_str().unwrap().to_owned();
    assert!(w.stored(old));
    let stored = w.server.store().read(doc_id(&copy), None).unwrap().unwrap();
    assert!(matches!(stored.content, revisions::Content::Blob(blob) if blob == BlobRef::of(old)));
    let (status, bytes) = w.download(&ann, &copy).await;
    assert_eq!((status, bytes.as_slice()), (StatusCode::OK, old));
    assert_eq!(w.refs_of(&copy).await, 1);
}

/// A copy needs `view` on what is copied and `add` where it goes.
#[tokio::test]
async fn copy_needs_view_and_add() {
    let w = world().await;
    let bob = w.server.user("Bob").await;
    let source = w.folder(&w.library, "Source").await;
    let file = w.upload(&source, "F.xlsx", b"to copy").await;
    let readable = w.folder(&w.library, "Readable").await;
    let writable = w.folder(&w.library, "Writable").await;
    w.grant(&readable, &bob, "viewer").await;
    w.grant(&writable, &bob, "editor").await;

    // No access to the file: as if it were not there.
    let (status, _) = w.copy(&bob, &file, &writable, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    w.grant(&source, &bob, "viewer").await;
    // Only view where it would go.
    let (status, _) = w.copy(&bob, &file, &readable, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // No access where it would go.
    let (status, _) = w.copy(&bob, &file, &w.library, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(w.children(&readable).await.len(), 0);

    let (status, copied) = w.copy(&bob, &file, &writable, None).await;
    assert_eq!(status, StatusCode::CREATED, "{copied}");
    let copy = copied["node"]["id"].as_str().unwrap();
    let (status, bytes) = w.download(&bob, copy).await;
    assert_eq!(
        (status, bytes.as_slice()),
        (StatusCode::OK, &b"to copy"[..])
    );
}

/// A bulk request does each item as the single-item route would, and answers each in the order
/// given, failures included.
#[tokio::test]
async fn bulk_reports_each_item() {
    let w = world().await;
    let from = w.folder(&w.library, "From").await;
    let to = w.folder(&w.library, "To").await;
    let a = w.upload(&from, "A.xlsx", b"bulk a").await;
    let b = w.upload(&from, "B.xlsx", b"bulk b").await;
    let missing = Uuid::new_v4().to_string();
    let bulk = |op: &str, items: Vec<&str>, parent: Option<&str>| {
        let mut body = json!({ "op": op, "items": items });
        if let Some(parent) = parent {
            body["parent"] = json!(parent);
        }
        body
    };

    let (status, results) = w
        .call(
            &w.root,
            Method::POST,
            "/api/bulk",
            Some(bulk("copy", vec![&a, &missing, "not an id", &b], Some(&to))),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{results}");
    let results = results.as_array().unwrap();
    assert_eq!(results.len(), 4);
    assert_eq!(results[0]["id"], a.as_str());
    assert_eq!(results[0]["ok"], true);
    assert!(results[0]["new_id"].is_string());
    assert_eq!(
        results[1],
        json!({ "id": missing, "ok": false, "error": "not_found" })
    );
    assert_eq!(
        results[2],
        json!({ "id": "not an id", "ok": false, "error": "invalid_request" })
    );
    assert_eq!(results[3]["ok"], true);
    let names: Vec<String> = w.children(&to).await.into_iter().map(|(n, _)| n).collect();
    assert_eq!(names, ["A.xlsx", "B.xlsx"]);

    // Moving A where the copy of A is: refused for its name; B goes.
    let sub = w.folder(&from, "Sub").await;
    let (_, results) = w
        .call(
            &w.root,
            Method::POST,
            "/api/bulk",
            Some(bulk("move", vec![&a, &b, &sub], Some(&to))),
        )
        .await;
    let oks: Vec<&Value> = results
        .as_array()
        .unwrap()
        .iter()
        .map(|r| &r["ok"])
        .collect();
    assert_eq!(
        oks,
        [&json!(false), &json!(false), &json!(true)],
        "{results}"
    );
    assert_eq!(results[0]["error"], "name_taken");

    let (_, results) = w
        .call(
            &w.root,
            Method::POST,
            "/api/bulk",
            Some(bulk("delete", vec![&a, &missing, &b], None)),
        )
        .await;
    let oks: Vec<&Value> = results
        .as_array()
        .unwrap()
        .iter()
        .map(|r| &r["ok"])
        .collect();
    assert_eq!(
        oks,
        [&json!(true), &json!(false), &json!(true)],
        "{results}"
    );
    assert_eq!(w.children(&from).await.len(), 0);

    let (_, results) = w
        .call(
            &w.root,
            Method::POST,
            "/api/bulk",
            Some(bulk("restore", vec![&b, &a, &b], None)),
        )
        .await;
    let oks: Vec<&Value> = results
        .as_array()
        .unwrap()
        .iter()
        .map(|r| &r["ok"])
        .collect();
    // B is no longer in the bin the second time.
    assert_eq!(
        oks,
        [&json!(true), &json!(true), &json!(false)],
        "{results}"
    );
    assert_eq!(results[2]["error"], "not_found");
    let names: Vec<String> = w
        .children(&from)
        .await
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    assert_eq!(names, ["A.xlsx", "B.xlsx"]);

    // A move or copy needs a destination.
    let (status, _) = w
        .call(
            &w.root,
            Method::POST,
            "/api/bulk",
            Some(bulk("move", vec![&a], None)),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

/// A bulk request takes up to 500 items; more are refused, and nothing is done.
#[tokio::test]
async fn bulk_limit_500() {
    let w = world().await;
    let a = w.upload(&w.library, "Kept.xlsx", b"kept").await;
    let mut items: Vec<String> = (0..500).map(|_| Uuid::new_v4().to_string()).collect();
    items[0].clone_from(&a);
    items.push(Uuid::new_v4().to_string());
    let (status, body) = w
        .call(
            &w.root,
            Method::POST,
            "/api/bulk",
            Some(json!({ "op": "delete", "items": items })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(w.children(&w.library).await.len(), 1);

    items.pop();
    let (status, results) = w
        .call(
            &w.root,
            Method::POST,
            "/api/bulk",
            Some(json!({ "op": "delete", "items": items })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let results = results.as_array().unwrap();
    assert_eq!(results.len(), 500);
    assert_eq!(results[0]["ok"], true);
    assert!(results[1..].iter().all(|r| r["error"] == "not_found"));
    assert_eq!(w.children(&w.library).await.len(), 0);
}
