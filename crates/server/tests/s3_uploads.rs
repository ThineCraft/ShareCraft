//! Uploads into a read-and-write S3 library: the content is accepted and held in ShareCraft,
//! scanned, then written to the bucket by the write worker with a conditional `PUT`. A change
//! made in the bucket meanwhile is kept, and the upload becomes a named copy beside it; a new
//! file whose key appeared meanwhile gets a numbered name. Nothing is ever overwritten.
//!
//! The worker is driven with `write_due(state, now)`; the servers here run no workers.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;
#[path = "../../../xtask/src/fake_icap.rs"]
#[allow(dead_code)]
mod fake_icap;

use std::time::Duration;

use collab_core::ids::NodeId;
use common::fake_s3::{FakeS3, Stored};
use common::{StoppableServer, TestServer, TestUser, spawn_server_config_without_workers};
use fake_icap::{Behaviour, FakeIcap, eicar};
use records::test_support::TestDb;
use records::{
    PendingState, PendingWrite, Principal, S3_ENDPOINTS, SCANNER_URL, ScanState, WriteState,
};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use server::AppState;
use server::source_sync::sync_once;
use server::source_write::{sibling_name, write_due};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

const BUCKET: &str = "uploads-bucket-6d3";
const ENDPOINT: &str = "uploads-endpoint-1b8";
const KEY_ID: &str = "AKIDUPLOADSEXAMPLE52";
const SECRET: &str = "planted-uploads-secret-7f1a-qq";
const PREFIX: &str = "team-uploads/";
const OWNER: &str = "Site owner";

fn key(path: &str) -> String {
    format!("{PREFIX}{path}")
}

fn sha_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn id_of(answer: &Value) -> NodeId {
    NodeId(answer["id"].as_str().unwrap().parse().unwrap())
}

fn endpoints(fake: &FakeS3) -> Value {
    json!([{
        "name": ENDPOINT,
        "url": fake.origin(),
        "region": "us-east-1",
        "path_style": true,
        "allow": ["127.0.0.0/8"],
    }])
}

async fn approve(db: &records::Db, fake: &FakeS3) {
    sqlx::query(
        "insert into settings (key, value, changed_at) values ($1, $2, now()) \
         on conflict (key) do update set value = excluded.value",
    )
    .bind(S3_ENDPOINTS)
    .bind(endpoints(fake))
    .execute(db.test_pool())
    .await
    .unwrap();
}

