//! Copying inside a read-and-write S3 library: every object copied in the bucket by the store
//! itself (no byte downloaded), the copies made new nodes with fresh revisions and the tags the
//! store answered, a taken name and a folder over the cap refused before anything is written, and
//! the next listing changing nothing.
//!
//! The servers here run no workers; listings are driven with `sync_once`.

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
use records::{Principal, S3_ENDPOINTS, WriteState};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use server::AppState;
use server::source_sync::sync_once;
use server::source_write::write_due;
use time::OffsetDateTime;

const BUCKET: &str = "copy-bucket-3d9";
const ENDPOINT: &str = "copy-endpoint-8b4";
const KEY_ID: &str = "AKIDCOPYEXAMPLE0007";
const SECRET: &str = "planted-copy-secret-4e2a-zz";
const PREFIX: &str = "team-copy/";

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

struct Env {
    server: TestServer,
    fake: FakeS3,
    state: AppState,
    admin: TestUser,
    owner: TestUser,
    site: NodeId,
    /// The site's ordinary library.
    ordinary: NodeId,
}

impl Env {
    async fn new() -> Env {
        let fake = FakeS3::start().await;
        fake.bucket(BUCKET);
        fake.require_keys(KEY_ID, SECRET);
        let server = spawn_server_config_without_workers(|_| {}).await;
        let admin = server.admin("Structure admin").await;
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
            ordinary: library,
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

    /// A library over `prefix` connected by the administrator in `mode`, listed once, its
    /// wakes forgotten.
    async fn library_at(&self, name: &str, prefix: &str, mode: &str) -> NodeId {
        let (status, answer) = self
            .call(
                &self.admin,
                Method::POST,
                &format!("/api/sites/{}/libraries/s3", self.site.0),
                Some(json!({
                    "name": name,
                    "endpoint": ENDPOINT,
                    "bucket": BUCKET,
                    "prefix": prefix,
                    "access_key_id": KEY_ID,
                    "secret_access_key": SECRET,
                    "mode": mode,
                })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{answer}");
        let library = id_of(&answer);
        self.sync(library).await;
        self.state.sources().take_wake_for_tests(library);
        library
    }

    async fn library(&self, name: &str) -> NodeId {
        self.library_at(name, PREFIX, "read_write").await
    }

    async fn sync(&self, library: NodeId) -> bool {
        sync_once(&self.state, library, OffsetDateTime::now_utc())
            .await
            .unwrap()
            .is_some()
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

    async fn copy(&self, node: NodeId, parent: NodeId, name: Option<&str>) -> (StatusCode, Value) {
        let mut body = json!({ "parent": parent.0.to_string() });
        if let Some(name) = name {
            body["name"] = json!(name);
        }
        self.call(
            &self.owner,
            Method::POST,
            &format!("/api/nodes/{}/copy", node.0),
            Some(body),
        )
        .await
    }

    fn object(&self, path: &str) -> Option<Stored> {
        self.fake.object(BUCKET, &key(path))
    }

    /// Every write the fake received, but the connect probe's own.
    fn writes(&self) -> Vec<(String, String, String)> {
        self.fake
            .writes()
            .into_iter()
            .filter(|(_, k, _)| !k.contains(".sharecraft-tmp-probe-"))
            .collect()
    }

    async fn repository_path(&self, node: NodeId) -> String {
        self.db()
            .source_item_of(node)
            .await
            .unwrap()
            .expect("tracked")
            .repository_path
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

    /// Uploads `bytes` as `name` into `parent` as the owner; the new file.
    async fn upload(&self, parent: NodeId, name: &str, bytes: &[u8]) -> NodeId {
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

    /// The bytes of the file `node`, as the owner downloads them.
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

    /// The bytes the site uses.
    async fn used(&self) -> u64 {
        self.db().site_usage(self.site).await.unwrap().used
    }

    /// Runs the write worker once, as it would run a minute from now.
    async fn write(&self) -> usize {
        write_due(
            &self.state,
            OffsetDateTime::now_utc() + time::Duration::minutes(1),
        )
        .await
    }

    /// The revision the tracked file `node` is at.
    async fn revision(&self, node: NodeId) -> String {
        self.db()
            .external_item(node)
            .await
            .unwrap()
            .expect("tracked")
            .revision
    }
}

fn assert_error(answer: &(StatusCode, Value), status: StatusCode, code: &str) {
    assert_eq!(answer.0, status, "{}", answer.1);
    assert_eq!(answer.1["error"], code, "{}", answer.1);
}

#[tokio::test]
async fn copying_within_an_s3_library_is_a_server_side_copy_with_no_get() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("docs/a.txt"), b"alpha");
    env.fake.put(BUCKET, &key("other/x.txt"), b"x");
    let library = env.library("Copies").await;
    let docs = env.child(library, "docs").await;
    let other = env.child(library, "other").await;
    let file = env.child(docs, "a.txt").await;
    let gets = env.fake.count("get");

    let (status, body) = env.copy(file, other, None).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let copy = id_of(&body["node"]);
    assert_ne!(copy, file);
    assert_eq!(body["node"]["name"], "a.txt");

    // The store copied it; the original stays; nothing was downloaded.
    assert_eq!(env.object("docs/a.txt").unwrap().bytes, b"alpha");
    assert_eq!(env.object("other/a.txt").unwrap().bytes, b"alpha");
    assert_eq!(env.fake.count("get"), gets, "a byte was downloaded");
    assert_eq!(env.child(other, "a.txt").await, copy);
    assert_eq!(env.child(docs, "a.txt").await, file);

    // A new node of its own: tracked at its path, with a revision of its own.
    assert_eq!(env.repository_path(copy).await, "other/a.txt");
    assert_eq!(env.repository_path(file).await, "docs/a.txt");
    assert_ne!(env.revision(copy).await, env.revision(file).await);
    let events = env.events_of("source.moved").await;
    assert_eq!(events[0]["objects"], 1, "{events:?}");
    assert_eq!(events[0]["left_behind"], 0, "{events:?}");
    assert_eq!(events[0]["kind"], "copy", "{events:?}");

    // The copy reads as the original does.
    let response = env
        .server
        .http(Some(&env.owner))
        .get(env.server.url(&format!("/api/docs/{}/content", copy.0)))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.bytes().await.unwrap().as_ref(), b"alpha");

    // Under a name of its own too.
    let (status, body) = env.copy(file, docs, Some("b.txt")).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(env.object("docs/b.txt").unwrap().bytes, b"alpha");
}

#[tokio::test]
async fn copying_a_folder_copies_every_object() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("docs/"), b"");
    env.fake.put(BUCKET, &key("docs/a.txt"), b"a");
    env.fake.put(BUCKET, &key("docs/sub/b.txt"), b"b");
    env.fake.put(BUCKET, &key("archive/x.txt"), b"x");
    let library = env.library("Folders").await;
    let docs = env.child(library, "docs").await;
    let archive = env.child(library, "archive").await;
    let a = env.child(docs, "a.txt").await;
    let sub = env.child(docs, "sub").await;
    let b = env.child(sub, "b.txt").await;
    let gets = env.fake.count("get");

    let (status, body) = env.copy(docs, archive, None).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let top = id_of(&body["node"]);
    assert_ne!(top, docs);

    for path in ["docs/a.txt", "docs/sub/b.txt"] {
        assert!(env.object(path).is_some(), "{path} is gone");
    }
    assert!(env.object("archive/docs/").is_some());
    assert_eq!(env.object("archive/docs/a.txt").unwrap().bytes, b"a");
    assert_eq!(env.object("archive/docs/sub/b.txt").unwrap().bytes, b"b");
    assert_eq!(env.fake.count("get"), gets, "a byte was downloaded");

    let copied_a = env.child(top, "a.txt").await;
    let copied_sub = env.child(top, "sub").await;
    let copied_b = env.child(copied_sub, "b.txt").await;
    for (new, old) in [(copied_a, a), (copied_sub, sub), (copied_b, b), (top, docs)] {
        assert_ne!(new, old);
    }
    assert_eq!(env.repository_path(top).await, "archive/docs/");
    assert_eq!(env.repository_path(copied_a).await, "archive/docs/a.txt");
    assert_eq!(
        env.repository_path(copied_b).await,
        "archive/docs/sub/b.txt"
    );
    assert_eq!(env.repository_path(b).await, "docs/sub/b.txt");
    assert_ne!(env.revision(copied_b).await, env.revision(b).await);
    let events = env.events_of("source.moved").await;
    assert_eq!(events[0]["objects"], 3, "{events:?}");
    assert_eq!(events[0]["kind"], "copy", "{events:?}");
}

#[tokio::test]
async fn a_copy_onto_a_taken_name_is_refused() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("docs/a.txt"), b"a");
    env.fake.put(BUCKET, &key("other/A.TXT"), b"other");
    let library = env.library("Names").await;
    let docs = env.child(library, "docs").await;
    let other = env.child(library, "other").await;
    let a = env.child(docs, "a.txt").await;

    // Taken ignoring case there; and taken beside the original itself.
    for parent in [other, docs] {
        let refused = env.copy(a, parent, None).await;
        assert_error(&refused, StatusCode::CONFLICT, "name_taken");
    }
    assert!(env.writes().is_empty(), "{:?}", env.writes());
    assert_eq!(env.children(other).await.len(), 1);
    assert_eq!(env.children(docs).await.len(), 1);
}

