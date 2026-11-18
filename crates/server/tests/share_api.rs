//! Connecting and managing file-share libraries and their organisation settings.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::sync::Arc;

use collab_core::ids::NodeId;
use common::fake_share::FakeShare;
use common::{TestServer, TestUser, spawn_server_config_without_workers};
use records::{Principal, SHARE_ROOTS, SourceKind, SourceState};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use server::AppState;

const ROOT: &str = r"C:\approved";

struct Env {
    server: TestServer,
    fake: FakeShare,
    admin: TestUser,
    site: NodeId,
    state: AppState,
}

impl Env {
    async fn new() -> Self {
        let server = spawn_server_config_without_workers(|config| {
            config.max_file_bytes = 512 * 1024 * 1024;
        })
        .await;
        let admin = server.admin("Share admin").await;
        let library = common::Site::library(&server).await;
        let site = server
            .records_db()
            .node(library)
            .await
            .unwrap()
            .unwrap()
            .site;
        let state = server.app_state().await;
        let fake = FakeShare::new(ROOT);
        state.shares().set_fs(Arc::new(fake.clone()));
        server
            .records_db()
            .set_setting(admin.id, SHARE_ROOTS, json!([ROOT]))
            .await
            .unwrap();
        Self {
            server,
            fake,
            admin,
            site,
            state,
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
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    async fn connect(&self, body: Value) -> (StatusCode, Value) {
        self.call(
            &self.admin,
            Method::POST,
            &format!("/api/sites/{}/libraries/share", self.site.0),
            Some(body),
        )
        .await
    }

    async fn connected(&self, name: &str) -> NodeId {
        let (status, answer) = self.connect(json!({"name": name, "path": ROOT})).await;
        assert_eq!(status, StatusCode::CREATED, "{answer}");
        NodeId(answer["id"].as_str().unwrap().parse().unwrap())
    }
}

#[tokio::test]
async fn connect_creates_a_library_and_the_first_listing_fills_it() {
    let env = Env::new().await;
    env.fake.put("Guide.txt", b"guide");
    let (status, answer) = env.connect(json!({"name": "Shared", "path": ROOT})).await;
    assert_eq!(status, StatusCode::CREATED, "{answer}");
    let library = NodeId(answer["id"].as_str().unwrap().parse().unwrap());
    let (config, _) = env
        .server
        .records_db()
        .source_of(library)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(config.kind, SourceKind::Share);
    assert_eq!(config.location.as_deref(), Some(ROOT));
    server::source_sync::sync_once(&env.state, library, time::OffsetDateTime::UNIX_EPOCH)
        .await
        .unwrap();
    assert_eq!(
        env.server
            .records_db()
            .live_children(library)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(env.fake.opens("Guide.txt"), 0);
}

#[tokio::test]
async fn connecting_outside_every_root_is_refused_and_makes_nothing() {
    let env = Env::new().await;
    let before: i64 = sqlx::query_scalar("select count(*) from library_sources")
        .fetch_one(env.server.records_db().test_pool())
        .await
        .unwrap();
    let (status, answer) = env
        .connect(json!({"name": "Outside", "path": r"C:\elsewhere"}))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
    assert_eq!(answer["error"], "share_not_approved");
    let after: i64 = sqlx::query_scalar("select count(*) from library_sources")
        .fetch_one(env.server.records_db().test_pool())
        .await
        .unwrap();
    assert_eq!(after, before);
    assert_eq!(env.fake.lists(), 0);
}

#[tokio::test]
async fn each_bad_path_is_refused_with_its_reason() {
    let env = Env::new().await;
    for (path, reason) in [
        ("relative", "relative"),
        (r"C:\approved\..\outside", "traversal"),
        (r"\\?\C:\approved", "device"),
        (r"C:\approved\CON", "characters"),
        (r"C:\approved\file.txt:secret", "stream"),
        (r"\\server\C$", "whole_drive"),
    ] {
        let (status, answer) = env
            .connect(json!({"name": format!("Bad {reason}"), "path": path}))
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{path}: {answer}");
        assert_eq!(answer["error"], "invalid_share_path", "{path}: {answer}");
        assert_eq!(answer["reason"], reason, "{path}: {answer}");
    }
    assert_eq!(env.fake.lists(), 0);
}

#[tokio::test]
async fn only_administrators_read_and_change_the_shares_settings() {
    let env = Env::new().await;
    let member = env.server.user("Ordinary member").await;
    for method in [Method::GET, Method::PUT] {
        let body = (method == Method::PUT).then(|| json!({"list_seconds": 120}));
        let (status, answer) = env
            .call(&member, method, "/api/settings/shares", body)
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{answer}");
    }
    let (status, answer) = env
        .call(&env.admin, Method::GET, "/api/settings/shares", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["roots"], json!([ROOT]));
    let (status, answer) = env
        .call(
            &env.admin,
            Method::PUT,
            "/api/settings/shares",
            Some(json!({"list_seconds": 120, "max_files": 1234})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["list_seconds"], 120);
    assert_eq!(answer["max_files"], 1234);
}

#[tokio::test]
async fn show_gives_the_path_to_owners_and_not_to_viewers() {
    let env = Env::new().await;
    env.fake.put("Guide.txt", b"guide");
    let library = env.connected("Status share").await;
    let viewer = env.server.user("Share viewer").await;
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
    env.fake.slow_list(std::time::Duration::from_secs(1));
    let syncing = tokio::spawn({
        let state = env.state.clone();
        async move {
            server::source_sync::sync_once(&state, library, time::OffsetDateTime::UNIX_EPOCH)
                .await
                .unwrap()
        }
    });
    let deadline = tokio::time::Instant::now() + common::TIMEOUT;
    while env.fake.lists() < 2 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "listing did not start"
        );
        tokio::task::yield_now().await;
    }
    let path = format!("/api/libraries/{}/source", library.0);
    let (status, owner) = env.call(&env.admin, Method::GET, &path, None).await;
    assert_eq!(status, StatusCode::OK, "{owner}");
    assert_eq!(owner["path"], ROOT);
    assert_eq!(owner["listing_now"]["files_so_far"], 0);
    let (status, viewed) = env.call(&viewer, Method::GET, &path, None).await;
    assert_eq!(status, StatusCode::OK, "{viewed}");
    assert_eq!(viewed["path"], Value::Null);
    syncing.await.unwrap();
}

#[tokio::test]
async fn the_data_folder_and_anything_around_it_is_refused() {
    let mut env = Env::new().await;
    let data = env.server.data_dir.path().to_string_lossy().into_owned();
    let parent = env
        .server
        .data_dir
        .path()
        .parent()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let fake = FakeShare::new(&parent);
    env.state.shares().set_fs(Arc::new(fake.clone()));
    env.fake = fake;
    env.server
        .records_db()
        .set_setting(env.admin.id, SHARE_ROOTS, json!([parent]))
        .await
        .unwrap();
    for path in [
        data,
        env.server
            .data_dir
            .path()
            .parent()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
    ] {
        let (status, answer) = env
            .connect(json!({"name": format!("Private {}", path.len()), "path": path}))
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
        assert_eq!(answer["error"], "share_not_approved");
    }
    assert_eq!(env.fake.lists(), 0);
}

#[tokio::test]
async fn an_unreachable_denied_or_missing_folder_is_refused_plainly() {
    for fault in ["missing", "denied", "offline"] {
        let env = Env::new().await;
        let path = if fault == "missing" {
            r"C:\approved\missing"
        } else {
            ROOT
        };
        match fault {
            "denied" => env.fake.deny(ROOT),
            "offline" => env.fake.offline(true),
            _ => {}
        }
        let (status, answer) = env.connect(json!({"name": fault, "path": path})).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
        assert_eq!(answer["error"], "share_unreachable");
        assert_eq!(
            answer["reason"],
            if fault == "missing" {
                "not_found"
            } else {
                fault
            }
        );
        assert!(!answer.to_string().contains("denied by Windows"));
    }
}

#[tokio::test]
async fn template_and_asset_kinds_are_refused() {
    let env = Env::new().await;
    for kind in ["template", "asset"] {
        let (status, answer) = env
            .connect(json!({"name": kind, "path": ROOT, "kind": kind}))
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
        assert_eq!(answer["error"], "invalid_request");
    }
    let (status, answer) = env
        .connect(json!({"name": "write", "path": ROOT, "mode": "read_write"}))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
}

#[tokio::test]
async fn only_site_creators_may_connect_and_only_owners_may_change_or_disconnect() {
    let env = Env::new().await;
    let viewer = env.server.user("Limited viewer").await;
    let (status, _) = env
        .call(
            &viewer,
            Method::POST,
            &format!("/api/sites/{}/libraries/share", env.site.0),
            Some(json!({"name":"No", "path":ROOT})),
        )
        .await;
    assert!(matches!(
        status,
        StatusCode::NOT_FOUND | StatusCode::FORBIDDEN
    ));
    let library = env.connected("Owned share").await;
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
    for method in [Method::PATCH, Method::DELETE] {
        let body = (method == Method::PATCH).then(|| json!({"poll_seconds": 600}));
        let (status, answer) = env
            .call(
                &viewer,
                method,
                &format!("/api/libraries/{}/source", library.0),
                body,
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{answer}");
    }
}

#[tokio::test]
async fn patch_changes_only_the_poll_interval() {
    let env = Env::new().await;
    let library = env.connected("Changed share").await;
    let route = format!("/api/libraries/{}/source", library.0);
    let (status, answer) = env
        .call(
            &env.admin,
            Method::PATCH,
            &route,
            Some(json!({"poll_seconds": 600})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["poll_seconds"], 600);
    for bad in [
        json!({"branch":"main"}),
        json!({"folder":"docs"}),
        json!({"token":"x"}),
        json!({"clear_token":false}),
    ] {
        let (status, answer) = env.call(&env.admin, Method::PATCH, &route, Some(bad)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
    }
    assert_eq!(
        env.server
            .records_db()
            .source_of(library)
            .await
            .unwrap()
            .unwrap()
            .0
            .location
            .as_deref(),
        Some(ROOT)
    );
}

#[tokio::test]
async fn the_webhook_route_is_refused_for_a_share() {
    let env = Env::new().await;
    let library = env.connected("No webhook").await;
    let (status, answer) = env
        .call(
            &env.admin,
            Method::POST,
            &format!("/api/libraries/{}/source/webhook", library.0),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
    assert_eq!(answer["error"], "invalid_request");
}

#[tokio::test]
async fn disconnect_empties_the_library_and_makes_it_editable() {
    let env = Env::new().await;
    env.fake.put("Old.txt", b"old");
    let library = env.connected("Disconnect share").await;
    server::source_sync::sync_once(&env.state, library, time::OffsetDateTime::UNIX_EPOCH)
        .await
        .unwrap();
    let (status, answer) = env
        .call(
            &env.admin,
            Method::DELETE,
            &format!("/api/libraries/{}/source", library.0),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{answer}");
    assert!(
        env.server
            .records_db()
            .source_of(library)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        env.server
            .records_db()
            .live_children(library)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(env.fake.opened_paths().is_empty());
}

#[tokio::test]
async fn disconnect_during_a_long_first_listing_stops_it_and_succeeds() {
    let env = Env::new().await;
    env.fake.put("Slow.txt", b"slow");
    let library = env.connected("Slow share").await;
    env.fake.slow_list(std::time::Duration::from_secs(1));
    let syncing = tokio::spawn({
        let state = env.state.clone();
        async move {
            server::source_sync::sync_once(&state, library, time::OffsetDateTime::UNIX_EPOCH).await
        }
    });
    let deadline = tokio::time::Instant::now() + common::TIMEOUT;
    while env.fake.lists() < 2 {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::task::yield_now().await;
    }
    let (status, answer) = env
        .call(
            &env.admin,
            Method::DELETE,
            &format!("/api/libraries/{}/source", library.0),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{answer}");
    let _ = syncing.await.unwrap();
    assert!(
        env.server
            .records_db()
            .source_of(library)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn disconnect_and_a_read_at_once_leave_no_tracking_row_and_no_error() {
    let env = Env::new().await;
    env.fake.put("Read.txt", b"read");
    let library = env.connected("Read race").await;
    server::source_sync::sync_once(&env.state, library, time::OffsetDateTime::UNIX_EPOCH)
        .await
        .unwrap();
    let node = env.server.records_db().source_items(library).await.unwrap()[0].node;
    env.fake
        .slow_open("Read.txt", std::time::Duration::from_millis(250));
    let reading = tokio::spawn({
        let state = env.state.clone();
        async move { server::external::materialise(&state, node).await }
    });
    let deadline = tokio::time::Instant::now() + common::TIMEOUT;
    while env.fake.open_started() == 0 {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::task::yield_now().await;
    }
    let (status, answer) = env
        .call(
            &env.admin,
            Method::DELETE,
            &format!("/api/libraries/{}/source", library.0),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{answer}");
    let _ = reading.await.unwrap();
    assert!(
        env.server
            .records_db()
            .external_item(node)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn a_bad_root_is_refused_and_the_setting_is_unchanged() {
    let env = Env::new().await;
    let (status, answer) = env
        .call(
            &env.admin,
            Method::PUT,
            "/api/settings/shares",
            Some(json!({"roots":[ROOT], "list_seconds": 1})),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
    let settings = env.server.records_db().share_settings().await.unwrap();
    assert_eq!(
        settings
            .roots
            .iter()
            .map(records::SharePath::display)
            .collect::<Vec<_>>(),
        [ROOT]
    );
    assert_eq!(settings.list_seconds, 600);
}

#[tokio::test]
async fn removing_a_root_stops_reads_at_once_and_the_next_listing_is_not_connected() {
    let env = Env::new().await;
    env.fake.put("Gone.txt", b"gone");
    let library = env.connected("Withdrawn root").await;
    server::source_sync::sync_once(&env.state, library, time::OffsetDateTime::UNIX_EPOCH)
        .await
        .unwrap();
    let node = env.server.records_db().source_items(library).await.unwrap()[0].node;
    let (status, answer) = env
        .call(
            &env.admin,
            Method::PUT,
            "/api/settings/shares",
            Some(json!({"roots":[]})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert!(matches!(
        server::external::materialise(&env.state, node).await,
        Err(server::external::ExternalError::NotApproved)
    ));
    let _ =
        server::source_sync::sync_once(&env.state, library, time::OffsetDateTime::UNIX_EPOCH).await;
    assert_eq!(
        env.server
            .records_db()
            .source_of(library)
            .await
            .unwrap()
            .unwrap()
            .1
            .state,
        SourceState::NotConnected
    );
}

#[tokio::test]
async fn no_event_holds_a_full_path() {
    let env = Env::new().await;
    let library = env.connected("Private event").await;
    let (status, answer) = env
        .call(
            &env.admin,
            Method::DELETE,
            &format!("/api/libraries/{}/source", library.0),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{answer}");
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let details: Vec<String> = sqlx::query_scalar("select detail::text from events")
        .fetch_all(env.server.records_db().test_pool())
        .await
        .unwrap();
    assert!(
        details.iter().all(|detail| !detail.contains(ROOT)),
        "{details:?}"
    );
}
