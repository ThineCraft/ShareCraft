//! Renaming and moving inside a read-and-write S3 library: the bucket first (every object
//! copied, then every original deleted), the tree second, under the library's source lock; the
//! node keeps its id, permissions and links; the next listing sees nothing new; and what is
//! refused before anything is written.
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

use collab_core::ids::{DocId, NodeId};
use common::fake_s3::{FakeS3, Opts, Stored};
use common::{FileClient, TestServer, TestUser, spawn_server_config_without_workers};
use records::{Principal, S3_ENDPOINTS, WriteState};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use server::AppState;
use server::source_sync::sync_once;
use server::source_write::write_due;
use time::OffsetDateTime;

const BUCKET: &str = "structure-bucket-5e1";
const ENDPOINT: &str = "structure-endpoint-2a7";
const KEY_ID: &str = "AKIDSTRUCTUREEXAMPLE04";
const SECRET: &str = "planted-structure-secret-8c1f-qq";
const PREFIX: &str = "team-structure/";

const ITEM_SAVING: &str = "This is still being saved to the storage. Try again when saving has finished, or discard the save that failed.";
const CHANGED: &str = "This changed in the storage since ShareCraft last looked. ShareCraft is checking it again; try again in a minute.";
const UNAVAILABLE: &str = "This cannot be done yet in a library stored in S3.";
const SEAT: &str = "x-sharecraft-seat";

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

    async fn change(&self, user: &TestUser, node: NodeId, body: Value) -> (StatusCode, Value) {
        self.call(
            user,
            Method::PATCH,
            &format!("/api/nodes/{}", node.0),
            Some(body),
        )
        .await
    }

    async fn rename(&self, node: NodeId, name: &str) -> (StatusCode, Value) {
        self.change(&self.owner, node, json!({ "name": name }))
            .await
    }

    async fn move_to(&self, node: NodeId, parent: NodeId) -> (StatusCode, Value) {
        self.change(&self.owner, node, json!({ "parent": parent.0.to_string() }))
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

    /// The revision the tracked file `node` is at.
    async fn revision(&self, node: NodeId) -> String {
        self.db()
            .external_item(node)
            .await
            .unwrap()
            .expect("tracked")
            .revision
    }

    fn woken(&self, library: NodeId) -> bool {
        self.state.sources().take_wake_for_tests(library)
    }

    /// Lists the library once and fails unless it added, changed, removed or moved nothing
    /// (its counts of files and bytes may follow a write).
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

    /// An editor's save of `bytes` over `base`, holding the seat `token`.
    async fn save(
        &self,
        node: NodeId,
        token: &str,
        base: &str,
        bytes: &[u8],
    ) -> (StatusCode, Value) {
        let response = self
            .server
            .http(Some(&self.owner))
            .put(self.server.url(&format!("/api/docs/{}/content", node.0)))
            .header(SEAT, token)
            .query(&[("base", base)])
            .body(bytes.to_vec())
            .send()
            .await
            .unwrap();
        answer(response).await
    }

    /// Runs the write worker once, past the coalescing wait.
    async fn write_now(&self) {
        write_due(
            &self.state,
            OffsetDateTime::now_utc() + time::Duration::seconds(31),
        )
        .await;
    }
}

fn assert_error(answer: &(StatusCode, Value), status: StatusCode, code: &str) {
    assert_eq!(answer.0, status, "{}", answer.1);
    assert_eq!(answer.1["error"], code, "{}", answer.1);
}

#[tokio::test]
async fn renaming_a_file_renames_the_object_and_keeps_the_node() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("docs/a.txt"), b"alpha");
    let library = env.library("Renames").await;
    let docs = env.child(library, "docs").await;
    let file = env.child(docs, "a.txt").await;

    let (status, body) = env.rename(file, "b.txt").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(id_of(&body), file);
    assert_eq!(body["name"], "b.txt");
    assert!(body.get("left_behind").is_none(), "{body}");

    assert!(env.object("docs/a.txt").is_none());
    assert_eq!(env.object("docs/b.txt").unwrap().bytes, b"alpha");
    assert_eq!(env.child(docs, "b.txt").await, file);
    assert_eq!(env.repository_path(file).await, "docs/b.txt");
    let events = env.events_of("source.moved").await;
    assert_eq!(events[0]["objects"], 1, "{events:?}");
    assert_eq!(events[0]["left_behind"], 0, "{events:?}");

    // The tag recorded is the copy's: the next listing finds nothing new.
    assert!(!env.sync(library).await, "the listing changed something");
    assert_eq!(env.child(docs, "b.txt").await, file);
    let response = env
        .server
        .http(Some(&env.owner))
        .get(env.server.url(&format!("/api/docs/{}/content", file.0)))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.bytes().await.unwrap().as_ref(), b"alpha");
}