fn connect_body(name: &str, mode: &str, prefix: &str) -> Value {
    json!({
        "name": name,
        "endpoint": ENDPOINT,
        "bucket": BUCKET,
        "prefix": prefix,
        "access_key_id": KEY_ID,
        "secret_access_key": SECRET,
        "mode": mode,
    })
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
        let admin = server.admin("Uploads admin").await;
        let owner = server.user(OWNER).await;
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
        state
            .s3()
            .set_timeouts_for_tests(Duration::from_secs(2), Duration::from_secs(2));
        approve(&db, &fake).await;
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

    /// A library connected by the administrator in `mode`, its bucket listed once.
    async fn library(&self, name: &str, mode: &str) -> NodeId {
        self.library_at(name, mode, PREFIX).await
    }

    /// A library connected by the administrator in `mode` on `prefix`, its bucket listed once.
    async fn library_at(&self, name: &str, mode: &str, prefix: &str) -> NodeId {
        let (status, answer) = self
            .call(
                &self.admin,
                Method::POST,
                &format!("/api/sites/{}/libraries/s3", self.site.0),
                Some(connect_body(name, mode, prefix)),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{answer}");
        let library = id_of(&answer);
        sync_once(&self.state, library, OffsetDateTime::now_utc())
            .await
            .unwrap();
        library
    }

    async fn sync(&self, library: NodeId) {
        sync_once(&self.state, library, OffsetDateTime::now_utc())
            .await
            .unwrap();
    }

    async fn child(&self, parent: NodeId, name: &str) -> NodeId {
        self.children(parent)
            .await
            .into_iter()
            .find(|(child, _)| child == name)
            .unwrap_or_else(|| panic!("{name} is not there"))
            .1
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

    async fn upload(
        &self,
        user: &TestUser,
        parent: NodeId,
        name: &str,
        bytes: &[u8],
        on_conflict: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut query = vec![("parent", parent.0.to_string()), ("name", name.to_owned())];
        if let Some(choice) = on_conflict {
            query.push(("on_conflict", choice.to_owned()));
        }
        let response = self
            .server
            .http(Some(user))
            .post(self.server.url("/api/docs/upload"))
            .query(&query)
            .body(bytes.to_vec())
            .send()
            .await
            .unwrap();
        answer(response).await
    }

    /// Uploads a new file, which must be accepted; its node.
    async fn new_file(&self, parent: NodeId, name: &str, bytes: &[u8]) -> NodeId {
        let (status, body) = self.upload(&self.owner, parent, name, bytes, None).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["saving"], true, "{body}");
        id_of(&body)
    }

    /// Replaces the file `name` in `parent`, which must be accepted.
    async fn replace(&self, parent: NodeId, name: &str, bytes: &[u8]) -> Value {
        let (status, body) = self
            .upload(&self.owner, parent, name, bytes, Some("replace"))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body
    }

    async fn download(&self, node: NodeId) -> (StatusCode, Vec<u8>) {
        let response = self
            .server
            .http(Some(&self.owner))
            .get(self.server.url(&format!("/api/docs/{}/content", node.0)))
            .send()
            .await
            .unwrap();
        let status = response.status();
        (status, response.bytes().await.unwrap().to_vec())
    }

    async fn doc(&self, node: NodeId) -> Value {
        let (status, body) = self
            .call(
                &self.owner,
                Method::GET,
                &format!("/api/docs/{}", node.0),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body
    }

    async fn pending(&self, node: NodeId) -> Option<PendingWrite> {
        self.db().pending_write(node).await.unwrap()
    }

    async fn write(&self, now: OffsetDateTime) -> usize {
        write_due(&self.state, now).await
    }

    fn object(&self, path: &str) -> Option<Stored> {
        self.fake.object(BUCKET, &key(path))
    }

    /// The writes the fake received for `path`: `(method, condition)`.
    fn writes_of(&self, path: &str) -> Vec<(String, String)> {
        let key = key(path);
        self.fake
            .writes()
            .into_iter()
            .filter(|(_, k, _)| *k == key)
            .map(|(method, _, cond)| (method, cond))
            .collect()
    }

    async fn events_of(&self, kind: &str) -> Vec<Value> {
        let deadline = tokio::time::Instant::now() + common::TIMEOUT;
        loop {
            let found = self.db().events_of_kind(kind).await.unwrap();
            if !found.is_empty() {
                return found.into_iter().map(|e| e.detail).collect();
            }
            assert!(tokio::time::Instant::now() < deadline, "no {kind} event");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn no_events_of(&self, kind: &str) {
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(self.db().events_of_kind(kind).await.unwrap().is_empty());
    }

    /// Uses a fake malware scanner from now on.
    async fn scanner(&self) -> FakeIcap {
        let scanner =
            FakeIcap::start("127.0.0.1:0".parse().unwrap(), Behaviour::default()).unwrap();
        self.db()
            .set_setting(self.admin.id, SCANNER_URL, json!(scanner.url()))
            .await
            .unwrap();
        scanner
    }

    /// Scans `bytes` and waits for its verdict.
    async fn scan(&self, bytes: &[u8]) -> ScanState {
        let sha: [u8; 32] = Sha256::digest(bytes).into();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let _ = server::scan_once(&self.state, OffsetDateTime::now_utc()).await;
            let info = self.db().scan_state(&sha).await.unwrap();
            if let Some(info) = info
                && !matches!(info.state, ScanState::Pending)
            {
                return info.state;
            }
            assert!(tokio::time::Instant::now() < deadline, "never scanned");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

#[tokio::test]
async fn an_upload_appears_as_saving_then_is_in_the_bucket_with_the_same_bytes() {
    let env = Env::new().await;
    let library = env.library("Uploads", "read_write").await;
    let node = env
        .new_file(library, "report.txt", b"quarterly numbers")
        .await;

    let pending = env.pending(node).await.expect("the upload is pending");
    assert_eq!(pending.path, "report.txt");
    assert!(pending.flush);
    assert!(pending.base_tag.is_none());
    assert!(env.object("report.txt").is_none());
    assert_eq!(env.doc(node).await["revision"], pending.revision.as_str());

    let now = OffsetDateTime::now_utc();
    assert_eq!(env.write(now).await, 1);
    let stored = env.object("report.txt").expect("written");
    assert_eq!(stored.bytes, b"quarterly numbers");
    assert_eq!(
        stored.sha256.as_deref(),
        Some(sha_hex(b"quarterly numbers").as_str())
    );
    assert_eq!(
        env.writes_of("report.txt"),
        [("PUT".into(), "absent".into())]
    );
    assert!(env.pending(node).await.is_none());
    let item = env.db().source_item_of(node).await.unwrap().unwrap();
    assert_eq!(item.path, "report.txt");
    // The revision the page knows stays the file's.
    assert_eq!(env.doc(node).await["revision"], pending.revision.as_str());
    let (status, bytes) = env.download(node).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, b"quarterly numbers");
}

#[tokio::test]
async fn nothing_is_written_before_the_scan_passes() {
    let env = Env::new().await;
    let library = env.library("Scanned", "read_write").await;
    let _scanner = env.scanner().await;
    let node = env.new_file(library, "clean.txt", b"clean upload").await;

    let now = OffsetDateTime::now_utc();
    env.write(now).await;
    assert!(env.writes_of("clean.txt").is_empty());
    assert!(env.pending(node).await.is_some());

    // Held back a few seconds, with no attempt counted.
    let held = env.pending(node).await.unwrap();
    assert_eq!(held.attempts, 0);
    assert!(held.next_try_at.unwrap() > now);

    assert_eq!(env.scan(b"clean upload").await, ScanState::Clean);
    env.write(now + time::Duration::seconds(6)).await;
    assert_eq!(env.object("clean.txt").unwrap().bytes, b"clean upload");
    assert!(env.pending(node).await.is_none());
}

#[tokio::test]
async fn infected_content_is_never_written() {
    let env = Env::new().await;
    let library = env.library("Infected", "read_write").await;
    let _scanner = env.scanner().await;
    let mut bytes = b"report prefix ".to_vec();
    bytes.extend_from_slice(&eicar());
    let node = env.new_file(library, "bad.txt", &bytes).await;

    assert_eq!(env.scan(&bytes).await, ScanState::Infected);
    let now = OffsetDateTime::now_utc();
    env.write(now).await;
    env.write(now + time::Duration::minutes(20)).await;
    assert!(env.writes_of("bad.txt").is_empty());
    assert!(env.object("bad.txt").is_none());
    assert!(env.pending(node).await.is_none(), "the write is withdrawn");
}

#[tokio::test]
async fn replacing_a_file_from_a_stale_revision_is_a_conflict_and_nothing_is_staged() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("docs/a.txt"), b"first");
    let library = env.library("Stale", "read_write").await;
    let docs = env.child(library, "docs").await;
    let file = env.child(docs, "a.txt").await;
    let (status, bytes) = env.download(file).await;
    assert_eq!((status, bytes.as_slice()), (StatusCode::OK, &b"first"[..]));
    // The file's session opens on the revision it has now; the same bytes change nothing.
    let unchanged = env.replace(docs, "a.txt", b"first").await;
    assert_eq!(unchanged["changed"], false, "{unchanged}");
    assert!(env.pending(file).await.is_none());

    // Changed in the bucket, and listed: the session's revision is stale.
    env.fake
        .put(BUCKET, &key("docs/a.txt"), b"changed in the bucket");
    env.sync(library).await;
    let (status, body) = env
        .upload(&env.owner, docs, "a.txt", b"mine", Some("replace"))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(env.pending(file).await.is_none());
    env.write(OffsetDateTime::now_utc()).await;
    assert!(env.writes_of("docs/a.txt").is_empty());
    assert_eq!(
        env.object("docs/a.txt").unwrap().bytes,
        b"changed in the bucket"
    );
}

#[tokio::test]
async fn replacing_a_file_writes_one_conditional_put_and_the_next_read_does_not_get() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("docs/a.txt"), b"old bytes");
    let library = env.library("Replace", "read_write").await;
    let docs = env.child(library, "docs").await;
    let file = env.child(docs, "a.txt").await;
    let body = env.replace(docs, "a.txt", b"new bytes").await;
    assert_eq!(body["changed"], true, "{body}");
    let pending = env.pending(file).await.unwrap();
    assert!(pending.flush, "a replacement is written at once");
    assert!(pending.base_tag.is_some());

    env.write(OffsetDateTime::now_utc()).await;
    assert_eq!(
        env.writes_of("docs/a.txt"),
        [("PUT".into(), "match".into())]
    );
    assert_eq!(env.object("docs/a.txt").unwrap().bytes, b"new bytes");
    assert!(env.pending(file).await.is_none());

    let gets = env.fake.count("get");
    let (status, bytes) = env.download(file).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, b"new bytes");
    assert_eq!(
        env.fake.count("get"),
        gets,
        "the written content is served from the cache"
    );
}