#[tokio::test]
async fn a_copy_of_a_folder_over_the_cap_is_refused() {
    let env = Env::new().await;
    // A marker and a thousand files: 1,001 objects.
    env.fake.put(BUCKET, &key("big/"), b"");
    for i in 0..1000 {
        env.fake.put(BUCKET, &key(&format!("big/{i:04}.txt")), b"x");
    }
    env.fake.put(BUCKET, &key("elsewhere/x.txt"), b"x");
    let library = env.library("Big").await;
    let big = env.child(library, "big").await;
    let elsewhere = env.child(library, "elsewhere").await;

    let refused = env.copy(big, elsewhere, None).await;
    assert_error(&refused, StatusCode::CONFLICT, "source_too_many_objects");
    assert_eq!(refused.1["count"], 1001);
    assert!(env.writes().is_empty());
    assert_eq!(env.children(elsewhere).await.len(), 1);
}

#[tokio::test]
async fn after_a_copy_the_next_listing_changes_nothing() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("docs/a.txt"), b"a");
    env.fake.put(BUCKET, &key("docs/sub/b.txt"), b"b");
    env.fake.put(BUCKET, &key("archive/x.txt"), b"x");
    let library = env.library("Echo").await;
    let docs = env.child(library, "docs").await;
    let archive = env.child(library, "archive").await;
    let a = env.child(docs, "a.txt").await;

    let (status, body) = env.copy(docs, archive, None).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = env.copy(a, archive, None).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let top = env.child(archive, "docs").await;
    let sub = env.child(top, "sub").await;
    let before = (
        env.children(library).await,
        env.children(archive).await,
        env.children(top).await,
        env.children(sub).await,
    );
    let rows_before = env.db().source_items(library).await.unwrap();
    // The library's file and byte counts are refreshed by the first listing after a copy, which
    // then reports them; it plans no add, change, removal or move.
    let report = sync_once(&env.state, library, OffsetDateTime::now_utc())
        .await
        .unwrap();
    if let Some(report) = report {
        assert_eq!(
            (report.added, report.changed, report.removed, report.moved),
            (0, 0, 0, 0),
            "the listing changed something: {report:?}"
        );
    }
    assert_eq!(env.db().source_items(library).await.unwrap(), rows_before);
    let after = (
        env.children(library).await,
        env.children(archive).await,
        env.children(top).await,
        env.children(sub).await,
    );
    assert_eq!(before, after);
}

