//! The write worker holds a library's source lock only to check a row and to record what it
//! wrote, never while the content travels: a rename or move of another item of the library
//! goes ahead during a long upload (one `PUT` or a multipart upload), a listing meanwhile
//! leaves the file alone, a finish that cannot get the lock is recorded at the next attempt with
//! nothing sent twice, and the file being written still cannot be renamed or moved.
//!
//! The servers here run no workers; the worker is driven with `write_due(state, now)` in a task
//! of its own while the fake S3 holds a `PUT`, and listings with `sync_once`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::time::Duration;

use collab_core::ids::NodeId;
use common::fake_s3::{FakeS3, Stored};
use common::{TestServer, TestUser, spawn_server_config_without_workers};
use records::{PendingState, PendingWrite, Principal, S3_ENDPOINTS, object_tag};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use server::AppState;
use server::source_sync::sync_once;
use server::source_write::write_due;
use time::OffsetDateTime;
use tokio::task::JoinHandle;

const BUCKET: &str = "unlocked-bucket-3c9";
const ENDPOINT: &str = "unlocked-endpoint-7d2";
const KEY_ID: &str = "AKIDUNLOCKEDEXAMPLE61";
const SECRET: &str = "planted-unlocked-secret-2e4b-qq";
const PREFIX: &str = "team-unlocked/";

fn key(path: &str) -> String {
    format!("{PREFIX}{path}")
}

fn id_of(answer: &Value) -> NodeId {
    NodeId(answer["id"].as_str().unwrap().parse().unwrap())
}

async fn answer(response: reqwest::Response) -> (StatusCode, Value) {
    let status = response.status();
    let text = response.text().await.unwrap();
    let body = if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).unwrap_or(Value::String(text))
    };
    (status, body)
}

/// The tag a listing gives `stored`.
fn tag_of(stored: &Stored) -> String {
    object_tag(
        stored.bytes.len() as u64,
        stored.modified_s,
        &stored.etag,
        false,
    )
}

struct Env {
    server: TestServer,
    fake: FakeS3,
    state: AppState,
    admin: TestUser,
    owner: TestUser,
    site: NodeId,
}

impl Env {
    async fn new() -> Env {
        let fake = FakeS3::start().await;
        fake.bucket(BUCKET);
        fake.require_keys(KEY_ID, SECRET);
        let server = spawn_server_config_without_workers(|_| {}).await;
        let admin = server.admin("Unlocked admin").await;
        let owner = server.user("Site owner").await;
        let library = common::Site::library(&server).await;
        let db = server.records_db();
        let site = db.node(library).await.unwrap().unwrap().site;
        db.set_node_access(admin.id, site, Principal::User(owner.id), Some("owner"))
            .await
            .unwrap();
        let state = server.app_state().await;
        state
            .s3()
            .set_retry_wait_for_tests(Duration::from_millis(5));
        // A held request must not time out while the test acts around it.
        state
            .s3()
            .set_timeouts_for_tests(Duration::from_secs(30), Duration::from_secs(30));
        sqlx::query(
            "insert into settings (key, value, changed_at) values ($1, $2, now()) \
             on conflict (key) do update set value = excluded.value",
        )
        .bind(S3_ENDPOINTS)
        .bind(json!([{
            "name": ENDPOINT,
            "url": fake.origin(),
            "region": "us-east-1",
            "path_style": true,
            "allow": ["127.0.0.0/8"],
        }]))
        .execute(db.test_pool())
        .await
        .unwrap();
        Env {
            server,
            fake,
            state,
            admin,
            owner,
            site,
        }
    }