/// The named copy of `name` in `folder` written at `now` by the owner: its path.
fn copy_path(folder: &str, name: &str, now: OffsetDateTime) -> String {
    format!("{folder}{}", sibling_name(name, OWNER, now))
}

#[tokio::test]
async fn an_object_changed_in_the_bucket_meanwhile_is_kept_and_a_named_copy_holds_the_upload() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("docs/a.txt"), b"original");
    let library = env.library("Changed", "read_write").await;
    let docs = env.child(library, "docs").await;
    let file = env.child(docs, "a.txt").await;
    env.replace(docs, "a.txt", b"my edit").await;
    env.fake.put(BUCKET, &key("docs/a.txt"), b"outside edit");

    let now = OffsetDateTime::now_utc();
    env.write(now).await;
    assert_eq!(env.object("docs/a.txt").unwrap().bytes, b"outside edit");
    let copy = copy_path("docs/", "a.txt", now);
    assert_eq!(env.object(&copy).unwrap().bytes, b"my edit");
    assert!(env.pending(file).await.is_none());

    let copy_name = copy.trim_start_matches("docs/");
    let copy_node = env.child(docs, copy_name).await;
    let item = env.db().source_item_of(copy_node).await.unwrap().unwrap();
    assert_eq!(item.path, copy);
    let (status, bytes) = env.download(copy_node).await;
    assert_eq!(
        (status, bytes.as_slice()),
        (StatusCode::OK, &b"my edit"[..])
    );
    let (status, bytes) = env.download(file).await;
    assert_eq!(
        (status, bytes.as_slice()),
        (StatusCode::OK, &b"outside edit"[..])
    );
    let events = env.events_of("source.conflict_copy").await;
    assert_eq!(
        events,
        vec![json!({ "node": file.0.to_string(), "copy": copy_node.0.to_string() })]
    );
}

