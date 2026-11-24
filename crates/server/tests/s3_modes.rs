//! Read-and-write S3 libraries: who may choose the mode, the write probe on connect and on a
//! change, what the guard answers in each mode and write state, and what the connection's
//! answer shows about writes.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::sync::Mutex;
use std::time::Duration;

use collab_core::ids::NodeId;
use common::fake_s3::FakeS3;
use common::{TestServer, TestUser, spawn_server_config_without_workers};
use records::{NewPendingWrite, Principal, S3_ENDPOINTS, Tri, WriteState};
use reqwest::{Method, StatusCode};
use revisions::BlobRef;
use serde_json::{Value, json};
use server::AppState;
use server::source_sync::sync_once;
use time::OffsetDateTime;

const BUCKET: &str = "modes-bucket-4c2";
const ENDPOINT: &str = "modes-endpoint-7e1";
const KEY_ID: &str = "AKIDMODESEXAMPLE7731";
const SECRET: &str = "planted-modes-secret-5d2e-kk";
const PREFIX: &str = "team-modes/";

const NOT_WRITABLE: &str = "This access key may read the bucket but not write to it. Connect the library read-only, or use a key that may write.";
const WRITE_BLOCKED: &str = "This library is read-only until its storage accepts changes again. Its files can still be opened.";
const ACTION_UNAVAILABLE: &str = "This cannot be done yet in a library stored in S3. Uploading files, saving and making folders work; renaming, moving, copying and deleting come later.";
const WRITES_PENDING: &str =
    "Some changes are still being saved to the storage. Wait for them, or discard them, first.";
const ADMIN_ONLY: &str = "Only an administrator may let people change the files in a bucket.";

struct Env {
    server: TestServer,
    fake: FakeS3,
    state: AppState,
    admin: TestUser,
    /// Owner of the site, not an administrator.
    owner: TestUser,
    /// No access to anything.
    bob: TestUser,
    site: NodeId,
    seen: Mutex<Vec<String>>,
}

async fn count(server: &TestServer, sql: &str) -> i64 {
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_owned()))
        .fetch_one(server.records_db().test_pool())
        .await
        .unwrap()
}

fn id_of(answer: &Value) -> NodeId {
    NodeId(answer["id"].as_str().unwrap().parse().unwrap())
}

impl Env {
    async fn new() -> Env {
        let fake = FakeS3::start().await;
        fake.bucket(BUCKET);
        fake.require_keys(KEY_ID, SECRET);
        let server = spawn_server_config_without_workers(|_| {}).await;
        let admin = server.admin("Modes admin").await;
        let owner = server.user("Site owner").await;
        let bob = server.user("Bob Nobody").await;
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
            bob,
            site,
            seen: Mutex::new(Vec::new()),
        }
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
        let response = request.send().await.unwrap();
        let status = response.status();
        let text = response.text().await.unwrap();
        self.seen.lock().unwrap().push(format!("{status} {text}"));
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    fn body(name: &str, mode: Option<&str>) -> Value {
        let mut body = json!({
            "name": name,
            "endpoint": ENDPOINT,
            "bucket": BUCKET,
            "prefix": PREFIX,
            "access_key_id": KEY_ID,
            "secret_access_key": SECRET,
        });
        if let Some(mode) = mode {
            body["mode"] = json!(mode);
        }
        body
    }

    async fn connect_as(&self, user: &TestUser, body: Value) -> (StatusCode, Value) {
        self.call(
            user,
            Method::POST,
            &format!("/api/sites/{}/libraries/s3", self.site.0),
            Some(body),
        )
        .await
    }

    /// A library connected by the administrator in `mode`, its bucket listed once.
    async fn library(&self, name: &str, mode: &str) -> NodeId {
        let (status, answer) = self
            .connect_as(&self.admin, Env::body(name, Some(mode)))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{answer}");
        let library = id_of(&answer);
        sync_once(&self.state, library, OffsetDateTime::now_utc())
            .await
            .unwrap();
        library
    }

