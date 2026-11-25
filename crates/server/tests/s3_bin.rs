//! Deleting inside a read-and-write S3 library: the objects go to the library's holding folder
//! (`<prefix>.sharecraft-bin/<date>/<entry>/<path>`) and the item to the recycle bin; Restore
//! brings them back (numbered when the name is taken); Purge and the 93-day purge delete the
//! held objects first (and leave them in a read-only library); the daily holding sweep deletes
//! only what no entry names.
//!
//! The servers here run no workers; listings are driven with `sync_once`, purges with
//! `purge_due` and the holding sweep with `sweep_holds`, each at a `now` the test gives.

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
use records::{Principal, S3_ENDPOINTS, SourceMode};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use server::AppState;
use server::s3_write::holding_root;
use server::source_sync::sync_once;
use server::source_write::sweep_holds;
use time::OffsetDateTime;
use uuid::Uuid;

const BUCKET: &str = "bin-bucket-7c2";
const ENDPOINT: &str = "bin-endpoint-3d9";
const KEY_ID: &str = "AKIDBINEXAMPLE0005";
const SECRET: &str = "planted-bin-secret-4e2a-zz";
const PREFIX: &str = "team-bin/";

const ITEM_SAVING: &str = "This is still being saved to the storage. Try again when saving has finished, or discard the save that failed.";

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

fn assert_error(answer: &(StatusCode, Value), status: StatusCode, code: &str) {
    assert_eq!(answer.0, status, "{}", answer.1);
    assert_eq!(answer.1["error"], code, "{}", answer.1);
}

/// The holding key of `path` for the entry `entry` deleted today.
fn held_key(entry: NodeId, path: &str) -> String {
    let root = holding_root(OffsetDateTime::now_utc().date(), entry);
    key(&format!("{root}{path}"))
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
        let admin = server.admin("Bin admin").await;
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
        state
            .s3()
            .set_timeouts_for_tests(Duration::from_secs(2), Duration::from_secs(2));
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

    /// A read-and-write library over [`PREFIX`], listed once, its wakes forgotten.
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
        self.state.sources().take_wake_for_tests(library);
        library
    }

    /// Lists the library once; whether the listing changed anything.
    async fn sync(&self, library: NodeId) -> bool {
        sync_once(&self.state, library, OffsetDateTime::now_utc())
            .await
            .unwrap()
            .is_some()
    }

    /// Lists the library once and fails unless it added, changed, removed or moved nothing
    /// (its counts of files and bytes may follow a delete or a restore).
    async fn plans_nothing(&self, library: NodeId) {
        if let Some(report) = sync_once(&self.state, library, OffsetDateTime::now_utc())
            .await
            .unwrap()
        {
            assert_eq!(
                (report.added, report.changed, report.removed, report.moved),
                (0, 0, 0, 0),
                "the listing changed something"
            );
        }
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

    async fn delete(&self, node: NodeId) -> (StatusCode, Value) {
        self.call(
            &self.owner,
            Method::DELETE,
            &format!("/api/nodes/{}", node.0),
            None,
        )
        .await
    }

    async fn restore(&self, entry: NodeId) -> (StatusCode, Value) {
        self.call(
            &self.owner,
            Method::POST,
            &format!("/api/bin/{}/restore", entry.0),
            None,
        )
        .await
    }

    /// `DELETE /api/bin/{id}` as the site's owner: to the second stage, or purged from it.
    async fn remove(&self, entry: NodeId) -> (StatusCode, Value) {
        self.call(
            &self.owner,
            Method::DELETE,
            &format!("/api/bin/{}", entry.0),
            None,
        )
        .await
    }

    fn object(&self, path: &str) -> Option<Stored> {
        self.fake.object(BUCKET, &key(path))
    }

    fn held(&self, entry: NodeId, path: &str) -> Option<Stored> {
        self.fake.object(BUCKET, &held_key(entry, path))
    }

    /// Every write the fake received, but the connect probe's own.
    fn writes(&self) -> Vec<(String, String, String)> {
        self.fake
            .writes()
            .into_iter()
            .filter(|(_, k, _)| !k.contains(".sharecraft-tmp-probe-"))
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

    async fn in_bin(&self, entry: NodeId) -> bool {
        self.db().bin_state(entry).await.unwrap().is_some()
    }

    async fn content(&self, node: NodeId) -> Vec<u8> {
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
}

#[tokio::test]
async fn deleting_copies_to_the_holding_prefix_then_deletes() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("docs/"), b"");
    env.fake.put(BUCKET, &key("docs/a.txt"), b"alpha");
    let library = env.library("Deletes").await;
    let docs = env.child(library, "docs").await;
    let file = env.child(docs, "a.txt").await;

    let (status, body) = env.delete(file).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    assert!(env.object("docs/a.txt").is_none());
    assert_eq!(env.held(file, "docs/a.txt").unwrap().bytes, b"alpha");
    // The copy first, then the delete of the original.
    let writes = env.writes();
    let held = held_key(file, "docs/a.txt");
    assert_eq!(
        writes
            .iter()
            .map(|(method, k, _)| (method.as_str(), k.as_str()))
            .collect::<Vec<_>>(),
        vec![
            ("COPY", held.as_str()),
            ("DELETE", key("docs/a.txt").as_str())
        ],
        "{writes:?}"
    );
    // In the recycle bin as an entry of its own, with the held root recorded.
    assert!(env.in_bin(file).await);
    let held = env.db().held(file).await.unwrap().expect("held");
    assert_eq!(held.library, library);
    assert_eq!(
        held.root,
        holding_root(OffsetDateTime::now_utc().date(), file)
    );
    assert_eq!(held.objects, 1);
    let events = env.events_of("source.held").await;
    assert_eq!(events[0]["objects"], 1, "{events:?}");
}