#[tokio::test]
async fn a_conflict_copy_is_made_when_its_author_has_lost_access_and_is_not_retried_for_ever() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("docs/a.txt"), b"original");
    let library = env.library("Removed", "read_write").await;
    let docs = env.child(library, "docs").await;
    let file = env.child(docs, "a.txt").await;
    env.replace(docs, "a.txt", b"my edit").await;
    env.fake.put(BUCKET, &key("docs/a.txt"), b"outside edit");
    // The author is taken off the site before the change is written.
    env.db()
        .set_node_access(env.admin.id, env.site, Principal::User(env.owner.id), None)
        .await
        .unwrap();

    let now = OffsetDateTime::now_utc();
    env.write(now).await;
    assert_eq!(env.object("docs/a.txt").unwrap().bytes, b"outside edit");
    let copy = copy_path("docs/", "a.txt", now);
    assert_eq!(
        env.object(&copy).map(|stored| stored.bytes),
        Some(b"my edit".to_vec()),
        "the change is kept in a copy named for its author"
    );
    assert!(
        env.pending(file).await.is_none(),
        "nothing is left to retry"
    );
    let copy_node = env.child(docs, copy.trim_start_matches("docs/")).await;
    assert!(env.pending(copy_node).await.is_none());
    let events = env.events_of("source.conflict_copy").await;
    assert_eq!(
        events,
        vec![json!({ "node": file.0.to_string(), "copy": copy_node.0.to_string() })]
    );
}

#[tokio::test]
async fn a_change_between_head_and_put_is_kept_and_becomes_a_named_copy() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("docs/a.txt"), b"original");
    let library = env.library("Raced", "read_write").await;
    let docs = env.child(library, "docs").await;
    let file = env.child(docs, "a.txt").await;
    env.replace(docs, "a.txt", b"my edit").await;
    env.fake.change_after_head(&key("docs/a.txt"), b"raced in");

    let now = OffsetDateTime::now_utc();
    env.write(now).await;
    assert_eq!(env.object("docs/a.txt").unwrap().bytes, b"raced in");
    let copy = copy_path("docs/", "a.txt", now);
    assert_eq!(env.object(&copy).unwrap().bytes, b"my edit");
    assert!(env.pending(file).await.is_none());
    env.child(docs, copy.trim_start_matches("docs/")).await;
}

#[tokio::test]
async fn a_new_file_whose_key_appeared_gets_a_numbered_name() {
    let env = Env::new().await;
    let library = env.library("Appeared", "read_write").await;
    let node = env.new_file(library, "notes.txt", b"my notes").await;
    env.fake
        .appear_after_miss(&key("notes.txt"), b"someone else's notes");

    env.write(OffsetDateTime::now_utc()).await;
    assert_eq!(
        env.object("notes.txt").unwrap().bytes,
        b"someone else's notes"
    );
    assert_eq!(env.object("notes (2).txt").unwrap().bytes, b"my notes");
    assert!(env.pending(node).await.is_none());
    assert_eq!(env.child(library, "notes (2).txt").await, node);
    let item = env.db().source_item_of(node).await.unwrap().unwrap();
    assert_eq!(item.path, "notes (2).txt");
}