    async fn source(&self, user: &TestUser, library: NodeId) -> (StatusCode, Value) {
        self.call(
            user,
            Method::GET,
            &format!("/api/libraries/{}/source", library.0),
            None,
        )
        .await
    }

    async fn patch(&self, user: &TestUser, library: NodeId, body: Value) -> (StatusCode, Value) {
        self.call(
            user,
            Method::PATCH,
            &format!("/api/libraries/{}/source", library.0),
            Some(body),
        )
        .await
    }

    async fn child(&self, parent: NodeId, name: &str) -> NodeId {
        self.server
            .records_db()
            .live_children(parent)
            .await
            .unwrap()
            .into_iter()
            .find(|node| node.name == name)
            .unwrap_or_else(|| panic!("{name} is not there"))
            .id
    }

    /// A new folder in `parent`, as `user`.
    async fn make_folder(&self, user: &TestUser, parent: NodeId) -> (StatusCode, Value) {
        self.call(
            user,
            Method::POST,
            &format!("/api/nodes/{}/children", parent.0),
            Some(json!({ "kind": "folder", "name": "New folder" })),
        )
        .await
    }

    async fn rename(&self, user: &TestUser, node: NodeId) -> (StatusCode, Value) {
        self.call(
            user,
            Method::PATCH,
            &format!("/api/nodes/{}", node.0),
            Some(json!({ "name": "renamed.txt" })),
        )
        .await
    }

    async fn upload(&self, user: &TestUser, parent: NodeId) -> (StatusCode, Value) {
        let response = self
            .server
            .http(Some(user))
            .post(self.server.url("/api/docs/upload"))
            .query(&[
                ("parent", parent.0.to_string()),
                ("name", "up.txt".to_owned()),
            ])
            .body(b"bytes".to_vec())
            .send()
            .await
            .unwrap();
        let status = response.status();
        let text = response.text().await.unwrap();
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    /// Stages a change of `node` in `library` straight in the records.
    async fn stage(&self, library: NodeId, node: NodeId, path: &str) {
        let db = self.server.records_db();
        let blob = BlobRef::of(b"staged change");
        let now = OffsetDateTime::now_utc();
        db.pin_cached(&blob, now).await.unwrap();
        db.stage_write(&NewPendingWrite {
            node,
            library,
            path,
            blob,
            base_tag: None,
            by: self.admin.id,
            flush: false,
            now,
        })
        .await
        .unwrap();
    }

    async fn libraries(&self) -> i64 {
        count(
            &self.server,
            "select count(*) from nodes where kind = 'library' and deleted_at is null",
        )
        .await
    }

    async fn secrets(&self) -> i64 {
        count(&self.server, "select count(*) from secrets").await
    }

    async fn sources(&self) -> i64 {
        count(&self.server, "select count(*) from library_sources").await
    }

    /// Every object key in the bucket.
    fn keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = Vec::new();
        for (method, key, _) in self.fake.writes() {
            if method == "PUT" && self.fake.object(BUCKET, &key).is_some() && !keys.contains(&key) {
                keys.push(key);
            }
        }
        keys
    }