#[tokio::test]
async fn the_holding_prefix_never_appears_in_the_library() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("docs/"), b"");
    env.fake.put(BUCKET, &key("docs/a.txt"), b"alpha");
    env.fake.put(BUCKET, &key("docs/b.txt"), b"beta");
    let library = env.library("Hidden").await;
    let docs = env.child(library, "docs").await;
    let file = env.child(docs, "a.txt").await;
    let (status, body) = env.delete(file).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    env.plans_nothing(library).await;
    let top: Vec<String> = env
        .children(library)
        .await
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    assert_eq!(top, vec!["docs".to_owned()]);
    let inside: Vec<String> = env
        .children(docs)
        .await
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    assert_eq!(inside, vec!["b.txt".to_owned()]);
    // Still in the bin after the listing, and still restorable.
    assert!(env.in_bin(file).await);
    assert!(env.db().held(file).await.unwrap().is_some());
}

#[tokio::test]
async fn the_deleted_file_is_in_the_bin_with_its_name_and_size() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("docs/"), b"");
    env.fake
        .put(BUCKET, &key("docs/report.txt"), b"twelve bytes");
    let library = env.library("Sizes").await;
    let docs = env.child(library, "docs").await;
    let file = env.child(docs, "report.txt").await;
    let (status, body) = env.delete(file).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    // A listing in between changes nothing the bin shows.
    env.sync(library).await;

    let (status, page) = env
        .call(
            &env.owner,
            Method::GET,
            &format!("/api/sites/{}/bin", env.site.0),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let item = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == file.0.to_string())
        .unwrap_or_else(|| panic!("not in the bin: {page}"));
    assert_eq!(item["name"], "report.txt");
    assert_eq!(item["size"], 12, "{item}");
    assert_eq!(item["kind"], "document", "{item}");
    assert_eq!(item["path"].as_array().unwrap().last().unwrap(), "docs");
}

