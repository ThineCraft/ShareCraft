//! Connecting and managing file-share libraries and their organisation settings.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use collab_core::ids::NodeId;
use common::fake_share::FakeShare;
use common::{TestServer, TestUser, spawn_server_config_without_workers};
use records::{Principal, SHARE_LIST_SECONDS, SHARE_ROOTS, SourceKind, SourceState};
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
async fn canonical_share_locations_are_exclusive_in_both_orders() {
    for alias_first in [false, true] {
        let env = Env::new().await;
        env.fake.mkdir("real/folder");
        env.fake.link("alias", "real");
        let (first, second) = if alias_first {
            (r"C:\approved\alias\folder", r"C:\approved\real\folder")
        } else {
            (r"C:\approved\real\folder", r"C:\approved\alias\folder")
        };
        let (status, answer) = env.connect(json!({"name":"First", "path":first})).await;
        assert_eq!(status, StatusCode::CREATED, "{answer}");
        let (status, answer) = env.connect(json!({"name":"Second", "path":second})).await;
        assert_eq!(status, StatusCode::CONFLICT, "{answer}");
        assert_eq!(answer["error"], "share_location_in_use");
        assert!(!answer.to_string().contains("real"));
    }
}

#[tokio::test]
async fn canonical_nested_alias_conflicts_but_a_sibling_is_allowed() {
    let env = Env::new().await;
    env.fake.mkdir("real/folder/nested");
    env.fake.mkdir("real/sibling");
    env.fake.link("alias", "real");
    let (status, answer) = env
        .connect(json!({"name":"Parent", "path":r"C:\approved\real\folder"}))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{answer}");
    let (status, answer) = env
        .connect(json!({"name":"Nested", "path":r"C:\approved\alias\folder\nested"}))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{answer}");
    assert_eq!(answer["error"], "share_location_in_use");
    let (status, answer) = env
        .connect(json!({"name":"Sibling", "path":r"C:\approved\alias\sibling"}))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{answer}");
}

