//! The API for connecting a library to a GitHub branch and managing the connection: connect
//! (a new library, probed first), show, change, sync now, the webhook secret and disconnect.
//!
//! A failed probe creates nothing; a secret (the token, the webhook secret) is never in an
//! answer (but the webhook secret's own, once), an event, a log line or the database in plain
//! text.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::time::Duration;

use collab_core::ids::NodeId;
use common::fake_github::FakeGithub;
use common::{TIMEOUT, TestServer, TestUser, spawn_server_config, start_fake_github};
use records::{NewSource, Principal, SourceKind};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use server::AppState;
use server::source_sync::abandon_uploads_into;
use sqlx::Connection;

const OWNER: &str = "octo-org";
const REPO: &str = "docs";
const TOKEN: &str = "ghp_planted_token_1";
/// Set, to a file path, in the child process [`no_secret_in_any_response_event_or_log`]
/// starts: [`leak_scenario`] then runs and writes the webhook secrets it was given there.
const INNER: &str = "SHARECRAFT_SOURCE_API_LEAK_SCENARIO";
/// What [`leak_scenario`] prints when it finished.
const DONE: &str = "leak scenario finished";

/// A server pointed at a fake GitHub serving `octo-org/docs`, an administrator, and the site
/// the harness made.
struct Env {
    server: TestServer,
    fake: FakeGithub,
    admin: TestUser,
    site: NodeId,
    state: AppState,
}