#[tokio::test]
async fn copying_a_folder_skips_what_the_copier_cannot_see() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("docs/a.txt"), b"a");
    env.fake.put(BUCKET, &key("docs/private.txt"), b"p");
    env.fake.put(BUCKET, &key("docs/secret/s.txt"), b"s");
    env.fake.put(BUCKET, &key("docs/secret/deep/d.txt"), b"d");
    env.fake.put(BUCKET, &key("archive/x.txt"), b"x");
    let library = env.library("Hidden").await;
    let docs = env.child(library, "docs").await;
    let archive = env.child(library, "archive").await;
    let secret = env.child(docs, "secret").await;
    let private = env.child(docs, "private.txt").await;
    let copier = env.server.user("Copier").await;
    let db = env.db();
    db.set_node_access(
        env.admin.id,
        library,
        Principal::User(copier.id),
        Some("editor"),
    )
    .await
    .unwrap();
    for hidden in [secret, private] {
        db.break_inheritance(env.admin.id, hidden).await.unwrap();
        db.set_node_access(env.admin.id, hidden, Principal::User(copier.id), None)
            .await
            .unwrap();
    }
    assert!(
        db.effective_access(copier.id, secret)
            .await
            .unwrap()
            .is_none()
    );
    let gets = env.fake.count("get");

    let (status, body) = env
        .call(
            &copier,
            Method::POST,
            &format!("/api/nodes/{}/copy", docs.0),
            Some(json!({ "parent": archive.0.to_string() })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let top = id_of(&body["node"]);

    // Only what the copier sees reached the destination.
    assert_eq!(env.object("archive/docs/a.txt").unwrap().bytes, b"a");
    for path in [
        "archive/docs/private.txt",
        "archive/docs/secret/s.txt",
        "archive/docs/secret/deep/d.txt",
        "archive/docs/secret/",
    ] {
        assert!(env.object(path).is_none(), "{path} was copied");
    }
    assert_eq!(env.fake.count("get"), gets);
    assert_eq!(env.object("docs/secret/s.txt").unwrap().bytes, b"s");
    let events = env.events_of("source.moved").await;
    assert_eq!(events[0]["objects"], 1, "{events:?}");
    let names: Vec<String> = env
        .children(top)
        .await
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert_eq!(names, ["a.txt"]);

    // The next listing adds nothing: no new node for what was left out.
    let report = sync_once(&env.state, library, OffsetDateTime::now_utc())
        .await
        .unwrap();
    if let Some(report) = report {
        assert_eq!(
            (report.added, report.changed, report.removed, report.moved),
            (0, 0, 0, 0),
            "{report:?}"
        );
    }
    let names: Vec<String> = env
        .children(top)
        .await
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert_eq!(names, ["a.txt"]);
}

/// The keys every write the fake received was for, in order.
fn written_keys(env: &Env) -> Vec<String> {
    env.writes().into_iter().map(|(_, key, _)| key).collect()
}

#[tokio::test]
async fn copying_an_s3_file_out_stores_it_and_counts_it_against_the_destination() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("docs/a.txt"), b"alpha");
    env.fake.put(BUCKET, &key("docs/sub/b.txt"), b"bravo!");
    let library = env.library("Out").await;
    let docs = env.child(library, "docs").await;
    let file = env.child(docs, "a.txt").await;
    let gets = env.fake.count("get");
    let used = env.used().await;

    let (status, body) = env.copy(file, env.ordinary, None).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let copy = id_of(&body["node"]);
    assert_ne!(copy, file);
    assert_eq!(body["node"]["name"], "a.txt");
    assert_eq!(env.child(env.ordinary, "a.txt").await, copy);
    // Read once from the bucket; an ordinary file now, which the site's usage counts.
    assert_eq!(env.fake.count("get"), gets + 1);
    assert!(env.db().source_item_of(copy).await.unwrap().is_none());
    assert_eq!(env.content(copy).await, b"alpha");
    assert_eq!(env.used().await, used + 5);
    // Nothing was written to the bucket, and the original stays.
    assert!(env.writes().is_empty(), "{:?}", env.writes());
    assert_eq!(env.child(docs, "a.txt").await, file);

    // A folder comes out with everything in it.
    let (status, body) = env.copy(docs, env.ordinary, None).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let top = id_of(&body["node"]);
    assert_eq!(body["node"]["name"], "docs");
    let a = env.child(top, "a.txt").await;
    let sub = env.child(top, "sub").await;
    let b = env.child(sub, "b.txt").await;
    assert_eq!(env.content(a).await, b"alpha");
    assert_eq!(env.content(b).await, b"bravo!");
    assert_eq!(env.used().await, used + 5 + 5 + 6);
    assert!(env.writes().is_empty(), "{:?}", env.writes());

    // The bulk route copies out too.
    let source_sub = env.child(docs, "sub").await;
    let source_b = env.child(source_sub, "b.txt").await;
    let (status, body) = env
        .call(
            &env.owner,
            Method::POST,
            "/api/bulk",
            Some(json!({
                "op": "copy",
                "items": [source_b.0.to_string()],
                "parent": env.ordinary.0.to_string(),
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body[0]["ok"], true, "{body}");
    let bulk = env.child(env.ordinary, "b.txt").await;
    assert_eq!(env.content(bulk).await, b"bravo!");
}

#[tokio::test]
async fn copying_an_ordinary_file_into_an_s3_library_writes_it_to_the_bucket() {
    let env = Env::new().await;
    let library = env.library("Into").await;
    let file = env.upload(env.ordinary, "plain.txt", b"plain words").await;
    let plans = env
        .call(
            &env.owner,
            Method::POST,
            &format!("/api/nodes/{}/children", env.ordinary.0),
            Some(json!({ "kind": "folder", "name": "plans" })),
        )
        .await;
    assert_eq!(plans.0, StatusCode::CREATED, "{}", plans.1);
    let plans = id_of(&plans.1);
    env.upload(plans, "p.txt", b"plan p").await;
    let used = env.used().await;

    let (status, body) = env.copy(file, library, None).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let copy = id_of(&body["node"]);
    assert_eq!(env.child(library, "plain.txt").await, copy);
    // Staged, not written in the request: the worker writes it once the scan gate passes.
    assert!(env.writes().is_empty(), "{:?}", env.writes());
    let pending = env.db().pending_write(copy).await.unwrap().expect("staged");
    assert_eq!(pending.path, "plain.txt");
    assert!(pending.base_tag.is_none());
    assert_eq!(env.content(copy).await, b"plain words");

    // A folder: its marker is made at once; its files are staged.
    let (status, body) = env.copy(plans, library, None).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let top = id_of(&body["node"]);
    assert_eq!(written_keys(&env), [key("plans/")]);
    let p = env.child(top, "p.txt").await;
    assert_eq!(
        env.db().pending_write(p).await.unwrap().unwrap().path,
        "plans/p.txt"
    );

    assert_eq!(env.write().await, 2);
    assert_eq!(env.object("plain.txt").unwrap().bytes, b"plain words");
    assert_eq!(env.object("plans/p.txt").unwrap().bytes, b"plan p");
    assert!(env.db().pending_write(copy).await.unwrap().is_none());
    assert_eq!(env.repository_path(copy).await, "plain.txt");
    // The bytes live in the bucket: the site's usage is unchanged.
    assert_eq!(env.used().await, used);
    // The originals stay.
    assert_eq!(env.child(env.ordinary, "plain.txt").await, file);
}

#[tokio::test]
async fn copying_between_two_s3_libraries_reads_once_and_writes_once() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("docs/a.txt"), b"alpha");
    let from = env.library("From").await;
    let to = env.library_at("To", "team-two/", "read_write").await;
    let docs = env.child(from, "docs").await;
    let file = env.child(docs, "a.txt").await;
    let gets = env.fake.count("get");
    let used = env.used().await;

    let (status, body) = env.copy(file, to, None).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let copy = id_of(&body["node"]);
    assert_eq!(env.fake.count("get"), gets + 1);
    assert!(env.writes().is_empty(), "{:?}", env.writes());
    assert_eq!(env.write().await, 1);
    assert_eq!(written_keys(&env), ["team-two/a.txt"]);
    assert_eq!(
        env.fake.object(BUCKET, "team-two/a.txt").unwrap().bytes,
        b"alpha"
    );
    assert_eq!(env.fake.count("get"), gets + 1, "read twice");
    assert_eq!(env.repository_path(copy).await, "a.txt");
    assert!(env.db().pending_write(copy).await.unwrap().is_none());
    // Nothing of the copy counts against the site: the bytes live in the bucket.
    assert_eq!(env.used().await, used);
    assert_eq!(env.object("docs/a.txt").unwrap().bytes, b"alpha");
}