#[tokio::test]
async fn restore_brings_bytes_node_and_permissions_back() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("docs/"), b"");
    env.fake.put(BUCKET, &key("docs/a.txt"), b"alpha");
    let library = env.library("Restores").await;
    let docs = env.child(library, "docs").await;
    let file = env.child(docs, "a.txt").await;
    let reader = env.server.user("Reader").await;
    env.db()
        .set_node_access(
            env.admin.id,
            file,
            Principal::User(reader.id),
            Some("viewer"),
        )
        .await
        .unwrap();
    let (status, body) = env.delete(file).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    let (status, body) = env.restore(file).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(id_of(&body["node"]), file);
    assert!(body["renamed"].is_null(), "{body}");

    assert_eq!(env.object("docs/a.txt").unwrap().bytes, b"alpha");
    assert!(env.held(file, "docs/a.txt").is_none());
    assert_eq!(env.child(docs, "a.txt").await, file);
    assert!(!env.in_bin(file).await);
    assert!(env.db().held(file).await.unwrap().is_none());
    assert!(
        env.db()
            .effective_access(reader.id, file)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(env.content(file).await, b"alpha");
    env.events_of("source.unheld").await;
    // The next listing finds nothing new.
    env.plans_nothing(library).await;
    assert_eq!(env.child(docs, "a.txt").await, file);
}

#[tokio::test]
async fn restore_when_the_name_is_taken_uses_a_numbered_name() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("docs/"), b"");
    env.fake.put(BUCKET, &key("docs/a.txt"), b"first");
    let library = env.library("Taken").await;
    let docs = env.child(library, "docs").await;
    let file = env.child(docs, "a.txt").await;
    let (status, body) = env.delete(file).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    // A new file of that name, listed.
    env.fake.put(BUCKET, &key("docs/A.TXT"), b"second");
    assert!(env.sync(library).await);
    let newer = env.child(docs, "A.TXT").await;

    let (status, body) = env.restore(file).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(id_of(&body["node"]), file);
    assert_eq!(body["node"]["name"], "a (2).txt", "{body}");
    assert_eq!(body["renamed"]["from"], "a.txt", "{body}");
    assert_eq!(body["renamed"]["to"], "a (2).txt", "{body}");

    assert_eq!(env.object("docs/a (2).txt").unwrap().bytes, b"first");
    assert_eq!(env.object("docs/A.TXT").unwrap().bytes, b"second");
    assert_eq!(env.child(docs, "a (2).txt").await, file);
    assert_eq!(env.child(docs, "A.TXT").await, newer);
    env.plans_nothing(library).await;
    assert_eq!(env.content(file).await, b"first");
}

#[tokio::test]
async fn restore_onto_a_name_taken_in_the_bucket_but_not_yet_listed_uses_a_numbered_name() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("docs/"), b"");
    env.fake.put(BUCKET, &key("docs/a.txt"), b"first");
    let library = env.library("Unlisted").await;
    let docs = env.child(library, "docs").await;
    let file = env.child(docs, "a.txt").await;
    let (status, body) = env.delete(file).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    // Written outside, not listed yet: the tree has the name free.
    env.fake.put(BUCKET, &key("docs/a.txt"), b"outside");

    let (status, body) = env.restore(file).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["node"]["name"], "a (2).txt", "{body}");
    assert_eq!(body["renamed"]["from"], "a.txt", "{body}");
    assert_eq!(env.object("docs/a.txt").unwrap().bytes, b"outside");
    assert_eq!(env.object("docs/a (2).txt").unwrap().bytes, b"first");
    assert_eq!(env.child(docs, "a (2).txt").await, file);

    // The outside file comes in as a new one; the restored one keeps its node.
    assert!(env.sync(library).await);
    assert_eq!(env.child(docs, "a (2).txt").await, file);
    assert_ne!(env.child(docs, "a.txt").await, file);
    env.plans_nothing(library).await;
}