    fn db(&self) -> records::Db {
        self.server.records_db()
    }

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
        answer(request.send().await.unwrap()).await
    }

    /// A read-and-write library connected by the administrator, listed once.
    async fn library(&self, name: &str) -> NodeId {
        let (status, answer) = self
            .call(
                &self.admin,
                Method::POST,
                &format!("/api/sites/{}/libraries/s3", self.site.0),
                Some(json!({
                    "name": name,
                    "endpoint": ENDPOINT,
                    "bucket": BUCKET,
                    "prefix": PREFIX,
                    "access_key_id": KEY_ID,
                    "secret_access_key": SECRET,
                    "mode": "read_write",
                })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{answer}");
        let library = id_of(&answer);
        self.sync(library).await;
        library
    }

    /// Lists the library; whether the listing changed anything.
    async fn sync(&self, library: NodeId) -> bool {
        sync_once(&self.state, library, OffsetDateTime::now_utc())
            .await
            .unwrap()
            .is_some_and(|r| (r.added, r.changed, r.removed, r.moved) != (0, 0, 0, 0))
    }

    async fn children(&self, parent: NodeId) -> Vec<(String, NodeId)> {
        let mut found: Vec<(String, NodeId)> = self
            .db()
            .live_children(parent)
            .await
            .unwrap()
            .into_iter()
            .map(|node| (node.name, node.id))
            .collect();
        found.sort();
        found
    }

    async fn child(&self, parent: NodeId, name: &str) -> NodeId {
        self.children(parent)
            .await
            .into_iter()
            .find(|(child, _)| child == name)
            .unwrap_or_else(|| panic!("{name} is not there"))
            .1
    }

    async fn upload(&self, parent: NodeId, name: &str, bytes: &[u8], replace: bool) -> Value {
        let mut query = vec![("parent", parent.0.to_string()), ("name", name.to_owned())];
        if replace {
            query.push(("on_conflict", "replace".to_owned()));
        }
        let response = self
            .server
            .http(Some(&self.owner))
            .post(self.server.url("/api/docs/upload"))
            .query(&query)
            .body(bytes.to_vec())
            .send()
            .await
            .unwrap();
        let (status, body) = answer(response).await;
        assert!(status.is_success(), "{status} {body}");
        body
    }

    async fn new_file(&self, parent: NodeId, name: &str, bytes: &[u8]) -> NodeId {
        id_of(&self.upload(parent, name, bytes, false).await)
    }

    async fn replace(&self, parent: NodeId, name: &str, bytes: &[u8]) {
        self.upload(parent, name, bytes, true).await;
    }

    async fn change(&self, node: NodeId, body: Value) -> (StatusCode, Value) {
        self.call(
            &self.owner,
            Method::PATCH,
            &format!("/api/nodes/{}", node.0),
            Some(body),
        )
        .await
    }

    async fn rename(&self, node: NodeId, name: &str) -> (StatusCode, Value) {
        self.change(node, json!({ "name": name })).await
    }

    async fn move_to(&self, node: NodeId, parent: NodeId) -> (StatusCode, Value) {
        self.change(node, json!({ "parent": parent.0.to_string() }))
            .await
    }

    async fn pending(&self, node: NodeId) -> Option<PendingWrite> {
        self.db().pending_write(node).await.unwrap()
    }

    /// `write_due` at `now`, in a task of its own.
    fn write_in_background(&self, now: OffsetDateTime) -> JoinHandle<usize> {
        let state = self.state.clone();
        tokio::spawn(async move { write_due(&state, now).await })
    }

    fn object(&self, path: &str) -> Option<Stored> {
        self.fake.object(BUCKET, &key(path))
    }

    /// The writes the fake received for `path`: `(kind, detail)`.
    fn writes_of(&self, path: &str) -> Vec<(String, String)> {
        let key = key(path);
        self.fake
            .writes()
            .into_iter()
            .filter(|(_, k, _)| *k == key)
            .map(|(method, _, detail)| (method, detail))
            .collect()
    }

    async fn conflict_copies(&self) -> usize {
        self.db()
            .events_of_kind("source.conflict_copy")
            .await
            .unwrap()
            .len()
    }
}

fn assert_saving(answer: &(StatusCode, Value)) {
    assert_eq!(answer.0, StatusCode::CONFLICT, "{}", answer.1);
    assert_eq!(answer.1["error"], "source_item_saving", "{}", answer.1);
}

fn assert_done(answer: &(StatusCode, Value)) {
    assert_eq!(answer.0, StatusCode::OK, "{}", answer.1);
}

#[tokio::test]
async fn other_items_are_renamed_and_moved_while_a_file_is_sent() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("docs/a.txt"), b"original");
    env.fake.put(BUCKET, &key("other.txt"), b"other");
    env.fake.put(BUCKET, &key("box/x.txt"), b"x");
    env.fake.put(BUCKET, &key("dest/y.txt"), b"y");
    let library = env.library("Sending").await;
    let docs = env.child(library, "docs").await;
    let file = env.child(docs, "a.txt").await;
    let other = env.child(library, "other.txt").await;
    let boxed = env.child(library, "box").await;
    let dest = env.child(library, "dest").await;
    env.replace(docs, "a.txt", b"my edit").await;

    let held = env.fake.pause_next_put();
    let writer = env.write_in_background(OffsetDateTime::now_utc());
    held.reached().await;

    // The library's lock is free while the bytes travel.
    assert_done(&env.rename(other, "renamed.txt").await);
    assert_done(&env.move_to(boxed, dest).await);
    // The file being written, and a folder with it inside, are still kept as they are.
    assert_saving(&env.rename(file, "b.txt").await);
    assert_saving(&env.move_to(docs, dest).await);

    held.release();
    assert_eq!(writer.await.unwrap(), 1);
    let stored = env.object("docs/a.txt").unwrap();
    assert_eq!(stored.bytes, b"my edit");
    assert_eq!(
        env.writes_of("docs/a.txt"),
        [("PUT".to_owned(), "match".to_owned())]
    );
    assert!(env.pending(file).await.is_none());
    let item = env.db().source_item_of(file).await.unwrap().unwrap();
    assert_eq!(item.blob_sha.as_deref(), Some(tag_of(&stored).as_str()));
    assert_eq!(env.children(docs).await.len(), 1, "no copy");
    assert_eq!(env.conflict_copies().await, 0);
    assert!(env.object("renamed.txt").is_some());
    assert!(env.object("dest/box/x.txt").is_some());
    assert!(!env.sync(library).await, "the listing changed something");
}