    /// The events of `kind`, once at least one is recorded.
    async fn events_of(&self, kind: &str) -> Vec<Value> {
        let deadline = tokio::time::Instant::now() + common::TIMEOUT;
        loop {
            let found = self.server.records_db().events_of_kind(kind).await.unwrap();
            if !found.is_empty() {
                return found.into_iter().map(|e| e.detail).collect();
            }
            assert!(tokio::time::Instant::now() < deadline, "no {kind} event");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn all_events_text(&self) -> String {
        let texts: Vec<String> =
            sqlx::query_scalar("select kind || ' ' || detail::text from events")
                .fetch_all(self.server.records_db().test_pool())
                .await
                .unwrap();
        texts.join("\n")
    }
}

fn assert_error(answer: &(StatusCode, Value), status: StatusCode, code: &str, message: &str) {
    assert_eq!(answer.0, status, "{}", answer.1);
    assert_eq!(answer.1["error"], code, "{}", answer.1);
    assert_eq!(answer.1["message"], message, "{}", answer.1);
}

#[tokio::test]
async fn an_administrator_may_connect_read_write_and_the_probe_records_condition_support() {
    let env = Env::new().await;
    let (status, answer) = env
        .connect_as(&env.admin, Env::body("Writable", Some("read_write")))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{answer}");
    let library = id_of(&answer);
    assert_eq!(answer["source"]["mode"], "read_write");
    assert_eq!(answer["source"]["conditional"], "yes");
    let place = env
        .server
        .records_db()
        .s3_place(library)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(place.conditional, Tri::Yes);
    // The probe's writes all went below the prefix.
    let writes = env.fake.writes();
    assert!(!writes.is_empty());
    for (_, key, _) in &writes {
        assert!(key.starts_with(PREFIX), "{key}");
    }

    // A store that ignores conditions is recorded as such.
    let env = Env::new().await;
    env.fake.ignore_conditions();
    let (status, answer) = env
        .connect_as(&env.admin, Env::body("Unconditional", Some("read_write")))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{answer}");
    let place = env
        .server
        .records_db()
        .s3_place(id_of(&answer))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(place.conditional, Tri::No);
}

#[tokio::test]
async fn a_non_administrator_is_refused_read_write_before_the_store_is_asked() {
    let env = Env::new().await;
    let libraries = env.libraries().await;
    let answer = env
        .connect_as(&env.owner, Env::body("Not yours", Some("read_write")))
        .await;
    assert_error(
        &answer,
        StatusCode::FORBIDDEN,
        "read_write_admin_only",
        ADMIN_ONLY,
    );
    assert!(env.fake.requests().is_empty());
    assert_eq!(env.libraries().await, libraries);
    // Any other mode word is refused too.
    let (status, answer) = env
        .connect_as(&env.admin, Env::body("Odd", Some("write_only")))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
    assert_eq!(answer["error"], "invalid_request");
    assert!(env.fake.requests().is_empty());
}

#[tokio::test]
async fn a_read_only_key_is_refused_for_read_write_in_plain_words_and_nothing_is_made() {
    let env = Env::new().await;
    env.fake.read_only();
    let (sources, secrets, libraries) = (
        env.sources().await,
        env.secrets().await,
        env.libraries().await,
    );
    let answer = env
        .connect_as(&env.admin, Env::body("Read only key", Some("read_write")))
        .await;
    assert_error(
        &answer,
        StatusCode::UNPROCESSABLE_ENTITY,
        "s3_not_writable",
        NOT_WRITABLE,
    );
    assert_eq!(env.sources().await, sources);
    assert_eq!(env.secrets().await, secrets);
    assert_eq!(env.libraries().await, libraries);
    // The same key connects read-only.
    let (status, answer) = env
        .connect_as(&env.admin, Env::body("Read only key", Some("read_only")))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{answer}");
}

#[tokio::test]
async fn the_connect_probe_leaves_nothing_in_the_bucket() {
    let env = Env::new().await;
    env.library("Clean", "read_write").await;
    assert!(env.keys().is_empty(), "{:?}", env.keys());
    let deletes = env
        .fake
        .writes()
        .iter()
        .filter(|(method, _, _)| method == "DELETE")
        .count();
    assert_eq!(deletes, 1);
    let events = env.all_events_text().await;
    assert!(!events.contains("source.probe_left"), "{events}");
}

#[tokio::test]
async fn connecting_without_a_mode_is_read_only() {
    let env = Env::new().await;
    let (status, answer) = env.connect_as(&env.admin, Env::body("Default", None)).await;
    assert_eq!(status, StatusCode::CREATED, "{answer}");
    assert_eq!(answer["source"]["mode"], "read_only");
    assert!(env.fake.writes().is_empty());
}

#[tokio::test]
async fn patch_to_read_write_probes_first_and_is_for_administrators_only() {
    let env = Env::new().await;
    let library = env.library("Switched", "read_only").await;
    let answer = env
        .patch(&env.owner, library, json!({ "mode": "read_write" }))
        .await;
    assert_error(
        &answer,
        StatusCode::FORBIDDEN,
        "read_write_admin_only",
        ADMIN_ONLY,
    );
    assert!(env.fake.writes().is_empty());

    // A key that may only read is refused, and the mode stays.
    env.fake.read_only();
    let answer = env
        .patch(&env.admin, library, json!({ "mode": "read_write" }))
        .await;
    assert_error(
        &answer,
        StatusCode::UNPROCESSABLE_ENTITY,
        "s3_not_writable",
        NOT_WRITABLE,
    );
    let (_, shown) = env.source(&env.admin, library).await;
    assert_eq!(shown["mode"], "read_only");

    // A writable store: the probe runs, counted against the library, and the mode changes.
    let env = Env::new().await;
    let library = env.library("Switched", "read_only").await;
    let (status, answer) = env
        .patch(&env.admin, library, json!({ "mode": "read_write" }))
        .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["mode"], "read_write");
    assert_eq!(answer["conditional"], "yes");
    assert!(!env.fake.writes().is_empty());
    assert!(env.keys().is_empty());
    let changed = env.events_of("source.mode_changed").await;
    assert_eq!(changed, vec![json!({ "mode": "read_write" })]);

    // An owner may switch it back.
    let (status, answer) = env
        .patch(&env.owner, library, json!({ "mode": "read_only" }))
        .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["mode"], "read_only");
    let (status, answer) = env
        .patch(&env.admin, library, json!({ "mode": "sideways" }))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
}

