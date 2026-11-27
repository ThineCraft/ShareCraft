//! S3 objects are served over HTTP as file-share files are: the download, range, preview,
//! thumbnail and anyone-link routes read through `materialise`; a failure answers with its
//! reason; a person without access never reaches the store; and nothing an answer carries names
//! the bucket, the key, the endpoint or the secret.

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
use common::fake_s3::{FakeS3, Opts};
use common::{
    TIMEOUT, TestServer, TestUser, spawn_server_config_without_workers, start_fake_github,
};
use fake_icap::{Behaviour, FakeIcap, eicar};
use records::{
    ANYONE_LINKS, Budget, NewSource, Principal, S3_ENDPOINTS, S3Place, SCANNER_URL, ScanState,
    SourceKind, SourceMode, Tri, Versioning, s3_location,
};
use reqwest::StatusCode;
use reqwest::header::{CONTENT_RANGE, RANGE};
use serde_json::{Value, json};
use server::AppState;
use server::external::{ExternalError, materialise};
use server::source_sync::{sync_once, token_context};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

const BUCKET: &str = "serving-bucket-7c2";
const ENDPOINT: &str = "serving-endpoint-4d9";
const KEY_ID: &str = "AKIDSERVINGEXAMPLE77";
const SECRET: &str = "planted-serving-secret-9e4f";

const JPEG: &[u8] = include_bytes!("fixtures/images/gradient.jpg");

const BUDGET_MESSAGE: &str = "This library has used its request budget for the month. Files opened before still open; a library owner can raise the budget.";
const ARCHIVED_MESSAGE: &str =
    "This file is in archive storage and must be restored there before it can be opened.";

struct Env {
    server: TestServer,
    fake: FakeS3,
    admin: TestUser,
    viewer: TestUser,
    outsider: TestUser,
    library: NodeId,
    ordinary: NodeId,
    state: AppState,
}

fn endpoint_at(origin: &str) -> Value {
    json!([{
        "name": ENDPOINT,
        "url": origin,
        "region": "us-east-1",
        "path_style": true,
        "allow": ["127.0.0.0/8"],
    }])
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

/// An origin on 127.0.0.1 where nothing listens.
async fn dead_origin() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{addr}")
}