#[tokio::test]
async fn moving_a_folder_moves_every_object_and_keeps_permissions_and_links() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("docs/"), b"");
    env.fake.put(BUCKET, &key("docs/a.txt"), b"a");
    env.fake.put(BUCKET, &key("docs/sub/b.txt"), b"b");
    env.fake.put(BUCKET, &key("archive/x.txt"), b"x");
    let library = env.library("Moves").await;
    let docs = env.child(library, "docs").await;
    let archive = env.child(library, "archive").await;
    let a = env.child(docs, "a.txt").await;
    let sub = env.child(docs, "sub").await;
    let b = env.child(sub, "b.txt").await;
    let reader = env.server.user("Reader").await;
    env.db()
        .set_node_access(
            env.admin.id,
            docs,
            Principal::User(reader.id),
            Some("viewer"),
        )
        .await
        .unwrap();
    let (status, link) = env
        .call(
            &env.owner,
            Method::POST,
            &format!("/api/nodes/{}/links", docs.0),
            Some(json!({ "kind": "signed_in", "role": "viewer" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{link}");

    let (status, body) = env.move_to(docs, archive).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(id_of(&body), docs);

    for path in ["docs/", "docs/a.txt", "docs/sub/b.txt"] {
        assert!(env.object(path).is_none(), "{path} is still there");
    }
    assert!(env.object("archive/docs/").is_some());
    assert_eq!(env.object("archive/docs/a.txt").unwrap().bytes, b"a");
    assert_eq!(env.object("archive/docs/sub/b.txt").unwrap().bytes, b"b");
    assert_eq!(env.child(archive, "docs").await, docs);
    assert_eq!(env.child(docs, "a.txt").await, a);
    assert_eq!(env.child(docs, "sub").await, sub);
    assert_eq!(env.child(sub, "b.txt").await, b);
    assert_eq!(env.repository_path(docs).await, "archive/docs/");
    assert_eq!(env.repository_path(b).await, "archive/docs/sub/b.txt");
    let events = env.events_of("source.moved").await;
    assert_eq!(events[0]["objects"], 3, "{events:?}");

    // The permission and the link stay with the node.
    assert!(
        env.db()
            .effective_access(reader.id, b)
            .await
            .unwrap()
            .is_some()
    );
    let (status, links) = env
        .call(
            &env.owner,
            Method::GET,
            &format!("/api/nodes/{}/links", docs.0),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{links}");
    assert!(
        links.to_string().contains(link["id"].as_str().unwrap()),
        "{links}"
    );
}

#[tokio::test]
async fn after_a_folder_move_the_next_listing_changes_nothing() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("docs/a.txt"), b"a");
    env.fake.put(BUCKET, &key("docs/sub/b.txt"), b"b");
    env.fake.put(BUCKET, &key("archive/x.txt"), b"x");
    let library = env.library("Echo").await;
    let docs = env.child(library, "docs").await;
    let archive = env.child(library, "archive").await;
    let sub = env.child(docs, "sub").await;

    let (status, body) = env.move_to(docs, archive).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = env.rename(sub, "Inner").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(env.object("archive/docs/Inner/b.txt").unwrap().bytes, b"b");

    let before = (
        env.children(library).await,
        env.children(archive).await,
        env.children(docs).await,
        env.children(sub).await,
    );
    assert!(!env.sync(library).await, "the listing changed something");
    let after = (
        env.children(library).await,
        env.children(archive).await,
        env.children(docs).await,
        env.children(sub).await,
    );
    assert_eq!(before, after);
}

#[tokio::test]
async fn a_name_taken_ignoring_case_is_refused() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("docs/a.txt"), b"a");
    env.fake.put(BUCKET, &key("docs/B.txt"), b"b");
    env.fake.put(BUCKET, &key("other/b.TXT"), b"c");
    let library = env.library("Names").await;
    let docs = env.child(library, "docs").await;
    let other = env.child(library, "other").await;
    let a = env.child(docs, "a.txt").await;
    let b = env.child(docs, "B.txt").await;

    assert_error(
        &env.rename(a, "b.txt").await,
        StatusCode::CONFLICT,
        "name_taken",
    );
    assert_error(
        &env.move_to(b, other).await,
        StatusCode::CONFLICT,
        "name_taken",
    );
    assert!(env.writes().is_empty(), "{:?}", env.writes());
    assert_eq!(env.child(docs, "a.txt").await, a);
    // A change of case alone is not a clash with itself.
    let (status, body) = env.rename(a, "A.txt").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(env.object("docs/A.txt").is_some());
    assert!(env.object("docs/a.txt").is_none());
}