#[tokio::test]
async fn patch_to_read_only_is_refused_while_writes_are_pending() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &format!("{PREFIX}a.txt"), b"a");
    let library = env.library("Pending", "read_write").await;
    let file = env.child(library, "a.txt").await;
    env.stage(library, file, "a.txt").await;
    let answer = env
        .patch(&env.owner, library, json!({ "mode": "read_only" }))
        .await;
    assert_error(
        &answer,
        StatusCode::CONFLICT,
        "source_writes_pending",
        WRITES_PENDING,
    );
    let (_, shown) = env.source(&env.admin, library).await;
    assert_eq!(shown["mode"], "read_write");
}

#[tokio::test]
async fn switching_modes_changes_what_the_guard_answers_at_once() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &format!("{PREFIX}docs/a.txt"), b"a");
    let library = env.library("Guarded", "read_write").await;
    let docs = env.child(library, "docs").await;
    let file = env.child(docs, "a.txt").await;

    let refused = env.rename(&env.owner, file).await;
    assert_error(
        &refused,
        StatusCode::CONFLICT,
        "source_action_unavailable",
        ACTION_UNAVAILABLE,
    );
    // A new folder goes through in a read-and-write library (since Task 6).
    let through = env.make_folder(&env.owner, docs).await;
    assert_eq!(through.0, StatusCode::CREATED, "{}", through.1);

    let (status, answer) = env
        .patch(&env.owner, library, json!({ "mode": "read_only" }))
        .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    for answer in [
        env.rename(&env.owner, file).await,
        env.make_folder(&env.owner, docs).await,
    ] {
        assert_eq!(answer.0, StatusCode::CONFLICT, "{}", answer.1);
        assert_eq!(answer.1["error"], "source_read_only");
    }

    let (status, answer) = env
        .patch(&env.admin, library, json!({ "mode": "read_write" }))
        .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let refused = env.rename(&env.owner, file).await;
    assert_eq!(refused.1["error"], "source_action_unavailable");
}

#[tokio::test]
async fn a_blocked_library_answers_source_write_blocked_and_still_reads() {
    let env = Env::new().await;
    env.fake
        .put(BUCKET, &format!("{PREFIX}docs/a.txt"), b"readable");
    let library = env.library("Blocked", "read_write").await;
    let docs = env.child(library, "docs").await;
    let file = env.child(docs, "a.txt").await;
    env.server
        .records_db()
        .set_write_state(library, WriteState::Blocked, Some("refused"))
        .await
        .unwrap();

    for answer in [
        env.make_folder(&env.owner, docs).await,
        env.upload(&env.owner, docs).await,
    ] {
        assert_error(
            &answer,
            StatusCode::CONFLICT,
            "source_write_blocked",
            WRITE_BLOCKED,
        );
    }
    // A refused action stays refused as unavailable, whatever the state.
    let refused = env.rename(&env.owner, file).await;
    assert_eq!(refused.1["error"], "source_action_unavailable");

    let response = env
        .server
        .http(Some(&env.owner))
        .get(env.server.url(&format!("/api/docs/{}/content", file.0)))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.bytes().await.unwrap().as_ref(), b"readable");

    let (_, shown) = env.source(&env.owner, library).await;
    assert_eq!(shown["write_state"], "blocked");
    assert_eq!(shown["write_error"], "refused");
}

