//! Multipart uploads of files over the single-`PUT` limit, against the fake S3: the parts, the
//! condition on the completion, a conflict at completion, aborts after any failure, a lost
//! completion answer, the sweep that aborts what a crash left open, and the 5 GiB limit.
//!
//! Small files take the multipart path through `set_single_put_max_for_tests` and
//! `set_part_bytes_for_tests`. The worker is driven with `write_due(state, now)`.

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
use records::{PendingState, PendingWrite, Principal, S3_ENDPOINTS};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use server::AppState;
use server::config::Config;
use server::source_sync::sync_once;
use server::source_write::{sweep_holds, write_due};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

const BUCKET: &str = "multipart-bucket-6a1";
const ENDPOINT: &str = "multipart-endpoint-2f7";
const KEY_ID: &str = "AKIDMULTIPARTEXAMPLE01";
const SECRET: &str = "planted-multipart-secret-9c4e-qq";
const PREFIX: &str = "team-parts/";
const GIB: u64 = 1024 * 1024 * 1024;
/// Files over this many bytes take the multipart path here.
const SINGLE: u64 = 16;
/// Each part's size here.
const PART: u64 = 10;

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

/// 35 bytes: four parts of 10, 10, 10 and 5.
fn big() -> Vec<u8> {
    b"0123456789abcdefghijABCDEFGHIJ-tail".to_vec()
}

fn parts(n: u16) -> Vec<(String, String)> {
    (1..=n)
        .map(|i| ("PART".to_owned(), i.to_string()))
        .collect()
}

async fn approve(db: &records::Db, fake: &FakeS3) {
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
}

impl Env {
    async fn new() -> Env {
        Env::with(|_| {}).await
    }