#[tokio::test]
async fn restoring_a_folder_writes_its_marker_and_every_file() {
    let env = Env::new().await;
    // No marker: the folder is only the files under it.
    env.fake.put(BUCKET, &key("docs/a.txt"), b"a");
    env.fake.put(BUCKET, &key("docs/sub/b.txt"), b"b");
    env.fake.put(BUCKET, &key("other/c.txt"), b"c");
    let library = env.library("Folders").await;
    let docs = env.child(library, "docs").await;
    let a = env.child(docs, "a.txt").await;
    let sub = env.child(docs, "sub").await;
    let b = env.child(sub, "b.txt").await;

    let (status, body) = env.delete(docs).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert!(env.object("docs/a.txt").is_none());
    assert!(env.object("docs/sub/b.txt").is_none());
    assert_eq!(env.held(docs, "docs/a.txt").unwrap().bytes, b"a");
    assert_eq!(env.held(docs, "docs/sub/b.txt").unwrap().bytes, b"b");
    assert_eq!(env.db().held(docs).await.unwrap().unwrap().objects, 2);
    env.plans_nothing(library).await;

    let (status, body) = env.restore(docs).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(env.object("docs/").is_some(), "no marker");
    assert_eq!(env.object("docs/a.txt").unwrap().bytes, b"a");
    assert_eq!(env.object("docs/sub/b.txt").unwrap().bytes, b"b");
    assert!(env.held(docs, "docs/a.txt").is_none());
    assert_eq!(env.child(library, "docs").await, docs);
    assert_eq!(env.child(docs, "a.txt").await, a);
    assert_eq!(env.child(docs, "sub").await, sub);
    assert_eq!(env.child(sub, "b.txt").await, b);
    env.plans_nothing(library).await;
    assert_eq!(env.child(sub, "b.txt").await, b);
}

#[tokio::test]
async fn purging_an_entry_removes_the_held_objects_only() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("docs/"), b"");
    env.fake.put(BUCKET, &key("docs/a.txt"), b"a");
    env.fake.put(BUCKET, &key("docs/b.txt"), b"b");
    env.fake.put(BUCKET, &key("docs/c.txt"), b"c");
    let library = env.library("Purges").await;
    let docs = env.child(library, "docs").await;
    let a = env.child(docs, "a.txt").await;
    let b = env.child(docs, "b.txt").await;
    for node in [a, b] {
        let (status, body) = env.delete(node).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    }

    // To the second stage: nothing is deleted.
    let (status, body) = env.remove(a).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert!(env.held(a, "docs/a.txt").is_some());
    // Out of it: purged, its held objects deleted, nothing else.
    let (status, body) = env.remove(a).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert!(!env.in_bin(a).await);
    assert!(env.held(a, "docs/a.txt").is_none());
    assert!(env.db().held(a).await.unwrap().is_none());
    assert_eq!(env.held(b, "docs/b.txt").unwrap().bytes, b"b");
    assert_eq!(env.object("docs/c.txt").unwrap().bytes, b"c");
    assert!(env.object("docs/").is_some());
    env.events_of("source.held_purged").await;
}

#[tokio::test]
async fn purge_while_offline_answers_503_and_keeps_the_entry() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("docs/"), b"");
    env.fake.put(BUCKET, &key("docs/a.txt"), b"a");
    let library = env.library("Offline").await;
    let docs = env.child(library, "docs").await;
    let a = env.child(docs, "a.txt").await;
    let (status, body) = env.delete(a).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, body) = env.remove(a).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    env.fake.throttle_next(10_000);
    let refused = env.remove(a).await;
    env.fake.throttle_next(0);
    assert_error(
        &refused,
        StatusCode::SERVICE_UNAVAILABLE,
        "source_unavailable",
    );
    assert_eq!(refused.1["reason"], "offline", "{}", refused.1);
    assert!(env.in_bin(a).await);
    assert!(env.db().held(a).await.unwrap().is_some());
    assert_eq!(env.held(a, "docs/a.txt").unwrap().bytes, b"a");

    // Online again, the purge goes through.
    let (status, body) = env.remove(a).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert!(!env.in_bin(a).await);
    assert!(env.held(a, "docs/a.txt").is_none());
}