#[tokio::test]
async fn copying_into_a_read_only_library_is_refused() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("docs/a.txt"), b"alpha");
    let source = env.library("Source").await;
    let docs = env.child(source, "docs").await;
    let s3_file = env.child(docs, "a.txt").await;
    let read_only = env.library_at("Read only", "team-ro/", "read_only").await;
    let blocked = env.library_at("Blocked", "team-bl/", "read_write").await;
    env.db()
        .set_write_state(blocked, WriteState::Blocked, Some("Refused."))
        .await
        .unwrap();
    let file = env.upload(env.ordinary, "plain.txt", b"plain").await;
    let gets = env.fake.count("get");

    for item in [file, s3_file] {
        let refused = env.copy(item, read_only, None).await;
        assert_error(&refused, StatusCode::CONFLICT, "source_read_only");
        let refused = env.copy(item, blocked, None).await;
        assert_error(&refused, StatusCode::CONFLICT, "source_write_blocked");
    }
    assert_eq!(env.fake.count("get"), gets);
    assert!(env.writes().is_empty(), "{:?}", env.writes());
    assert!(env.children(read_only).await.is_empty());
    assert!(env.children(blocked).await.is_empty());
}

#[tokio::test]
async fn a_copy_over_the_limits_is_refused_before_anything_is_read() {
    let env = Env::new().await;
    // A marker and a thousand files: 1,001 items.
    env.fake.put(BUCKET, &key("big/"), b"");
    for i in 0..1000 {
        env.fake.put(BUCKET, &key(&format!("big/{i:04}.txt")), b"x");
    }
    env.fake.put(BUCKET, &key("docs/a.txt"), b"alpha");
    let library = env.library("Limits").await;
    let other = env.library_at("Other", "team-two/", "read_write").await;
    let big = env.child(library, "big").await;
    let docs = env.child(library, "docs").await;
    let a = env.child(docs, "a.txt").await;
    let gets = env.fake.count("get");

    for parent in [env.ordinary, other] {
        let refused = env.copy(big, parent, None).await;
        assert_error(&refused, StatusCode::CONFLICT, "source_too_many_objects");
        assert_eq!(refused.1["count"], 1001);
    }
    // The ordinary library takes files of at most 3 bytes; the bucket's one write as much.
    let (status, body) = env
        .call(
            &env.admin,
            Method::PATCH,
            &format!("/api/libraries/{}", env.ordinary.0),
            Some(json!({ "max_file_bytes": 3 })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    env.state.s3().set_single_put_max_for_tests(3);
    for item in [a, docs] {
        let refused = env.copy(item, env.ordinary, None).await;
        assert_error(&refused, StatusCode::PAYLOAD_TOO_LARGE, "file_too_large");
        assert_eq!(refused.1["limit"], 3);
        let refused = env.copy(item, other, None).await;
        assert_error(
            &refused,
            StatusCode::PAYLOAD_TOO_LARGE,
            "source_file_too_large",
        );
        assert_eq!(refused.1["limit"], 3);
    }
    assert_eq!(env.fake.count("get"), gets, "a byte was read");
    assert!(env.writes().is_empty(), "{:?}", env.writes());
    assert!(env.children(other).await.is_empty());
    let names: Vec<String> = env
        .children(env.ordinary)
        .await
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    for name in ["big", "docs", "a.txt"] {
        assert!(!names.contains(&name.to_owned()), "{names:?}");
    }
}

#[tokio::test]
async fn copying_out_over_the_site_quota_is_refused_before_anything_is_read() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("docs/a.txt"), b"alpha");
    let library = env.library("Quota").await;
    let docs = env.child(library, "docs").await;
    let used = env.used().await;
    env.db()
        .set_site_quota(env.admin.id, env.site, Some(used + 4))
        .await
        .unwrap();
    let gets = env.fake.count("get");

    for item in [env.child(docs, "a.txt").await, docs] {
        let refused = env.copy(item, env.ordinary, None).await;
        assert_error(&refused, StatusCode::INSUFFICIENT_STORAGE, "quota_exceeded");
    }
    assert_eq!(env.fake.count("get"), gets, "a byte was read");
    let usage = env.db().site_usage(env.site).await.unwrap();
    assert_eq!((usage.used, usage.reserved), (used, 0));
    let names: Vec<String> = env
        .children(env.ordinary)
        .await
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert!(!names.contains(&"a.txt".to_owned()), "{names:?}");
    assert!(!names.contains(&"docs".to_owned()), "{names:?}");
}