    async fn with(edit: impl FnOnce(&mut Config)) -> Env {
        let fake = FakeS3::start().await;
        fake.bucket(BUCKET);
        fake.require_keys(KEY_ID, SECRET);
        fake.min_part_bytes(1);
        let server = spawn_server_config_without_workers(edit).await;
        let admin = server.admin("Parts admin").await;
        let owner = server.user("Site owner").await;
        let library = common::Site::library(&server).await;
        let db = server.records_db();
        let site = db.node(library).await.unwrap().unwrap().site;
        db.set_node_access(admin.id, site, Principal::User(owner.id), Some("owner"))
            .await
            .unwrap();
        let state = server.app_state().await;
        let s3 = state.s3();
        s3.set_retry_wait_for_tests(Duration::from_millis(5));
        s3.set_timeouts_for_tests(Duration::from_secs(2), Duration::from_secs(2));
        s3.set_single_put_max_for_tests(SINGLE);
        s3.set_part_bytes_for_tests(PART);
        approve(&db, &fake).await;
        Env {
            server,
            fake,
            state,
            admin,
            owner,
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

    /// A library connected by the administrator in read and write mode, its bucket listed once.
    async fn library(&self, name: &str) -> NodeId {
        let site = self
            .db()
            .node(common::Site::library(&self.server).await)
            .await
            .unwrap()
            .unwrap()
            .site;
        let (status, answer) = self
            .call(
                &self.admin,
                Method::POST,
                &format!("/api/sites/{}/libraries/s3", site.0),
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

    async fn sync(&self, library: NodeId) {
        let _ = sync_once(&self.state, library, OffsetDateTime::now_utc())
            .await
            .unwrap();
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

    /// Uploads a new file in one request as the owner, which must be accepted; its node.
    async fn new_file(&self, parent: NodeId, name: &str, bytes: &[u8]) -> NodeId {
        let response = self
            .server
            .http(Some(&self.owner))
            .post(self.server.url("/api/docs/upload"))
            .query(&[("parent", parent.0.to_string()), ("name", name.to_owned())])
            .body(bytes.to_vec())
            .send()
            .await
            .unwrap();
        let (status, body) = answer(response).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        id_of(&body)
    }

    /// Replaces the file `name` in `parent` as the owner, which must be accepted.
    async fn replace(&self, parent: NodeId, name: &str, bytes: &[u8]) {
        let response = self
            .server
            .http(Some(&self.owner))
            .post(self.server.url("/api/docs/upload"))
            .query(&[
                ("parent", parent.0.to_string()),
                ("name", name.to_owned()),
                ("on_conflict", "replace".to_owned()),
            ])
            .body(bytes.to_vec())
            .send()
            .await
            .unwrap();
        let (status, body) = answer(response).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    async fn download(&self, node: NodeId) -> Vec<u8> {
        let response = self
            .server
            .http(Some(&self.owner))
            .get(self.server.url(&format!("/api/docs/{}/content", node.0)))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response.bytes().await.unwrap().to_vec()
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

    /// How many multipart uploads the records hold open.
    async fn open_rows(&self) -> i64 {
        sqlx::query_scalar("select count(*) from source_multipart")
            .fetch_one(self.db().test_pool())
            .await
            .unwrap()
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
}

#[tokio::test]
async fn a_file_over_the_single_put_limit_is_written_in_parts_and_matches() {
    let env = Env::new().await;
    let library = env.library("Parts").await;
    let bytes = big();
    let node = env.new_file(library, "big.bin", &bytes).await;

    assert_eq!(env.write(OffsetDateTime::now_utc()).await, 1);
    let stored = env.object("big.bin").expect("written");
    assert_eq!(stored.bytes, bytes);
    // The whole file's hash rides on the object, so a lost answer is recognised.
    assert_eq!(stored.sha256.as_deref(), Some(sha_hex(&bytes).as_str()));
    let mut expected = parts(4);
    expected.push(("COMPLETE".to_owned(), "absent".to_owned()));
    assert_eq!(env.writes_of("big.bin"), expected);
    assert!(env.fake.open_multiparts().is_empty());
    assert_eq!(env.open_rows().await, 0, "no row is left after completion");
    assert!(env.pending(node).await.is_none());
    assert_eq!(env.download(node).await, bytes);

    // A file at the single-PUT limit is still one PUT.
    let small = b"exactly 16 bytes".to_vec();
    assert_eq!(small.len() as u64, SINGLE);
    env.new_file(library, "small.bin", &small).await;
    env.write(OffsetDateTime::now_utc()).await;
    assert_eq!(
        env.writes_of("small.bin"),
        [("PUT".to_owned(), "absent".to_owned())]
    );
}

#[tokio::test]
async fn the_condition_rides_on_the_completion() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("a.txt"), b"old bytes");
    let library = env.library("Condition").await;
    let file = env.child(library, "a.txt").await;
    let bytes = big();
    env.replace(library, "a.txt", &bytes).await;

    env.write(OffsetDateTime::now_utc()).await;
    assert_eq!(env.object("a.txt").unwrap().bytes, bytes);
    let mut expected = parts(4);
    expected.push(("COMPLETE".to_owned(), "match".to_owned()));
    assert_eq!(env.writes_of("a.txt"), expected);
    // No part carries a condition: only the completion does.
    let conditioned_parts = env
        .fake
        .requests()
        .iter()
        .filter(|r| r.method == "PUT" && r.query.contains("partNumber"))
        .filter(|r| {
            r.headers
                .iter()
                .any(|h| h == "if-match" || h == "if-none-match")
        })
        .count();
    assert_eq!(conditioned_parts, 0);
    assert!(env.pending(file).await.is_none());
    assert_eq!(env.open_rows().await, 0);
}

#[tokio::test]
async fn a_conflict_at_completion_makes_a_named_copy() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("a.txt"), b"old bytes");
    let library = env.library("Conflict").await;
    let file = env.child(library, "a.txt").await;
    let bytes = big();
    env.replace(library, "a.txt", &bytes).await;
    // Someone else changes the file between the worker's HEAD and its completion.
    env.fake.change_after_head(&key("a.txt"), b"their bytes");

    env.write(OffsetDateTime::now_utc()).await;
    // Theirs is kept; ours is written beside it, in parts too.
    assert_eq!(env.object("a.txt").unwrap().bytes, b"their bytes");
    let writes = env.writes_of("a.txt");
    assert_eq!(writes.last().unwrap(), &("ABORT".to_owned(), String::new()));
    assert!(writes.contains(&("COMPLETE".to_owned(), "match".to_owned())));
    let names: Vec<String> = env
        .children(library)
        .await
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    let copy = names
        .iter()
        .find(|name| name.as_str() != "a.txt")
        .unwrap_or_else(|| panic!("no named copy in {names:?}"));
    assert!(copy.starts_with("a ("), "{copy}");
    assert_eq!(env.object(copy).unwrap().bytes, bytes);
    let mut expected = parts(4);
    expected.push(("COMPLETE".to_owned(), "absent".to_owned()));
    assert_eq!(env.writes_of(copy), expected);
    assert!(env.pending(file).await.is_none());
    assert!(env.fake.open_multiparts().is_empty());
    assert_eq!(env.open_rows().await, 0);
}

#[tokio::test]
async fn a_failed_part_aborts_the_upload_and_forgets_it() {
    let env = Env::new().await;
    let library = env.library("Failed part").await;
    let node = env.new_file(library, "big.bin", &big()).await;
    env.fake.fail_part(2);

    env.write(OffsetDateTime::now_utc()).await;
    let row = env.pending(node).await.expect("still pending");
    assert_eq!(row.state, PendingState::Failed);
    assert!(env.object("big.bin").is_none());
    let writes = env.writes_of("big.bin");
    assert_eq!(
        writes,
        [
            ("PART".to_owned(), "1".to_owned()),
            ("PART".to_owned(), "2".to_owned()),
            ("ABORT".to_owned(), String::new()),
        ]
    );
    assert!(env.fake.open_multiparts().is_empty());
    assert_eq!(env.open_rows().await, 0);
}

#[tokio::test]
async fn an_error_inside_a_200_completion_aborts() {
    let env = Env::new().await;
    let library = env.library("Error in 200").await;
    let bytes = big();
    let node = env.new_file(library, "big.bin", &bytes).await;
    env.fake.error_in_200_next();

    let now = OffsetDateTime::now_utc();
    env.write(now).await;
    let row = env.pending(node).await.expect("still pending");
    assert_eq!(row.state, PendingState::Failed);
    assert!(env.object("big.bin").is_none());
    assert_eq!(
        env.writes_of("big.bin").last().unwrap(),
        &("ABORT".to_owned(), String::new())
    );
    assert!(env.fake.open_multiparts().is_empty());
    assert_eq!(env.open_rows().await, 0);

    // The retry writes it whole.
    env.write(now + time::Duration::seconds(31)).await;
    assert_eq!(env.object("big.bin").unwrap().bytes, bytes);
    assert!(env.pending(node).await.is_none());
    assert_eq!(env.open_rows().await, 0);
}

#[tokio::test]
async fn a_lost_completion_answer_is_recognised() {
    let env = Env::new().await;
    let library = env.library("Lost").await;
    let bytes = big();
    let node = env.new_file(library, "big.bin", &bytes).await;
    env.fake.lose_complete_answer();

    let now = OffsetDateTime::now_utc();
    env.write(now).await;
    // The store has it; the worker did not hear so.
    assert_eq!(env.object("big.bin").unwrap().bytes, bytes);
    assert!(env.pending(node).await.is_some());
    assert_eq!(env.open_rows().await, 0);
    let before = env.writes_of("big.bin").len();

    env.write(now + time::Duration::seconds(31)).await;
    assert!(env.pending(node).await.is_none(), "recognised as ours");
    assert_eq!(env.writes_of("big.bin").len(), before, "nothing sent again");
    assert_eq!(env.object("big.bin").unwrap().bytes, bytes);
}

#[tokio::test]
async fn a_left_over_upload_row_is_aborted_by_the_sweep_after_a_day() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("a.txt"), b"a");
    let library = env.library("Left over").await;
    let file = env.child(library, "a.txt").await;
    // A crash left an upload open, and its row.
    let target = env.state.s3().target(&env.state, library).await.unwrap();
    let started = OffsetDateTime::now_utc();
    let id = env
        .state
        .s3()
        .start_multipart(&target, &key("a.txt"), &[7u8; 32])
        .await
        .unwrap();
    env.db()
        .note_multipart(library, file, &key("a.txt"), &id, started)
        .await
        .unwrap();

    // Less than a day: left alone.
    sweep_holds(&env.state, started + time::Duration::hours(23)).await;
    assert_eq!(env.fake.open_multiparts().len(), 1);
    assert_eq!(env.open_rows().await, 1);

    sweep_holds(&env.state, started + time::Duration::hours(25)).await;
    assert!(env.fake.open_multiparts().is_empty());
    assert_eq!(env.open_rows().await, 0);
    assert_eq!(
        env.writes_of("a.txt"),
        [("ABORT".to_owned(), String::new())]
    );
    assert_eq!(env.events_of("source.multipart_aborted").await.len(), 1);
    // The file itself is untouched.
    assert_eq!(env.object("a.txt").unwrap().bytes, b"a");
}

#[tokio::test]
async fn an_upload_over_five_gib_is_refused_in_plain_words_before_any_byte() {
    let env = Env::with(|config| config.max_file_bytes = 10 * GIB).await;
    env.state.shares().set_cache_max_bytes_for_tests(20 * GIB);
    env.state
        .s3()
        .set_single_put_max_for_tests(512 * 1024 * 1024);
    let library = env.library("Huge").await;
    let start = |size: u64, name: &str| {
        json!({
            "parent": library.0.to_string(),
            "name": name,
            "size": size,
            "sha256": "ab".repeat(32),
        })
    };
    let (status, body) = env
        .call(
            &env.owner,
            Method::POST,
            "/api/uploads",
            Some(start(5 * GIB + 1, "huge.bin")),
        )
        .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    assert_eq!(body["error"], "source_file_too_large", "{body}");
    assert_eq!(body["limit"], 5 * GIB, "{body}");
    assert_eq!(
        body["message"], "Files over 5 GiB cannot be saved to this S3 storage.",
        "{body}"
    );
    let open: i64 = sqlx::query_scalar("select count(*) from upload_sessions")
        .fetch_one(env.db().test_pool())
        .await
        .unwrap();
    assert_eq!(open, 0, "no session was started");

    // Over the old 512 MiB limit, a file is accepted.
    let (status, body) = env
        .call(
            &env.owner,
            Method::POST,
            "/api/uploads",
            Some(start(600 * 1024 * 1024, "large.bin")),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["id"].as_str().unwrap();
    let (status, _) = env
        .call(
            &env.owner,
            Method::DELETE,
            &format!("/api/uploads/{id}"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(
        env.fake
            .writes()
            .iter()
            .all(|(_, k, _)| k.contains(".sharecraft-tmp-probe-")),
        "nothing was written"
    );
}

#[tokio::test]
async fn read_only_is_refused_while_a_multipart_is_open() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("a.txt"), b"a");
    let library = env.library("Open upload").await;
    let file = env.child(library, "a.txt").await;
    env.db()
        .note_multipart(
            library,
            file,
            &key("a.txt"),
            "upload-open",
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
    let (status, body) = env
        .call(
            &env.admin,
            Method::PATCH,
            &format!("/api/libraries/{}/source", library.0),
            Some(json!({ "mode": "read_only" })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "source_writes_pending", "{body}");

    // Once it is gone, the switch goes ahead.
    env.db()
        .forget_multipart(library, "upload-open")
        .await
        .unwrap();
    let (status, body) = env
        .call(
            &env.admin,
            Method::PATCH,
            &format!("/api/libraries/{}/source", library.0),
            Some(json!({ "mode": "read_only" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}