#[tokio::test]
async fn the_93_day_purge_removes_held_objects_and_retries_an_hour_later_when_offline() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("docs/"), b"");
    env.fake.put(BUCKET, &key("docs/a.txt"), b"a");
    let library = env.library("Ninety-three").await;
    let docs = env.child(library, "docs").await;
    let a = env.child(docs, "a.txt").await;
    let (status, body) = env.delete(a).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let due = OffsetDateTime::now_utc() + time::Duration::days(94);

    // Offline: nothing is purged, and the entry waits an hour.
    env.fake.throttle_next(10_000);
    assert_eq!(server::tree::purge_due(&env.state, due).await, 0);
    env.fake.throttle_next(0);
    assert!(env.in_bin(a).await);
    let held = env.db().held(a).await.unwrap().expect("held");
    let retry = held.retry_at.expect("a retry time");
    assert!(
        (retry - (due + time::Duration::hours(1))).abs() < time::Duration::seconds(1),
        "{retry}"
    );

    // Before the hour has passed, the bucket is not asked again.
    let asked = env.fake.requests().len();
    assert_eq!(
        server::tree::purge_due(&env.state, due + time::Duration::minutes(30)).await,
        0
    );
    assert_eq!(env.fake.requests().len(), asked);
    assert!(env.in_bin(a).await);

    // After it, the held objects go and then the entry.
    assert_eq!(
        server::tree::purge_due(&env.state, due + time::Duration::minutes(61)).await,
        1
    );
    assert!(!env.in_bin(a).await);
    assert!(env.held(a, "docs/a.txt").is_none());
    assert!(env.db().held(a).await.unwrap().is_none());
    env.events_of("source.held_purged").await;
}

#[tokio::test]
async fn purging_in_a_read_only_library_leaves_the_objects_and_says_so() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("docs/"), b"");
    env.fake.put(BUCKET, &key("docs/a.txt"), b"a");
    let library = env.library("Read only later").await;
    let docs = env.child(library, "docs").await;
    let a = env.child(docs, "a.txt").await;
    let (status, body) = env.delete(a).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    env.db()
        .set_source_mode(library, SourceMode::ReadOnly)
        .await
        .unwrap();
    let before = env.writes().len();

    for _ in 0..2 {
        let (status, body) = env.remove(a).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    }
    assert!(!env.in_bin(a).await);
    assert!(env.db().held(a).await.unwrap().is_none());
    assert_eq!(env.held(a, "docs/a.txt").unwrap().bytes, b"a");
    assert_eq!(env.writes().len(), before, "{:?}", env.writes());
    env.events_of("source.held_left").await;
}

#[tokio::test]
async fn restore_in_a_read_only_library_is_refused() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("docs/"), b"");
    env.fake.put(BUCKET, &key("docs/a.txt"), b"a");
    let library = env.library("Read only restore").await;
    let docs = env.child(library, "docs").await;
    let a = env.child(docs, "a.txt").await;
    let (status, body) = env.delete(a).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    env.db()
        .set_source_mode(library, SourceMode::ReadOnly)
        .await
        .unwrap();
    let before = env.writes().len();

    assert_error(
        &env.restore(a).await,
        StatusCode::CONFLICT,
        "source_read_only",
    );
    assert!(env.in_bin(a).await);
    assert!(env.db().held(a).await.unwrap().is_some());
    assert_eq!(env.held(a, "docs/a.txt").unwrap().bytes, b"a");
    assert_eq!(env.writes().len(), before);
}