#[tokio::test]
async fn patch_to_read_write_on_a_blocked_library_restores_it_when_the_probe_passes() {
    let env = Env::new().await;
    let library = env.library("Restored", "read_write").await;
    let db = env.server.records_db();
    db.set_write_state(library, WriteState::Blocked, Some("refused"))
        .await
        .unwrap();
    let blocked = env.make_folder(&env.owner, library).await;
    assert_eq!(blocked.1["error"], "source_write_blocked");

    let (status, answer) = env
        .patch(&env.admin, library, json!({ "mode": "read_write" }))
        .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["write_state"], "ok");
    assert!(answer["write_error"].is_null());
    assert_eq!(
        db.write_state(library).await.unwrap().unwrap().0,
        WriteState::Ok
    );
    let through = env.make_folder(&env.owner, library).await;
    assert_eq!(through.0, StatusCode::CREATED, "{}", through.1);
}

#[tokio::test]
async fn patch_to_read_write_on_a_blocked_library_makes_its_waiting_changes_due_at_once() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &format!("{PREFIX}a.txt"), b"a");
    let library = env.library("Waiting", "read_write").await;
    let file = env.child(library, "a.txt").await;
    env.stage(library, file, "a.txt").await;
    let db = env.server.records_db();
    let now = OffsetDateTime::now_utc();
    // The write was refused: it waits out its back-off, and the library is blocked.
    assert!(
        db.fail_write(
            file,
            &BlobRef::of(b"staged change").sha256,
            "refused",
            now + time::Duration::minutes(10),
        )
        .await
        .unwrap()
    );
    db.set_write_state(library, WriteState::Blocked, Some("refused"))
        .await
        .unwrap();
    assert!(db.due_writes(now, 10).await.unwrap().is_empty());

    let (status, answer) = env
        .patch(&env.admin, library, json!({ "mode": "read_write" }))
        .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    // Due at once, as after the worker's passing probe, and announced.
    let due = db.due_writes(OffsetDateTime::now_utc(), 10).await.unwrap();
    assert_eq!(
        due.iter().map(|row| row.node).collect::<Vec<_>>(),
        [file],
        "the change waits out its back-off"
    );
    assert_eq!(env.events_of("source.write_restored").await.len(), 1);
}

#[tokio::test]
async fn disconnect_is_refused_while_writes_are_pending() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &format!("{PREFIX}a.txt"), b"a");
    let library = env.library("Kept", "read_write").await;
    let file = env.child(library, "a.txt").await;
    env.stage(library, file, "a.txt").await;
    let answer = env
        .call(
            &env.owner,
            Method::DELETE,
            &format!("/api/libraries/{}/source", library.0),
            None,
        )
        .await;
    assert_error(
        &answer,
        StatusCode::CONFLICT,
        "source_writes_pending",
        WRITES_PENDING,
    );
    let db = env.server.records_db();
    assert!(db.source_of(library).await.unwrap().is_some());
    // Nothing was removed.
    assert_eq!(db.live_children(library).await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_person_who_cannot_see_the_node_still_gets_404() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &format!("{PREFIX}docs/a.txt"), b"a");
    let library = env.library("Hidden", "read_write").await;
    let docs = env.child(library, "docs").await;
    let file = env.child(docs, "a.txt").await;
    for answer in [
        env.make_folder(&env.bob, docs).await,
        env.upload(&env.bob, docs).await,
        env.rename(&env.bob, file).await,
    ] {
        assert_eq!(answer.0, StatusCode::NOT_FOUND, "{}", answer.1);
    }
    // Also when the library is blocked.
    env.server
        .records_db()
        .set_write_state(library, WriteState::Blocked, Some("refused"))
        .await
        .unwrap();
    let answer = env.make_folder(&env.bob, docs).await;
    assert_eq!(answer.0, StatusCode::NOT_FOUND, "{}", answer.1);
}

