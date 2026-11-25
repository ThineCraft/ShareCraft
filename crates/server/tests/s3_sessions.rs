//! Editor saves of files in a read-and-write S3 library: the editor gets the editing seat, each
//! save is answered at once with its staged revision, and the bucket gets at most one write per
//! file per 30 seconds (always the latest content), or at once when the session ends. A change
//! made in the bucket meanwhile is kept and the edits become a named copy. A bucket file never
//! opens in a live room.
//!
//! The write worker is driven with `write_due(state, now)`; the servers here run no workers.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::time::Duration;

use collab_core::ids::{DocId, NodeId};
use common::fake_s3::{FakeS3, Stored};
use common::{
    FileClient, SHORT_GRACE, TestServer, TestUser, spawn_server_config_without_workers,
    wait_out_grace,
};
use records::{PendingWrite, Principal, S3_ENDPOINTS, WriteState};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use server::source_sync::sync_once;
use server::source_write::{sibling_name, write_due};
use server::{AppState, Config};
use time::OffsetDateTime;

const BUCKET: &str = "sessions-bucket-4c2";
const ENDPOINT: &str = "sessions-endpoint-9e1";
const KEY_ID: &str = "AKIDSESSIONSEXAMPLE7";
const SECRET: &str = "planted-sessions-secret-3b9d-zz";
const PREFIX: &str = "team-sessions/";
const OWNER: &str = "Session owner";
const SEAT: &str = "x-sharecraft-seat";

fn key(path: &str) -> String {
    format!("{PREFIX}{path}")
}

fn id_of(answer: &Value) -> NodeId {
    NodeId(answer["id"].as_str().unwrap().parse().unwrap())
}

