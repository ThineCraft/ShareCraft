//! S3 listings become external library nodes through the source engine, without reading or
//! storing an object's bytes. A listing cut short changes nothing.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::time::{Duration, Instant};

use collab_core::ids::{DocId, NodeId};
use common::fake_s3::{FakeS3, Opts};
use common::fake_share::FakeShare;
use common::{TestServer, TestUser, spawn_server_config_without_workers, start_fake_github};
use records::{
    Budget, NewSource, S3_ENDPOINTS, S3Place, SHARE_ROOTS, SOURCE_MAX_FILES, SourceKind,
    SourceMode, SourceState, SourceStatus, Tri, Unavailable, Versioning, object_tag, s3_location,
};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use server::AppState;
use server::config::Config;
use server::source_sync::{SyncError, SyncReport, sync_once, token_context};
use time::OffsetDateTime;

const BUCKET: &str = "sync-bucket-7c1";
const ENDPOINT: &str = "sync-endpoint-4e2";
const KEY_ID: &str = "AKIDSYNCEXAMPLE93";
const SECRET: &str = "planted-sync-secret-5d0a";

struct Env {
    server: TestServer,
    fake: FakeS3,
    admin: TestUser,
    library: NodeId,
    state: AppState,
}

/// The fake's origin as an approved endpoint, written straight to the settings table: the
/// plain-HTTP loopback form is refused by the settings route and only the server's test switch
/// lets it be used.
async fn approve(server: &TestServer, fake: &FakeS3) {
    set_endpoints(
        server,
        json!([{
            "name": ENDPOINT,
            "url": fake.origin(),
            "region": "us-east-1",
            "path_style": true,
            "allow": ["127.0.0.0/8"],
        }]),
    )
    .await;
}

async fn set_endpoints(server: &TestServer, value: Value) {
    sqlx::query(
        "insert into settings (key, value, changed_at) values ($1, $2, now()) \
         on conflict (key) do update set value = excluded.value",
    )
    .bind(S3_ENDPOINTS)
    .bind(value)
    .execute(server.records_db().test_pool())
    .await
    .unwrap();
}

impl Env {
    async fn new() -> Env {
        Self::build("", |_| {}).await
    }

    async fn build(prefix: &str, edit: impl FnOnce(&mut Config)) -> Env {
        let fake = FakeS3::start().await;
        fake.bucket(BUCKET);
        fake.require_keys(KEY_ID, SECRET);
        let server = spawn_server_config_without_workers(|config| {
            config.max_file_bytes = 512 * 1024 * 1024;
            edit(config);
        })
        .await;
        let admin = server.admin("Ada Admin").await;
        let library = common::Site::library(&server).await;
        let state = server.app_state().await;
        state
            .s3()
            .set_retry_wait_for_tests(Duration::from_millis(5));
        approve(&server, &fake).await;
        let db = server.records_db();
        let token = state
            .secret_store()
            .put(&db, SECRET.as_bytes(), &token_context(library))
            .await
            .unwrap();
        let place = S3Place {
            endpoint: ENDPOINT.to_owned(),
            bucket: BUCKET.to_owned(),
            prefix: prefix.to_owned(),
            access_key_id: KEY_ID.to_owned(),
            versioning: Versioning::Unknown,
            conditional: Tri::Unknown,
        };
        db.connect_s3_source(
            &NewSource {
                library,
                kind: SourceKind::S3,
                owner: String::new(),
                repo: String::new(),
                branch: String::new(),
                folder: String::new(),
                location: Some(s3_location(ENDPOINT, BUCKET, prefix)),
                mode: SourceMode::ReadOnly,
                token: Some(token),
                poll_seconds: 60,
                connected_by: admin.id,
            },
            &place,
        )
        .await
        .unwrap();
        Env {
            server,
            fake,
            admin,
            library,
            state,
        }
    }