#[tokio::test]
async fn a_file_with_a_pending_save_cannot_be_renamed() {
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

    for refused in [
        env.rename(file, "b.txt").await,
        // The folder holding it too.
        env.rename(docs, "papers").await,
    ] {
        assert_error(&refused, StatusCode::CONFLICT, "source_item_saving");
        assert_eq!(refused.1["message"], ITEM_SAVING);
    }
    assert!(env.writes().is_empty(), "{:?}", env.writes());
}

#[tokio::test]
async fn a_folder_over_the_cap_is_refused_with_its_count() {
    let env = Env::new().await;
    // A marker and a thousand files: 1,001 objects.
    env.fake.put(BUCKET, &key("big/"), b"");
    for i in 0..1000 {
        env.fake.put(BUCKET, &key(&format!("big/{i:04}.txt")), b"x");
    }
    let library = env.library("Big").await;
    let big = env.child(library, "big").await;

    let refused = env.rename(big, "huge").await;
    assert_error(&refused, StatusCode::CONFLICT, "source_too_many_objects");
    assert_eq!(refused.1["count"], 1001);
    assert_eq!(
        refused.1["message"],
        "This folder holds 1,001 objects in the bucket. ShareCraft renames, moves, copies or deletes at most 1,000 at a time."
    );
    assert!(env.writes().is_empty());
    assert_eq!(env.child(library, "big").await, big);
}

#[tokio::test]
async fn a_change_outside_since_the_listing_is_refused_and_the_listing_is_woken() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("docs/a.txt"), b"a");
    let library = env.library("Outside").await;
    let docs = env.child(library, "docs").await;
    let file = env.child(docs, "a.txt").await;
    // Changed outside after the listing.
    env.fake.put(BUCKET, &key("docs/a.txt"), b"changed outside");
    assert!(!env.woken(library));

    let refused = env.rename(file, "b.txt").await;
    assert_error(&refused, StatusCode::CONFLICT, "source_changed");
    assert_eq!(refused.1["message"], CHANGED);
    assert!(env.woken(library), "the listing was not woken");
    assert!(env.writes().is_empty(), "{:?}", env.writes());
    assert_eq!(env.object("docs/a.txt").unwrap().bytes, b"changed outside");
    assert_eq!(env.child(docs, "a.txt").await, file);

    // A new file outside inside a folder refuses the folder's move too.
    env.sync(library).await;
    env.woken(library);
    env.fake.put(BUCKET, &key("docs/new.txt"), b"new");
    let refused = env.rename(docs, "papers").await;
    assert_error(&refused, StatusCode::CONFLICT, "source_changed");
    assert!(env.woken(library));
    assert!(env.writes().is_empty(), "{:?}", env.writes());
}

#[tokio::test]
async fn left_behind_originals_are_reported_and_the_listing_is_woken() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("docs/a.txt"), b"a");
    let library = env.library("Behind").await;
    let docs = env.child(library, "docs").await;
    let file = env.child(docs, "a.txt").await;
    // The copy is accepted; the delete of the original is refused.
    env.fake.deny_writes_after(env.fake.writes().len() + 1);

    let (status, body) = env.rename(file, "b.txt").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["left_behind"], 1, "{body}");
    assert_eq!(body["name"], "b.txt");
    assert!(env.object("docs/a.txt").is_some());
    assert!(env.object("docs/b.txt").is_some());
    assert!(env.woken(library), "the listing was not woken");
    let events = env.events_of("source.moved").await;
    assert_eq!(events[0]["left_behind"], 1, "{events:?}");
    env.fake.allow_writes();

    // The original comes back as a new file; the moved one keeps its node.
    assert!(env.sync(library).await);
    assert_eq!(env.child(docs, "b.txt").await, file);
    assert_ne!(env.child(docs, "a.txt").await, file);
}