#[tokio::test]
async fn other_items_are_renamed_and_moved_while_a_file_is_sent_in_parts() {
    let env = Env::new().await;
    env.fake.min_part_bytes(1);
    env.state.s3().set_single_put_max_for_tests(16);
    env.state.s3().set_part_bytes_for_tests(16);
    env.fake.put(BUCKET, &key("other.txt"), b"other");
    env.fake.put(BUCKET, &key("box/x.txt"), b"x");
    env.fake.put(BUCKET, &key("dest/y.txt"), b"y");
    let library = env.library("Parts").await;
    let other = env.child(library, "other.txt").await;
    let boxed = env.child(library, "box").await;
    let dest = env.child(library, "dest").await;
    let bytes: Vec<u8> = (0..60u8).collect();
    let node = env.new_file(library, "big.bin", &bytes).await;

    // The first part is held: the upload is open.
    let held = env.fake.pause_next_put();
    let writer = env.write_in_background(OffsetDateTime::now_utc());
    held.reached().await;
    assert_eq!(env.fake.open_multiparts().len(), 1);

    assert_done(&env.rename(other, "renamed.txt").await);
    assert_done(&env.move_to(boxed, dest).await);
    assert_saving(&env.rename(node, "huge.bin").await);
    assert_saving(&env.move_to(node, dest).await);

    held.release();
    assert_eq!(writer.await.unwrap(), 1);
    let stored = env.object("big.bin").unwrap();
    assert_eq!(stored.bytes, bytes);
    let writes = env.writes_of("big.bin");
    assert_eq!(writes.len(), 5, "{writes:?}");
    assert_eq!(
        writes.last().unwrap(),
        &("COMPLETE".to_owned(), "absent".to_owned())
    );
    assert!(env.pending(node).await.is_none());
    let item = env.db().source_item_of(node).await.unwrap().unwrap();
    assert_eq!(item.path, "big.bin");
    assert_eq!(item.blob_sha.as_deref(), Some(tag_of(&stored).as_str()));
    assert!(env.fake.open_multiparts().is_empty());
    assert_eq!(env.conflict_copies().await, 0);
    assert!(!env.sync(library).await, "the listing changed something");
}