impl Env {
    async fn new() -> Env {
        let fake = start_fake_github(OWNER, REPO).await;
        let origin = fake.origin();
        let server = spawn_server_config(|c| c.github_api = origin).await;
        let admin = server.admin("Ada Admin").await;
        let library = common::Site::library(&server).await;
        let site = server
            .records_db()
            .node(library)
            .await
            .unwrap()
            .unwrap()
            .site;
        let state = server.app_state().await;
        Env {
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
        let (status, text) = self.call_text(user, method, path, body).await;
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    /// The status and the raw text of the answer.
    async fn call_text(
        &self,
        user: &TestUser,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, String) {
        let (status, text, _) = self.send(user, method, path, body).await;
        (status, text)
    }

    /// The status, and the raw text of the answer on its first line followed by its headers,
    /// for scanning.
    async fn call_scan(
        &self,
        user: &TestUser,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, String) {
        let (status, text, headers) = self.send(user, method, path, body).await;
        (status, format!("{text}\n{headers}"))
    }

    async fn send(
        &self,
        user: &TestUser,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, String, String) {
        let mut request = self
            .server
            .http(Some(user))
            .request(method, self.server.url(path));
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        let status = response.status();
        let headers = format!("{:?}", response.headers());
        let text = response.text().await.unwrap();
        (status, text, headers)
    }

    /// `POST /api/sites/{site}/libraries/github` with `body`, as `user`.
    async fn connect(&self, user: &TestUser, body: Value) -> (StatusCode, Value) {
        self.call(
            user,
            Method::POST,
            &format!("/api/sites/{}/libraries/github", self.site.0),
            Some(body),
        )
        .await
    }

    /// Connects a new library named `name` to `main`, as the administrator; its id.
    async fn connected(&self, name: &str, extra: Value) -> NodeId {
        let mut body = json!({ "name": name, "repository": format!("{OWNER}/{REPO}") });
        for (key, value) in extra.as_object().unwrap() {
            body[key] = value.clone();
        }
        let (status, answer) = self.connect(&self.admin, body).await;
        assert_eq!(status, StatusCode::CREATED, "{answer}");
        NodeId(answer["id"].as_str().unwrap().parse().unwrap())
    }

    /// The names of the live items of `parent`, as `user` sees them.
    async fn names(&self, user: &TestUser, parent: NodeId) -> Vec<String> {
        let (status, page) = self
            .call(
                user,
                Method::GET,
                &format!("/api/nodes/{}/children", parent.0),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{page}");
        let mut names: Vec<String> = page["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["name"].as_str().unwrap().to_owned())
            .collect();
        names.sort();
        names
    }

    /// Waits until the library `library` holds exactly `expected` at its top.
    async fn wait_names(&self, library: NodeId, expected: &[&str]) {
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        loop {
            let names = self.names(&self.admin, library).await;
            if names == expected {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the library holds {names:?}, not {expected:?}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    async fn show(&self, user: &TestUser, library: NodeId) -> (StatusCode, Value) {
        self.call(
            user,
            Method::GET,
            &format!("/api/libraries/{}/source", library.0),
            None,
        )
        .await
    }

    async fn sql(&self) -> sqlx::PgConnection {
        sqlx::PgConnection::connect(&self.server.db.url())
            .await
            .unwrap()
    }

    async fn count(&self, query: &str) -> i64 {
        let mut conn = self.sql().await;
        sqlx::query_scalar(sqlx::AssertSqlSafe(query.to_owned()))
            .fetch_one(&mut conn)
            .await
            .unwrap()
    }

    /// Libraries, connections and secrets: what a failed connect must not leave behind.
    async fn made(&self) -> (i64, i64, i64) {
        (
            self.count("select count(*) from nodes where kind = 'library'")
                .await,
            self.count("select count(*) from library_sources").await,
            self.count("select count(*) from secrets").await,
        )
    }

    /// Gives `user` the role `role` on `node`.
    async fn grant(&self, node: NodeId, user: &TestUser, role: &str) {
        self.server
            .records_db()
            .set_node_access(self.admin.id, node, Principal::User(user.id), Some(role))
            .await
            .unwrap();
    }
}

/// `(path, text)` pairs as the fake commits them.
fn files<'a>(list: &[(&'a str, &str)]) -> Vec<(&'a str, Vec<u8>)> {
    list.iter()
        .map(|(path, text)| (*path, text.as_bytes().to_vec()))
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[tokio::test]
async fn connect_creates_a_library_and_the_first_sync_fills_it() {
    let env = Env::new().await;
    let commit = env
        .fake
        .commit("main", &files(&[("a.md", "a"), ("guide/b.md", "b")]));
    let (status, answer) = env
        .connect(
            &env.admin,
            json!({ "name": "Docs mirror", "repository": "octo-org/docs" }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{answer}");
    let library = NodeId(answer["id"].as_str().unwrap().parse().unwrap());
    assert_eq!(answer["source"]["repository"], "octo-org/docs");
    // No branch given: the repository's default.
    assert_eq!(answer["source"]["branch"], "main");
    assert_eq!(answer["source"]["folder"], "");
    assert_eq!(answer["source"]["kind"], "github");
    assert_eq!(answer["source"]["token_set"], false);
    env.wait_names(library, &["a.md", "guide"]).await;

    let deadline = tokio::time::Instant::now() + TIMEOUT;
    let shown = loop {
        let (status, shown) = env.show(&env.admin, library).await;
        assert_eq!(status, StatusCode::OK, "{shown}");
        if shown["state"] == "ok" {
            break shown;
        }
        assert!(tokio::time::Instant::now() < deadline, "{shown}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert_eq!(shown["last_commit"], &commit.sha[..7]);
    assert_eq!(shown["files"], 2);
    assert!(shown["last_synced_at"].is_string());
    assert!(shown["last_error"].is_null());
    assert!(shown.get("etag").is_none());

    // The library is an ordinary one of the site, of the kind asked for (document by default).
    let (status, settings) = env
        .call(
            &env.admin,
            Method::GET,
            &format!("/api/libraries/{}", library.0),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(settings["kind"], "document");
    assert_eq!(settings["name"], "Docs mirror");
    assert_eq!(settings["site"], env.site.0.to_string());
    let connected = env
        .server
        .records_db()
        .events_of_kind("source.connected")
        .await
        .unwrap();
    assert_eq!(connected.len(), 1);
}

#[tokio::test]
async fn the_create_route_makes_its_own_empty_library() {
    let env = Env::new().await;
    env.fake.commit("main", &files(&[("a.md", "a")]));
    let before = env.made().await;
    let library = env
        .connected(
            "Images mirror",
            json!({ "kind": "image", "branch": "main" }),
        )
        .await;
    let after = env.made().await;
    assert_eq!(after.0, before.0 + 1, "one new library");
    assert_eq!(after.1, before.1 + 1, "one new connection");
    let (_, settings) = env
        .call(
            &env.admin,
            Method::GET,
            &format!("/api/libraries/{}", library.0),
            None,
        )
        .await;
    assert_eq!(settings["kind"], "image");
    // A name taken in the site is refused as making a library refuses it, and nothing is made.
    let before = env.made().await;
    let (status, answer) = env
        .connect(
            &env.admin,
            json!({ "name": "Images mirror", "repository": "octo-org/docs" }),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{answer}");
    assert_eq!(answer["error"], "name_taken");
    assert_eq!(env.made().await, before);
}

#[tokio::test]
async fn a_failed_probe_creates_no_library() {
    let env = Env::new().await;
    env.fake
        .commit("main", &files(&[("a.md", "a"), ("b.md", "b")]));
    let before = env.made().await;
    let cases = [
        (
            json!({ "name": "X", "repository": "octo-org/nothing-here" }),
            "repository_not_found",
        ),
        (
            json!({ "name": "X", "repository": "octo-org/docs", "branch": "gone" }),
            "branch_not_found",
        ),
    ];
    for (body, code) in cases {
        let (status, answer) = env.connect(&env.admin, body).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
        assert_eq!(answer["error"], code);
        assert_eq!(env.made().await, before, "{code}");
    }
    let (_, answer) = env
        .connect(
            &env.admin,
            json!({ "name": "X", "repository": "octo-org/nothing-here" }),
        )
        .await;
    assert!(
        answer["message"]
            .as_str()
            .unwrap()
            .contains("not found, or the token cannot see it"),
        "{answer}"
    );

    // Over the byte cap, judged by the size GitHub reports.
    let db = env.server.records_db();
    db.set_setting(env.admin.id, "source_max_bytes", json!(1024))
        .await
        .unwrap();
    env.fake.set_size_kb(5);
    let (status, answer) = env
        .connect(
            &env.admin,
            json!({ "name": "X", "repository": "octo-org/docs" }),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
    assert_eq!(answer["error"], "repository_too_large");
    assert_eq!(env.made().await, before);

    // Over the file cap.
    db.set_setting(env.admin.id, "source_max_bytes", json!(1 << 30))
        .await
        .unwrap();
    db.set_setting(env.admin.id, "source_max_files", json!(1))
        .await
        .unwrap();
    let (status, answer) = env
        .connect(
            &env.admin,
            json!({ "name": "X", "repository": "octo-org/docs" }),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
    assert_eq!(answer["error"], "repository_too_large");
    assert_eq!(env.made().await, before);

    // GitHub failing: nothing made either.
    db.set_setting(env.admin.id, "source_max_files", json!(100))
        .await
        .unwrap();
    env.fake.fail_next(500);
    let (status, answer) = env
        .connect(
            &env.admin,
            json!({ "name": "X", "repository": "octo-org/docs" }),
        )
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{answer}");
    assert_eq!(env.made().await, before);
}

#[tokio::test]
async fn names_github_does_not_use_are_refused_before_asking_it() {
    let env = Env::new().await;
    env.fake.commit("main", &files(&[("a.md", "a")]));
    let before = env.made().await;
    let bad = [
        json!({ "repository": "octo-org/docs/more" }),
        json!({ "repository": "octo-org" }),
        json!({ "repository": "https://github.com/octo-org/docs" }),
        json!({ "repository": "octo-org/.." }),
        json!({ "repository": "octo-org/docs", "branch": "" }),
        json!({ "repository": "octo-org/docs", "branch": "a..b" }),
        json!({ "repository": "octo-org/docs", "branch": "-x" }),
        json!({ "repository": "octo-org/docs", "branch": "x.lock" }),
        json!({ "repository": "octo-org/docs", "branch": "x@{1}" }),
        json!({ "repository": "octo-org/docs", "branch": "a\\b" }),
        json!({ "repository": "octo-org/docs", "branch": "a\nb" }),
        json!({ "repository": "octo-org/docs", "folder": "../up" }),
        json!({ "repository": "octo-org/docs", "folder": "/abs" }),
        json!({ "repository": "octo-org/docs", "folder": "a\\b" }),
        json!({ "repository": "octo-org/docs", "folder": "a//b" }),
        json!({ "repository": "octo-org/docs", "folder": "c:/x" }),
        json!({ "repository": "octo-org/docs", "token": "" }),
        json!({ "repository": "octo-org/docs", "kind": "spreadsheet" }),
    ];
    for mut body in bad {
        body["name"] = json!("X");
        let (status, answer) = env.connect(&env.admin, body.clone()).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}: {answer}");
        assert_eq!(env.made().await, before, "{body}");
    }
    assert_eq!(env.fake.count("repo"), 0, "GitHub was asked");
    // A nested folder of plain names is fine.
    env.fake.commit("main", &files(&[("docs/guide/a.md", "a")]));
    let library = env
        .connected("Guide", json!({ "folder": "docs/guide" }))
        .await;
    env.wait_names(library, &["a.md"]).await;
}

#[tokio::test]
async fn a_private_repository_needs_a_token() {
    let env = Env::new().await;
    env.fake.commit("main", &files(&[("secret.md", "s")]));
    env.fake.require_token(TOKEN);
    let before = env.made().await;
    let (status, answer) = env
        .connect(
            &env.admin,
            json!({ "name": "P", "repository": "octo-org/docs" }),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
    assert_eq!(answer["error"], "repository_not_found");
    let (status, answer) = env
        .connect(
            &env.admin,
            json!({ "name": "P", "repository": "octo-org/docs", "token": "ghp_wrong" }),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
    assert_eq!(answer["error"], "token_rejected");
    assert_eq!(env.made().await, before);

    let library = env.connected("P", json!({ "token": TOKEN })).await;
    env.wait_names(library, &["secret.md"]).await;
    let (_, shown) = env.show(&env.admin, library).await;
    assert_eq!(shown["token_set"], true);
}

#[tokio::test]
async fn show_never_returns_a_secret() {
    let env = Env::new().await;
    env.fake.commit("main", &files(&[("a.md", "a")]));
    let library = env.connected("Docs", json!({ "token": TOKEN })).await;
    env.wait_names(library, &["a.md"]).await;
    let (status, hook) = env
        .call(
            &env.admin,
            Method::POST,
            &format!("/api/libraries/{}/source/webhook", library.0),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let secret = hook["secret"].as_str().unwrap().to_owned();
    let viewer = env.server.user("Vera Viewer").await;
    env.grant(library, &viewer, "viewer").await;
    for user in [&env.admin, &viewer] {
        let (status, text) = env
            .call_text(
                user,
                Method::GET,
                &format!("/api/libraries/{}/source", library.0),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert!(!text.contains(TOKEN));
        assert!(!text.contains(&secret));
        let shown: Value = serde_json::from_str(&text).unwrap();
        let keys: Vec<&str> = shown
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        for key in keys {
            assert!(
                [
                    "kind",
                    "repository",
                    "branch",
                    "folder",
                    "state",
                    "last_commit",
                    "last_synced_at",
                    "last_checked_at",
                    "last_error",
                    "files",
                    "bytes",
                    "hidden",
                    "poll_seconds",
                    "token_set",
                    "webhook",
                ]
                .contains(&key),
                "unexpected field {key}"
            );
        }
        assert_eq!(shown["webhook"]["configured"], true);
        assert!(shown["webhook"].get("secret").is_none());
    }
}

#[tokio::test]
async fn the_token_is_stored_only_as_ciphertext() {
    let env = Env::new().await;
    env.fake.commit("main", &files(&[("a.md", "a")]));
    env.fake.require_token(TOKEN);
    let library = env.connected("Docs", json!({ "token": TOKEN })).await;
    env.wait_names(library, &["a.md"]).await;
    let mut conn = env.sql().await;
    let rows: Vec<String> = sqlx::query_scalar(
        "select row_to_json(s)::text || encode(s.ciphertext, 'escape') from secrets s",
    )
    .fetch_all(&mut conn)
    .await
    .unwrap();
    assert_eq!(rows.len(), 1);
    for row in rows {
        assert!(!row.contains(TOKEN));
        assert!(!row.contains(&hex(TOKEN.as_bytes())));
    }
    let sources: Vec<String> =
        sqlx::query_scalar("select row_to_json(s)::text from library_sources s")
            .fetch_all(&mut conn)
            .await
            .unwrap();
    for row in sources {
        assert!(!row.contains(TOKEN));
    }
}

#[tokio::test]
async fn only_site_creators_may_connect_and_only_owners_may_change() {
    let env = Env::new().await;
    env.fake.commit("main", &files(&[("a.md", "a")]));
    let library = env.connected("Docs", json!({})).await;
    env.wait_names(library, &["a.md"]).await;
    let viewer = env.server.user("Vic Viewer").await;
    let editor = env.server.user("Eddie Editor").await;
    let stranger = env.server.user("Stan Stranger").await;
    env.grant(env.site, &viewer, "viewer").await;
    env.grant(env.site, &editor, "editor").await;

    let before = env.made().await;
    let body = json!({ "name": "Theirs", "repository": "octo-org/docs" });
    for (user, expected) in [
        (&viewer, StatusCode::FORBIDDEN),
        (&editor, StatusCode::FORBIDDEN),
        (&stranger, StatusCode::NOT_FOUND),
    ] {
        let (status, answer) = env.connect(user, body.clone()).await;
        assert_eq!(status, expected, "{user:?}: {answer}");
    }
    assert_eq!(env.made().await, before);
    assert_eq!(env.fake.count("repo"), 1, "only the administrator's probe");

    // Viewers and editors see the connection; strangers do not.
    for user in [&viewer, &editor] {
        assert_eq!(env.show(user, library).await.0, StatusCode::OK);
    }
    assert_eq!(env.show(&stranger, library).await.0, StatusCode::NOT_FOUND);

    let base = format!("/api/libraries/{}/source", library.0);
    let changes = [
        (
            Method::PATCH,
            base.clone(),
            Some(json!({ "poll_seconds": 600 })),
        ),
        (Method::POST, format!("{base}/sync"), None),
        (Method::POST, format!("{base}/webhook"), None),
        (Method::DELETE, base.clone(), None),
    ];
    for (method, path, body) in &changes {
        for (user, expected) in [
            (&viewer, StatusCode::FORBIDDEN),
            (&editor, StatusCode::FORBIDDEN),
            (&stranger, StatusCode::NOT_FOUND),
        ] {
            let (status, answer) = env.call(user, method.clone(), path, body.clone()).await;
            assert_eq!(status, expected, "{method} {path} as {user:?}: {answer}");
        }
    }
    // The owner of the site (an owner of the library) may.
    let olive = env.server.user("Olive Owner").await;
    env.grant(env.site, &olive, "owner").await;
    let (status, answer) = env
        .call(
            &olive,
            Method::PATCH,
            &base,
            Some(json!({ "poll_seconds": 600 })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["poll_seconds"], 600);
    // A library that is not connected has no source.
    let plain = common::Site::library(&env.server).await;
    let (status, _) = env.show(&env.admin, plain).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn patch_branch_reprobes_and_resyncs() {
    let env = Env::new().await;
    env.fake.commit("main", &files(&[("main.md", "m")]));
    let next = env.fake.commit("next", &files(&[("next.md", "n")]));
    let library = env.connected("Docs", json!({ "branch": "main" })).await;
    env.wait_names(library, &["main.md"]).await;
    let path = format!("/api/libraries/{}/source", library.0);

    let (status, answer) = env
        .call(
            &env.admin,
            Method::PATCH,
            &path,
            Some(json!({ "branch": "missing" })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
    assert_eq!(answer["error"], "branch_not_found");
    assert_eq!(env.show(&env.admin, library).await.1["branch"], "main");

    let (status, answer) = env
        .call(
            &env.admin,
            Method::PATCH,
            &path,
            Some(json!({ "branch": "next" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["branch"], "next");
    env.wait_names(library, &["next.md"]).await;
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        let (_, shown) = env.show(&env.admin, library).await;
        if shown["last_commit"] == next.sha[..7] && shown["state"] == "ok" {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "{shown}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // Both a token and clearing it is not a request that makes sense.
    let (status, _) = env
        .call(
            &env.admin,
            Method::PATCH,
            &path,
            Some(json!({ "token": TOKEN, "clear_token": true })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _) = env
        .call(
            &env.admin,
            Method::PATCH,
            &path,
            Some(json!({ "poll_seconds": 5 })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // "Sync now" brings in a new commit.
    env.fake
        .commit("next", &files(&[("next.md", "n"), ("more.md", "m")]));
    let (status, _) = env
        .call(&env.admin, Method::POST, &format!("{path}/sync"), None)
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    env.wait_names(library, &["more.md", "next.md"]).await;
}

#[tokio::test]
async fn clear_token_removes_the_secret_row() {
    let env = Env::new().await;
    env.fake.commit("main", &files(&[("a.md", "a")]));
    // A public repository answers with or without a token.
    let library = env.connected("Docs", json!({ "token": TOKEN })).await;
    env.wait_names(library, &["a.md"]).await;
    let path = format!("/api/libraries/{}/source", library.0);
    assert_eq!(env.count("select count(*) from secrets").await, 1);
    let first: uuid::Uuid = {
        let mut conn = env.sql().await;
        sqlx::query_scalar("select id from secrets")
            .fetch_one(&mut conn)
            .await
            .unwrap()
    };

    // Replacing the token replaces its row.
    let (status, answer) = env
        .call(
            &env.admin,
            Method::PATCH,
            &path,
            Some(json!({ "token": "ghp_another_token" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["token_set"], true);
    let ids: Vec<uuid::Uuid> = {
        let mut conn = env.sql().await;
        sqlx::query_scalar("select id from secrets")
            .fetch_all(&mut conn)
            .await
            .unwrap()
    };
    assert_eq!(ids.len(), 1);
    assert_ne!(ids[0], first);

    let (status, answer) = env
        .call(
            &env.admin,
            Method::PATCH,
            &path,
            Some(json!({ "clear_token": true })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["token_set"], false);
    assert_eq!(env.count("select count(*) from secrets").await, 0);
}

#[tokio::test]
async fn webhook_secret_is_shown_once() {
    let env = Env::new().await;
    env.fake.commit("main", &files(&[("a.md", "a")]));
    let library = env.connected("Docs", json!({})).await;
    let (_, shown) = env.show(&env.admin, library).await;
    assert_eq!(shown["webhook"]["configured"], false);
    let expected_url = env.server.url(&format!("/hooks/github/{}", library.0));
    assert_eq!(shown["webhook"]["url"], expected_url);

    let path = format!("/api/libraries/{}/source/webhook", library.0);
    let (status, first) = env.call(&env.admin, Method::POST, &path, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first["url"], expected_url);
    assert_eq!(first["replaced"], false);
    let secret = first["secret"].as_str().unwrap().to_owned();
    assert_eq!(secret.len(), 64);
    assert!(secret.bytes().all(|b| b.is_ascii_hexdigit()));

    let (status, text) = env
        .call_text(
            &env.admin,
            Method::GET,
            &format!("/api/libraries/{}/source", library.0),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!text.contains(&secret));
    let shown: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(shown["webhook"]["configured"], true);

    let (status, second) = env.call(&env.admin, Method::POST, &path, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(second["replaced"], true);
    assert_ne!(second["secret"], first["secret"]);
    // The old secret's row is gone.
    assert_eq!(env.count("select count(*) from secrets").await, 1);
}

#[tokio::test]
async fn disconnect_keeps_files_and_makes_the_library_editable() {
    let env = Env::new().await;
    env.fake.commit("main", &files(&[("a.md", "a")]));
    let library = env.connected("Docs", json!({ "token": TOKEN })).await;
    env.wait_names(library, &["a.md"]).await;
    let path = format!("/api/libraries/{}/source", library.0);
    env.call(&env.admin, Method::POST, &format!("{path}/webhook"), None)
        .await;
    assert_eq!(env.count("select count(*) from secrets").await, 2);

    // Read-only while connected.
    let upload = || {
        env.server
            .http(Some(&env.admin))
            .post(env.server.url("/api/docs/upload"))
            .query(&[
                ("parent", library.0.to_string()),
                ("name", "mine.txt".to_owned()),
            ])
            .body(b"hello".to_vec())
            .send()
    };
    assert_eq!(upload().await.unwrap().status(), StatusCode::CONFLICT);

    let (status, _) = env.call(&env.admin, Method::DELETE, &path, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(env.show(&env.admin, library).await.0, StatusCode::NOT_FOUND);
    assert_eq!(env.count("select count(*) from secrets").await, 0);
    assert_eq!(env.count("select count(*) from library_sources").await, 0);
    assert_eq!(env.names(&env.admin, library).await, ["a.md"]);
    assert_eq!(upload().await.unwrap().status(), StatusCode::CREATED);
    let events = env
        .server
        .records_db()
        .events_of_kind("source.disconnected")
        .await
        .unwrap();
    assert_eq!(events.len(), 1);
    // Disconnecting again: there is nothing to disconnect.
    let (status, _) = env.call(&env.admin, Method::DELETE, &path, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn connecting_abandons_uploads_into_the_library() {
    let env = Env::new().await;
    let (status, made) = env
        .call(
            &env.admin,
            Method::POST,
            &format!("/api/sites/{}/libraries", env.site.0),
            Some(json!({ "name": "Racing" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    let library = NodeId(made["id"].as_str().unwrap().parse().unwrap());
    let other = common::Site::library(&env.server).await;
    let start = |parent: NodeId| {
        env.call(
            &env.admin,
            Method::POST,
            "/api/uploads",
            Some(json!({
                "parent": parent.0.to_string(),
                "name": "late.txt",
                "size": 2,
                "sha256": revisions::BlobRef::of(b"ab").hex(),
            })),
        )
    };
    let (status, into) = start(library).await;
    assert_eq!(status, StatusCode::CREATED, "{into}");
    let (status, elsewhere) = start(other).await;
    assert_eq!(status, StatusCode::CREATED, "{elsewhere}");

    env.server
        .records_db()
        .connect_source(&NewSource {
            library,
            kind: SourceKind::Github,
            owner: OWNER.to_owned(),
            repo: REPO.to_owned(),
            branch: "main".to_owned(),
            folder: String::new(),
            token: None,
            poll_seconds: 300,
            connected_by: env.admin.id,
        })
        .await
        .unwrap();
    assert_eq!(abandon_uploads_into(&env.state, library).await, 1);
    let (status, _) = env
        .call(
            &env.admin,
            Method::GET,
            &format!("/api/uploads/{}", into["id"].as_str().unwrap()),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = env
        .call(
            &env.admin,
            Method::GET,
            &format!("/api/uploads/{}", elsewhere["id"].as_str().unwrap()),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
}

/// Runs [`leak_scenario`] in a child process of this test binary, so the server's log lines
/// (standard error) are captured, and scans everything the child wrote for the planted token
/// and the webhook secrets the scenario generated.
#[test]
fn no_secret_in_any_response_event_or_log() {
    let dir = tempfile::tempdir().unwrap();
    let secrets_file = dir.path().join("webhook-secrets");
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "leak_scenario",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(INNER, &secrets_file)
        .output()
        .unwrap();
    let mut written = String::from_utf8_lossy(&output.stdout).into_owned();
    written.push_str(&String::from_utf8_lossy(&output.stderr));
    let generated = std::fs::read_to_string(&secrets_file).unwrap_or_default();
    let generated: Vec<&str> = generated.lines().filter(|l| !l.is_empty()).collect();
    assert!(
        !written.contains(TOKEN),
        "the token is in the output of the scenario"
    );
    for secret in &generated {
        assert!(
            !written.contains(secret),
            "a webhook secret is in the output of the scenario"
        );
    }
    assert!(
        output.status.success() && written.contains(DONE),
        "the scenario failed:\n{written}"
    );
    assert_eq!(generated.len(), 2, "two webhook secrets were generated");
}

/// Every route, with a planted token and two generated webhook secrets: no answer (but each
/// webhook secret's own), event, `library_sources` row or `secrets` row holds either in plain
/// text. Run only by [`no_secret_in_any_response_event_or_log`].
#[tokio::test]
#[ignore = "run in a child process by no_secret_in_any_response_event_or_log"]
async fn leak_scenario() {
    let Some(secrets_file) = std::env::var_os(INNER) else {
        return;
    };
    let env = Env::new().await;
    env.fake.commit("main", &files(&[("a.md", "a")]));
    env.fake.commit("other", &files(&[("b.md", "b")]));
    env.fake.require_token(TOKEN);
    let mut answers: Vec<String> = Vec::new();

    // Refusals that involve the token.
    let site_path = format!("/api/sites/{}/libraries/github", env.site.0);
    let (_, text) = env
        .call_scan(
            &env.admin,
            Method::POST,
            &site_path,
            Some(
                json!({ "name": "D", "repository": "octo-org/docs", "token": format!("{TOKEN}x") }),
            ),
        )
        .await;
    answers.push(text);
    env.fake.fail_next(500);
    let (_, text) = env
        .call_scan(
            &env.admin,
            Method::POST,
            &site_path,
            Some(json!({ "name": "D", "repository": "octo-org/docs", "token": TOKEN })),
        )
        .await;
    answers.push(text);

    let (status, text) = env
        .call_scan(
            &env.admin,
            Method::POST,
            &site_path,
            Some(json!({ "name": "D", "repository": "octo-org/docs", "token": TOKEN })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let id: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    let library = NodeId(id["id"].as_str().unwrap().parse().unwrap());
    answers.push(text);
    env.wait_names(library, &["a.md"]).await;
    let path = format!("/api/libraries/{}/source", library.0);

    let mut hooks = Vec::new();
    let mut own_answers = Vec::new();
    for _ in 0..2 {
        let (status, text) = env
            .call_scan(&env.admin, Method::POST, &format!("{path}/webhook"), None)
            .await;
        assert_eq!(status, StatusCode::OK);
        let hook: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        hooks.push(hook["secret"].as_str().unwrap().to_owned());
        own_answers.push(text);
    }
    std::fs::write(&secrets_file, hooks.join("\n")).unwrap();

    for (method, route, body) in [
        (Method::GET, path.clone(), None),
        (
            Method::PATCH,
            path.clone(),
            Some(json!({ "poll_seconds": 900 })),
        ),
        (Method::PATCH, path.clone(), Some(json!({ "token": TOKEN }))),
        (
            Method::PATCH,
            path.clone(),
            Some(json!({ "token": format!("{TOKEN}x") })),
        ),
        (
            Method::PATCH,
            path.clone(),
            Some(json!({ "branch": "other" })),
        ),
        (Method::POST, format!("{path}/sync"), None),
        (Method::GET, path.clone(), None),
        (
            Method::GET,
            format!("/api/nodes/{}/children", library.0),
            None,
        ),
        (Method::GET, format!("/api/libraries/{}", library.0), None),
    ] {
        let (_, text) = env.call_scan(&env.admin, method, &route, body).await;
        answers.push(text);
    }
    env.wait_names(library, &["b.md"]).await;

    // A sync that fails with an answer echoing the token.
    env.fake.commit("other", &files(&[("b.md", "b2")]));
    env.fake.fail_next(500);
    let (_, text) = env
        .call_scan(&env.admin, Method::POST, &format!("{path}/sync"), None)
        .await;
    answers.push(text);
    // A sync woken earlier may take the failure, and a later one put the state back to `ok`, so
    // the event is waited for, not the state.
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    let db = env.server.records_db();
    loop {
        let (_, text) = env.call_scan(&env.admin, Method::GET, &path, None).await;
        answers.push(text);
        if !db.events_of_kind("source.failed").await.unwrap().is_empty() {
            let (_, text) = env.call_scan(&env.admin, Method::GET, &path, None).await;
            answers.push(text);
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the sync never failed"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // What the database holds, before the connection goes.
    let mut conn = env.sql().await;
    let mut stored: Vec<String> = sqlx::query_scalar(
        "select row_to_json(s)::text || encode(s.ciphertext, 'escape') from secrets s",
    )
    .fetch_all(&mut conn)
    .await
    .unwrap();
    stored.extend(
        sqlx::query_scalar::<_, String>("select row_to_json(s)::text from library_sources s")
            .fetch_all(&mut conn)
            .await
            .unwrap(),
    );

    let (status, text) = env.call_scan(&env.admin, Method::DELETE, &path, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    answers.push(text);
    let (_, text) = env.call_scan(&env.admin, Method::GET, &path, None).await;
    answers.push(text);
    stored.extend(
        sqlx::query_scalar::<_, String>("select detail::text from events")
            .fetch_all(&mut conn)
            .await
            .unwrap(),
    );
    let kinds: Vec<String> = sqlx::query_scalar("select kind from events")
        .fetch_all(&mut conn)
        .await
        .unwrap();
    for kind in [
        "source.connected",
        "source.changed",
        "source.failed",
        "source.disconnected",
    ] {
        assert!(kinds.iter().any(|k| k == kind), "no {kind} event");
    }

    let mut needles: Vec<String> = vec![TOKEN.to_owned(), hex(TOKEN.as_bytes())];
    for hook in &hooks {
        needles.push(hook.clone());
        needles.push(hex(hook.as_bytes()));
    }
    for (i, answer) in answers.iter().enumerate() {
        for needle in &needles {
            assert!(
                !answer.contains(needle.as_str()),
                "answer {i} leaks a secret"
            );
        }
    }
    for (i, row) in stored.iter().enumerate() {
        for needle in &needles {
            assert!(
                !row.contains(needle.as_str()),
                "stored row {i} leaks a secret"
            );
        }
    }
    // Each webhook answer holds its own secret and no other.
    for (i, answer) in own_answers.iter().enumerate() {
        assert!(!answer.contains(TOKEN));
        for (j, hook) in hooks.iter().enumerate() {
            assert_eq!(answer.contains(hook.as_str()), i == j);
        }
    }
    println!("{DONE}");
}