#[tokio::test]
async fn moving_into_another_library_is_still_refused() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("docs/a.txt"), b"a");
    env.fake.put(BUCKET, "elsewhere/inbox/x.txt", b"x");
    let library = env.library("Here").await;
    let other = env.library_at("There", "elsewhere/", "read_write").await;
    let inbox = env.child(other, "inbox").await;
    let ordinary = common::Site::library(&env.server).await;
    let docs = env.child(library, "docs").await;
    let file = env.child(docs, "a.txt").await;

    for parent in [inbox, other, ordinary] {
        let refused = env.move_to(file, parent).await;
        assert_error(&refused, StatusCode::CONFLICT, "source_action_unavailable");
        assert_eq!(refused.1["message"], UNAVAILABLE);
    }
    // And out of an ordinary library into this one.
    let (status, body) = env
        .call(
            &env.owner,
            Method::POST,
            &format!("/api/nodes/{}/children", ordinary.0),
            Some(json!({ "kind": "folder", "name": "Plain" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let plain = id_of(&body);
    let refused = env.move_to(plain, docs).await;
    assert_error(&refused, StatusCode::CONFLICT, "source_action_unavailable");
    assert!(env.writes().is_empty(), "{:?}", env.writes());
    assert_eq!(env.child(docs, "a.txt").await, file);
}

#[tokio::test]
async fn a_read_only_library_still_refuses_renames() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("docs/a.txt"), b"a");
    let library = env.library_at("Read only", PREFIX, "read_only").await;
    let docs = env.child(library, "docs").await;
    let file = env.child(docs, "a.txt").await;

    assert_error(
        &env.rename(file, "b.txt").await,
        StatusCode::CONFLICT,
        "source_read_only",
    );
    assert!(env.writes().is_empty(), "{:?}", env.writes());
}

#[tokio::test]
async fn a_bulk_move_of_two_files_moves_both() {
    let env = Env::new().await;
    // A marked folder, so it is still there once emptied.
    env.fake.put(BUCKET, &key("docs/"), b"");
    env.fake.put(BUCKET, &key("docs/a.txt"), b"a");
    env.fake.put(BUCKET, &key("docs/b.txt"), b"b");
    env.fake.put(BUCKET, &key("archive/x.txt"), b"x");
    let library = env.library("Bulk").await;
    let docs = env.child(library, "docs").await;
    let archive = env.child(library, "archive").await;
    let a = env.child(docs, "a.txt").await;
    let b = env.child(docs, "b.txt").await;

    let (status, body) = env
        .call(
            &env.owner,
            Method::POST,
            "/api/bulk",
            Some(json!({
                "op": "move",
                "items": [a.0.to_string(), b.0.to_string()],
                "parent": archive.0.to_string(),
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body[0]["ok"], true, "{body}");
    assert_eq!(body[1]["ok"], true, "{body}");
    assert_eq!(env.object("archive/a.txt").unwrap().bytes, b"a");
    assert_eq!(env.object("archive/b.txt").unwrap().bytes, b"b");
    assert!(env.object("docs/a.txt").is_none());
    assert!(env.object("docs/b.txt").is_none());
    assert_eq!(env.child(archive, "a.txt").await, a);
    assert_eq!(env.child(archive, "b.txt").await, b);
    assert!(!env.sync(library).await, "the listing changed something");
}

#[tokio::test]
async fn an_open_file_renamed_keeps_its_revision_and_its_editor_saves_on() {
    let env = Env::new().await;
    env.fake
        .put(BUCKET, &key("docs/sheet.xlsx"), b"bucket sheet");
    let library = env.library("Sessions").await;
    let docs = env.child(library, "docs").await;
    let file = env.child(docs, "sheet.xlsx").await;
    let editor = FileClient::connect_as(&env.server, DocId(file.0), &env.owner).await;
    let token = editor.token().expect("the editor holds the seat");
    let base = env.revision(file).await;

    let (status, body) = env.rename(file, "renamed.xlsx").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // The same content under a new tag: no new revision.
    assert_eq!(env.revision(file).await, base);

    let response = env
        .server
        .http(Some(&env.owner))
        .put(env.server.url(&format!("/api/docs/{}/content", file.0)))
        .header(SEAT, token)
        .query(&[("base", base.as_str())])
        .body(b"edited sheet".to_vec())
        .send()
        .await
        .unwrap();
    let (status, body) = answer(response).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["changed"], true, "{body}");
    write_due(
        &env.state,
        OffsetDateTime::now_utc() + time::Duration::seconds(31),
    )
    .await;
    assert_eq!(
        env.object("docs/renamed.xlsx").unwrap().bytes,
        b"edited sheet"
    );
    assert!(env.object("docs/sheet.xlsx").is_none());
}

#[tokio::test]
async fn moving_a_folder_into_itself_is_refused() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("docs/sub/a.txt"), b"a");
    let library = env.library("Itself").await;
    let docs = env.child(library, "docs").await;
    let sub = env.child(docs, "sub").await;

    for into in [docs, sub] {
        assert_error(
            &env.move_to(docs, into).await,
            StatusCode::BAD_REQUEST,
            "into_itself",
        );
    }
    assert!(env.writes().is_empty(), "{:?}", env.writes());
    assert_eq!(env.child(library, "docs").await, docs);
    assert_eq!(env.repository_path(sub).await, "docs/sub/");
}

#[tokio::test]
async fn an_archived_file_is_refused_plainly() {
    let env = Env::new().await;
    env.fake.put_with(
        BUCKET,
        &key("cold/frozen.bin"),
        b"frozen",
        Opts {
            class: Some("GLACIER".to_owned()),
            ..Opts::default()
        },
    );
    let library = env.library("Archive").await;
    let cold = env.child(library, "cold").await;
    let file = env.child(cold, "frozen.bin").await;

    // The file, and the folder holding it: not "try again", which would never help.
    for refused in [
        env.rename(file, "thawed.bin").await,
        env.rename(cold, "warm").await,
    ] {
        assert_error(&refused, StatusCode::CONFLICT, "source_unavailable");
        assert_eq!(refused.1["reason"], "archived", "{}", refused.1);
    }
    assert!(env.writes().is_empty(), "{:?}", env.writes());
    assert!(!env.woken(library));
}

#[tokio::test]
async fn a_file_too_large_to_copy_cannot_be_renamed_or_moved() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("docs/huge.bin"), b"huge");
    env.fake.put(BUCKET, &key("archive/"), b"");
    let library = env.library("Huge").await;
    let docs = env.child(library, "docs").await;
    let archive = env.child(library, "archive").await;
    let file = env.child(docs, "huge.bin").await;
    // Over CopyObject's 5 GiB, as the bucket answers it (a test cannot store one).
    env.fake
        .claim_size(&key("docs/huge.bin"), server::s3::COPY_MAX + 1);

    for refused in [
        env.rename(file, "bigger.bin").await,
        env.move_to(file, archive).await,
        // The folder holding it too.
        env.rename(docs, "papers").await,
    ] {
        assert_error(&refused, StatusCode::CONFLICT, "source_too_large_to_move");
    }
    assert!(env.writes().is_empty(), "{:?}", env.writes());
    assert_eq!(env.child(docs, "huge.bin").await, file);
    assert_eq!(env.child(library, "docs").await, docs);
    assert!(!env.woken(library));
}