#[tokio::test]
async fn an_unreachable_existing_location_falls_back_to_lexical_exclusivity() {
    let env = Env::new().await;
    env.fake.mkdir("gone/folder");
    let (status, answer) = env
        .connect(json!({"name":"Existing", "path":r"C:\approved\gone\folder"}))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{answer}");
    env.fake.remove("gone");
    let (status, answer) = env.connect(json!({"name":"Parent", "path":ROOT})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{answer}");
    assert_eq!(answer["error"], "share_location_in_use");
}

#[tokio::test]
async fn roots_removed_after_probe_are_rechecked_before_connect_commit() {
    let env = Env::new().await;
    env.fake.pause_list_after_read();
    let client = env.server.http(Some(&env.admin));
    let url = env
        .server
        .url(&format!("/api/sites/{}/libraries/share", env.site.0));
    let connecting = tokio::spawn({
        async move {
            let response = client
                .post(url)
                .json(&json!({"name":"Racing", "path":ROOT}))
                .send()
                .await
                .unwrap();
            let status = response.status();
            let answer: Value = response.json().await.unwrap();
            (status, answer)
        }
    });
    let deadline = tokio::time::Instant::now() + common::TIMEOUT;
    while env.fake.lists() == 0 {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::task::yield_now().await;
    }
    let (status, answer) = env
        .call(
            &env.admin,
            Method::PUT,
            "/api/settings/shares",
            Some(json!({"roots":[]})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    env.fake.resume_list_after_read();
    let (status, answer) = connecting.await.unwrap();
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
    assert_eq!(answer["error"], "share_not_approved");
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
async fn show_gives_the_path_to_administrators_and_the_connector_after_access_is_reduced() {
    let env = Env::new().await;
    let connector = env.server.user("Original connector").await;
    env.server
        .records_db()
        .set_node_access(
            env.admin.id,
            env.site,
            Principal::User(connector.id),
            Some("owner"),
        )
        .await
        .unwrap();
    let (status, answer) = env
        .call(
            &connector,
            Method::POST,
            &format!("/api/sites/{}/libraries/share", env.site.0),
            Some(json!({"name":"Connector path", "path":ROOT})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{answer}");
    let library = NodeId(answer["id"].as_str().unwrap().parse().unwrap());
    env.server
        .records_db()
        .set_node_access(
            env.admin.id,
            library,
            Principal::User(connector.id),
            Some("viewer"),
        )
        .await
        .unwrap();
    let route = format!("/api/libraries/{}/source", library.0);
    for user in [&connector, &env.admin] {
        let (status, answer) = env.call(user, Method::GET, &route, None).await;
        assert_eq!(status, StatusCode::OK, "{answer}");
        assert_eq!(answer["path"], ROOT);
    }
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
async fn disconnect_ignores_child_permissions_after_library_authorisation() {
    let env = Env::new().await;
    env.fake.put("Private.txt", b"private");
    let owner = env.server.user("Share owner").await;
    env.server
        .records_db()
        .set_node_access(
            env.admin.id,
            env.site,
            Principal::User(owner.id),
            Some("owner"),
        )
        .await
        .unwrap();
    let (status, answer) = env
        .call(
            &owner,
            Method::POST,
            &format!("/api/sites/{}/libraries/share", env.site.0),
            Some(json!({"name":"Private child", "path":ROOT})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{answer}");
    let library = NodeId(answer["id"].as_str().unwrap().parse().unwrap());
    server::source_sync::sync_once(&env.state, library, time::OffsetDateTime::UNIX_EPOCH)
        .await
        .unwrap();
    let child = env
        .server
        .records_db()
        .live_children(library)
        .await
        .unwrap()[0]
        .id;
    env.server
        .records_db()
        .set_node_access(env.admin.id, child, Principal::User(owner.id), None)
        .await
        .unwrap();

    let (status, answer) = env
        .call(
            &owner,
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
}

#[tokio::test]
async fn a_failed_disconnect_batch_leaves_the_source_retryable() {
    let env = Env::new().await;
    env.fake.put("First.txt", b"first");
    env.fake.put("Second.txt", b"second");
    let library = env.connected("Retry disconnect").await;
    server::source_sync::sync_once(&env.state, library, time::OffsetDateTime::UNIX_EPOCH)
        .await
        .unwrap();
    let db = env.server.records_db();
    let children = db.live_children(library).await.unwrap();
    let child_ids: Vec<uuid::Uuid> = children.iter().map(|child| child.id.0).collect();
    assert_eq!(child_ids.len(), 2);
    let pool = db.test_pool();
    sqlx::query(
        "create function fail_share_disconnect() returns trigger language plpgsql as $$ \
         begin if old.name = 'Second.txt' and new.deleted_at is not null then \
         raise exception 'injected disconnect failure'; end if; return new; end $$",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "create trigger fail_share_disconnect before update on nodes \
         for each row execute function fail_share_disconnect()",
    )
    .execute(pool)
    .await
    .unwrap();

    let route = format!("/api/libraries/{}/source", library.0);
    let (status, _) = env.call(&env.admin, Method::DELETE, &route, None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        env.server
            .records_db()
            .source_of(library)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(db.live_children(library).await.unwrap().len(), 2);
    sqlx::query("drop trigger fail_share_disconnect on nodes")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("drop function fail_share_disconnect()")
        .execute(pool)
        .await
        .unwrap();

    let (status, answer) = env.call(&env.admin, Method::DELETE, &route, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{answer}");
    assert!(
        env.server
            .records_db()
            .source_of(library)
            .await
            .unwrap()
            .is_none()
    );
    let deleted_events: i64 = sqlx::query_scalar(
        "select count(*) from events where kind = 'node.deleted' and node_id = any($1)",
    )
    .bind(&child_ids)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(deleted_events, 2);
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

/// A client for requests that may legitimately take as long as the listing deadline.
fn patient_client(env: &Env, user: &TestUser) -> reqwest::Client {
    use reqwest::header::{COOKIE, HeaderMap, HeaderValue, ORIGIN};
    let mut headers = HeaderMap::new();
    headers.insert(ORIGIN, HeaderValue::from_str(&env.server.origin()).unwrap());
    headers.insert(COOKIE, HeaderValue::from_str(&user.cookie).unwrap());
    reqwest::Client::builder()
        .default_headers(headers)
        .timeout(Duration::from_secs(120))
        .build()
        .unwrap()
}

/// Sends `method` to `route` with `body` on a [`patient_client`], in the background.
fn spawn_call(
    env: &Env,
    user: &TestUser,
    method: Method,
    route: &str,
    body: Option<Value>,
) -> tokio::task::JoinHandle<(StatusCode, Value)> {
    let client = patient_client(env, user);
    let url = env.server.url(route);
    tokio::spawn(async move {
        let mut request = client.request(method, url);
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
    })
}

/// Waits, within [`common::TIMEOUT`], until `ready` holds.
async fn wait_until<F: std::future::Future<Output = bool>>(what: &str, ready: impl Fn() -> F) {
    let deadline = tokio::time::Instant::now() + common::TIMEOUT;
    while !ready().await {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Whether a session of this test's database waits for the advisory lock `key`: `gate` if
/// given, the organisation-wide share-connection turn if not.
async fn advisory_waiter(pool: &sqlx::PgPool, gate: Option<i64>) -> bool {
    sqlx::query_scalar::<_, bool>(
        "with k as (select coalesce($1::bigint, \
             hashtextextended('share-connection', 0)) as key) \
         select exists (select 1 from pg_locks, k where locktype = 'advisory' and not granted \
         and database = (select oid from pg_database where datname = current_database()) \
         and classid::bigint = ((k.key >> 32) & 4294967295) \
         and objid::bigint = (k.key & 4294967295))",
    )
    .bind(gate)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn a_hanging_existing_share_never_holds_the_connection_turn() {
    let env = Env::new().await;
    env.server
        .records_db()
        .set_setting(env.admin.id, SHARE_LIST_SECONDS, json!(30))
        .await
        .unwrap();
    env.fake.mkdir("held");
    env.fake.mkdir("second");
    env.fake.mkdir("third");
    let (status, answer) = env
        .connect(json!({"name":"Held", "path":r"C:\approved\held"}))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{answer}");
    env.fake.hang_canonical("held");
    // Released however the test ends, so no blocked resolution outlives it.
    struct Release(FakeShare);
    impl Drop for Release {
        fn drop(&mut self) {
            self.0.release_canonical();
        }
    }
    let _release = Release(env.fake.clone());

    let started = tokio::time::Instant::now();
    let route = format!("/api/sites/{}/libraries/share", env.site.0);
    let second = spawn_call(
        &env,
        &env.admin,
        Method::POST,
        &route,
        Some(json!({"name":"Second", "path":r"C:\approved\second"})),
    );
    wait_until("the second connect resolves the hanging share", || async {
        env.fake.canonical_hanging() >= 1
    })
    .await;
    let third = spawn_call(
        &env,
        &env.admin,
        Method::POST,
        &route,
        Some(json!({"name":"Third", "path":r"C:\approved\third"})),
    );
    wait_until("the third connect resolves the hanging share", || async {
        env.fake.canonical_hanging() >= 2
    })
    .await;

    // A roots change is answered at once (within the ordinary client timeout) while both
    // connects wait on the hanging share.
    let (status, answer) = env
        .call(
            &env.admin,
            Method::PUT,
            "/api/settings/shares",
            Some(json!({"roots":[ROOT]})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert!(!second.is_finished() && !third.is_finished());

    // Neither connect is held beyond the listing deadline: the hanging share falls back to its
    // typed path, which neither sibling overlaps.
    let limit = started + Duration::from_secs(30 + 15);
    for connecting in [second, third] {
        let (status, answer) = tokio::time::timeout_at(limit, connecting)
            .await
            .expect("a connect was held beyond the deadline")
            .unwrap();
        assert_eq!(status, StatusCode::CREATED, "{answer}");
    }
}

#[tokio::test]
async fn a_roots_change_waits_for_a_connect_holding_the_turn() {
    let env = Env::new().await;
    let pool = env.server.records_db().test_pool().clone();
    sqlx::query(
        "create function hold_share_insert() returns trigger language plpgsql as $$ \
         begin perform pg_advisory_xact_lock(4242); return new; end $$",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "create trigger hold_share_insert before insert on library_sources \
         for each row execute function hold_share_insert()",
    )
    .execute(&pool)
    .await
    .unwrap();
    let mut gate = pool.acquire().await.unwrap();
    sqlx::query("select pg_advisory_lock(4242)")
        .execute(&mut *gate)
        .await
        .unwrap();

    let route = format!("/api/sites/{}/libraries/share", env.site.0);
    let connecting = spawn_call(
        &env,
        &env.admin,
        Method::POST,
        &route,
        Some(json!({"name":"Held turn", "path":ROOT})),
    );
    wait_until("the connect holds the turn at its insert", || {
        advisory_waiter(&pool, Some(4242))
    })
    .await;
    let putting = spawn_call(
        &env,
        &env.admin,
        Method::PUT,
        "/api/settings/shares",
        Some(json!({"roots":[]})),
    );
    wait_until("the roots change waits for the turn", || {
        advisory_waiter(&pool, None)
    })
    .await;
    assert!(!putting.is_finished());

    sqlx::query("select pg_advisory_unlock(4242)")
        .execute(&mut *gate)
        .await
        .unwrap();
    let (status, answer) = connecting.await.unwrap();
    assert_eq!(status, StatusCode::CREATED, "{answer}");
    let (status, answer) = putting.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert!(
        env.server
            .records_db()
            .share_settings()
            .await
            .unwrap()
            .roots
            .is_empty()
    );
    drop(gate);
    sqlx::query("drop trigger hold_share_insert on library_sources")
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn disconnect_rechecks_library_access_after_waiting_for_the_source_turn() {
    let env = Env::new().await;
    env.fake.put("Kept.txt", b"kept");
    let owner = env.server.user("Revoked owner").await;
    env.server
        .records_db()
        .set_node_access(
            env.admin.id,
            env.site,
            Principal::User(owner.id),
            Some("owner"),
        )
        .await
        .unwrap();
    let (status, answer) = env
        .call(
            &owner,
            Method::POST,
            &format!("/api/sites/{}/libraries/share", env.site.0),
            Some(json!({"name":"Revoked", "path":ROOT})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{answer}");
    let library = NodeId(answer["id"].as_str().unwrap().parse().unwrap());
    server::source_sync::sync_once(&env.state, library, time::OffsetDateTime::UNIX_EPOCH)
        .await
        .unwrap();
    let db = env.server.records_db();
    assert_eq!(db.live_children(library).await.unwrap().len(), 1);

    let held = db.try_lock_source(library).await.unwrap().unwrap();
    let before = db.source_generation(library).await.unwrap().unwrap();
    let disconnecting = spawn_call(
        &env,
        &owner,
        Method::DELETE,
        &format!("/api/libraries/{}/source", library.0),
        None,
    );
    wait_until("the disconnect waits for the source turn", || async {
        db.source_generation(library).await.unwrap().unwrap() > before
    })
    .await;
    db.set_node_access(
        env.admin.id,
        library,
        Principal::User(owner.id),
        Some("viewer"),
    )
    .await
    .unwrap();
    drop(held);

    let (status, answer) = disconnecting.await.unwrap();
    assert_eq!(status, StatusCode::FORBIDDEN, "{answer}");
    assert!(db.source_of(library).await.unwrap().is_some());
    assert_eq!(db.live_children(library).await.unwrap().len(), 1);
}

#[tokio::test]
async fn disconnect_of_an_empty_library_rechecks_library_access() {
    let env = Env::new().await;
    env.fake.mkdir("empty");
    let owner = env.server.user("Revoked empty owner").await;
    env.server
        .records_db()
        .set_node_access(
            env.admin.id,
            env.site,
            Principal::User(owner.id),
            Some("owner"),
        )
        .await
        .unwrap();
    let (status, answer) = env
        .call(
            &owner,
            Method::POST,
            &format!("/api/sites/{}/libraries/share", env.site.0),
            Some(json!({"name":"Empty revoked", "path":r"C:\approved\empty"})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{answer}");
    let library = NodeId(answer["id"].as_str().unwrap().parse().unwrap());
    let db = env.server.records_db();
    assert!(db.live_children(library).await.unwrap().is_empty());

    let held = db.try_lock_source(library).await.unwrap().unwrap();
    let before = db.source_generation(library).await.unwrap().unwrap();
    let disconnecting = spawn_call(
        &env,
        &owner,
        Method::DELETE,
        &format!("/api/libraries/{}/source", library.0),
        None,
    );
    wait_until("the disconnect waits for the source turn", || async {
        db.source_generation(library).await.unwrap().unwrap() > before
    })
    .await;
    db.set_node_access(
        env.admin.id,
        library,
        Principal::User(owner.id),
        Some("viewer"),
    )
    .await
    .unwrap();
    drop(held);

    let (status, answer) = disconnecting.await.unwrap();
    assert_eq!(status, StatusCode::FORBIDDEN, "{answer}");
    assert!(db.source_of(library).await.unwrap().is_some());
}

#[tokio::test]
async fn lookups_against_a_hanging_share_are_bounded_across_connects() {
    const CONNECTS: usize = server::share::SHARE_LOOKUPS + 4;
    let env = Env::new().await;
    env.fake.mkdir("held");
    let (status, answer) = env
        .connect(json!({"name":"Held", "path":r"C:\approved\held"}))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{answer}");
    for n in 0..CONNECTS {
        env.fake.mkdir(&format!("sibling{n}"));
    }
    env.fake.hang_canonical("held");
    struct Release(FakeShare);
    impl Drop for Release {
        fn drop(&mut self) {
            self.0.release_canonical();
        }
    }
    let release = Release(env.fake.clone());

    let route = format!("/api/sites/{}/libraries/share", env.site.0);
    let connecting: Vec<_> = (0..CONNECTS)
        .map(|n| {
            spawn_call(
                &env,
                &env.admin,
                Method::POST,
                &route,
                Some(json!({
                    "name": format!("Sibling {n}"),
                    "path": format!(r"C:\approved\sibling{n}"),
                })),
            )
        })
        .collect();
    wait_until("the lookup bound is reached", || async {
        env.fake.canonical_hanging() >= server::share::SHARE_LOOKUPS
    })
    .await;
    // Every connect has had ample time to start its own lookup of the hanging share.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        env.fake.canonical_hanging_peak(),
        server::share::SHARE_LOOKUPS
    );

    drop(release);
    for connecting in connecting {
        let (status, answer) = connecting.await.unwrap();
        assert!(
            status == StatusCode::CREATED || status == StatusCode::SERVICE_UNAVAILABLE,
            "{status} {answer}"
        );
    }
    assert_eq!(
        env.fake.canonical_hanging_peak(),
        server::share::SHARE_LOOKUPS
    );
}