fn doc_of(node: NodeId) -> DocId {
    DocId(node.0)
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
        Env::with(|_| {}).await
    }

    /// A server whose empty rooms end after [`SHORT_GRACE`].
    async fn short_grace() -> Env {
        Env::with(|config| config.room_grace = SHORT_GRACE).await
    }

    async fn with(edit: impl FnOnce(&mut Config)) -> Env {
        let fake = FakeS3::start().await;
        fake.bucket(BUCKET);
        fake.require_keys(KEY_ID, SECRET);
        let server = spawn_server_config_without_workers(edit).await;
        let admin = server.admin("Sessions admin").await;
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

    /// A library connected by the administrator in `mode` on `prefix`, its bucket listed once.
    async fn library_at(&self, name: &str, mode: &str, prefix: &str) -> NodeId {
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
        library
    }

    /// A read-and-write library on [`PREFIX`] holding `docs/sheet.xlsx`; that file's node.
    async fn sheet(&self) -> (NodeId, NodeId) {
        self.fake
            .put(BUCKET, &key("docs/sheet.xlsx"), b"bucket sheet");
        let library = self.library_at("Sheets", "read_write", PREFIX).await;
        let docs = self.child(library, "docs").await;
        (library, self.child(docs, "sheet.xlsx").await)
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

    async fn revision(&self, node: NodeId) -> Option<String> {
        self.doc(node).await["revision"].as_str().map(str::to_owned)
    }

    async fn revision_as(&self, user: &TestUser, node: NodeId) -> Option<String> {
        let (status, body) = self
            .call(user, Method::GET, &format!("/api/docs/{}", node.0), None)
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["revision"].as_str().map(str::to_owned)
    }

    /// An editor of `node` opened by `user`.
    async fn editor(&self, user: &TestUser, node: NodeId) -> FileClient {
        FileClient::connect_as(&self.server, doc_of(node), user).await
    }

    /// A save of `bytes` to `node` by `user` with the seat token `token`, made from `base`.
    async fn save(
        &self,
        user: &TestUser,
        node: NodeId,
        token: &str,
        base: Option<&str>,
        bytes: &[u8],
    ) -> (StatusCode, Value) {
        let mut request = self
            .server
            .http(Some(user))
            .put(self.server.url(&format!("/api/docs/{}/content", node.0)))
            .header(SEAT, token)
            .body(bytes.to_vec());
        if let Some(base) = base {
            request = request.query(&[("base", base)]);
        }
        answer(request.send().await.unwrap()).await
    }

    /// A save by the seat holder `editor` from `base`, which must be accepted; its revision.
    async fn saved(&self, editor: &FileClient, node: NodeId, base: &str, bytes: &[u8]) -> String {
        let token = editor.token().expect("the editor holds the seat");
        let (status, body) = self
            .save(&editor.user, node, &token, Some(base), bytes)
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["changed"], true, "{body}");
        body["revision"].as_str().unwrap().to_owned()
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

    /// A disconnect of `library` by the owner, sent at once and answered later.
    fn disconnect(&self, library: NodeId) -> tokio::task::JoinHandle<(StatusCode, Value)> {
        let request = self.server.http(Some(&self.owner)).delete(
            self.server
                .url(&format!("/api/libraries/{}/source", library.0)),
        );
        tokio::spawn(async move { answer(request.send().await.unwrap()).await })
    }

    /// The library's connection row: its mode and generation.
    async fn connection(&self, library: NodeId) -> (String, i64) {
        sqlx::query_as("select mode, generation from library_sources where library_id = $1")
            .bind(library.0)
            .fetch_one(self.db().test_pool())
            .await
            .unwrap()
    }

    /// Waits (at most [`common::TIMEOUT`]) until `library`'s connection row satisfies `done`.
    async fn connection_until(&self, library: NodeId, done: impl Fn(&(String, i64)) -> bool) {
        let deadline = tokio::time::Instant::now() + common::TIMEOUT;
        loop {
            let found = self.connection(library).await;
            if done(&found) {
                return;
            }
            assert!(tokio::time::Instant::now() < deadline, "{found:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
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
}

#[tokio::test]
async fn an_editor_file_in_a_writable_library_gets_the_editing_seat_and_read_only_otherwise() {
    let env = Env::new().await;
    for prefix in ["writable/", "read-only/", "blocked/"] {
        env.fake
            .put(BUCKET, &format!("{prefix}sheet.xlsx"), b"bucket sheet");
    }
    let writable = env.library_at("Writable", "read_write", "writable/").await;
    let read_only = env.library_at("Read only", "read_only", "read-only/").await;
    let blocked = env.library_at("Blocked", "read_write", "blocked/").await;
    env.db()
        .set_write_state(blocked, WriteState::Blocked, Some("Refused."))
        .await
        .unwrap();

    for user in [&env.owner, &env.admin] {
        let file = env.child(writable, "sheet.xlsx").await;
        let mut editor = env.editor(user, file).await;
        assert!(editor.can_edit, "{}", user.email);
        assert!(editor.token().is_some(), "{} has no seat", user.email);
        editor.close().await;

        for library in [read_only, blocked] {
            let file = env.child(library, "sheet.xlsx").await;
            let mut editor = env.editor(user, file).await;
            assert!(!editor.can_edit, "{}", user.email);
            assert!(editor.token().is_none(), "{} got the seat", user.email);
            editor.close().await;
        }
    }
}

#[tokio::test]
async fn a_save_is_answered_at_once_and_reaches_the_bucket_after_the_throttle() {
    let env = Env::new().await;
    let (_, file) = env.sheet().await;
    let editor = env.editor(&env.owner, file).await;
    let base = env.revision(file).await.unwrap();
    let revision = env.saved(&editor, file, &base, b"edited sheet").await;

    let pending = env.pending(file).await.expect("the save is pending");
    assert_eq!(pending.revision, revision);
    assert!(!pending.flush, "an editor's save waits for the throttle");
    assert!(pending.base_tag.is_some());
    assert_eq!(env.revision(file).await.as_deref(), Some(revision.as_str()));

    let now = OffsetDateTime::now_utc();
    env.write(now).await;
    assert!(env.writes_of("docs/sheet.xlsx").is_empty());
    assert_eq!(
        env.object("docs/sheet.xlsx").unwrap().bytes,
        b"bucket sheet"
    );

    env.write(now + time::Duration::seconds(31)).await;
    assert_eq!(
        env.writes_of("docs/sheet.xlsx"),
        [("PUT".into(), "match".into())]
    );
    assert_eq!(
        env.object("docs/sheet.xlsx").unwrap().bytes,
        b"edited sheet"
    );
    assert!(env.pending(file).await.is_none());
    // The session and the page still agree on the revision.
    assert_eq!(env.revision(file).await.as_deref(), Some(revision.as_str()));
    // No Git revision was made for the bucket file.
    assert!(env.server.records_revisions(doc_of(file)).await.is_empty());
}

#[tokio::test]
async fn ten_quick_saves_make_one_put_with_the_last_bytes() {
    let env = Env::new().await;
    let (_, file) = env.sheet().await;
    let editor = env.editor(&env.owner, file).await;
    let mut base = env.revision(file).await.unwrap();
    let mut tag: Option<String> = None;
    for n in 0..10 {
        base = env
            .saved(&editor, file, &base, format!("version {n}").as_bytes())
            .await;
        let pending = env.pending(file).await.unwrap();
        // Every save keeps the tag the editing run began from.
        match &tag {
            None => tag = pending.base_tag.clone(),
            Some(tag) => assert_eq!(pending.base_tag.as_ref(), Some(tag)),
        }
    }
    let now = OffsetDateTime::now_utc();
    env.write(now).await;
    assert!(env.writes_of("docs/sheet.xlsx").is_empty());
    env.write(now + time::Duration::seconds(31)).await;
    assert_eq!(
        env.writes_of("docs/sheet.xlsx"),
        [("PUT".into(), "match".into())]
    );
    assert_eq!(env.object("docs/sheet.xlsx").unwrap().bytes, b"version 9");
    assert_eq!(env.revision(file).await.as_deref(), Some(base.as_str()));
}

#[tokio::test]
async fn leaving_the_session_flushes_the_last_save() {
    let env = Env::short_grace().await;
    let (_, file) = env.sheet().await;
    let mut editor = env.editor(&env.owner, file).await;
    let base = env.revision(file).await.unwrap();
    env.saved(&editor, file, &base, b"last words").await;
    assert!(!env.pending(file).await.unwrap().flush);
    editor.close().await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !env.pending(file).await.unwrap().flush {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the session's end did not flush the save"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Written without waiting out the throttle.
    env.write(OffsetDateTime::now_utc()).await;
    assert_eq!(env.object("docs/sheet.xlsx").unwrap().bytes, b"last words");
    assert!(env.pending(file).await.is_none());
}

/// Waits (at most five seconds) until `file`'s pending write is flushed.
async fn flushed(env: &Env, file: NodeId) -> PendingWrite {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(pending) = env.pending(file).await
            && pending.flush
        {
            return pending;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the room emptying did not flush the save"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn leaving_the_room_makes_the_last_save_due_at_once() {
    // The default grace period (60 s) is far from over: only the empty room can flush.
    let env = Env::new().await;
    let (_, file) = env.sheet().await;
    let mut editor = env.editor(&env.owner, file).await;
    let base = env.revision(file).await.unwrap();
    env.saved(&editor, file, &base, b"last words").await;
    // A retry time left on the row (a scan that was waited out) must not hold the write back.
    sqlx::query(
        "update source_writes set next_try_at = now() + interval '1 hour' where node_id = $1",
    )
    .bind(file.0)
    .execute(env.db().test_pool())
    .await
    .unwrap();
    assert!(!env.pending(file).await.unwrap().flush);
    editor.close().await;

    let pending = flushed(&env, file).await;
    assert!(pending.next_try_at.is_none(), "{pending:?}");
    env.write(OffsetDateTime::now_utc()).await;
    assert_eq!(env.object("docs/sheet.xlsx").unwrap().bytes, b"last words");
    assert!(env.pending(file).await.is_none());
}

#[tokio::test]
async fn a_rejoin_before_the_write_keeps_one_write() {
    let env = Env::new().await;
    let (_, file) = env.sheet().await;
    let mut editor = env.editor(&env.owner, file).await;
    let base = env.revision(file).await.unwrap();
    env.saved(&editor, file, &base, b"first and last").await;
    editor.close().await;
    flushed(&env, file).await;

    // Back in the room before the worker has written anything.
    let again = env.editor(&env.owner, file).await;
    assert!(again.can_edit);
    let now = OffsetDateTime::now_utc();
    env.write(now).await;
    env.write(now + time::Duration::seconds(31)).await;
    assert_eq!(
        env.writes_of("docs/sheet.xlsx"),
        [("PUT".into(), "match".into())]
    );
    assert_eq!(
        env.object("docs/sheet.xlsx").unwrap().bytes,
        b"first and last"
    );
    assert!(env.pending(file).await.is_none());
}

#[tokio::test]
async fn an_ordinary_document_is_unchanged_when_its_room_empties() {
    let env = Env::new().await;
    let home = common::Site::library(&env.server).await;
    let response = env
        .server
        .http(Some(&env.admin))
        .post(env.server.url("/api/docs/upload"))
        .query(&[
            ("parent", home.0.to_string()),
            ("name", "plain.xlsx".into()),
        ])
        .body(b"plain workbook".to_vec())
        .send()
        .await
        .unwrap();
    let (status, body) = answer(response).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let plain = id_of(&body);
    let mut editor = env.editor(&env.admin, plain).await;
    let base = env.revision_as(&env.admin, plain).await.unwrap();
    let token = editor.token().expect("the editor holds the seat");
    let (status, body) = env
        .save(&env.admin, plain, &token, Some(&base), b"plain edits")
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    editor.close().await;
    // Let the room see the departure: a later join is answered only after it has.
    let mut again = env.editor(&env.admin, plain).await;
    again.close().await;

    // The save is in the store as ever; nothing was staged for any bucket.
    assert!(env.pending(plain).await.is_none());
    let (rows,): (i64,) = sqlx::query_as("select count(*) from source_writes")
        .fetch_one(env.db().test_pool())
        .await
        .unwrap();
    assert_eq!(rows, 0);
    assert_eq!(env.write(OffsetDateTime::now_utc()).await, 0);
    assert!(env.fake.writes().is_empty());
}

#[tokio::test]
async fn a_save_from_a_stale_revision_is_a_conflict_as_for_any_file() {
    let env = Env::new().await;
    let (_, file) = env.sheet().await;
    let editor = env.editor(&env.owner, file).await;
    let token = editor.token().unwrap();
    let base = env.revision(file).await.unwrap();
    let first = env.saved(&editor, file, &base, b"first").await;

    // From the revision before the first save, and from none at all.
    for stale in [Some(base.as_str()), None] {
        let (status, body) = env.save(&env.owner, file, &token, stale, b"second").await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["current"], first.as_str(), "{body}");
    }
    let pending = env.pending(file).await.unwrap();
    assert_eq!(pending.revision, first);
    env.write(OffsetDateTime::now_utc() + time::Duration::seconds(31))
        .await;
    assert_eq!(env.object("docs/sheet.xlsx").unwrap().bytes, b"first");
}

#[tokio::test]
async fn an_outside_change_during_the_session_is_kept_and_the_edits_become_a_named_copy() {
    let env = Env::new().await;
    let (library, file) = env.sheet().await;
    let docs = env.child(library, "docs").await;
    let editor = env.editor(&env.owner, file).await;
    let token = editor.token().unwrap();
    let base = env.revision(file).await.unwrap();
    let mine = env.saved(&editor, file, &base, b"my edits").await;
    env.fake
        .put(BUCKET, &key("docs/sheet.xlsx"), b"outside edit");

    let now = OffsetDateTime::now_utc() + time::Duration::seconds(31);
    env.write(now).await;
    assert_eq!(
        env.object("docs/sheet.xlsx").unwrap().bytes,
        b"outside edit"
    );
    let copy = format!("docs/{}", sibling_name("sheet.xlsx", OWNER, now));
    assert_eq!(env.object(&copy).unwrap().bytes, b"my edits");
    assert!(env.pending(file).await.is_none());
    let copy_name = copy.trim_start_matches("docs/");
    assert!(
        env.children(docs)
            .await
            .iter()
            .any(|(name, _)| name == copy_name),
        "{:?}",
        env.children(docs).await
    );
    let current = env.revision(file).await.unwrap();
    assert_ne!(current, mine, "the file shows the bucket's content");

    // The next save from the session is a conflict, with the revision the file has now.
    let (status, body) = env
        .save(&env.owner, file, &token, Some(&mine), b"more edits")
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["current"], current.as_str(), "{body}");
    assert!(env.pending(file).await.is_none());
}

#[tokio::test]
async fn two_sessions_one_after_the_other_chain_their_revisions() {
    let env = Env::short_grace().await;
    let (library, file) = env.sheet().await;
    let docs = env.child(library, "docs").await;
    let mut first = env.editor(&env.owner, file).await;
    let base = env.revision(file).await.unwrap();
    let one = env.saved(&first, file, &base, b"first session").await;
    first.close().await;
    wait_out_grace().await;
    env.write(OffsetDateTime::now_utc()).await;
    assert_eq!(
        env.object("docs/sheet.xlsx").unwrap().bytes,
        b"first session"
    );
    assert_eq!(env.revision(file).await.as_deref(), Some(one.as_str()));

    let mut second = env.editor(&env.admin, file).await;
    let two = env.saved(&second, file, &one, b"second session").await;
    assert_ne!(two, one);
    second.close().await;
    wait_out_grace().await;
    env.write(OffsetDateTime::now_utc()).await;
    assert_eq!(
        env.object("docs/sheet.xlsx").unwrap().bytes,
        b"second session"
    );
    assert_eq!(
        env.writes_of("docs/sheet.xlsx"),
        [
            ("PUT".into(), "match".into()),
            ("PUT".into(), "match".into())
        ]
    );
    // No conflict copy: each session was based on the other's write.
    assert_eq!(
        env.children(docs)
            .await
            .into_iter()
            .map(|(name, _)| name)
            .collect::<Vec<_>>(),
        ["sheet.xlsx"]
    );
    assert_eq!(env.revision(file).await.as_deref(), Some(two.as_str()));
}

#[tokio::test]
async fn a_library_blocked_mid_session_narrows_the_seat_and_keeps_the_last_save() {
    let env = Env::new().await;
    let (library, file) = env.sheet().await;
    let bob = env.server.user("Bob").await;
    let mut editor = env.editor(&env.owner, file).await;
    let base = env.revision(file).await.unwrap();
    env.saved(&editor, file, &base, b"kept while blocked").await;

    env.db()
        .set_write_state(library, WriteState::Blocked, Some("Refused."))
        .await
        .unwrap();
    // An access change below re-checks the open files.
    let (status, body) = env
        .call(
            &env.owner,
            Method::PUT,
            &format!("/api/docs/{}/access", file.0),
            Some(json!({ "email": bob.email, "role": "viewer" })),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    // The holder loses editing, and the seat with it.
    editor
        .pump_until(|c| !c.can_edit && c.token().is_none())
        .await;

    let now = OffsetDateTime::now_utc() + time::Duration::seconds(31);
    env.write(now).await;
    assert!(env.writes_of("docs/sheet.xlsx").is_empty());
    assert!(env.pending(file).await.is_some(), "the save is kept");

    env.db()
        .set_write_state(library, WriteState::Ok, None)
        .await
        .unwrap();
    env.write(now).await;
    assert_eq!(
        env.object("docs/sheet.xlsx").unwrap().bytes,
        b"kept while blocked"
    );
    assert!(env.pending(file).await.is_none());
    editor.close().await;
}

#[tokio::test]
async fn a_refused_write_narrows_the_seat_and_a_passing_reprobe_widens_it_without_an_access_change()
{
    let env = Env::new().await;
    let (library, file) = env.sheet().await;
    let mut editor = env.editor(&env.owner, file).await;
    assert!(editor.can_edit);
    let base = env.revision(file).await.unwrap();
    env.saved(&editor, file, &base, b"kept through the block")
        .await;

    // The store refuses the write: the library is blocked, and the block alone narrows the seat.
    env.fake.read_only();
    let now = OffsetDateTime::now_utc() + time::Duration::seconds(31);
    env.write(now).await;
    assert_eq!(
        env.db().write_state(library).await.unwrap().unwrap().0,
        WriteState::Blocked
    );
    editor
        .pump_until(|c| !c.can_edit && c.token().is_none())
        .await;
    assert!(env.pending(file).await.is_some(), "the save is kept");

    // The key may write again: the next probe passes, restores the library and widens the seat.
    env.fake.allow_writes();
    let later = now + time::Duration::seconds(301);
    env.write(later).await;
    assert_eq!(
        env.db().write_state(library).await.unwrap().unwrap().0,
        WriteState::Ok
    );
    editor.pump_until(|c| c.can_edit).await;

    env.write(later).await;
    assert_eq!(
        env.object("docs/sheet.xlsx").unwrap().bytes,
        b"kept through the block"
    );
    assert!(env.pending(file).await.is_none());
    editor.close().await;
}

#[tokio::test]
async fn a_save_staged_while_a_disconnect_waits_makes_it_refuse_with_nothing_moved() {
    let env = Env::new().await;
    let (library, file) = env.sheet().await;
    let mut editor = env.editor(&env.owner, file).await;
    let base = env.revision(file).await.unwrap();
    let (_, generation) = env.connection(library).await;

    // A listing holds the library's turn: the disconnect has judged the writes settled and waits.
    let turn = env.db().try_lock_source(library).await.unwrap().unwrap();
    let disconnect = env.disconnect(library);
    env.connection_until(library, |(_, now)| *now > generation)
        .await;
    env.saved(&editor, file, &base, b"saved during the disconnect")
        .await;
    drop(turn);

    let (status, body) = disconnect.await.unwrap();
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "source_writes_pending", "{body}");
    // Nothing was moved: the library is still connected, read and write, with its files.
    assert!(env.db().source_of(library).await.unwrap().is_some());
    assert_eq!(env.connection(library).await.0, "read_write");
    let docs = env.child(library, "docs").await;
    assert_eq!(env.child(docs, "sheet.xlsx").await, file);
    assert!(env.pending(file).await.is_some());

    // And the save still reaches the bucket.
    env.write(OffsetDateTime::now_utc() + time::Duration::seconds(31))
        .await;
    assert_eq!(
        env.object("docs/sheet.xlsx").unwrap().bytes,
        b"saved during the disconnect"
    );
    editor.close().await;
}

#[tokio::test]
async fn a_save_after_a_disconnect_has_closed_the_library_is_refused_and_the_disconnect_completes()
{
    let env = Env::new().await;
    let (library, file) = env.sheet().await;
    let docs = env.child(library, "docs").await;
    let editor = env.editor(&env.owner, file).await;
    let token = editor.token().expect("the editor holds the seat");
    let base = env.revision(file).await.unwrap();

    // A lock on one child holds the move to the bin back, after the disconnect has decided.
    let mut hold = env.db().test_pool().begin().await.unwrap();
    sqlx::query("select id from nodes where id = $1 for update")
        .bind(docs.0)
        .execute(&mut *hold)
        .await
        .unwrap();
    let disconnect = env.disconnect(library);
    env.connection_until(library, |(mode, _)| mode == "read_only")
        .await;

    let request = env
        .server
        .http(Some(&env.owner))
        .put(env.server.url(&format!("/api/docs/{}/content", file.0)))
        .header(SEAT, token)
        .query(&[("base", base)])
        .body(b"too late".to_vec());
    let mut save = tokio::spawn(async move { answer(request.send().await.unwrap()).await });
    // The save is answered while the children are held, or waits for them: refused either way.
    let early = tokio::time::timeout(Duration::from_secs(5), &mut save).await;
    hold.rollback().await.unwrap();
    let (status, body) = match early {
        Ok(done) => done.unwrap(),
        Err(_) => save.await.unwrap(),
    };
    assert_ne!(status, StatusCode::OK, "{body}");
    assert!(env.pending(file).await.is_none(), "nothing was staged");

    let (status, body) = disconnect.await.unwrap();
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert!(env.db().source_of(library).await.unwrap().is_none());
    assert!(env.children(library).await.is_empty());
    assert!(env.writes_of("docs/sheet.xlsx").is_empty());
    drop(editor);
}

#[tokio::test]
async fn saves_use_no_site_quota() {
    let env = Env::new().await;
    let (_, file) = env.sheet().await;
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

    let editor = env.editor(&env.owner, file).await;
    let base = env.revision(file).await.unwrap();
    let big = vec![b'x'; 4096];
    env.saved(&editor, file, &base, &big).await;
    let after = env.db().site_usage(env.site).await.unwrap();
    assert_eq!((after.used, after.reserved), (usage.used, usage.reserved));
    env.write(OffsetDateTime::now_utc() + time::Duration::seconds(31))
        .await;
    assert_eq!(env.object("docs/sheet.xlsx").unwrap().bytes, big);
    let after = env.db().site_usage(env.site).await.unwrap();
    assert_eq!((after.used, after.reserved), (usage.used, usage.reserved));
}

#[tokio::test]
async fn a_bucket_file_never_opens_in_a_live_room() {
    let env = Env::with(common::enable_live_gridcraft).await;
    let (_, file) = env.sheet().await;
    assert_eq!(env.doc(file).await["session"], "file");
    let editor = env.editor(&env.owner, file).await;
    assert!(editor.can_edit);
    let base = env.revision(file).await.unwrap();
    env.saved(&editor, file, &base, b"edited in a file session")
        .await;
    assert!(env.pending(file).await.is_some());

    // An ordinary workbook on the same server opens live.
    let home = common::Site::library(&env.server).await;
    let response = env
        .server
        .http(Some(&env.admin))
        .post(env.server.url("/api/docs/upload"))
        .query(&[
            ("parent", home.0.to_string()),
            ("name", "plain.xlsx".into()),
        ])
        .body(b"plain workbook".to_vec())
        .send()
        .await
        .unwrap();
    let (status, body) = answer(response).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = env
        .call(
            &env.admin,
            Method::GET,
            &format!("/api/docs/{}", id_of(&body).0),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["session"], "live");
}

#[tokio::test]
async fn an_upload_while_someone_holds_the_seat_is_being_edited_as_for_any_file() {
    let env = Env::new().await;
    let (library, file) = env.sheet().await;
    let docs = env.child(library, "docs").await;
    let editor = env.editor(&env.owner, file).await;
    assert!(editor.token().is_some());

    let response = env
        .server
        .http(Some(&env.admin))
        .post(env.server.url("/api/docs/upload"))
        .query(&[
            ("parent", docs.0.to_string()),
            ("name", "sheet.xlsx".into()),
            ("on_conflict", "replace".into()),
        ])
        .body(b"uploaded over it".to_vec())
        .send()
        .await
        .unwrap();
    let (status, body) = answer(response).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "being_edited", "{body}");
    assert!(env.pending(file).await.is_none(), "nothing staged");
    env.write(OffsetDateTime::now_utc() + time::Duration::seconds(31))
        .await;
    assert!(env.writes_of("docs/sheet.xlsx").is_empty());
}