fn sha(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

impl Env {
    /// A server with an S3 library on the fake holding `objects`, listed once.
    async fn new(objects: &[(&str, &[u8])]) -> Env {
        let fake = FakeS3::start().await;
        fake.bucket(BUCKET);
        fake.require_keys(KEY_ID, SECRET);
        for (key, bytes) in objects {
            fake.put(BUCKET, key, bytes);
        }
        Self::with_fake(fake).await
    }

    async fn with_fake(fake: FakeS3) -> Env {
        let server = spawn_server_config_without_workers(|config| {
            config.max_file_bytes = 512 * 1024 * 1024;
        })
        .await;
        let admin = server.admin("Serving admin").await;
        let viewer = server.user("Serving viewer").await;
        let outsider = server.user("Serving outsider").await;
        // `Site::library` hands back one shared library, so the copy destination is made here.
        let library = common::Site::library(&server).await;
        let site = server
            .records_db()
            .node(library)
            .await
            .unwrap()
            .unwrap()
            .site;
        let created = server
            .http(Some(&admin))
            .post(server.url(&format!("/api/sites/{}/libraries", site.0)))
            .json(&json!({ "name": "Ordinary files", "kind": "document" }))
            .send()
            .await
            .unwrap();
        assert_eq!(created.status(), StatusCode::CREATED);
        let ordinary = NodeId(
            created.json::<Value>().await.unwrap()["id"]
                .as_str()
                .unwrap()
                .parse()
                .unwrap(),
        );
        let state = server.app_state().await;
        state
            .s3()
            .set_retry_wait_for_tests(Duration::from_millis(5));
        set_endpoints(&server, endpoint_at(&fake.origin())).await;
        let db = server.records_db();
        let token = state
            .secret_store()
            .put(&db, SECRET.as_bytes(), &token_context(library))
            .await
            .unwrap();
        let place = S3Place {
            endpoint: ENDPOINT.to_owned(),
            bucket: BUCKET.to_owned(),
            prefix: String::new(),
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
                location: Some(s3_location(ENDPOINT, BUCKET, "")),
                mode: SourceMode::ReadOnly,
                token: Some(token),
                poll_seconds: 60,
                connected_by: admin.id,
            },
            &place,
        )
        .await
        .unwrap();
        db.set_node_access(
            admin.id,
            library,
            Principal::User(viewer.id),
            Some("viewer"),
        )
        .await
        .unwrap();
        let _ = sync_once(&state, library, OffsetDateTime::UNIX_EPOCH)
            .await
            .unwrap();
        Env {
            server,
            fake,
            admin,
            viewer,
            outsider,
            library,
            ordinary,
            state,
        }
    }

    fn db(&self) -> records::Db {
        self.server.records_db()
    }

    async fn item(&self, path: &str) -> records::ExternalItem {
        let db = self.db();
        let source = db
            .source_items(self.library)
            .await
            .unwrap()
            .into_iter()
            .find(|item| item.repository_path == path)
            .unwrap_or_else(|| panic!("no source item at {path}"));
        db.external_item(source.node).await.unwrap().unwrap()
    }

    async fn get(&self, user: &TestUser, path: &str) -> reqwest::Response {
        self.server
            .http(Some(user))
            .get(self.server.url(path))
            .send()
            .await
            .unwrap()
    }

    async fn download(&self, user: &TestUser, path: &str) -> reqwest::Response {
        let item = self.item(path).await;
        self.get(user, &format!("/api/docs/{}/content", item.node.0))
            .await
    }

    async fn listing(&self) -> Value {
        let response = self
            .get(
                &self.viewer,
                &format!("/api/nodes/{}/children", self.library.0),
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        response.json().await.unwrap()
    }
}

async fn body_json(response: reqwest::Response) -> (StatusCode, Value) {
    let status = response.status();
    let body = response.json().await.unwrap();
    (status, body)
}

/// A response's status, headers and body as text, for searching.
async fn as_text(response: reqwest::Response) -> String {
    let status = response.status();
    let headers = format!("{:?}", response.headers());
    let body = response.text().await.unwrap();
    format!("{status} {headers} {body}")
}

async fn assert_source_unavailable(
    env: &Env,
    path: &str,
    expected: StatusCode,
    reason: &str,
) -> Value {
    let (status, body) = body_json(env.download(&env.viewer, path).await).await;
    assert_eq!(status, expected, "{path}: {body}");
    assert_eq!(body["error"], "source_unavailable", "{path}: {body}");
    assert_eq!(body["reason"], reason, "{path}: {body}");
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|message| !message.contains(BUCKET) && !message.contains(KEY_ID)),
        "{path}: {body}"
    );
    body
}

async fn wait_scan_row(db: &records::Db, bytes: &[u8]) {
    let digest = sha(bytes);
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        if db.scan_state(&digest).await.unwrap().is_some() {
            return;
        }
        assert!(tokio::time::Instant::now() < deadline, "scan row timed out");
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn download_returns_the_objects_bytes_and_the_items_revision_id() {
    let env = Env::new(&[("report.txt", b"quarterly")]).await;
    let item = env.item("report.txt").await;

    let response = env.download(&env.viewer, "report.txt").await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-sharecraft-revision"], item.revision);
    assert_eq!(response.bytes().await.unwrap(), b"quarterly".as_slice());
}

#[tokio::test]
async fn a_range_request_returns_206_from_the_stored_content() {
    let env = Env::new(&[("range.bin", b"0123456789")]).await;
    let item = env.item("range.bin").await;

    let response = env
        .server
        .http(Some(&env.viewer))
        .get(
            env.server
                .url(&format!("/api/docs/{}/content", item.node.0)),
        )
        .header(RANGE, "bytes=2-5")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.headers()[CONTENT_RANGE], "bytes 2-5/10");
    assert_eq!(response.bytes().await.unwrap(), b"2345".as_slice());
}