#[tokio::test]
async fn a_lost_answer_does_not_make_a_conflict_copy() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("docs/a.txt"), b"original");
    let library = env.library("Lost", "read_write").await;
    let docs = env.child(library, "docs").await;
    let file = env.child(docs, "a.txt").await;
    env.replace(docs, "a.txt", b"my edit").await;
    env.fake.lose_answer_after_write();

    let now = OffsetDateTime::now_utc();
    env.write(now).await;
    assert_eq!(env.object("docs/a.txt").unwrap().bytes, b"my edit");
    let failed = env.pending(file).await.expect("kept for the next attempt");
    assert_eq!(failed.state, PendingState::Failed);
    assert_eq!(failed.attempts, 1);

    env.write(now + time::Duration::minutes(1)).await;
    assert!(env.pending(file).await.is_none());
    assert_eq!(env.writes_of("docs/a.txt").len(), 1, "no second PUT");
    assert_eq!(env.children(docs).await.len(), 1, "no copy");
    env.no_events_of("source.conflict_copy").await;
}

#[tokio::test]
async fn an_offline_store_keeps_the_write_and_writes_it_when_back() {
    let env = Env::new().await;
    let library = env.library("Offline", "read_write").await;
    let node = env.new_file(library, "later.txt", b"written later").await;
    env.fake.throttle_next(50);

    let now = OffsetDateTime::now_utc();
    env.write(now).await;
    let failed = env.pending(node).await.unwrap();
    assert_eq!(failed.state, PendingState::Failed);
    let error = failed.error.clone().unwrap();
    assert!(!error.is_empty() && error.chars().count() <= 300, "{error}");
    assert!(
        !error.contains(BUCKET) && !error.contains(PREFIX),
        "{error}"
    );
    // Thirty seconds on (as the database keeps it, to the microsecond).
    let retry = failed.next_try_at.unwrap();
    let wait = retry - now;
    assert!(
        (wait - time::Duration::seconds(30)).abs() < time::Duration::milliseconds(1),
        "{wait}"
    );

    // Not before its time.
    env.fake.throttle_next(0);
    let requests = env.fake.requests().len();
    env.write(now + time::Duration::seconds(10)).await;
    assert_eq!(env.fake.requests().len(), requests);

    env.write(now + time::Duration::seconds(31)).await;
    assert_eq!(env.object("later.txt").unwrap().bytes, b"written later");
    assert!(env.pending(node).await.is_none());
}

#[tokio::test]
async fn a_denied_write_blocks_the_library_and_a_passing_reprobe_writes_it() {
    let env = Env::new().await;
    let library = env.library("Denied", "read_write").await;
    let node = env.new_file(library, "kept.txt", b"kept bytes").await;
    env.fake.read_only();

    let now = OffsetDateTime::now_utc();
    env.write(now).await;
    let (state, error) = env.db().write_state(library).await.unwrap().unwrap();
    assert_eq!(state, WriteState::Blocked);
    assert_eq!(
        error.as_deref(),
        Some("The storage refused a change with this access key.")
    );
    env.events_of("source.write_blocked").await;
    assert!(env.pending(node).await.is_some(), "nothing is dropped");
    let (status, body) = env
        .upload(&env.owner, library, "more.txt", b"more", None)
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "source_write_blocked");

    env.fake.allow_writes();
    // Not probed again before five minutes have passed.
    env.write(now + time::Duration::minutes(1)).await;
    assert!(env.object("kept.txt").is_none());
    assert_eq!(
        env.db().write_state(library).await.unwrap().unwrap().0,
        WriteState::Blocked
    );

    let later = now + time::Duration::minutes(6);
    env.write(later).await;
    assert_eq!(
        env.db().write_state(library).await.unwrap().unwrap().0,
        WriteState::Ok
    );
    env.events_of("source.write_restored").await;
    env.write(later).await;
    assert_eq!(env.object("kept.txt").unwrap().bytes, b"kept bytes");
    assert!(env.pending(node).await.is_none());
    // The probe left nothing behind.
    let left: Vec<String> = env
        .fake
        .writes()
        .into_iter()
        .filter(|(method, k, _)| {
            method == "PUT" && *k != key("kept.txt") && env.fake.object(BUCKET, k).is_some()
        })
        .map(|(_, k, _)| k)
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

