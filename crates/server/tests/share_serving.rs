//! Share-backed documents are served through the ordinary HTTP surfaces without bypassing scans.

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

use std::sync::Arc;

use collab_core::ids::{DocId, NodeId};
use common::fake_share::FakeShare;
use common::{
    FileClient, TestServer, TestUser, spawn_server_config_without_workers, start_fake_github,
};
use fake_icap::{Behaviour, FakeIcap, eicar};
use records::{
    ANYONE_LINKS, NewSource, Principal, SCANNER_URL, SHARE_ROOTS, ScanState, SourceKind, SourceMode,
};
use reqwest::StatusCode;
use reqwest::header::{CONTENT_RANGE, RANGE};
use serde_json::{Value, json};
use server::AppState;
use server::external::evict;
use server::source_sync::sync_once;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

const ROOT: &str = r"C:\approved";

struct Env {
    server: TestServer,
    fake: FakeShare,
    state: AppState,
    admin: TestUser,
    viewer: TestUser,
    outsider: TestUser,
    library: NodeId,
    ordinary: NodeId,
}

impl Env {
    async fn new(files: &[(&str, &[u8], i64)]) -> Self {
        let server = spawn_server_config_without_workers(|config| {
            config.max_file_bytes = 512 * 1024 * 1024;
        })
        .await;
        let admin = server.admin("Share admin").await;
        let viewer = server.user("Share viewer").await;
        let outsider = server.user("Share outsider").await;
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
            .json(&json!({ "name": "Shared files", "kind": "document" }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let library = NodeId(
            response.json::<Value>().await.unwrap()["id"]
                .as_str()
                .unwrap()
                .parse()
                .unwrap(),
        );
        let state = server.app_state().await;
        let fake = FakeShare::new(ROOT);
        for (path, bytes, modified) in files {
            fake.put_at(path, bytes, *modified);
        }
        state.shares().set_fs(Arc::new(fake.clone()));
        let db = server.records_db();
        db.set_setting(admin.id, SHARE_ROOTS, json!([ROOT]))
            .await
            .unwrap();
        db.connect_source(&NewSource {
            library,
            kind: SourceKind::Share,
            owner: String::new(),
            repo: String::new(),
            branch: String::new(),
            folder: String::new(),
            location: Some(ROOT.to_owned()),
            mode: SourceMode::ReadOnly,
            token: None,
            poll_seconds: 60,
            connected_by: admin.id,
        })
        .await
        .unwrap();
        if !files.is_empty() {
            sync_once(&state, library, OffsetDateTime::UNIX_EPOCH)
                .await
                .unwrap()
                .expect("the listing changed");
        }
        db.set_node_access(
            admin.id,
            library,
            Principal::User(viewer.id),
            Some("viewer"),
        )
        .await
        .unwrap();
        Self {
            server,
            fake,
            state,
            admin,
            viewer,
            outsider,
            library,
            ordinary,
        }
    }

    fn db(&self) -> records::Db {
        self.server.records_db()
    }

    async fn item(&self, path: &str) -> records::ExternalItem {
        let source = self
            .db()
            .source_items(self.library)
            .await
            .unwrap()
            .into_iter()
            .find(|item| item.repository_path == path)
            .unwrap_or_else(|| panic!("no source item at {path}"));
        self.db().external_item(source.node).await.unwrap().unwrap()
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
}

fn jpeg() -> &'static [u8] {
    include_bytes!("fixtures/images/gradient.jpg")
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

async fn body_json(response: reqwest::Response) -> (StatusCode, Value) {
    let status = response.status();
    let body = response.json().await.unwrap();
    (status, body)
}

#[tokio::test]
async fn download_returns_the_shares_bytes_and_the_items_revision_id() {
    let env = Env::new(&[("report.txt", b"quarterly", 1)]).await;
    let item = env.item("report.txt").await;
    let response = env.download(&env.viewer, "report.txt").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-sharecraft-revision"], item.revision);
    assert_eq!(response.bytes().await.unwrap(), b"quarterly".as_slice());
}

#[tokio::test]
async fn a_range_request_returns_206_with_the_right_bytes() {
    let env = Env::new(&[("range.bin", b"0123456789", 1)]).await;
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
async fn download_with_the_reported_revision_works_and_a_stale_one_is_404() {
    let env = Env::new(&[("version.txt", b"version", 1)]).await;
    let item = env.item("version.txt").await;
    let good = env
        .get(
            &env.viewer,
            &format!(
                "/api/docs/{}/content?revision={}",
                item.node.0, item.revision
            ),
        )
        .await;
    assert_eq!(good.status(), StatusCode::OK);
    assert_eq!(good.bytes().await.unwrap(), b"version".as_slice());
    let stale = env
        .get(
            &env.viewer,
            &format!(
                "/api/docs/{}/content?revision={}",
                item.node.0,
                "0".repeat(40)
            ),
        )
        .await;
    assert_eq!(stale.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn docs_show_reports_the_revision_id_and_external_details() {
    let env = Env::new(&[("details.txt", b"details", 1_000_000_000)]).await;
    let item = env.item("details.txt").await;
    let (_, before) = body_json(
        env.get(&env.viewer, &format!("/api/docs/{}", item.node.0))
            .await,
    )
    .await;
    assert_eq!(before["revision"], item.revision);
    assert_eq!(before["external"]["bytes"], 7);
    assert_eq!(before["external"]["modified_at"], "1970-01-01T00:00:01Z");
    assert_eq!(before["external"]["read"], false);
    assert_eq!(
        env.download(&env.viewer, "details.txt").await.status(),
        StatusCode::OK
    );
    let (_, after) = body_json(
        env.get(&env.viewer, &format!("/api/docs/{}", item.node.0))
            .await,
    )
    .await;
    assert_eq!(after["external"]["read"], true);
}

#[tokio::test]
async fn preview_of_an_image_and_text_preview_work() {
    let env = Env::new(&[("picture.jpg", jpeg(), 1), ("note.txt", b"hello share", 1)]).await;
    for (path, expected_type, expected) in [
        ("picture.jpg", "image/jpeg", jpeg()),
        (
            "note.txt",
            "text/plain; charset=utf-8",
            b"hello share".as_slice(),
        ),
    ] {
        let item = env.item(path).await;
        let response = env
            .get(&env.viewer, &format!("/api/docs/{}/preview", item.node.0))
            .await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        assert_eq!(response.headers()["content-type"], expected_type, "{path}");
        assert_eq!(response.bytes().await.unwrap(), expected, "{path}");
    }
}

#[tokio::test]
async fn thumbnail_is_made_from_the_share_on_first_view() {
    let env = Env::new(&[("picture.jpg", jpeg(), 1)]).await;
    let item = env.item("picture.jpg").await;
    let response = env
        .get(&env.viewer, &format!("/api/docs/{}/thumbnail", item.node.0))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "image/png");
    assert!(!response.bytes().await.unwrap().is_empty());
    assert_eq!(env.fake.opens("picture.jpg"), 1);
}

#[tokio::test]
async fn an_anyone_link_download_works_and_goes_through_the_scan_gate() {
    let env = Env::new(&[("linked.txt", b"linked", 1)]).await;
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
            .scan_state(&sha256(b"linked"))
            .await
            .unwrap()
            .unwrap()
            .state,
        ScanState::Unscanned
    );
    assert_eq!(env.fake.opens("linked.txt"), 1);
}

#[tokio::test]
async fn an_infected_file_is_423_everywhere_and_listed_as_quarantined() {
    let mut bytes = b"prefix ".to_vec();
    bytes.extend_from_slice(&eicar());
    let env = Env::new(&[("infected.jpg", &bytes, 1)]).await;
    let scanner = FakeIcap::start("127.0.0.1:0".parse().unwrap(), Behaviour::default()).unwrap();
    env.db()
        .set_setting(env.admin.id, SCANNER_URL, json!(scanner.url()))
        .await
        .unwrap();
    let item = env.item("infected.jpg").await;
    let state = env.state.clone();
    let node = item.node;
    let read = tokio::spawn(async move { server::external::materialise(&state, node).await });
    let deadline = tokio::time::Instant::now() + common::TIMEOUT;
    while env
        .db()
        .scan_state(&sha256(&bytes))
        .await
        .unwrap()
        .is_none()
    {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::task::yield_now().await;
    }
    assert_eq!(
        server::scan_once(&env.state, OffsetDateTime::now_utc()).await,
        1
    );
    assert!(matches!(
        read.await.unwrap(),
        Err(server::external::ExternalError::Held(ScanState::Infected))
    ));
    for suffix in ["content", "preview", "thumbnail"] {
        let (status, body) = body_json(
            env.get(&env.viewer, &format!("/api/docs/{}/{suffix}", item.node.0))
                .await,
        )
        .await;
        assert_eq!(status, StatusCode::LOCKED, "{suffix}: {body}");
        assert_eq!(body["scan"], "infected");
    }
    let (_, page) = body_json(
        env.get(
            &env.viewer,
            &format!("/api/nodes/{}/children", env.library.0),
        )
        .await,
    )
    .await;
    assert_eq!(page["items"][0]["scan"], "quarantined");
}

async fn assert_source_unavailable_status(
    env: Env,
    path: &str,
    reason: &str,
    expected: StatusCode,
) {
    let (status, body) = body_json(env.download(&env.viewer, path).await).await;
    assert_eq!(status, expected, "{body}");
    assert_eq!(body["error"], "source_unavailable");
    assert_eq!(body["reason"], reason);
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|message| !message.contains(ROOT))
    );
}

async fn assert_source_unavailable(env: Env, path: &str, reason: &str) {
    assert_source_unavailable_status(env, path, reason, StatusCode::CONFLICT).await;
}

#[tokio::test]
async fn a_vanished_file_is_409_source_unavailable_gone() {
    let env = Env::new(&[("gone.txt", b"gone", 1)]).await;
    env.fake.remove("gone.txt");
    assert_source_unavailable(env, "gone.txt", "gone").await;
}

#[tokio::test]
async fn a_denied_file_is_409_source_unavailable_denied() {
    let env = Env::new(&[("denied.txt", b"denied", 1)]).await;
    env.fake.deny("denied.txt");
    assert_source_unavailable(env, "denied.txt", "denied").await;
}

#[tokio::test]
async fn an_uncached_offline_file_is_503_source_unavailable_offline() {
    let env = Env::new(&[("offline.txt", b"offline", 1)]).await;
    env.fake.offline(true);
    assert_source_unavailable_status(
        env,
        "offline.txt",
        "offline",
        StatusCode::SERVICE_UNAVAILABLE,
    )
    .await;
}

#[tokio::test]
async fn a_file_outside_current_approval_is_409_source_unavailable_not_approved() {
    let env = Env::new(&[("withdrawn.txt", b"withdrawn", 1)]).await;
    env.db()
        .set_setting(env.admin.id, SHARE_ROOTS, json!([]))
        .await
        .unwrap();
    assert_source_unavailable(env, "withdrawn.txt", "not_approved").await;
}

#[tokio::test]
async fn an_offline_share_still_serves_a_cached_file() {
    let env = Env::new(&[("cached.txt", b"cached", 1)]).await;
    assert_eq!(
        env.download(&env.viewer, "cached.txt").await.status(),
        StatusCode::OK
    );
    env.fake.offline(true);
    let response = env.download(&env.viewer, "cached.txt").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.bytes().await.unwrap(), b"cached".as_slice());
}

#[tokio::test]
async fn a_person_without_access_gets_404_and_the_share_is_not_touched() {
    let env = Env::new(&[("private.txt", b"private", 1)]).await;
    let before = env.fake.stats();
    let response = env.download(&env.outsider, "private.txt").await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(env.fake.stats(), before);
    assert_eq!(env.fake.opens("private.txt"), 0);
}

#[tokio::test]
async fn the_listing_shows_the_modified_time_and_marks_unavailable_files() {
    let env = Env::new(&[
        ("available.jpg", jpeg(), 1_000_000_000),
        ("missing.txt", b"missing", 2),
    ])
    .await;
    env.fake.remove("missing.txt");
    let _ = env.download(&env.viewer, "missing.txt").await;
    let (_, page) = body_json(
        env.get(
            &env.viewer,
            &format!("/api/nodes/{}/children", env.library.0),
        )
        .await,
    )
    .await;
    let items = page["items"].as_array().unwrap();
    let image = items
        .iter()
        .find(|item| item["name"] == "available.jpg")
        .unwrap();
    assert_eq!(image["external"], true);
    assert_eq!(image["revised_at"], "1970-01-01T00:00:01Z");
    assert_eq!(image["thumbnail"], true);
    let missing = items
        .iter()
        .find(|item| item["name"] == "missing.txt")
        .unwrap();
    assert_eq!(missing["external"], true);
    assert_eq!(missing["unavailable"], "gone");
}

#[tokio::test]
async fn copying_a_share_file_or_folder_is_refused_and_copying_out_of_github_still_works() {
    let env = Env::new(&[("folder/file.txt", b"copy", 1)]).await;
    let item = env.item("folder/file.txt").await;
    let folder = env
        .db()
        .node(item.node)
        .await
        .unwrap()
        .unwrap()
        .parent
        .unwrap();
    for node in [item.node, folder] {
        let (status, body) = body_json(
            env.server
                .http(Some(&env.admin))
                .post(env.server.url(&format!("/api/nodes/{}/copy", node.0)))
                .json(&json!({ "parent": env.ordinary.0.to_string() }))
                .send()
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["error"], "source_copy_unavailable");
    }
    let response = env.server.http(Some(&env.admin)).post(env.server.url("/api/bulk"))
        .json(&json!({ "op": "copy", "items": [item.node.0.to_string()], "parent": env.ordinary.0.to_string() }))
        .send().await.unwrap();
    let (_, body) = body_json(response).await;
    assert_eq!(body[0]["error"], "source_copy_unavailable");

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
async fn an_editor_file_opens_read_only_and_its_page_content_request_works() {
    let env = Env::new(&[("sheet.xlsx", b"sheet bytes", 1)]).await;
    let item = env.item("sheet.xlsx").await;
    let client = FileClient::connect_as(&env.server, DocId(item.node.0), &env.viewer).await;
    assert!(!client.can_edit);
    assert!(client.token().is_none());
    let response = env
        .get(
            &env.viewer,
            &format!(
                "/api/docs/{}/content?revision={}",
                item.node.0, item.revision
            ),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.bytes().await.unwrap(), b"sheet bytes".as_slice());
}

#[tokio::test]
async fn every_write_route_is_refused_in_a_share_library() {
    let env = Env::new(&[("write.txt", b"write", 1)]).await;
    let item = env.item("write.txt").await;
    let (status, body) = body_json(
        env.server
            .http(Some(&env.admin))
            .patch(env.server.url(&format!("/api/nodes/{}", item.node.0)))
            .json(&json!({ "name": "changed.txt" }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "source_read_only");
    assert_eq!(
        body["message"],
        "This library is kept in step with an outside source, so it cannot be changed here."
    );
}

#[tokio::test]
async fn a_download_in_progress_survives_eviction() {
    let bytes = vec![b'x'; 2 * 1024 * 1024];
    let env = Env::new(&[("large.bin", &bytes, 1)]).await;
    let response = env.download(&env.viewer, "large.bin").await;
    assert_eq!(response.status(), StatusCode::OK);
    env.state.shares().set_cache_max_bytes_for_tests(0);
    assert_eq!(evict(&env.state).await, 0);
    assert_eq!(response.bytes().await.unwrap(), bytes);
}