#[tokio::test]
async fn a_store_that_refuses_the_copy_blocks_the_library() {
    let env = Env::new().await;
    env.fake.put(BUCKET, &key("docs/a.txt"), b"a");
    env.fake.put(BUCKET, &key("archive/"), b"");
    let library = env.library("Denied").await;
    let docs = env.child(library, "docs").await;
    let archive = env.child(library, "archive").await;
    let file = env.child(docs, "a.txt").await;
    env.fake.read_only();

    let refused = env.move_to(file, archive).await;
    assert_error(&refused, StatusCode::CONFLICT, "source_write_blocked");
    assert!(matches!(
        env.db().write_state(library).await.unwrap(),
        Some((WriteState::Blocked, _))
    ));
    assert_eq!(env.object("docs/a.txt").unwrap().bytes, b"a");
    assert!(env.object("archive/a.txt").is_none());
    assert_eq!(env.child(docs, "a.txt").await, file);
    // Blocked now: a rename is refused before the bucket is asked.
    let writes = env.fake.writes().len();
    assert_error(
        &env.rename(file, "b.txt").await,
        StatusCode::CONFLICT,
        "source_write_blocked",
    );
    assert_eq!(env.fake.writes().len(), writes);
}

#[tokio::test]
async fn a_save_made_during_a_rename_is_written_to_the_renamed_file() {
    let env = Env::new().await;
    env.fake
        .put(BUCKET, &key("docs/sheet.xlsx"), b"bucket sheet");
    let library = env.library("Racing").await;
    let docs = env.child(library, "docs").await;
    let file = env.child(docs, "sheet.xlsx").await;
    let editor = FileClient::connect_as(&env.server, DocId(file.0), &env.owner).await;
    let token = editor.token().expect("the editor holds the seat");
    let base = env.revision(file).await;

    // The save lands after the copy and before the original's delete.
    let paused = env.fake.pause_next_delete();
    let save = async {
        paused.reached().await;
        let saved = env
            .save(file, &token, &base, b"edited during the rename")
            .await;
        paused.release();
        saved
    };
    let (renamed, saved) = tokio::join!(env.rename(file, "renamed.xlsx"), save);
    assert_eq!(renamed.0, StatusCode::OK, "{}", renamed.1);
    assert_eq!(saved.0, StatusCode::OK, "{}", saved.1);
    assert_eq!(saved.1["changed"], true, "{}", saved.1);
    let row = env.db().pending_write(file).await.unwrap().unwrap();
    assert_eq!(row.path, "docs/renamed.xlsx");

    env.write_now().await;
    assert!(env.db().pending_write(file).await.unwrap().is_none());
    assert_eq!(
        env.object("docs/renamed.xlsx").unwrap().bytes,
        b"edited during the rename"
    );
    // Written on the moved object, not as a copy beside the old name.
    assert!(env.object("docs/sheet.xlsx").is_none());
    assert_eq!(
        env.children(docs).await,
        [("renamed.xlsx".to_owned(), file)]
    );
    let last = env.writes().pop().unwrap();
    assert_eq!(
        last,
        (
            "PUT".to_owned(),
            key("docs/renamed.xlsx"),
            "match".to_owned()
        )
    );
    env.plans_nothing(library).await;
}