#[tokio::test]
async fn a_listing_while_the_content_lands_leaves_the_file_alone() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("docs/a.txt"), b"original");
    let library = env.library("Listed").await;
    let docs = env.child(library, "docs").await;
    let file = env.child(docs, "a.txt").await;
    // Each write lands, then its answer is held while the library is listed.
    let fresh = env.new_file(library, "fresh.txt", b"fresh bytes").await;
    listed_while_landing(&env, library, "fresh.txt", fresh).await;
    env.replace(docs, "a.txt", b"my edit").await;
    listed_while_landing(&env, library, "docs/a.txt", file).await;

    // Neither adopted twice, nor taken for a change made in the store, nor copied.
    let names: Vec<String> = env
        .children(library)
        .await
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert_eq!(names, ["docs", "fresh.txt"]);
    assert_eq!(env.child(library, "fresh.txt").await, fresh);
    assert_eq!(env.children(docs).await, [("a.txt".to_owned(), file)]);
    assert_eq!(env.conflict_copies().await, 0);
    assert_eq!(env.writes_of("fresh.txt").len(), 1);
    assert_eq!(env.writes_of("docs/a.txt").len(), 1);
    assert!(!env.sync(library).await, "the listing changed something");
}

/// Writes the one due row, of `node` at `path`, holding the answer to its `PUT` (the content
/// has landed) while the library is listed.
async fn listed_while_landing(env: &Env, library: NodeId, path: &str, node: NodeId) {
    let held = env.fake.pause_after_next_put();
    let writer = env.write_in_background(OffsetDateTime::now_utc());
    held.reached().await;
    assert!(env.object(path).is_some(), "{path} has landed");
    sync_once(&env.state, library, OffsetDateTime::now_utc())
        .await
        .unwrap();
    assert!(env.pending(node).await.is_some(), "{path} is still pending");
    held.release();
    assert_eq!(writer.await.unwrap(), 1);
    assert!(env.pending(node).await.is_none(), "{path} is written");
}

/// Holds the next `PUT`'s answer, takes the library's lock while it is held, lets the writer
/// finish its transfer (it cannot record it), then gives the lock up.
async fn written_without_the_lock(env: &Env, library: NodeId, now: OffsetDateTime) {
    env.state
        .writes()
        .set_finish_wait_for_tests(Duration::from_millis(50));
    let held = env.fake.pause_after_next_put();
    let writer = env.write_in_background(now);
    held.reached().await;
    let lock = env
        .db()
        .try_lock_source(library)
        .await
        .unwrap()
        .expect("the writer does not hold the lock while it sends");
    held.release();
    assert_eq!(writer.await.unwrap(), 1);
    drop(lock);
}

#[tokio::test]
async fn a_tracked_file_whose_finish_waits_is_recorded_next_time_without_a_second_put() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("docs/a.txt"), b"original");
    let library = env.library("Tracked").await;
    let docs = env.child(library, "docs").await;
    let file = env.child(docs, "a.txt").await;
    env.replace(docs, "a.txt", b"my edit").await;

    let now = OffsetDateTime::now_utc();
    written_without_the_lock(&env, library, now).await;
    let stored = env.object("docs/a.txt").unwrap();
    assert_eq!(stored.bytes, b"my edit");
    // Still pending, held back without an attempt counted, based on what was written.
    let row = env.pending(file).await.expect("the finish waits");
    assert_ne!(row.state, PendingState::Failed);
    assert_eq!(row.attempts, 0);
    assert!(row.next_try_at.unwrap() > now);
    assert_eq!(row.base_tag.as_deref(), Some(tag_of(&stored).as_str()));
    // A listing meanwhile leaves it alone.
    sync_once(&env.state, library, OffsetDateTime::now_utc())
        .await
        .unwrap();
    assert!(env.pending(file).await.is_some());

    env.write_in_background(now + time::Duration::seconds(6))
        .await
        .unwrap();
    assert!(env.pending(file).await.is_none());
    assert_eq!(env.writes_of("docs/a.txt").len(), 1, "no second PUT");
    let item = env.db().source_item_of(file).await.unwrap().unwrap();
    assert_eq!(item.blob_sha.as_deref(), Some(tag_of(&stored).as_str()));
    assert_eq!(env.children(docs).await.len(), 1, "no copy");
    assert_eq!(env.conflict_copies().await, 0);
    assert!(!env.sync(library).await, "the listing changed something");
}