#[tokio::test]
async fn a_restart_between_stage_and_write_loses_nothing() {
    let data_dir = tempfile::tempdir().unwrap();
    let db = TestDb::create().await;
    let fake = FakeS3::start().await;
    fake.bucket(BUCKET);
    fake.require_keys(KEY_ID, SECRET);
    let mut first = StoppableServer::start(data_dir.path(), &db, |_| {});
    // The first server never writes: it stops between the stage and the write.
    first.app_state().await.writes().set_paused_for_tests(true);
    let admin = first.admin("Restart admin").await;
    let home = common::Site::library(&first).await;
    let site = first.records_db().node(home).await.unwrap().unwrap().site;
    approve(&first.records_db(), &fake).await;
    let response = first
        .http(Some(&admin))
        .post(first.url(&format!("/api/sites/{}/libraries/s3", site.0)))
        .json(&connect_body("Restarted", "read_write", PREFIX))
        .send()
        .await
        .unwrap();
    let (status, body) = answer(response).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let library = id_of(&body);
    let response = first
        .http(Some(&admin))
        .post(first.url("/api/docs/upload"))
        .query(&[
            ("parent", library.0.to_string()),
            ("name", "kept.txt".into()),
        ])
        .body(b"survives a restart".to_vec())
        .send()
        .await
        .unwrap();
    let (status, body) = answer(response).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let node = id_of(&body);
    assert!(
        first
            .records_db()
            .pending_write(node)
            .await
            .unwrap()
            .is_some()
    );
    first.stop();
    assert!(fake.object(BUCKET, &key("kept.txt")).is_none());

    let second = StoppableServer::start(data_dir.path(), &db, |_| {});
    let _state = second.app_state().await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while second
        .records_db()
        .pending_write(node)
        .await
        .unwrap()
        .is_some()
    {
        assert!(tokio::time::Instant::now() < deadline, "never written");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        fake.object(BUCKET, &key("kept.txt")).unwrap().bytes,
        b"survives a restart"
    );
}

#[tokio::test]
async fn staged_content_is_served_while_saving_and_is_not_evicted() {
    let env = Env::new().await;
    let library = env.library("Served", "read_write").await;
    let node = env.new_file(library, "draft.txt", b"staged bytes").await;
    let heads = env.fake.count("head");

    let (status, bytes) = env.download(node).await;
    assert_eq!(
        (status, bytes.as_slice()),
        (StatusCode::OK, &b"staged bytes"[..])
    );
    assert_eq!(env.fake.count("head"), heads, "the store is not asked");

    env.state.shares().set_cache_max_bytes_for_tests(1);
    server::external::evict(&env.state).await;
    let (status, bytes) = env.download(node).await;
    assert_eq!(
        (status, bytes.as_slice()),
        (StatusCode::OK, &b"staged bytes"[..])
    );
    assert!(env.pending(node).await.is_some());
    env.write(OffsetDateTime::now_utc()).await;
    assert_eq!(env.object("draft.txt").unwrap().bytes, b"staged bytes");
}

#[tokio::test]
async fn a_listing_during_saving_keeps_the_new_file_and_its_revision() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("docs/a.txt"), b"original");
    let library = env.library("Listed", "read_write").await;
    let docs = env.child(library, "docs").await;
    let file = env.child(docs, "a.txt").await;
    let node = env.new_file(library, "new.txt", b"new bytes").await;
    let revision = env.pending(node).await.unwrap().revision;
    env.replace(docs, "a.txt", b"my edit").await;
    let staged = env.pending(file).await.unwrap().revision;
    let tag = env
        .db()
        .source_item_of(file)
        .await
        .unwrap()
        .unwrap()
        .blob_sha;

    // The tracked file changed outside too; the listing leaves both alone.
    env.fake.put(BUCKET, &key("docs/a.txt"), b"outside edit");
    env.sync(library).await;
    assert_eq!(env.child(library, "new.txt").await, node);
    assert_eq!(env.doc(node).await["revision"], revision.as_str());
    assert_eq!(env.doc(file).await["revision"], staged.as_str());
    assert_eq!(
        env.db()
            .source_item_of(file)
            .await
            .unwrap()
            .unwrap()
            .blob_sha,
        tag
    );

    // The new file's PUT landed but its finish was not recorded: still one file.
    env.fake.put(BUCKET, &key("new.txt"), b"new bytes");
    env.sync(library).await;
    let named: Vec<String> = env
        .children(library)
        .await
        .into_iter()
        .map(|(name, _)| name)
        .filter(|name| name.starts_with("new"))
        .collect();
    assert_eq!(named, ["new.txt"]);
    assert_eq!(env.child(library, "new.txt").await, node);
    assert!(env.pending(node).await.is_some());
}