#[tokio::test]
async fn preview_and_thumbnail_work() {
    let env = Env::new(&[("picture.jpg", JPEG), ("note.txt", b"hello store")]).await;

    let picture = env.item("picture.jpg").await;
    let preview = env
        .get(
            &env.viewer,
            &format!("/api/docs/{}/preview", picture.node.0),
        )
        .await;
    assert_eq!(preview.status(), StatusCode::OK);
    assert_eq!(preview.headers()["content-type"], "image/jpeg");
    assert_eq!(preview.bytes().await.unwrap(), JPEG);

    let note = env.item("note.txt").await;
    let text = env
        .get(&env.viewer, &format!("/api/docs/{}/preview", note.node.0))
        .await;
    assert_eq!(text.status(), StatusCode::OK);
    assert_eq!(text.headers()["content-type"], "text/plain; charset=utf-8");
    assert_eq!(text.bytes().await.unwrap(), b"hello store".as_slice());

    let thumbnail = env
        .get(
            &env.viewer,
            &format!("/api/docs/{}/thumbnail", picture.node.0),
        )
        .await;
    assert_eq!(thumbnail.status(), StatusCode::OK);
    assert_eq!(thumbnail.headers()["content-type"], "image/png");
    assert!(!thumbnail.bytes().await.unwrap().is_empty());
    assert_eq!(env.fake.count("get"), 2);
}

#[tokio::test]
async fn an_anyone_link_download_goes_through_the_scan_gate() {
    let env = Env::new(&[("linked.txt", b"linked")]).await;
    let item = env.item("linked.txt").await;
    env.db()
        .set_setting(env.admin.id, ANYONE_LINKS, json!(true))
        .await
        .unwrap();
    let expires = (OffsetDateTime::now_utc() + time::Duration::days(1))
        .format(&Rfc3339)
        .unwrap();
    let made = env
        .server
        .http(Some(&env.admin))
        .post(env.server.url(&format!("/api/nodes/{}/links", item.node.0)))
        .json(&json!({ "kind": "anyone", "role": "viewer", "expires_at": expires }))
        .send()
        .await
        .unwrap();
    assert_eq!(made.status(), StatusCode::CREATED);
    let value: Value = made.json().await.unwrap();
    let token = value["url"].as_str().unwrap().rsplit('/').next().unwrap();

    let response = reqwest::Client::new()
        .get(
            env.server
                .url(&format!("/api/l/{token}/content/{}", item.node.0)),
        )
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.bytes().await.unwrap(), b"linked".as_slice());
    assert_eq!(
        env.db()
            .scan_state(&sha(b"linked"))
            .await
            .unwrap()
            .unwrap()
            .state,
        ScanState::Unscanned
    );
    assert_eq!(env.fake.count("get"), 1);
}

#[tokio::test]
async fn an_infected_object_is_423_everywhere() {
    let mut bytes = b"prefix ".to_vec();
    bytes.extend_from_slice(&eicar());
    let env = Env::new(&[("infected.jpg", &bytes)]).await;
    let scanner = FakeIcap::start("127.0.0.1:0".parse().unwrap(), Behaviour::default()).unwrap();
    env.db()
        .set_setting(env.admin.id, SCANNER_URL, json!(scanner.url()))
        .await
        .unwrap();
    let item = env.item("infected.jpg").await;
    let state = env.state.clone();
    let node = item.node;
    let read = tokio::spawn(async move { materialise(&state, node).await });
    wait_scan_row(&env.db(), &bytes).await;
    assert_eq!(
        server::scan_once(&env.state, OffsetDateTime::now_utc()).await,
        1
    );
    assert!(matches!(
        read.await.unwrap(),
        Err(ExternalError::Held(ScanState::Infected))
    ));

    for suffix in ["content", "preview", "thumbnail"] {
        let (status, body) = body_json(
            env.get(&env.viewer, &format!("/api/docs/{}/{suffix}", item.node.0))
                .await,
        )
        .await;
        assert_eq!(status, StatusCode::LOCKED, "{suffix}: {body}");
        assert_eq!(body["scan"], "infected", "{suffix}: {body}");
    }
    let page = env.listing().await;
    assert_eq!(page["items"][0]["scan"], "quarantined");
}