#[tokio::test]
async fn the_source_answer_shows_write_state_and_counts_and_hides_conditional_from_viewers() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &format!("{PREFIX}a.txt"), b"a");
    let library = env.library("Shown", "read_write").await;
    let file = env.child(library, "a.txt").await;
    env.stage(library, file, "a.txt").await;
    let viewer = env.server.user("Library viewer").await;
    env.server
        .records_db()
        .set_node_access(
            env.admin.id,
            library,
            Principal::User(viewer.id),
            Some("viewer"),
        )
        .await
        .unwrap();

    let (status, shown) = env.source(&env.admin, library).await;
    assert_eq!(status, StatusCode::OK, "{shown}");
    assert_eq!(shown["mode"], "read_write");
    assert_eq!(shown["write_state"], "ok");
    assert!(shown["write_error"].is_null());
    assert_eq!(shown["writes"], json!({ "saving": 1, "failed": 0 }));
    assert_eq!(shown["may_choose_read_write"], true);
    assert_eq!(shown["conditional"], "yes");

    let (status, shown) = env.source(&env.owner, library).await;
    assert_eq!(status, StatusCode::OK, "{shown}");
    assert_eq!(shown["may_choose_read_write"], false);
    assert_eq!(shown["conditional"], "yes");

    let (status, shown) = env.source(&viewer, library).await;
    assert_eq!(status, StatusCode::OK, "{shown}");
    assert_eq!(shown["write_state"], "ok");
    assert_eq!(shown["writes"], json!({ "saving": 1, "failed": 0 }));
    assert_eq!(shown["may_choose_read_write"], false);
    assert!(shown["conditional"].is_null(), "{shown}");
    assert!(shown["bucket"].is_null(), "{shown}");
}

#[tokio::test]
async fn a_probe_left_behind_is_logged_and_announced_without_the_bucket() {
    let env = Env::new().await;
    let library = env.library("Leftover", "read_only").await;
    // Three probe writes are accepted, then the store refuses: the DELETE fails.
    env.fake.deny_writes_after(3);
    let (status, answer) = env
        .patch(&env.admin, library, json!({ "mode": "read_write" }))
        .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let left = env.events_of("source.probe_left").await;
    assert_eq!(left.len(), 1);
    let name = left[0]["name"].as_str().unwrap();
    assert!(name.starts_with(".sharecraft-tmp-probe-"), "{name}");
    assert!(!name.contains(PREFIX.trim_end_matches('/')), "{name}");
    let events = env.all_events_text().await;
    for hidden in [BUCKET, ENDPOINT, KEY_ID, "team-modes", "127.0.0.1"] {
        assert!(!events.contains(hidden), "{hidden} in {events}");
    }
    let seen = env.seen.lock().unwrap().join("\n");
    assert!(!seen.contains(SECRET));
}

#[tokio::test]
async fn read_write_over_another_librarys_place_is_refused_before_anything_is_written() {
    let env = Env::new().await;
    env.library("Existing", "read_only").await;
    let libraries = env.libraries().await;
    let mut body = Env::body("Overlapping", Some("read_write"));
    body["prefix"] = json!(format!("{PREFIX}inner/"));
    let (status, answer) = env.connect_as(&env.admin, body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{answer}");
    assert_eq!(answer["error"], "s3_place_in_use");
    let (status, answer) = env
        .connect_as(&env.admin, Env::body("Same place", Some("read_write")))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{answer}");
    assert_eq!(answer["error"], "s3_place_in_use");
    assert!(env.fake.writes().is_empty(), "{:?}", env.fake.writes());
    assert_eq!(env.libraries().await, libraries);
}