#[tokio::test]
async fn a_listing_after_a_write_plans_no_change() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("docs/a.txt"), b"original");
    let library = env.library("Echo", "read_write").await;
    let docs = env.child(library, "docs").await;
    let file = env.child(docs, "a.txt").await;
    let node = env.new_file(docs, "fresh.txt", b"fresh bytes").await;
    env.replace(docs, "a.txt", b"replaced bytes").await;
    env.write(OffsetDateTime::now_utc()).await;
    assert!(env.pending(node).await.is_none());
    assert!(env.pending(file).await.is_none());

    let before = env.children(docs).await;
    let tags = (
        env.db()
            .source_item_of(file)
            .await
            .unwrap()
            .unwrap()
            .blob_sha,
        env.db()
            .source_item_of(node)
            .await
            .unwrap()
            .unwrap()
            .blob_sha,
    );
    let revisions = (
        env.doc(file).await["revision"].clone(),
        env.doc(node).await["revision"].clone(),
    );
    env.sync(library).await;
    assert_eq!(env.children(docs).await, before);
    assert_eq!(
        (
            env.db()
                .source_item_of(file)
                .await
                .unwrap()
                .unwrap()
                .blob_sha,
            env.db()
                .source_item_of(node)
                .await
                .unwrap()
                .unwrap()
                .blob_sha,
        ),
        tags
    );
    assert_eq!(
        (
            env.doc(file).await["revision"].clone(),
            env.doc(node).await["revision"].clone()
        ),
        revisions
    );
    let gets = env.fake.count("get");
    let (_, bytes) = env.download(file).await;
    assert_eq!(bytes, b"replaced bytes");
    assert_eq!(env.fake.count("get"), gets);
}

#[tokio::test]
async fn uploads_do_not_touch_site_usage_or_reserve_quota() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("docs/a.txt"), b"original");
    let library = env.library("Quota", "read_write").await;
    let docs = env.child(library, "docs").await;
    let (status, body) = env
        .call(
            &env.admin,
            Method::PUT,
            &format!("/api/sites/{}/quota", env.site.0),
            Some(json!({ "bytes": 1 })),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let usage = env.db().site_usage(env.site).await.unwrap();

    let big = vec![b'x'; 4096];
    env.new_file(docs, "big.txt", &big).await;
    env.replace(docs, "a.txt", &big).await;
    let after = env.db().site_usage(env.site).await.unwrap();
    assert_eq!((after.used, after.reserved), (usage.used, usage.reserved));
    env.write(OffsetDateTime::now_utc()).await;
    let after = env.db().site_usage(env.site).await.unwrap();
    assert_eq!((after.used, after.reserved), (usage.used, usage.reserved));
}

#[tokio::test]
async fn on_a_store_that_ignores_conditions_the_event_is_sent() {
    let env = Env::new().await;
    env.fake.ignore_conditions();
    let library = env.library("Unconditional", "read_write").await;
    env.new_file(library, "plain.txt", b"plain").await;
    env.write(OffsetDateTime::now_utc()).await;
    assert_eq!(env.writes_of("plain.txt"), [("PUT".into(), "none".into())]);
    assert_eq!(
        env.events_of("source.unconditional_write").await,
        vec![json!({ "count": 1 })]
    );
}

#[tokio::test]
async fn every_write_is_counted_by_the_meter() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("docs/a.txt"), b"original");
    let library = env.library("Metered", "read_write").await;
    let docs = env.child(library, "docs").await;
    let before = env.state.s3().unflushed_usage(library).write;
    let writes = env.fake.writes().len();
    env.new_file(docs, "one.txt", b"one").await;
    env.replace(docs, "a.txt", b"two").await;
    env.fake.put(BUCKET, &key("docs/a.txt"), b"outside");
    env.write(OffsetDateTime::now_utc()).await;
    let sent = env.fake.writes().len() - writes;
    assert!(sent >= 2, "{:?}", env.fake.writes());
    assert_eq!(
        env.state.s3().unflushed_usage(library).write - before,
        sent as u64
    );
}

#[tokio::test]
async fn no_key_outside_the_prefix_is_written() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("docs/a.txt"), b"original");
    let library = env.library("Prefixed", "read_write").await;
    let docs = env.child(library, "docs").await;
    for name in [".sharecraft-tmp-x.txt", ".sharecraft-bin"] {
        let (status, body) = env.upload(&env.owner, docs, name, b"x", None).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{name}: {body}");
        assert_eq!(body["error"], "invalid_name", "{name}: {body}");
    }
    env.new_file(docs, "inside.txt", b"inside").await;
    env.replace(docs, "a.txt", b"edit").await;
    env.fake.put(BUCKET, &key("docs/a.txt"), b"outside");
    env.write(OffsetDateTime::now_utc()).await;
    let writes = env.fake.writes();
    assert!(!writes.is_empty());
    for (_, k, _) in &writes {
        assert!(k.starts_with(PREFIX), "{k}");
    }
}