    async fn sync_at(&self, now: OffsetDateTime) -> Result<Option<SyncReport>, SyncError> {
        sync_once(&self.state, self.library, now).await
    }

    async fn synced_at(&self, now: OffsetDateTime) -> SyncReport {
        self.sync_at(now)
            .await
            .unwrap()
            .expect("the listing changed")
    }

    async fn items(&self) -> Vec<records::SourceItem> {
        self.server
            .records_db()
            .source_items(self.library)
            .await
            .unwrap()
    }

    async fn item(&self, path: &str) -> records::SourceItem {
        self.items()
            .await
            .into_iter()
            .find(|item| item.path == path)
            .unwrap_or_else(|| panic!("no source item at {path}"))
    }

    async fn paths(&self) -> Vec<String> {
        let mut paths: Vec<String> = self.items().await.into_iter().map(|i| i.path).collect();
        paths.sort();
        paths
    }

    async fn status(&self) -> SourceStatus {
        self.server
            .records_db()
            .source_of(self.library)
            .await
            .unwrap()
            .unwrap()
            .1
    }

    async fn in_bin(&self, node: NodeId) -> bool {
        self.server
            .records_db()
            .node(node)
            .await
            .unwrap()
            .is_some_and(|node| node.deleted)
    }

    /// The first event of `kind`, waited for: events are written by a background task.
    async fn first_event(&self, kind: &str) -> records::Event {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let events = self.server.records_db().events_of_kind(kind).await.unwrap();
            if let Some(event) = events.into_iter().next() {
                return event;
            }
            assert!(tokio::time::Instant::now() < deadline, "{kind} event");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

fn at(seconds: i64) -> OffsetDateTime {
    OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(seconds)
}

fn failed_with(answer: &Result<Option<SyncReport>, SyncError>, sentence: &str) -> bool {
    matches!(answer, Err(SyncError::Failed(message)) if message == sentence)
}

fn not_connected_with(answer: &Result<Option<SyncReport>, SyncError>, sentence: &str) -> bool {
    matches!(answer, Err(SyncError::NotConnected(message)) if message == sentence)
}

async fn call(
    server: &TestServer,
    user: &TestUser,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = server.http(Some(user)).request(method, server.url(path));
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.unwrap();
    let status = response.status();
    let text = response.text().await.unwrap();
    let body = serde_json::from_str(&text).unwrap_or(Value::String(text));
    (status, body)
}

fn etag(text: &str) -> Opts {
    Opts {
        etag: Some(format!("\"{text}\"")),
        ..Opts::default()
    }
}

#[tokio::test]
async fn first_listing_creates_folders_and_files_without_revisions_and_fetches_nothing() {
    let env = Env::new().await;
    env.fake
        .put_with_at(BUCKET, "docs/report.txt", b"not opened", 42, etag("r1"));

    let report = env.synced_at(at(0)).await;

    assert_eq!(report.added, 1);
    let folder = env.item("docs").await;
    let file = env.item("docs/report.txt").await;
    assert!(folder.blob_sha.is_none());
    assert_eq!(
        file.blob_sha.as_deref(),
        Some(object_tag(10, 42, "\"r1\"", false).as_str())
    );
    let document = env
        .server
        .records_db()
        .document(DocId(file.node.0))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(document.revision, None);
    assert_eq!(env.fake.count("get"), 0);
    assert_eq!(env.fake.count("head"), 0);
    assert_eq!(env.status().await.state, SourceState::Ok);
}

#[tokio::test]
async fn three_thousand_objects_arrive_over_several_pages() {
    let env = Env::new().await;
    env.fake.page_size(1000);
    for number in 0..3000 {
        env.fake
            .put_at(BUCKET, &format!("f/{number:04}.txt"), b"x", 7);
    }

    let report = env.synced_at(at(0)).await;

    assert_eq!(report.added, 3000);
    assert_eq!(env.items().await.len(), 3001);
    // Three full pages: the third is the last one, so no fourth request is needed.
    assert_eq!(env.fake.count("list"), 3);
    assert_eq!(env.status().await.files, 3000);
}

#[tokio::test]
async fn a_second_listing_with_no_change_records_no_event() {
    let env = Env::new().await;
    env.fake.put(BUCKET, "same.txt", b"same");
    env.synced_at(at(0)).await;
    env.first_event("source.synced").await;
    let before = env
        .server
        .records_db()
        .events_of_kind("source.synced")
        .await
        .unwrap()
        .len();

    assert_eq!(env.sync_at(at(60)).await, Ok(None));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        env.server
            .records_db()
            .events_of_kind("source.synced")
            .await
            .unwrap()
            .len(),
        before
    );
}

#[tokio::test]
async fn a_changed_object_keeps_its_node_and_gets_a_new_tag_and_revision() {
    let env = Env::new().await;
    env.fake.put_at(BUCKET, "draft.txt", b"one", 1);
    env.synced_at(at(0)).await;
    let before = env.item("draft.txt").await;
    let db = env.server.records_db();
    let revision_before = db
        .external_item(before.node)
        .await
        .unwrap()
        .unwrap()
        .revision;

    env.fake.put_at(BUCKET, "draft.txt", b"second", 2);
    assert_eq!(env.synced_at(at(60)).await.changed, 1);

    let after = env.item("draft.txt").await;
    assert_eq!(after.node, before.node);
    assert_ne!(after.blob_sha, before.blob_sha);
    let external = db.external_item(after.node).await.unwrap().unwrap();
    assert_ne!(external.revision, revision_before);
    assert_eq!(external.size, 6);
    assert_eq!(env.fake.count("get"), 0);
}

#[tokio::test]
async fn a_changed_etag_with_the_same_size_and_time_is_a_change() {
    let env = Env::new().await;
    env.fake
        .put_with_at(BUCKET, "same.bin", b"abc", 5, etag("first"));
    env.synced_at(at(0)).await;
    let before = env.item("same.bin").await;

    env.fake
        .put_with_at(BUCKET, "same.bin", b"xyz", 5, etag("second"));
    let report = env.synced_at(at(60)).await;

    assert_eq!(report.changed, 1);
    let after = env.item("same.bin").await;
    assert_eq!(after.node, before.node);
    assert_eq!(
        after.blob_sha.as_deref(),
        Some(object_tag(3, 5, "\"second\"", false).as_str())
    );
}

#[tokio::test]
async fn a_missing_object_stays_for_one_poll_then_goes_to_the_bin() {
    let env = Env::new().await;
    env.fake.put(BUCKET, "gone.txt", b"gone");
    env.synced_at(at(0)).await;
    let node = env.item("gone.txt").await.node;
    env.fake.remove(BUCKET, "gone.txt");

    env.sync_at(at(1)).await.unwrap();
    assert!(!env.in_bin(node).await);
    env.sync_at(at(61)).await.unwrap();
    assert!(env.in_bin(node).await);
}

#[tokio::test]
async fn a_failed_page_changes_nothing() {
    let env = Env::new().await;
    env.fake.page_size(1);
    for name in ["a.txt", "b.txt", "c.txt"] {
        env.fake.put(BUCKET, name, name.as_bytes());
    }
    env.synced_at(at(0)).await;
    let before = env.items().await;
    let lists = env.fake.count("list");

    // One object gone, one new; every page after the first then fails past the client's
    // retries.
    env.fake.remove(BUCKET, "a.txt");
    env.fake.put(BUCKET, "d.txt", b"d");
    env.fake.fail_continued_lists(503, "SlowDown");
    for now in [at(60), at(200)] {
        let answer = env.sync_at(now).await;
        assert!(
            failed_with(
                &answer,
                "The storage could not be reached, so nothing was changed."
            ),
            "{answer:?}"
        );
        assert_eq!(env.items().await, before);
        for item in &before {
            assert!(!env.in_bin(item.node).await);
        }
    }
    // The first page, then the second with three retries, in each of the two syncs.
    assert_eq!(env.fake.count("list"), lists + 2 * (1 + 4));
}

#[tokio::test]
async fn a_repeated_token_or_a_truncated_page_without_a_token_changes_nothing() {
    for fault in ["repeat", "truncated"] {
        let env = Env::new().await;
        env.fake.page_size(1);
        env.fake.put(BUCKET, "a.txt", b"a");
        env.fake.put(BUCKET, "b.txt", b"b");
        env.fake.put(BUCKET, "c.txt", b"c");
        env.fake.put(BUCKET, "d.txt", b"d");
        env.synced_at(at(0)).await;
        let before = env.items().await;
        // Three objects stay, so the second page is itself truncated and names a next token.
        env.fake.remove(BUCKET, "d.txt");
        match fault {
            "repeat" => env.fake.repeat_token(),
            _ => env.fake.truncated_without_token(),
        }

        for now in [at(60), at(200)] {
            let answer = env.sync_at(now).await;
            assert!(
                failed_with(
                    &answer,
                    "The storage's listing was incomplete, so nothing was changed."
                ),
                "{fault}: {answer:?}"
            );
        }
        assert_eq!(env.items().await, before, "{fault}");
        for item in &before {
            assert!(!env.in_bin(item.node).await, "{fault}");
        }
    }
}

#[tokio::test]
async fn over_the_file_cap_the_time_or_the_request_budget_changes_nothing_and_says_which() {
    let env = Env::new().await;
    let db = env.server.records_db();
    env.fake.put(BUCKET, "a.txt", b"a");
    env.fake.put(BUCKET, "b.txt", b"b");

    db.set_setting(env.admin.id, SOURCE_MAX_FILES, json!(1))
        .await
        .unwrap();
    let answer = env.sync_at(at(0)).await;
    assert!(
        failed_with(
            &answer,
            "The bucket holds more than 1 files, the most this server shows for one library, so nothing was changed."
        ),
        "{answer:?}"
    );
    assert!(env.items().await.is_empty());

    db.set_setting(env.admin.id, SOURCE_MAX_FILES, json!(10))
        .await
        .unwrap();
    env.state.s3().set_list_time_for_tests(Some(Duration::ZERO));
    let answer = env.sync_at(at(60)).await;
    assert!(
        failed_with(
            &answer,
            "Listing the bucket took longer than 0 seconds, so nothing was changed."
        ),
        "{answer:?}"
    );
    assert!(env.items().await.is_empty());
    env.state.s3().set_list_time_for_tests(None);

    // One list request was sent so far; a budget of two leaves one, and two pages are needed.
    env.fake.page_size(1);
    let sent = env.fake.count("list");
    assert_eq!(sent, 1);
    db.set_budget(
        env.library,
        &Budget {
            list_requests: 2,
            read_requests: 1000,
            bytes_down: None,
        },
    )
    .await
    .unwrap();
    let answer = env.sync_at(at(120)).await;
    assert!(
        failed_with(
            &answer,
            "Listing was stopped to stay inside this library's request budget, so nothing was changed."
        ),
        "{answer:?}"
    );
    assert!(env.items().await.is_empty());
    assert_eq!(env.fake.count("list"), sent + 1);
}

#[tokio::test]
async fn refused_keys_are_not_connected_and_are_not_retried_in_a_loop() {
    let env = Env::new().await;
    env.fake.put(BUCKET, "kept.txt", b"kept");
    env.synced_at(at(0)).await;
    let before = env.items().await;
    env.fake.require_keys(KEY_ID, "a-different-secret");
    let lists = env.fake.count("list");

    let answer = env.sync_at(at(60)).await;
    assert!(
        not_connected_with(
            &answer,
            "The storage refused the access key. A library owner can replace it under Source."
        ),
        "{answer:?}"
    );
    assert_eq!(
        env.fake.count("list"),
        lists + 1,
        "a refused key is asked once"
    );
    assert_eq!(env.status().await.state, SourceState::NotConnected);
    assert_eq!(env.items().await, before);
    let due = env
        .server
        .records_db()
        .due_sources(at(100_000), 100)
        .await
        .unwrap();
    assert!(
        !due.contains(&env.library),
        "a not-connected library is not polled"
    );
}

#[tokio::test]
async fn an_endpoint_no_longer_approved_is_not_connected_and_files_stay_listed() {
    let env = Env::new().await;
    env.fake.put(BUCKET, "kept.txt", b"kept");
    env.synced_at(at(0)).await;
    let before = env.items().await;
    let requests = env.fake.requests().len();
    set_endpoints(&env.server, json!([])).await;

    let answer = env.sync_at(at(60)).await;
    assert!(
        not_connected_with(
            &answer,
            "This storage endpoint is no longer approved for ShareCraft. An administrator can approve it again."
        ),
        "{answer:?}"
    );
    assert_eq!(env.items().await, before);
    assert_eq!(env.fake.requests().len(), requests);
}

#[tokio::test]
async fn case_colliding_keys_are_both_numbered() {
    let env = Env::new().await;
    env.fake.put_at(BUCKET, "A.md", b"upper", 1);
    env.fake.put_at(BUCKET, "a.md", b"lower", 2);

    let report = env.synced_at(at(0)).await;
    assert_eq!(env.paths().await, ["A (1).md", "a (2).md"]);
    assert_eq!(report.hidden.renamed, 2);
}

#[tokio::test]
async fn a_file_and_a_folder_of_one_name_both_show() {
    let env = Env::new().await;
    env.fake.put(BUCKET, "a", b"file");
    env.fake.put(BUCKET, "a/b.txt", b"inside");

    let report = env.synced_at(at(0)).await;
    assert_eq!(report.added, 2);
    assert_eq!(env.paths().await, ["a", "a (1)", "a/b.txt"]);
    let db = env.server.records_db();
    let folder = env.item("a").await;
    let file = env.item("a (1)").await;
    assert!(folder.blob_sha.is_none());
    assert_eq!(file.repository_path, "a");
    assert!(db.document(DocId(file.node.0)).await.unwrap().is_some());
    let inside = db
        .node(env.item("a/b.txt").await.node)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(inside.parent, Some(folder.node));

    // A second listing finds nothing to change.
    assert_eq!(env.sync_at(at(60)).await, Ok(None));
}

#[tokio::test]
async fn an_empty_marker_folder_shows_and_goes_to_the_bin_after_the_grace() {
    let env = Env::new().await;
    env.fake.put(BUCKET, "empty/", b"");
    env.fake.put(BUCKET, "kept.txt", b"kept");

    env.synced_at(at(0)).await;
    assert_eq!(env.paths().await, ["empty", "kept.txt"]);
    let folder = env.item("empty").await;
    assert!(folder.blob_sha.is_none());
    assert_eq!(folder.repository_path, "empty/");
    assert!(
        env.server
            .records_db()
            .node(folder.node)
            .await
            .unwrap()
            .is_some_and(|node| !node.deleted)
    );
    assert_eq!(env.sync_at(at(30)).await, Ok(None));

    env.fake.remove(BUCKET, "empty/");
    env.sync_at(at(60)).await.unwrap();
    assert!(!env.in_bin(folder.node).await, "the one-poll grace applies");
    env.sync_at(at(121)).await.unwrap();
    assert!(env.in_bin(folder.node).await);
    assert_eq!(env.paths().await, ["kept.txt"]);
}

#[tokio::test]
async fn a_file_added_then_removed_beside_a_folder_of_its_name_never_fails_a_pass() {
    let env = Env::new().await;
    env.fake.put(BUCKET, "a/b.txt", b"inside");
    env.synced_at(at(0)).await;
    let folder = env.item("a").await.node;

    env.fake.put(BUCKET, "a", b"file");
    env.synced_at(at(60)).await;
    assert_eq!(env.paths().await, ["a", "a (1)", "a/b.txt"]);
    assert_eq!(env.item("a").await.node, folder);
    let file = env.item("a (1)").await.node;

    env.fake.remove(BUCKET, "a");
    env.sync_at(at(120)).await.unwrap();
    assert!(!env.in_bin(file).await);
    env.sync_at(at(200)).await.unwrap();
    assert!(env.in_bin(file).await);
    assert_eq!(env.paths().await, ["a", "a/b.txt"]);
    assert_eq!(env.item("a").await.node, folder);
    assert!(!env.in_bin(folder).await);
    assert_eq!(env.status().await.state, SourceState::Ok);
}

#[tokio::test]
async fn a_folder_appearing_beside_a_shown_file_leaves_the_file_its_name() {
    let env = Env::new().await;
    env.fake.put(BUCKET, "a", b"file");
    env.synced_at(at(0)).await;
    let file = env.item("a").await.node;

    env.fake.put(BUCKET, "a/b.txt", b"inside");
    env.synced_at(at(60)).await;
    assert_eq!(env.paths().await, ["a", "a (2)", "a (2)/b.txt"]);
    assert_eq!(env.item("a").await.node, file);
    assert_eq!(env.sync_at(at(120)).await, Ok(None));
}

#[tokio::test]
async fn blocked_types_and_over_size_objects_are_hidden_and_counted() {
    let env = Env::build("", |config| config.max_file_bytes = 4).await;
    let (status, body) = call(
        &env.server,
        &env.admin,
        Method::PATCH,
        &format!("/api/libraries/{}", env.library.0),
        Some(json!({ "blocked_types": [".exe"] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    env.fake.put(BUCKET, "bad.exe", b"MZ");
    env.fake.put(BUCKET, "large.txt", b"12345");
    env.fake.put(BUCKET, "ok.txt", b"1234");

    let report = env.synced_at(at(0)).await;
    assert_eq!(report.hidden.policy, 1);
    assert_eq!(report.hidden.too_big, 1);
    assert_eq!(env.paths().await, ["ok.txt"]);
}

#[tokio::test]
async fn archived_objects_are_listed_and_marked_unavailable() {
    let env = Env::new().await;
    env.fake.put_with(
        BUCKET,
        "cold.bin",
        b"frozen",
        Opts {
            class: Some("GLACIER".to_owned()),
            ..Opts::default()
        },
    );
    env.fake.put(BUCKET, "warm.txt", b"warm");

    let report = env.synced_at(at(0)).await;
    assert_eq!(report.added, 2);
    assert_eq!(report.hidden.archived, 1);
    assert_eq!(report.hidden.total(), 0);
    let cold = env.item("cold.bin").await;
    assert!(cold.blob_sha.as_deref().unwrap().ends_with("!a"));
    let db = env.server.records_db();
    assert_eq!(
        db.external_item(cold.node)
            .await
            .unwrap()
            .unwrap()
            .unavailable,
        Some(Unavailable::Archived)
    );
    let warm = env.item("warm.txt").await;
    assert_eq!(
        db.external_item(warm.node)
            .await
            .unwrap()
            .unwrap()
            .unavailable,
        None
    );
    let detail: Value =
        sqlx::query_scalar("select hidden_detail from library_sources where library_id = $1")
            .bind(env.library.0)
            .fetch_one(db.test_pool())
            .await
            .unwrap();
    assert_eq!(detail["archived"], 1);
    assert_eq!(env.fake.count("get") + env.fake.count("head"), 0);
}

#[tokio::test]
async fn list_requests_are_counted_in_the_meter() {
    let env = Env::new().await;
    env.fake.page_size(2);
    for number in 0..5 {
        env.fake.put(BUCKET, &format!("{number}.txt"), b"x");
    }
    let _ = env.state.s3().take_usage();

    env.synced_at(at(0)).await;
    let usage = env.state.s3().take_usage();
    let (_, counted) = usage
        .iter()
        .find(|(library, _)| *library == env.library)
        .expect("the library's usage");
    assert_eq!(counted.list, 3);
    assert_eq!(counted.read, 0);
    assert_eq!(env.fake.count("list"), 3);
}

#[tokio::test]
async fn site_usage_is_unchanged_by_a_listing() {
    let env = Env::new().await;
    let db = env.server.records_db();
    let site = db.node(env.library).await.unwrap().unwrap().site;
    let before = db.site_usage(site).await.unwrap();
    env.fake.put(BUCKET, "large.txt", &vec![b'x'; 50_000]);

    env.synced_at(at(0)).await;
    assert_eq!(db.site_usage(site).await.unwrap(), before);
}

#[tokio::test]
async fn a_generation_change_stops_a_long_apply() {
    let env = Env::new().await;
    for number in 0..120 {
        env.fake.put(BUCKET, &format!("file-{number:03}.txt"), b"x");
    }
    env.fake.slow_list(Duration::from_millis(200));
    let state = env.state.clone();
    let library = env.library;
    let task = tokio::spawn(async move { sync_once(&state, library, at(0)).await });
    let deadline = tokio::time::Instant::now() + common::TIMEOUT;
    while env.fake.count("list") == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "listing did not start"
        );
        tokio::task::yield_now().await;
    }
    env.server
        .records_db()
        .bump_source_generation(env.library)
        .await
        .unwrap();

    assert!(matches!(task.await.unwrap(), Err(SyncError::Failed(_))));
    let status = env.status().await;
    assert_eq!(status.files, 0);
    assert_eq!(status.last_synced_at, None);
    assert!(
        env.server
            .records_db()
            .try_lock_source(env.library)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn events_carry_counts_and_no_bucket_key_or_endpoint() {
    let env = Env::build("team-space/", |_| {}).await;
    env.fake.put(BUCKET, "team-space/folder/a.txt", b"a");
    env.fake.put(BUCKET, "team-space/b.txt", b"b");
    env.fake.put(BUCKET, "elsewhere/c.txt", b"c");

    let report = env.synced_at(at(0)).await;
    assert_eq!(report.added, 2);
    assert_eq!(env.paths().await, ["b.txt", "folder", "folder/a.txt"]);
    let event = env.first_event("source.synced").await;
    assert_eq!(event.detail["added"], 2);
    assert_eq!(event.detail["files"], 2);
    assert!(event.detail.get("seconds").is_some());
    assert!(event.detail.get("commit").is_none());

    env.fake.deny(BUCKET, "team-space/");
    let answer = env.sync_at(at(60)).await;
    assert!(
        not_connected_with(&answer, "The access key may not list this bucket."),
        "{answer:?}"
    );
    let failed = env.first_event("source.failed").await;

    let status = env.status().await;
    let texts = [
        event.detail.to_string(),
        failed.detail.to_string(),
        status.last_error.clone().unwrap_or_default(),
    ];
    for text in texts {
        for secret in [BUCKET, ENDPOINT, KEY_ID, SECRET, "team-space", "127.0.0.1"] {
            assert!(!text.contains(secret), "{secret} in {text}");
        }
    }
}

#[tokio::test]
async fn the_listing_sends_no_write() {
    let env = Env::new().await;
    env.fake.page_size(2);
    for number in 0..5 {
        env.fake.put(BUCKET, &format!("dir/{number}.txt"), b"x");
    }
    env.synced_at(at(0)).await;
    env.fake.remove(BUCKET, "dir/0.txt");
    env.fake.put(BUCKET, "dir/9.txt", b"new");
    env.sync_at(at(60)).await.unwrap();
    env.sync_at(at(200)).await.unwrap();

    assert_eq!(env.fake.count("write"), 0);
    let requests = env.fake.requests();
    assert!(!requests.is_empty());
    assert!(requests.iter().all(|request| request.method == "GET"));
}

#[cfg(windows)]
const SHARE_ROOT: &str = r"C:\approved";
#[cfg(not(windows))]
const SHARE_ROOT: &str = "/approved";

#[tokio::test]
async fn share_and_github_sync_are_unaffected() {
    // The file share's first listing (from share_sync), unchanged.
    {
        let server = spawn_server_config_without_workers(|config| {
            config.max_file_bytes = 512 * 1024 * 1024;
        })
        .await;
        let admin = server.admin("Ada Admin").await;
        let library = common::Site::library(&server).await;
        let state = server.app_state().await;
        let fake = FakeShare::new(SHARE_ROOT);
        state.shares().set_fs(std::sync::Arc::new(fake.clone()));
        let db = server.records_db();
        db.set_setting(admin.id, SHARE_ROOTS, json!([SHARE_ROOT]))
            .await
            .unwrap();
        db.connect_source(&NewSource {
            library,
            kind: SourceKind::Share,
            owner: String::new(),
            repo: String::new(),
            branch: String::new(),
            folder: String::new(),
            location: Some(SHARE_ROOT.to_owned()),
            mode: SourceMode::ReadOnly,
            token: None,
            poll_seconds: 60,
            connected_by: admin.id,
        })
        .await
        .unwrap();
        fake.mkdir("docs");
        fake.put_at("docs/report.txt", b"not opened", 42);

        let report = sync_once(&state, library, at(0)).await.unwrap().unwrap();
        assert_eq!(report.added, 1);
        let items = db.source_items(library).await.unwrap();
        let file = items
            .iter()
            .find(|item| item.path == "docs/report.txt")
            .unwrap();
        assert_eq!(file.blob_sha.as_deref(), Some("x10-42"));
        assert!(fake.opened_paths().is_empty());
    }

    // GitHub (from share_sync), unchanged.
    {
        const OWNER: &str = "octo-org";
        const REPO: &str = "docs";
        let fake = start_fake_github(OWNER, REPO).await;
        let origin = fake.origin();
        let server = spawn_server_config_without_workers(|config| config.github_api = origin).await;
        let admin = server.admin("Ada Admin").await;
        let library = common::Site::library(&server).await;
        server
            .records_db()
            .connect_source(&NewSource {
                library,
                kind: SourceKind::Github,
                owner: OWNER.to_owned(),
                repo: REPO.to_owned(),
                branch: "main".to_owned(),
                folder: String::new(),
                location: None,
                mode: SourceMode::ReadOnly,
                token: None,
                poll_seconds: 300,
                connected_by: admin.id,
            })
            .await
            .unwrap();
        fake.commit("main", &[("hello.txt", b"hello".to_vec())]);

        let report = sync_once(&server.app_state().await, library, at(0))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(report.added, 1);
        let item = server
            .records_db()
            .source_items(library)
            .await
            .unwrap()
            .remove(0);
        assert!(
            server
                .records_db()
                .document(DocId(item.node.0))
                .await
                .unwrap()
                .unwrap()
                .revision
                .is_some()
        );
    }
}

#[tokio::test]
#[ignore = "task-card performance measurement"]
async fn measure_first_listing_of_5000_objects() {
    let env = Env::new().await;
    for number in 0..5_000 {
        env.fake.put_at(
            BUCKET,
            &format!("d{}/file-{number:04}.txt", number % 50),
            b"x",
            9,
        );
    }
    let started = Instant::now();
    let report = env.synced_at(at(0)).await;
    let took = started.elapsed();
    assert_eq!(report.added, 5_000);
    eprintln!(
        "TASK7_MEASUREMENT objects=5000 seconds={:.3} files_per_second={:.2} list_requests={}",
        took.as_secs_f64(),
        5_000.0 / took.as_secs_f64(),
        env.fake.count("list")
    );
}