#[tokio::test]
async fn a_save_made_during_a_folder_move_follows_the_folder() {
    let env = Env::new().await;
    env.fake
        .put(BUCKET, &key("docs/sheet.xlsx"), b"bucket sheet");
    env.fake.put(BUCKET, &key("archive/"), b"");
    let library = env.library("Racing folder").await;
    let docs = env.child(library, "docs").await;
    let archive = env.child(library, "archive").await;
    let file = env.child(docs, "sheet.xlsx").await;
    let editor = FileClient::connect_as(&env.server, DocId(file.0), &env.owner).await;
    let token = editor.token().expect("the editor holds the seat");
    let base = env.revision(file).await;

    let paused = env.fake.pause_next_delete();
    let save = async {
        paused.reached().await;
        let saved = env
            .save(file, &token, &base, b"edited during the move")
            .await;
        paused.release();
        saved
    };
    let (moved, saved) = tokio::join!(env.move_to(docs, archive), save);
    assert_eq!(moved.0, StatusCode::OK, "{}", moved.1);
    assert_eq!(saved.0, StatusCode::OK, "{}", saved.1);

    env.write_now().await;
    assert!(env.db().pending_write(file).await.unwrap().is_none());
    assert_eq!(
        env.object("archive/docs/sheet.xlsx").unwrap().bytes,
        b"edited during the move"
    );
    assert!(env.object("docs/sheet.xlsx").is_none());
    assert_eq!(env.children(docs).await, [("sheet.xlsx".to_owned(), file)]);
    env.plans_nothing(library).await;
}