#[tokio::test]
async fn a_read_only_s3_library_still_refuses_the_upload() {
    let env = Env::new().await;
    let library = env.library("Read only", "read_only").await;
    let (status, body) = env
        .upload(&env.owner, library, "nope.txt", b"nope", None)
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "source_read_only");
    env.write(OffsetDateTime::now_utc()).await;
    assert!(env.fake.writes().is_empty());
    assert!(env.children(library).await.is_empty());
}

#[tokio::test]
async fn held_and_blocked_writes_do_not_keep_a_ready_write_waiting() {
    let env = Env::new().await;
    let blocked = env.library_at("Blocked", "read_write", "team-a/").await;
    let busy = env.library_at("Busy", "read_write", "team-c/").await;
    let ready = env.library_at("Ready", "read_write", "team-b/").await;
    // Staged oldest first: the four held rows come before the ready one.
    let a1 = env.new_file(blocked, "a1.txt", b"a one").await;
    let a2 = env.new_file(blocked, "a2.txt", b"a two").await;
    let c1 = env.new_file(busy, "c1.txt", b"c one").await;
    let c2 = env.new_file(busy, "c2.txt", b"c two").await;
    env.new_file(ready, "b.txt", b"b ready").await;
    env.db()
        .set_write_state(blocked, WriteState::Blocked, Some("Refused."))
        .await
        .unwrap();
    // A listing holds the busy library for the whole pass.
    let lock = env.db().try_lock_source(busy).await.unwrap().unwrap();

    let now = OffsetDateTime::now_utc();
    env.write(now).await;
    assert_eq!(
        env.fake.object(BUCKET, "team-b/b.txt").unwrap().bytes,
        b"b ready"
    );
    assert!(env.fake.object(BUCKET, "team-a/a1.txt").is_none());
    assert!(env.fake.object(BUCKET, "team-c/c1.txt").is_none());
    // The busy library's rows are looked at again in five seconds, with no attempt counted.
    for node in [c1, c2] {
        let held = env.pending(node).await.unwrap();
        assert_eq!(held.attempts, 0);
        let wait = held.next_try_at.unwrap() - now;
        assert!(
            (wait - time::Duration::seconds(5)).abs() < time::Duration::milliseconds(1),
            "{wait}"
        );
    }

    drop(lock);
    env.write(now + time::Duration::seconds(6)).await;
    assert_eq!(
        env.fake.object(BUCKET, "team-c/c1.txt").unwrap().bytes,
        b"c one"
    );
    assert_eq!(
        env.fake.object(BUCKET, "team-c/c2.txt").unwrap().bytes,
        b"c two"
    );
    // The blocked library's rows wait for the probe that unblocks it, then are written.
    assert_eq!(
        env.db().write_state(blocked).await.unwrap().unwrap().0,
        WriteState::Ok
    );
    env.write(now + time::Duration::seconds(6)).await;
    for node in [a1, a2] {
        assert!(env.pending(node).await.is_none());
    }
    assert_eq!(
        env.fake.object(BUCKET, "team-a/a2.txt").unwrap().bytes,
        b"a two"
    );
}

#[tokio::test]
async fn a_failed_bucket_lookup_opens_no_session_and_commits_nothing_to_git() {
    let env = Env::new().await;
    let library = env.library("Lookup", "read_write").await;
    // A file an editor opens, so its session could be joined.
    let node = env.new_file(library, "a.pdf", b"first").await;
    env.write(OffsetDateTime::now_utc()).await;
    assert!(env.pending(node).await.is_none());
    let doc = collab_core::ids::DocId(node.0);
    let revisions = env.server.records_revisions(doc).await;

    env.state.writes().set_bucket_lookup_fails_for_tests(true);
    let (status, body) = env
        .upload(&env.owner, library, "a.pdf", b"second", Some("replace"))
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(env.pending(node).await.is_none(), "nothing staged");
    assert_eq!(env.server.records_revisions(doc).await, revisions);
    // The editor's join is refused too.
    let request = common::ws_request(
        env.server.addr,
        &format!("/ws/docs/{}", node.0),
        Some(&env.owner.cookie),
    );
    assert_eq!(common::handshake_status(request).await, 503);
    assert_eq!(env.server.records_revisions(doc).await, revisions);

    env.state.writes().set_bucket_lookup_fails_for_tests(false);
    env.replace(library, "a.pdf", b"second").await;
    let staged = env
        .pending(node)
        .await
        .expect("staged once the lookup works");
    assert!(staged.base_tag.is_some());
}