#[tokio::test]
async fn deleting_a_folder_over_the_cap_is_refused() {
    let env = Env::new().await;
    // A marker and a thousand files: 1,001 objects.
    env.fake.put(BUCKET, &key("big/"), b"");
    for i in 0..1000 {
        env.fake.put(BUCKET, &key(&format!("big/{i:04}.txt")), b"x");
    }
    let library = env.library("Big").await;
    let big = env.child(library, "big").await;

    let refused = env.delete(big).await;
    assert_error(&refused, StatusCode::CONFLICT, "source_too_many_objects");
    assert_eq!(refused.1["count"], 1001);
    assert!(env.writes().is_empty(), "{:?}", env.writes());
    assert_eq!(env.child(library, "big").await, big);
    assert!(!env.in_bin(big).await);
}

#[tokio::test]
async fn a_file_with_a_pending_save_cannot_be_deleted() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("docs/a.txt"), b"a");
    let library = env.library("Saving").await;
    let docs = env.child(library, "docs").await;
    let file = env.child(docs, "a.txt").await;
    let response = env
        .server
        .http(Some(&env.owner))
        .post(env.server.url("/api/docs/upload"))
        .query(&[
            ("parent", docs.0.to_string()),
            ("name", "a.txt".to_owned()),
            ("on_conflict", "replace".to_owned()),
        ])
        .body(b"newer".to_vec())
        .send()
        .await
        .unwrap();
    let (status, body) = answer(response).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(env.db().pending_write(file).await.unwrap().is_some());

    for refused in [env.delete(file).await, env.delete(docs).await] {
        assert_error(&refused, StatusCode::CONFLICT, "source_item_saving");
        assert_eq!(refused.1["message"], ITEM_SAVING);
    }
    assert!(env.writes().is_empty(), "{:?}", env.writes());
    assert!(!env.in_bin(file).await);
    assert!(!env.in_bin(docs).await);
}

#[tokio::test]
async fn the_daily_sweep_runs_once_a_day() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("docs/"), b"");
    env.fake.put(BUCKET, &key("docs/a.txt"), b"a");
    let library = env.library("Sweeps").await;
    let docs = env.child(library, "docs").await;
    let a = env.child(docs, "a.txt").await;
    let (status, body) = env.delete(a).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let now = OffsetDateTime::now_utc();
    let old = (now - time::Duration::days(100)).date();
    let recent = (now - time::Duration::days(10)).date();
    // Old and named by no entry: swept. Recent, or named by an entry: kept.
    let orphan = holding_root(old, NodeId(Uuid::new_v4()));
    env.fake
        .put(BUCKET, &key(&format!("{orphan}docs/gone.txt")), b"gone");
    let young = holding_root(recent, NodeId(Uuid::new_v4()));
    env.fake
        .put(BUCKET, &key(&format!("{young}docs/young.txt")), b"young");
    let named = holding_root(old, a);
    env.fake
        .put(BUCKET, &key(&format!("{named}docs/named.txt")), b"named");

    sweep_holds(&env.state, now).await;
    assert!(env.object(&format!("{orphan}docs/gone.txt")).is_none());
    assert!(env.object(&format!("{young}docs/young.txt")).is_some());
    assert!(env.object(&format!("{named}docs/named.txt")).is_some());
    assert_eq!(env.held(a, "docs/a.txt").unwrap().bytes, b"a");
    assert!(env.object("docs/").is_some());
    let events = env.events_of("source.holding_swept").await;
    assert_eq!(events[0]["count"], 1, "{events:?}");

    // Within the day it does not run again; a day later it does.
    let another = holding_root(old, NodeId(Uuid::new_v4()));
    env.fake
        .put(BUCKET, &key(&format!("{another}late.txt")), b"late");
    sweep_holds(&env.state, now + time::Duration::hours(23)).await;
    assert!(env.object(&format!("{another}late.txt")).is_some());
    sweep_holds(&env.state, now + time::Duration::hours(25)).await;
    assert!(env.object(&format!("{another}late.txt")).is_none());
}