#[tokio::test]
async fn an_archived_object_is_409_source_unavailable_archived_and_listed_so() {
    let fake = FakeS3::start().await;
    fake.bucket(BUCKET);
    fake.require_keys(KEY_ID, SECRET);
    fake.put_with(
        BUCKET,
        "cold.bin",
        b"frozen",
        Opts {
            class: Some("GLACIER".to_owned()),
            ..Opts::default()
        },
    );
    let env = Env::with_fake(fake).await;

    let body = assert_source_unavailable(&env, "cold.bin", StatusCode::CONFLICT, "archived").await;
    assert_eq!(body["message"], ARCHIVED_MESSAGE);
    assert_eq!(env.fake.count("get"), 0);

    let item = env.item("cold.bin").await;
    let (_, detail) = body_json(
        env.get(&env.viewer, &format!("/api/docs/{}", item.node.0))
            .await,
    )
    .await;
    assert_eq!(detail["external"]["storage_class_archived"], true);
    assert_eq!(detail["external"]["unavailable"], "archived");
    let page = env.listing().await;
    let entry = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == "cold.bin")
        .unwrap();
    assert_eq!(entry["unavailable"], "archived");
}

#[tokio::test]
async fn over_budget_is_409_source_unavailable_budget() {
    let env = Env::new(&[("cached.txt", b"cached"), ("cold.txt", b"cold")]).await;
    env.db()
        .set_budget(
            env.library,
            &Budget {
                list_requests: 1_000,
                read_requests: 2,
                bytes_down: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        env.download(&env.viewer, "cached.txt").await.status(),
        StatusCode::OK
    );
    let requests = env.fake.requests().len();

    let body = assert_source_unavailable(&env, "cold.txt", StatusCode::CONFLICT, "budget").await;
    assert_eq!(body["message"], BUDGET_MESSAGE);
    assert_eq!(env.fake.requests().len(), requests);
    assert_eq!(
        env.download(&env.viewer, "cached.txt").await.status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn gone_denied_offline_and_not_approved_are_409_with_their_reasons() {
    // Offline is answered 503 `source_unavailable` (as the file-share suite pins it); the
    // other three are 409 with their reasons.
    let env = Env::new(&[
        ("gone.txt", b"gone"),
        ("denied.txt", b"denied"),
        ("offline.txt", b"offline"),
        ("withdrawn.txt", b"withdrawn"),
    ])
    .await;
    env.fake.remove(BUCKET, "gone.txt");
    env.fake.deny(BUCKET, "denied.txt");
    let gone = assert_source_unavailable(&env, "gone.txt", StatusCode::CONFLICT, "gone").await;
    assert_eq!(
        gone["message"],
        "This file is no longer in the library's S3 storage."
    );
    assert_source_unavailable(&env, "denied.txt", StatusCode::CONFLICT, "denied").await;

    set_endpoints(&env.server, endpoint_at(&dead_origin().await)).await;
    let offline = assert_source_unavailable(
        &env,
        "offline.txt",
        StatusCode::SERVICE_UNAVAILABLE,
        "offline",
    )
    .await;
    assert_eq!(
        offline["message"],
        "The library's S3 storage cannot be reached. Files opened before still open; others will open when it is back."
    );

    set_endpoints(&env.server, json!([])).await;
    let withdrawn =
        assert_source_unavailable(&env, "withdrawn.txt", StatusCode::CONFLICT, "not_approved")
            .await;
    assert_eq!(
        withdrawn["message"],
        "The S3 endpoint this library uses is no longer approved by an administrator."
    );
}

#[tokio::test]
async fn a_person_without_access_gets_404_and_the_store_is_not_contacted() {
    let env = Env::new(&[("private.txt", b"private")]).await;
    let requests = env.fake.requests().len();

    let response = env.download(&env.outsider, "private.txt").await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(env.fake.requests().len(), requests);
    assert_eq!(env.fake.count("head"), 0);
    assert_eq!(env.fake.count("get"), 0);
}

#[tokio::test]
async fn copying_out_of_s3_and_out_of_github_works() {
    let env = Env::new(&[("folder/file.txt", b"copy")]).await;
    let item = env.item("folder/file.txt").await;
    let folder = env
        .db()
        .node(item.node)
        .await
        .unwrap()
        .unwrap()
        .parent
        .unwrap();
    // Out of S3, in bulk and alone: ordinary copies of the bytes (see `s3_copy.rs`).
    let (status, body) = body_json(
        env.server
            .http(Some(&env.admin))
            .post(env.server.url("/api/bulk"))
            .json(&json!({
                "op": "copy",
                "items": [item.node.0.to_string()],
                "parent": env.ordinary.0.to_string(),
            }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body[0]["ok"], true, "{body}");
    let (status, body) = body_json(
        env.server
            .http(Some(&env.admin))
            .post(env.server.url(&format!("/api/nodes/{}/copy", folder.0)))
            .json(&json!({ "parent": env.ordinary.0.to_string() }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let github = start_fake_github("copy-owner", "copy-repo").await;
    github.commit("main", &[("github.txt", b"github".to_vec())]);
    let origin = github.origin();
    let server = spawn_server_config_without_workers(|config| config.github_api = origin).await;
    let admin = server.admin("GitHub copy admin").await;
    let ordinary = common::Site::library(&server).await;
    let site = server
        .records_db()
        .node(ordinary)
        .await
        .unwrap()
        .unwrap()
        .site;
    let response = server
        .http(Some(&admin))
        .post(server.url(&format!("/api/sites/{}/libraries", site.0)))
        .json(&json!({ "name": "GitHub files", "kind": "document" }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let github_library = NodeId(
        response.json::<Value>().await.unwrap()["id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap(),
    );
    let state = server.app_state().await;
    server
        .records_db()
        .connect_source(&NewSource {
            library: github_library,
            kind: SourceKind::Github,
            owner: "copy-owner".to_owned(),
            repo: "copy-repo".to_owned(),
            branch: "main".to_owned(),
            folder: String::new(),
            location: None,
            mode: SourceMode::ReadOnly,
            token: None,
            poll_seconds: 60,
            connected_by: admin.id,
        })
        .await
        .unwrap();
    sync_once(&state, github_library, OffsetDateTime::now_utc())
        .await
        .unwrap()
        .unwrap();
    let github_item = server
        .records_db()
        .source_items(github_library)
        .await
        .unwrap()
        .into_iter()
        .find(|item| item.repository_path == "github.txt")
        .unwrap()
        .node;
    let copied = server
        .http(Some(&admin))
        .post(server.url(&format!("/api/nodes/{}/copy", github_item.0)))
        .json(&json!({ "parent": ordinary.0.to_string() }))
        .send()
        .await
        .unwrap();
    assert_eq!(copied.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn no_answer_holds_the_bucket_key_id_or_endpoint_for_a_viewer() {
    let fake = FakeS3::start().await;
    fake.bucket(BUCKET);
    fake.require_keys(KEY_ID, SECRET);
    fake.put(BUCKET, "ok.txt", b"ok");
    fake.put(BUCKET, "gone.txt", b"gone");
    fake.put(BUCKET, "denied.txt", b"denied");
    fake.put(BUCKET, "later.txt", b"later");
    fake.put_with(
        BUCKET,
        "cold.bin",
        b"frozen",
        Opts {
            class: Some("GLACIER".to_owned()),
            ..Opts::default()
        },
    );
    let origin = fake.origin();
    let env = Env::with_fake(fake).await;
    env.fake.remove(BUCKET, "gone.txt");
    env.fake.deny(BUCKET, "denied.txt");

    let mut answers = Vec::new();
    for path in ["ok.txt", "gone.txt", "denied.txt", "cold.bin"] {
        let item = env.item(path).await;
        for suffix in ["content", "preview", "thumbnail"] {
            let response = env
                .get(&env.viewer, &format!("/api/docs/{}/{suffix}", item.node.0))
                .await;
            answers.push(as_text(response).await);
        }
        let response = env
            .get(&env.viewer, &format!("/api/docs/{}", item.node.0))
            .await;
        answers.push(as_text(response).await);
    }
    answers.push(
        as_text(
            env.get(
                &env.viewer,
                &format!("/api/nodes/{}/children", env.library.0),
            )
            .await,
        )
        .await,
    );
    set_endpoints(&env.server, endpoint_at(&dead_origin().await)).await;
    let item = env.item("later.txt").await;
    answers.push(as_text(env.download(&env.viewer, "later.txt").await).await);
    answers.push(
        as_text(
            env.get(&env.viewer, &format!("/api/docs/{}", item.node.0))
                .await,
        )
        .await,
    );

    let authority = origin.trim_start_matches("http://");
    for answer in &answers {
        for secret in [BUCKET, KEY_ID, SECRET, ENDPOINT, authority, "127.0.0.1"] {
            assert!(
                !answer.contains(secret),
                "an answer holds {secret}: {answer}"
            );
        }
    }
}