#[tokio::test]
async fn a_newer_save_while_a_finish_waits_is_based_on_what_was_written() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("docs/a.txt"), b"original");
    let library = env.library("Newer").await;
    let docs = env.child(library, "docs").await;
    let file = env.child(docs, "a.txt").await;
    env.replace(docs, "a.txt", b"first edit").await;

    let now = OffsetDateTime::now_utc();
    written_without_the_lock(&env, library, now).await;
    env.replace(docs, "a.txt", b"second edit").await;

    env.write_in_background(now + time::Duration::seconds(6))
        .await
        .unwrap();
    assert_eq!(env.object("docs/a.txt").unwrap().bytes, b"second edit");
    assert_eq!(
        env.writes_of("docs/a.txt"),
        [
            ("PUT".to_owned(), "match".to_owned()),
            ("PUT".to_owned(), "match".to_owned())
        ]
    );
    assert!(env.pending(file).await.is_none());
    assert_eq!(env.children(docs).await.len(), 1, "no copy");
    assert_eq!(env.conflict_copies().await, 0);
}

#[tokio::test]
async fn a_new_file_whose_finish_waits_is_recorded_next_time_without_a_second_put() {
    let env = Env::new().await;
    let library = env.library("New").await;
    let node = env.new_file(library, "notes.txt", b"my notes").await;

    let now = OffsetDateTime::now_utc();
    written_without_the_lock(&env, library, now).await;
    assert_eq!(env.object("notes.txt").unwrap().bytes, b"my notes");
    assert!(env.pending(node).await.is_some(), "the finish waits");
    // A listing meanwhile does not take the object for a new file of its own.
    sync_once(&env.state, library, OffsetDateTime::now_utc())
        .await
        .unwrap();
    assert_eq!(
        env.children(library).await,
        [("notes.txt".to_owned(), node)]
    );

    env.write_in_background(now + time::Duration::seconds(6))
        .await
        .unwrap();
    assert!(env.pending(node).await.is_none());
    assert_eq!(env.writes_of("notes.txt").len(), 1, "no second PUT");
    let item = env.db().source_item_of(node).await.unwrap().unwrap();
    assert_eq!(item.path, "notes.txt");
    assert_eq!(
        env.children(library).await,
        [("notes.txt".to_owned(), node)]
    );
    assert!(!env.sync(library).await, "the listing changed something");
}

#[tokio::test]
async fn a_discard_while_the_content_travels_leaves_nothing_half_recorded() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("docs/a.txt"), b"original");
    let library = env.library("Discarded").await;
    let docs = env.child(library, "docs").await;
    let file = env.child(docs, "a.txt").await;
    env.replace(docs, "a.txt", b"my edit").await;

    let held = env.fake.pause_next_put();
    let writer = env.write_in_background(OffsetDateTime::now_utc());
    held.reached().await;
    let (status, body) = env
        .call(
            &env.owner,
            Method::DELETE,
            &format!("/api/libraries/{}/source/writes/{}", library.0, file.0),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    held.release();
    writer.await.unwrap();

    // The bytes landed; nothing recorded them, and nothing was overwritten or copied.
    assert!(env.pending(file).await.is_none());
    assert_eq!(env.object("docs/a.txt").unwrap().bytes, b"my edit");
    assert_eq!(env.children(docs).await, [("a.txt".to_owned(), file)]);
    // The next listing takes them as a change made in the store.
    assert!(env.sync(library).await);
    assert_eq!(env.children(docs).await, [("a.txt".to_owned(), file)]);
    let item = env.db().source_item_of(file).await.unwrap().unwrap();
    assert_eq!(
        item.blob_sha.as_deref(),
        Some(tag_of(&env.object("docs/a.txt").unwrap()).as_str())
    );
    assert_eq!(env.conflict_copies().await, 0);
}
