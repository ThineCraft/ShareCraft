//! The routes of a read-and-write S3 library that stage a change for the bucket: chunked
//! uploads, new folders (a zero-byte marker written at once), the size limit in plain words,
//! the `saving` and `failed` marks, and Retry and Discard of a change not yet written.
//!
//! The worker is driven with `write_due(state, now)`; the servers here run no workers.

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
use server::source_sync::sync_once;
use server::source_write::write_due;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

const BUCKET: &str = "routes-bucket-4c9";
const ENDPOINT: &str = "routes-endpoint-8e2";
const KEY_ID: &str = "AKIDROUTESEXAMPLE0071";
const SECRET: &str = "planted-routes-secret-3d5b-zz";
const PREFIX: &str = "team-routes/";
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
        let admin = server.admin("Routes admin").await;
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

    /// A person with `role` on the whole site.
    async fn person(&self, name: &str, role: &str) -> TestUser {
        let user = self.server.user(name).await;
        self.db()
            .set_node_access(
                self.admin.id,
                self.site,
                Principal::User(user.id),
                Some(role),
            )
            .await
            .unwrap();
        user
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

    async fn sync(&self, library: NodeId) {
        let _ = sync_once(&self.state, library, OffsetDateTime::now_utc())
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

    /// Starts an upload session as `user`.
    async fn start(&self, user: &TestUser, target: Value, bytes: &[u8]) -> (StatusCode, Value) {
        let mut body = json!({ "size": bytes.len(), "sha256": sha_hex(bytes) });
        for (name, value) in target.as_object().unwrap() {
            body[name] = value.clone();
        }
        self.call(user, Method::POST, "/api/uploads", Some(body))
            .await
    }

    /// Sends `bytes` of the session `id` in two chunks.
    async fn send(&self, user: &TestUser, id: &str, bytes: &[u8]) {
        let half = bytes.len() / 2;
        for (from, to) in [(0, half), (half, bytes.len())] {
            if from == to {
                continue;
            }
            let response = self
                .server
                .http(Some(user))
                .put(self.server.url(&format!("/api/uploads/{id}")))
                .header(
                    "content-range",
                    format!("bytes {from}-{}/{}", to - 1, bytes.len()),
                )
                .body(bytes.get(from..to).unwrap().to_vec())
                .send()
                .await
                .unwrap();
            let (status, body) = answer(response).await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }
    }

    /// A whole chunked upload of a new file by `user`; the answer of its finish.
    async fn chunked(
        &self,
        user: &TestUser,
        parent: NodeId,
        name: &str,
        bytes: &[u8],
    ) -> (StatusCode, Value) {
        let (status, session) = self
            .start(
                user,
                json!({ "parent": parent.0.to_string(), "name": name }),
                bytes,
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{session}");
        let id = session["id"].as_str().unwrap().to_owned();
        self.send(user, &id, bytes).await;
        self.call(
            user,
            Method::POST,
            &format!("/api/uploads/{id}/finish"),
            None,
        )
        .await
    }

    /// Uploads a new file in one request as `user`, which must be accepted; its node.
    async fn new_file(&self, user: &TestUser, parent: NodeId, name: &str, bytes: &[u8]) -> NodeId {
        let response = self
            .server
            .http(Some(user))
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

    async fn doc_as(&self, user: &TestUser, node: NodeId) -> Value {
        let (status, body) = self
            .call(user, Method::GET, &format!("/api/docs/{}", node.0), None)
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body
    }

    async fn doc(&self, node: NodeId) -> Value {
        self.doc_as(&self.owner, node).await
    }

    async fn make_folder(
        &self,
        user: &TestUser,
        parent: NodeId,
        name: &str,
    ) -> (StatusCode, Value) {
        self.call(
            user,
            Method::POST,
            &format!("/api/nodes/{}/children", parent.0),
            Some(json!({ "kind": "folder", "name": name })),
        )
        .await
    }

    async fn pending(&self, node: NodeId) -> Option<PendingWrite> {
        self.db().pending_write(node).await.unwrap()
    }

    async fn write(&self, now: OffsetDateTime) -> usize {
        write_due(&self.state, now).await
    }

    /// Every write the fake received, but the connect probe's own.
    fn writes(&self) -> Vec<(String, String, String)> {
        self.fake
            .writes()
            .into_iter()
            .filter(|(_, k, _)| !k.contains(".sharecraft-tmp-probe-"))
            .collect()
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

    async fn reserved(&self) -> u64 {
        self.db().site_usage(self.site).await.unwrap().reserved
    }

    /// Makes the write of `node` fail, so it shows as failed.
    async fn fail_the_write_of(&self, node: NodeId, now: OffsetDateTime) {
        self.fake.throttle_next(50);
        self.write(now).await;
        self.fake.throttle_next(0);
        let failed = self.pending(node).await.expect("still pending");
        assert_eq!(failed.state, PendingState::Failed);
    }

    async fn listed(&self, user: &TestUser, parent: NodeId) -> Vec<Value> {
        let (status, body) = self
            .call(
                user,
                Method::GET,
                &format!("/api/nodes/{}/children", parent.0),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["items"].as_array().unwrap().clone()
    }
}

fn retry_path(library: NodeId, node: NodeId) -> String {
    format!(
        "/api/libraries/{}/source/writes/{}/retry",
        library.0, node.0
    )
}

fn discard_path(library: NodeId, node: NodeId) -> String {
    format!("/api/libraries/{}/source/writes/{}", library.0, node.0)
}

#[tokio::test]
async fn a_chunked_upload_is_staged_once_when_finished_and_reaches_the_bucket() {
    let env = Env::new().await;
    let library = env.library("Chunked").await;
    let bytes = b"0123456789 chunked bytes";
    let (status, session) = env
        .start(
            &env.owner,
            json!({ "parent": library.0.to_string(), "name": "big.bin" }),
            bytes,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{session}");
    let id = session["id"].as_str().unwrap().to_owned();
    env.send(&env.owner, &id, bytes).await;
    // Nothing is staged, and nothing is in the bucket, until the session finishes.
    assert!(
        env.db()
            .pending_writes_of(library)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(env.writes().is_empty());

    let (status, body) = env
        .call(
            &env.owner,
            Method::POST,
            &format!("/api/uploads/{id}/finish"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["saving"], true, "{body}");
    let node = id_of(&body);
    let rows = env.db().pending_writes_of(library).await.unwrap();
    assert_eq!(rows.len(), 1, "staged once");
    assert_eq!(rows[0].node, node);
    assert_eq!(rows[0].path, "big.bin");
    assert!(rows[0].flush);
    assert!(rows[0].base_tag.is_none());
    assert!(env.writes().is_empty());
    // The session is over.
    let (status, _) = env
        .call(&env.owner, Method::GET, &format!("/api/uploads/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    assert_eq!(env.write(OffsetDateTime::now_utc()).await, 1);
    assert_eq!(env.object("big.bin").unwrap().bytes, bytes);
    assert_eq!(env.writes_of("big.bin"), [("PUT".into(), "absent".into())]);
    assert!(env.pending(node).await.is_none());
    assert_eq!(env.download(node).await.1, bytes);
}

#[tokio::test]
async fn a_chunked_upload_reserves_no_quota_in_a_full_site() {
    let env = Env::new().await;
    let library = env.library("Full").await;
    env.db()
        .set_site_quota(env.admin.id, env.site, Some(1))
        .await
        .unwrap();
    let bytes = b"much more than one byte";

    // The same upload into an ordinary library of the full site is refused.
    let ordinary = common::Site::library(&env.server).await;
    let (status, body) = env
        .start(
            &env.owner,
            json!({ "parent": ordinary.0.to_string(), "name": "ordinary.bin" }),
            bytes,
        )
        .await;
    assert_eq!(status, StatusCode::INSUFFICIENT_STORAGE, "{body}");

    let before = env.db().site_usage(env.site).await.unwrap();
    let (status, session) = env
        .start(
            &env.owner,
            json!({ "parent": library.0.to_string(), "name": "kept.bin" }),
            bytes,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{session}");
    let id = session["id"].as_str().unwrap().to_owned();
    let stored = env
        .db()
        .upload_session(id.parse().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert!(stored.external, "a session into a bucket is external");
    assert_eq!(env.reserved().await, before.reserved);
    env.send(&env.owner, &id, bytes).await;
    let (status, body) = env
        .call(
            &env.owner,
            Method::POST,
            &format!("/api/uploads/{id}/finish"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let after = env.db().site_usage(env.site).await.unwrap();
    assert_eq!((after.used, after.reserved), (before.used, before.reserved));

    // Abandoning one gives back nothing that was not taken.
    let (status, session) = env
        .start(
            &env.owner,
            json!({ "parent": library.0.to_string(), "name": "dropped.bin" }),
            bytes,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{session}");
    let id = session["id"].as_str().unwrap();
    let (status, _) = env
        .call(
            &env.owner,
            Method::DELETE,
            &format!("/api/uploads/{id}"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let after = env.db().site_usage(env.site).await.unwrap();
    assert_eq!((after.used, after.reserved), (before.used, before.reserved));
}

#[tokio::test]
async fn a_file_over_the_limit_is_refused_in_plain_words_before_any_byte() {
    let env = Env::new().await;
    let library = env.library("Limit").await;
    // Over the default limit: the plain 512 MiB sentence.
    let (status, body) = env
        .call(
            &env.owner,
            Method::POST,
            "/api/uploads",
            Some(json!({
                "parent": library.0.to_string(),
                "name": "huge.bin",
                "size": 513u64 * 1024 * 1024,
                "sha256": "ab".repeat(32),
            })),
        )
        .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    assert_eq!(body["error"], "source_file_too_large", "{body}");
    assert_eq!(body["limit"], 64u64 * 1024 * 1024, "{body}");
    assert_eq!(
        body["message"], "Files over 64 MiB cannot be saved to S3 storage yet.",
        "{body}"
    );

    // A smaller limit is named in words too, and nothing is started.
    env.state.s3().set_single_put_max_for_tests(2048);
    let bytes = vec![7u8; 2049];
    let (status, body) = env
        .start(
            &env.owner,
            json!({ "parent": library.0.to_string(), "name": "bigger.bin" }),
            &bytes,
        )
        .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    assert_eq!(body["error"], "source_file_too_large", "{body}");
    assert_eq!(body["limit"], 2048, "{body}");
    assert_eq!(
        body["message"], "Files over 2 KiB cannot be saved to S3 storage yet.",
        "{body}"
    );
    let open: i64 = sqlx::query_scalar("select count(*) from upload_sessions")
        .fetch_one(env.db().test_pool())
        .await
        .unwrap();
    assert_eq!(open, 0, "no session was started");
    assert!(env.writes().is_empty());

    // A new version too.
    env.fake.put(BUCKET, &key("a.txt"), b"old");
    env.sync(library).await;
    let file = env.child(library, "a.txt").await;
    let (status, body) = env
        .start(
            &env.owner,
            json!({ "doc": file.0.to_string(), "base": env.doc(file).await["revision"] }),
            &bytes,
        )
        .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    assert_eq!(body["error"], "source_file_too_large", "{body}");
}

#[tokio::test]
async fn a_new_version_by_chunked_upload_is_written_through_the_session() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("docs/a.txt"), b"old bytes");
    let library = env.library("Version").await;
    let docs = env.child(library, "docs").await;
    let file = env.child(docs, "a.txt").await;
    let revision = env.doc(file).await["revision"].clone();
    let bytes = b"new bytes by chunks";
    let (status, session) = env
        .start(
            &env.owner,
            json!({ "doc": file.0.to_string(), "base": revision }),
            bytes,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{session}");
    let id = session["id"].as_str().unwrap().to_owned();
    env.send(&env.owner, &id, bytes).await;
    let (status, body) = env
        .call(
            &env.owner,
            Method::POST,
            &format!("/api/uploads/{id}/finish"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["changed"], true, "{body}");
    let pending = env.pending(file).await.expect("staged through the session");
    assert!(pending.base_tag.is_some());
    assert!(pending.flush, "a new version is written at once");
    assert_eq!(env.doc(file).await["revision"], pending.revision.as_str());
    assert!(env.writes().is_empty());

    env.write(OffsetDateTime::now_utc()).await;
    assert_eq!(env.object("docs/a.txt").unwrap().bytes, bytes);
    assert_eq!(
        env.writes_of("docs/a.txt"),
        [("PUT".into(), "match".into())]
    );
    assert!(env.pending(file).await.is_none());
}

#[tokio::test]
async fn creating_a_folder_writes_a_marker_and_tracks_it() {
    let env = Env::new().await;
    let library = env.library("Folders").await;
    let (status, body) = env.make_folder(&env.owner, library, "Reports").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let node = id_of(&body);
    assert_eq!(body["name"], "Reports");
    assert_eq!(env.writes_of("Reports/"), [("PUT".into(), "absent".into())]);
    let marker = env.object("Reports/").expect("the marker is in the bucket");
    assert!(marker.bytes.is_empty());
    let item = env.db().source_item_of(node).await.unwrap().unwrap();
    assert_eq!(item.path, "Reports");
    assert_eq!(item.repository_path, "Reports/");
    assert!(item.blob_sha.is_none(), "a folder has no content");

    // A folder inside it.
    let (status, body) = env.make_folder(&env.owner, node, "2026").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert!(env.object("Reports/2026/").is_some());
    let inner = env
        .db()
        .source_item_of(id_of(&body))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(inner.path, "Reports/2026");
    assert_eq!(inner.repository_path, "Reports/2026/");
}

#[tokio::test]
async fn a_folder_name_the_listing_would_hide_is_refused_and_nothing_is_written() {
    let env = Env::new().await;
    let library = env.library("Hidden").await;
    for name in [".sharecraft-bin", ".sharecraft-tmp-x", "CON", "Reports."] {
        let (status, body) = env.make_folder(&env.owner, library, name).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{name}: {body}");
        assert_eq!(body["error"], "invalid_name", "{name}: {body}");
    }
    assert!(env.writes().is_empty());
    assert!(env.children(library).await.is_empty());
}

#[tokio::test]
async fn a_folder_name_taken_ignoring_case_is_refused_before_the_bucket_is_asked() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("Reports/"), b"");
    env.fake.put(BUCKET, &key("Notes.txt"), b"a file");
    let library = env.library("Taken").await;
    let asked = env.fake.requests().len();
    for name in ["reports", "REPORTS", "notes.TXT"] {
        let (status, body) = env.make_folder(&env.owner, library, name).await;
        assert_eq!(status, StatusCode::CONFLICT, "{name}: {body}");
        assert_eq!(body["error"], "name_taken", "{name}: {body}");
    }
    assert_eq!(env.fake.requests().len(), asked, "the bucket was not asked");
}

#[tokio::test]
async fn a_folder_marker_already_in_the_bucket_is_accepted() {
    let env = Env::new().await;
    let library = env.library("Marked").await;
    // Made in the bucket after the last listing, so the library does not show it yet.
    env.fake.put(BUCKET, &key("Fresh/"), b"");
    let (status, body) = env.make_folder(&env.owner, library, "Fresh").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let node = id_of(&body);
    assert!(env.object("Fresh/").unwrap().bytes.is_empty());
    let item = env.db().source_item_of(node).await.unwrap().unwrap();
    assert_eq!(item.repository_path, "Fresh/");
    // Nothing was overwritten or deleted.
    assert!(env.writes().iter().all(|(method, _, _)| method == "PUT"));
}

#[tokio::test]
async fn a_listing_after_a_new_folder_plans_no_change() {
    let env = Env::new().await;
    let library = env.library("Listed").await;
    let (status, body) = env.make_folder(&env.owner, library, "Reports").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let node = id_of(&body);
    let (status, body) = env.make_folder(&env.owner, node, "Q1").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let inner = id_of(&body);

    let report = sync_once(&env.state, library, OffsetDateTime::now_utc())
        .await
        .unwrap();
    assert!(report.is_none(), "{report:?}");
    // The same nodes, in the same places.
    assert_eq!(env.child(library, "Reports").await, node);
    assert_eq!(env.child(node, "Q1").await, inner);
    assert_eq!(env.children(library).await.len(), 1);
    assert_eq!(env.children(node).await.len(), 1);
}

#[tokio::test]
async fn an_upload_into_a_new_folder_lands_below_its_marker() {
    let env = Env::new().await;
    let library = env.library("Inbox").await;
    let (status, body) = env.make_folder(&env.owner, library, "Inbox").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let folder = id_of(&body);

    let file = env.new_file(&env.owner, folder, "one.txt", b"one").await;
    let (status, body) = env
        .chunked(&env.owner, folder, "two.txt", b"two by chunks")
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let second = id_of(&body);
    assert_eq!(env.pending(file).await.unwrap().path, "Inbox/one.txt");
    assert_eq!(env.pending(second).await.unwrap().path, "Inbox/two.txt");

    env.write(OffsetDateTime::now_utc()).await;
    assert_eq!(env.object("Inbox/one.txt").unwrap().bytes, b"one");
    assert_eq!(env.object("Inbox/two.txt").unwrap().bytes, b"two by chunks");
    assert!(env.object("Inbox/").is_some(), "the marker stays");
    assert_eq!(env.child(folder, "one.txt").await, file);
}

#[tokio::test]
async fn retry_makes_a_failed_write_due_now() {
    let env = Env::new().await;
    let library = env.library("Retry").await;
    let node = env
        .new_file(&env.owner, library, "later.txt", b"later")
        .await;
    let now = OffsetDateTime::now_utc();
    env.fail_the_write_of(node, now).await;
    let failed = env.pending(node).await.unwrap();
    assert!(failed.next_try_at.unwrap() > now + time::Duration::seconds(20));
    // Not before its time.
    assert_eq!(env.write(now + time::Duration::seconds(5)).await, 0);
    assert!(env.object("later.txt").is_none());

    let (status, body) = env
        .call(&env.owner, Method::POST, &retry_path(library, node), None)
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let row = env.pending(node).await.unwrap();
    assert_eq!(row.state, PendingState::Scanning);
    assert!(row.next_try_at.unwrap() <= OffsetDateTime::now_utc());
    assert!(row.flush);

    env.write(OffsetDateTime::now_utc()).await;
    assert_eq!(env.object("later.txt").unwrap().bytes, b"later");
    assert!(env.pending(node).await.is_none());
}

#[tokio::test]
async fn discard_returns_a_replaced_file_to_the_bucket_content() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("docs/a.txt"), b"bucket content");
    let library = env.library("Discard").await;
    let docs = env.child(library, "docs").await;
    let file = env.child(docs, "a.txt").await;
    let before = env.doc(file).await["revision"].clone();
    env.replace(docs, "a.txt", b"my unwanted edit").await;
    let staged = env.pending(file).await.unwrap();
    let during = env.doc(file).await["revision"].clone();
    assert_eq!(during, staged.revision.as_str());
    assert_eq!(env.download(file).await.1, b"my unwanted edit");

    let (status, body) = env
        .call(
            &env.owner,
            Method::DELETE,
            &discard_path(library, file),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert!(env.pending(file).await.is_none());
    let after = env.doc(file).await;
    assert_ne!(after["revision"], during);
    assert_ne!(after["revision"], before, "a new revision");
    assert!(after.get("writes").is_none_or(Value::is_null), "{after}");
    assert_eq!(env.download(file).await.1, b"bucket content");
    assert!(env.writes().is_empty(), "nothing reached the bucket");
    let events = env.events_of("source.write_discarded").await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["node"], file.0.to_string());
    // The worker has nothing left to write.
    assert_eq!(env.write(OffsetDateTime::now_utc()).await, 0);
    assert!(env.writes().is_empty());
}

#[tokio::test]
async fn discard_of_a_new_file_removes_it() {
    let env = Env::new().await;
    let library = env.library("Gone").await;
    let node = env
        .new_file(&env.owner, library, "never.txt", b"never")
        .await;
    let (status, body) = env
        .call(
            &env.owner,
            Method::DELETE,
            &discard_path(library, node),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert!(env.pending(node).await.is_none());
    assert!(env.children(library).await.is_empty(), "the node is gone");
    assert!(env.db().node(node).await.unwrap().unwrap().deleted);
    assert_eq!(env.write(OffsetDateTime::now_utc()).await, 0);
    assert!(env.writes().is_empty());
    env.events_of("source.write_discarded").await;
}

#[tokio::test]
async fn only_the_author_and_owners_may_retry_or_discard() {
    let env = Env::new().await;
    let library = env.library("Who").await;
    let eve = env.person("Eve Editor", "editor").await;
    let dan = env.person("Dan Editor", "editor").await;
    let vic = env.person("Vic Viewer", "viewer").await;
    let stranger = env.server.user("Stranger").await;
    let node = env.new_file(&eve, library, "eves.txt", b"eve").await;

    for (user, expected) in [
        (&dan, StatusCode::FORBIDDEN),
        (&vic, StatusCode::FORBIDDEN),
        (&stranger, StatusCode::NOT_FOUND),
    ] {
        let (status, body) = env
            .call(user, Method::POST, &retry_path(library, node), None)
            .await;
        assert_eq!(status, expected, "retry as {}: {body}", user.email);
        let (status, body) = env
            .call(user, Method::DELETE, &discard_path(library, node), None)
            .await;
        assert_eq!(status, expected, "discard as {}: {body}", user.email);
    }
    assert!(env.pending(node).await.is_some(), "nothing was discarded");

    // The author, and an owner.
    let (status, body) = env
        .call(&eve, Method::POST, &retry_path(library, node), None)
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let (status, body) = env
        .call(&env.owner, Method::POST, &retry_path(library, node), None)
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let (status, body) = env
        .call(&eve, Method::DELETE, &discard_path(library, node), None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    let other = env.new_file(&eve, library, "more.txt", b"more").await;
    let (status, body) = env
        .call(
            &env.owner,
            Method::DELETE,
            &discard_path(library, other),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    // A node with no pending change.
    env.fake.put(BUCKET, &key("plain.txt"), b"plain");
    env.sync(library).await;
    let plain = env.child(library, "plain.txt").await;
    for method in [Method::POST, Method::DELETE] {
        let path = if method == Method::POST {
            retry_path(library, plain)
        } else {
            discard_path(library, plain)
        };
        let (status, body) = env.call(&env.owner, method, &path, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    }
}

#[tokio::test]
async fn the_listing_and_the_file_answer_show_saving_and_failed() {
    let env = Env::new().await;
    let library = env.library("Marks").await;
    let eve = env.person("Eve Editor", "editor").await;
    let dan = env.person("Dan Editor", "editor").await;
    env.fake.put(BUCKET, &key("plain.txt"), b"plain");
    env.sync(library).await;
    let node = env.new_file(&eve, library, "eves.txt", b"eve").await;

    let mark = |items: &[Value], name: &str| {
        items
            .iter()
            .find(|item| item["name"] == name)
            .unwrap_or_else(|| panic!("{name} is not listed"))["saving"]
            .clone()
    };
    let items = env.listed(&env.owner, library).await;
    assert_eq!(mark(&items, "eves.txt"), "saving");
    assert!(mark(&items, "plain.txt").is_null(), "{items:?}");
    let detail = env.doc(node).await;
    assert_eq!(detail["writes"]["state"], "saving", "{detail}");
    assert!(detail["writes"]["error"].is_null(), "{detail}");
    assert!(env.doc(env.child(library, "plain.txt").await).await["writes"].is_null());

    let now = OffsetDateTime::now_utc();
    env.fail_the_write_of(node, now).await;
    let items = env.listed(&dan, library).await;
    assert_eq!(mark(&items, "eves.txt"), "failed");
    for (user, sees_error) in [(&eve, true), (&env.owner, true), (&dan, false)] {
        let detail = env.doc_as(user, node).await;
        assert_eq!(detail["writes"]["state"], "failed", "{detail}");
        let error = &detail["writes"]["error"];
        if sees_error {
            let text = error.as_str().expect("the error is shown");
            assert!(!text.is_empty() && text.chars().count() <= 300, "{text}");
        } else {
            assert!(error.is_null(), "{detail}");
        }
    }

    // Written: no mark any more.
    env.write(now + time::Duration::seconds(31)).await;
    let items = env.listed(&env.owner, library).await;
    assert!(mark(&items, "eves.txt").is_null(), "{items:?}");
    assert!(env.doc(node).await["writes"].is_null());
}

#[tokio::test]
async fn a_viewer_cannot_upload_or_make_a_folder_and_the_bucket_is_untouched() {
    let env = Env::new().await;
    let library = env.library("Viewers").await;
    env.fake.put(BUCKET, &key("a.txt"), b"a");
    env.sync(library).await;
    let file = env.child(library, "a.txt").await;
    let vic = env.person("Vic Viewer", "viewer").await;
    let asked = env.fake.requests().len();

    let (status, body) = env.make_folder(&vic, library, "Nope").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body) = env
        .start(
            &vic,
            json!({ "parent": library.0.to_string(), "name": "nope.txt" }),
            b"nope",
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body) = env
        .start(
            &vic,
            json!({ "doc": file.0.to_string(), "base": env.doc(file).await["revision"] }),
            b"nope",
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    assert!(env.writes().is_empty());
    assert_eq!(env.fake.requests().len(), asked, "the bucket was not asked");
    assert!(
        env.db()
            .pending_writes_of(library)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(env.children(library).await.len(), 1);
}
