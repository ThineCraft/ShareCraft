//! The sync worker and the GitHub webhook: connected libraries are synced when due (their poll
//! time passed, no retry time ahead), a rate limit is waited out, a connection that no longer
//! works stops until an owner acts, other failures back off, wakes are coalesced, at most two
//! syncs run at once, and a signed push to the connected branch syncs at once.
//!
//! Tests drive [`server::source_tick_once`] with a chosen `now`; the server's own worker runs
//! meanwhile with the real time, so a library a test drives is never due for it.

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
use common::{
    TIMEOUT, TestServer, TestUser, spawn_server_config, spawn_server_config_without_workers,
    start_fake_github,
};
use records::{SealedRow, SourceChange, SourceState, SourceStatus};
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use server::AppState;
use server::deployment::Deployment;
use server::source_sync::{sync_once, webhook_context};
use sha2::{Digest, Sha256};
use sqlx::Connection;
use time::OffsetDateTime;

const OWNER: &str = "octo-org";
const REPO: &str = "docs";

/// A server pointed at a fake GitHub, with a library connected (through the API, as the
/// administrator) to `main` and synced once by the server's worker.
struct Env {
    server: TestServer,
    fake: FakeGithub,
    admin: TestUser,
    site: NodeId,
    library: NodeId,
    state: AppState,
}

impl Env {
    async fn new() -> Env {
        Env::with_token(None).await
    }

    /// A connected and initially synced library served without an automatic source worker.
    async fn without_worker() -> Env {
        let fake = start_fake_github(OWNER, REPO).await;
        fake.commit("main", &files(&[("a.md", "first"), ("guide/b.md", "b")]));
        let origin = fake.origin();
        let server = spawn_server_config_without_workers(|c| c.github_api = origin).await;
        let admin = server.admin("Ada Admin").await;
        let harness = common::Site::library(&server).await;
        let site = server
            .records_db()
            .node(harness)
            .await
            .unwrap()
            .unwrap()
            .site;
        let state = server.app_state().await;
        let (status, answer) = call(
            &server,
            &admin,
            Method::POST,
            &format!("/api/sites/{}/libraries/github", site.0),
            Some(json!({ "name": "Mirror", "repository": format!("{OWNER}/{REPO}") })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{answer}");
        let library = NodeId(answer["id"].as_str().unwrap().parse().unwrap());
        sync_once(&state, library, OffsetDateTime::now_utc())
            .await
            .unwrap();
        Env {
            server,
            fake,
            admin,
            site,
            library,
            state,
        }
    }

    /// Like [`Env::new`], the repository readable only with `token` when given.
    async fn with_token(token: Option<&str>) -> Env {
        let fake = start_fake_github(OWNER, REPO).await;
        if let Some(token) = token {
            fake.require_token(token);
        }
        fake.commit("main", &files(&[("a.md", "first"), ("guide/b.md", "b")]));
        let origin = fake.origin();
        let server = spawn_server_config(|c| c.github_api = origin).await;
        let admin = server.admin("Ada Admin").await;
        let harness = common::Site::library(&server).await;
        let site = server
            .records_db()
            .node(harness)
            .await
            .unwrap()
            .unwrap()
            .site;
        let state = server.app_state().await;
        let mut body = json!({ "name": "Mirror", "repository": format!("{OWNER}/{REPO}") });
        if let Some(token) = token {
            body["token"] = json!(token);
        }
        let (status, answer) = call(
            &server,
            &admin,
            Method::POST,
            &format!("/api/sites/{}/libraries/github", site.0),
            Some(body),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{answer}");
        let library = NodeId(answer["id"].as_str().unwrap().parse().unwrap());
        let env = Env {
            server,
            fake,
            admin,
            site,
            library,
            state,
        };
        let first = env.settled().await;
        assert_eq!(first.state, SourceState::Ok, "{first:?}");
        env
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

    /// Adds another connected library in the same site and waits for its first sync.
    async fn add_library(&self, name: &str) -> NodeId {
        let (status, answer) = self
            .call(
                Method::POST,
                &format!("/api/sites/{}/libraries/github", self.site.0),
                Some(json!({ "name": name, "repository": format!("{OWNER}/{REPO}") })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{answer}");
        let library = NodeId(answer["id"].as_str().unwrap().parse().unwrap());
        let _ = sync_once(&self.state, library, OffsetDateTime::now_utc())
            .await
            .unwrap();
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        loop {
            let status = self
                .server
                .records_db()
                .source_of(library)
                .await
                .unwrap()
                .unwrap()
                .1;
            if status.state == SourceState::Ok && status.last_checked_at.is_some() {
                return library;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the added library never settled: {status:?}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Waits until no sync of the library is due at once or running, and answers its status.
    async fn settled(&self) -> SourceStatus {
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        loop {
            let status = self.status().await;
            let done = !matches!(status.state, SourceState::Connecting | SourceState::Syncing)
                && status.last_checked_at.is_some();
            if done {
                return status;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the library never settled: {status:?}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Waits until the library mirrors `commit`.
    async fn wait_commit(&self, commit: &str) {
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        loop {
            let status = self.status().await;
            if status.last_commit.as_deref() == Some(commit) && status.state == SourceState::Ok {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the library never reached {commit}: {status:?}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Waits until every `library` mirrors `commit`.
    async fn wait_libraries_commit(&self, libraries: &[NodeId], commit: &str) {
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        loop {
            let mut pending = Vec::new();
            for &library in libraries {
                let status = self
                    .server
                    .records_db()
                    .source_of(library)
                    .await
                    .unwrap()
                    .unwrap()
                    .1;
                if status.last_commit.as_deref() != Some(commit) || status.state != SourceState::Ok
                {
                    pending.push((library, status));
                }
            }
            if pending.is_empty() {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the libraries never reached {commit}: {pending:?}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// When the library was last checked.
    async fn checked(&self) -> OffsetDateTime {
        self.status().await.last_checked_at.unwrap()
    }

    async fn tick(&self, now: OffsetDateTime) -> usize {
        server::source_tick_once(&self.state, now).await
    }

    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        call(&self.server, &self.admin, method, path, body).await
    }

    /// A new webhook secret, made through the API; waits for the sync it starts.
    async fn webhook_secret(&self) -> String {
        let secret = self.webhook_secret_for(self.library).await;
        self.settled().await;
        secret
    }

    /// A new webhook secret for `library`, made through the API.
    async fn webhook_secret_for(&self, library: NodeId) -> String {
        let (status, answer) = self
            .call(
                Method::POST,
                &format!("/api/libraries/{}/source/webhook", library.0),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{answer}");
        answer["secret"].as_str().unwrap().to_owned()
    }

    /// The node at `path` in the library.
    async fn node_at(&self, path: &str) -> NodeId {
        let mut at = self.library;
        for name in path.split('/') {
            let (status, page) = self
                .call(Method::GET, &format!("/api/nodes/{}/children", at.0), None)
                .await;
            assert_eq!(status, StatusCode::OK, "{page}");
            let found = page["items"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["name"] == name)
                .unwrap_or_else(|| panic!("{path} is not in the library"));
            at = NodeId(found["id"].as_str().unwrap().parse().unwrap());
        }
        at
    }

    /// The bytes of the file at `path`, downloaded by the administrator.
    async fn download(&self, path: &str) -> (StatusCode, Vec<u8>) {
        let id = self.node_at(path).await;
        let response = self
            .server
            .http(Some(&self.admin))
            .get(self.server.url(&format!("/api/docs/{}/content", id.0)))
            .send()
            .await
            .unwrap();
        let status = response.status();
        (status, response.bytes().await.unwrap().to_vec())
    }

    /// `POST /hooks/github/{library}` from GitHub (no `Origin`, no cookie).
    async fn hook(&self, event: &str, body: &[u8], signature: Option<&str>) -> StatusCode {
        self.hook_for(self.library, event, body, signature).await
    }

    async fn hook_for(
        &self,
        library: NodeId,
        event: &str,
        body: &[u8],
        signature: Option<&str>,
    ) -> StatusCode {
        hook(
            &reqwest::Client::new(),
            &self.server.url(&format!("/hooks/github/{}", library.0)),
            event,
            body,
            signature,
        )
        .await
    }

    async fn source_synced_events(&self) -> Vec<Value> {
        self.server
            .records_db()
            .events_of_kind("source.synced")
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.detail)
            .collect()
    }
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
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

/// Sends a delivery as GitHub does: `X-GitHub-Event`, `X-Hub-Signature-256` when given, a JSON
/// body.
async fn hook(
    client: &reqwest::Client,
    url: &str,
    event: &str,
    body: &[u8],
    signature: Option<&str>,
) -> StatusCode {
    let mut request = client
        .post(url)
        .header("x-github-event", event)
        .header("x-github-delivery", "72d3162e-cc78-11e3-81ab-4c9367dc0958")
        .header("content-type", "application/json")
        .body(body.to_vec());
    if let Some(signature) = signature {
        request = request.header("x-hub-signature-256", signature);
    }
    match request.send().await {
        Ok(response) => response.status(),
        Err(_) => panic!("a webhook delivery did not reach the server"),
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

/// HMAC-SHA256 (RFC 2104), written out here so the server's own is checked against it.
fn hmac_sha256(key: &[u8], message: &[u8]) -> Vec<u8> {
    let mut block = [0u8; 64];
    if key.len() > 64 {
        let digest = Sha256::digest(key);
        block[..32].copy_from_slice(&digest[..]);
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let ipad: Vec<u8> = block.iter().map(|b| b ^ 0x36).collect();
    let opad: Vec<u8> = block.iter().map(|b| b ^ 0x5c).collect();
    let mut inner = Sha256::new();
    inner.update(&ipad);
    inner.update(message);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(&opad);
    outer.update(&inner[..]);
    outer.finalize()[..].to_vec()
}

/// The `X-Hub-Signature-256` GitHub sends for `body` with `secret`.
fn signature(secret: &str, body: &[u8]) -> String {
    format!("sha256={}", hex(&hmac_sha256(secret.as_bytes(), body)))
}

/// A push delivery's body for `reference`.
fn push(reference: &str) -> Vec<u8> {
    json!({
        "ref": reference,
        "before": "0000000000000000000000000000000000000000",
        "after": "1111111111111111111111111111111111111111",
        "repository": { "full_name": format!("{OWNER}/{REPO}"), "name": "not trusted" },
        "pusher": { "name": "octo" },
    })
    .to_string()
    .into_bytes()
}

fn seconds(n: i64) -> time::Duration {
    time::Duration::seconds(n)
}

#[tokio::test]
async fn tick_syncs_a_due_library_and_not_one_checked_a_minute_ago() {
    let env = Env::new().await;
    let checked = env.checked().await;
    let before = env.fake.count("commits");
    assert_eq!(env.tick(checked + seconds(60)).await, 0);
    assert_eq!(env.fake.count("commits"), before, "no request for it");
    let c2 = env
        .fake
        .commit("main", &files(&[("a.md", "second"), ("guide/b.md", "b")]));
    assert_eq!(env.tick(checked + seconds(300)).await, 1);
    assert_eq!(env.fake.count("commits"), before + 1);
    let status = env.status().await;
    assert_eq!(status.state, SourceState::Ok);
    assert_eq!(status.last_commit.as_deref(), Some(c2.sha.as_str()));
    assert_eq!(status.last_checked_at, Some(checked + seconds(300)));
    assert_eq!(env.download("a.md").await.1, b"second");
}

#[tokio::test]
async fn poll_interval_is_respected() {
    let env = Env::new().await;
    let (status, answer) = env
        .call(
            Method::PATCH,
            &format!("/api/libraries/{}/source", env.library.0),
            Some(json!({ "poll_seconds": 600 })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    env.settled().await;
    let checked = env.checked().await;
    let before = env.fake.count("commits");
    let unchanged = env.fake.count("commits_not_modified");
    assert_eq!(env.tick(checked + seconds(300)).await, 0);
    assert_eq!(env.tick(checked + seconds(599)).await, 0);
    assert_eq!(env.fake.count("commits"), before);
    assert_eq!(env.tick(checked + seconds(600)).await, 1);
    assert_eq!(env.fake.count("commits"), before + 1);
    // Unchanged branch: a conditional request, nothing else.
    assert_eq!(env.fake.count("commits_not_modified"), unchanged + 1);
    assert_eq!(env.status().await.state, SourceState::Ok);
    // Checked again just now: not due.
    assert_eq!(env.tick(checked + seconds(1000)).await, 0);
}

#[tokio::test]
async fn a_rate_limit_sets_a_retry_time_and_no_request_is_made_before_it() {
    let env = Env::new().await;
    let checked = env.checked().await;
    let first = checked + seconds(300);
    let reset = OffsetDateTime::now_utc() + seconds(3600);
    env.fake.rate_limit_next(reset);
    assert_eq!(env.tick(first).await, 1);
    let status = env.status().await;
    assert_eq!(status.state, SourceState::RateLimited);
    assert_eq!(
        status.retry_at.map(OffsetDateTime::unix_timestamp),
        Some(reset.unix_timestamp())
    );
    assert!(status.last_error.unwrap().contains("rate limit"));
    let asked = env.fake.count("commits");
    // Past the poll time, but before the reset: nothing is asked.
    for at in [
        first + seconds(300),
        first + seconds(1800),
        reset - seconds(1),
    ] {
        assert_eq!(env.tick(at).await, 0, "{at}");
    }
    assert_eq!(
        env.fake.count("commits"),
        asked,
        "no request before the reset"
    );
    // A webhook delivery does not hurry it either.
    env.state.sources().wake(env.library);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(env.fake.count("commits"), asked);
    // At the reset it is asked again, and is in step.
    assert_eq!(env.tick(reset + seconds(1)).await, 1);
    assert_eq!(env.fake.count("commits"), asked + 1);
    let status = env.status().await;
    assert_eq!(status.state, SourceState::Ok);
    assert_eq!(status.retry_at, None);

    stale_rate_limit_is_still_waited_out().await;
}

async fn stale_rate_limit_is_still_waited_out() {
    let env = Env::new().await;
    env.fake.commit(
        "main",
        &files(&[("a.md", "first"), ("guide/b.md", "b"), ("c.md", "new")]),
    );
    let held = env.fake.hold_blobs().await;
    let running = {
        let state = env.state.clone();
        let library = env.library;
        tokio::spawn(async move { sync_once(&state, library, OffsetDateTime::now_utc()).await })
    };
    env.fake.wait_blob_held().await;

    let reset = OffsetDateTime::now_utc() + seconds(3600);
    env.fake.rate_limit_next(reset);
    let (status, answer) = env
        .call(
            Method::POST,
            &format!("/api/libraries/{}/source/sync", env.library.0),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{answer}");
    let asked = env.fake.count("commits");

    drop(held);
    assert!(running.await.unwrap().is_err());
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let status = env.status().await;
    assert_eq!(status.state, SourceState::RateLimited, "{status:?}");
    assert_eq!(
        status.retry_at.map(OffsetDateTime::unix_timestamp),
        Some(reset.unix_timestamp())
    );
    assert_eq!(
        env.fake.count("commits"),
        asked,
        "no GitHub request before the stale sync's rate limit expires"
    );
}

#[tokio::test]
async fn a_rate_limit_recorded_while_the_worker_waits_for_the_source_lock_is_preserved() {
    let env = Env::without_worker().await;
    let now = env.checked().await + seconds(300);
    let retry_at = now + seconds(3600);
    let before = env.fake.count("commits");
    let start_gate = env.state.sources().hold_starts().await;
    let ticking = {
        let state = env.state.clone();
        tokio::spawn(async move { server::source_tick_once(&state, now).await })
    };
    // The worker has taken its global turn and checked the old eligibility before this seam.
    // Record a newer reason that the source must not start before releasing it.
    env.state.sources().wait_start_held().await;
    env.server
        .records_db()
        .rate_limit_source(env.library, retry_at, "GitHub rate limit reached.", None)
        .await
        .unwrap();
    drop(start_gate);
    assert_eq!(ticking.await.unwrap(), 1);

    let status = env.status().await;
    assert_eq!(status.state, SourceState::RateLimited, "{status:?}");
    assert_eq!(status.retry_at, Some(retry_at));
    assert_eq!(
        env.fake.count("commits"),
        before,
        "the stale worker decision must not make a GitHub request"
    );
}

#[tokio::test]
async fn a_revoked_token_stops_retrying_until_an_owner_acts() {
    let env = Env::with_token(Some("ghp_first_token")).await;
    env.fake.require_token("ghp_second_token");
    let checked = env.checked().await;
    let before = env.fake.count("commits");
    let mut at = checked;
    let mut ran = 0;
    for _ in 0..3 {
        at += seconds(300);
        ran += env.tick(at).await;
    }
    assert_eq!(ran, 1, "one attempt, then no more");
    assert_eq!(env.fake.count("commits"), before + 1, "one request");
    let status = env.status().await;
    assert_eq!(status.state, SourceState::NotConnected);
    let message = status.last_error.unwrap();
    assert!(message.contains("GitHub refused the token"), "{message}");
    assert!(!message.contains("Bad credentials"), "{message}");
    // A day later, still nothing.
    assert_eq!(env.tick(at + seconds(86_400)).await, 0);
    // The files stay readable meanwhile.
    assert_eq!(
        env.download("a.md").await,
        (StatusCode::OK, b"first".to_vec())
    );

    // An owner gives the new token: the connection works again at once.
    let c2 = env
        .fake
        .commit("main", &files(&[("a.md", "after"), ("guide/b.md", "b")]));
    let (status, answer) = env
        .call(
            Method::PATCH,
            &format!("/api/libraries/{}/source", env.library.0),
            Some(json!({ "token": "ghp_second_token" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    env.wait_commit(&c2.sha).await;
    assert_eq!(env.download("a.md").await.1, b"after");
}

#[tokio::test]
async fn sync_now_clears_a_stop() {
    let env = Env::new().await;
    env.fake.delete_branch("main");
    let checked = env.checked().await;
    assert_eq!(env.tick(checked + seconds(300)).await, 1);
    assert_eq!(env.status().await.state, SourceState::NotConnected);
    let c2 = env.fake.commit("main", &files(&[("a.md", "back")]));
    let (status, _) = env
        .call(
            Method::POST,
            &format!("/api/libraries/{}/source/sync", env.library.0),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    env.wait_commit(&c2.sha).await;
}

#[tokio::test]
async fn a_deleted_branch_is_not_connected_and_files_stay_readable() {
    let env = Env::new().await;
    env.fake.delete_branch("main");
    let checked = env.checked().await;
    let before = env.fake.count("commits");
    assert_eq!(env.tick(checked + seconds(300)).await, 1);
    let status = env.status().await;
    assert_eq!(status.state, SourceState::NotConnected);
    let message = status.last_error.unwrap();
    assert!(message.contains("was not found on GitHub"), "{message}");
    assert!(!message.contains("No commit found"), "{message}");
    assert_eq!(env.tick(checked + seconds(900)).await, 0);
    assert_eq!(env.fake.count("commits"), before + 1);
    // Nothing was removed.
    assert_eq!(
        env.download("a.md").await,
        (StatusCode::OK, b"first".to_vec())
    );
    assert_eq!(
        env.download("guide/b.md").await,
        (StatusCode::OK, b"b".to_vec())
    );
    assert_eq!(status.files, 2);
}

#[tokio::test]
async fn a_renamed_repository_gives_a_plain_message() {
    let env = Env::new().await;
    env.fake.rename_repo("handbook");
    let checked = env.checked().await;
    assert_eq!(env.tick(checked + seconds(300)).await, 1);
    let status = env.status().await;
    assert_eq!(status.state, SourceState::NotConnected);
    let message = status.last_error.unwrap();
    assert_eq!(
        message,
        "The repository or branch was not found on GitHub, or the token cannot see it."
    );
    assert_eq!(env.tick(checked + seconds(900)).await, 0);
    assert_eq!(
        env.download("a.md").await,
        (StatusCode::OK, b"first".to_vec())
    );
}

#[tokio::test]
async fn a_network_failure_backs_off_and_recovers() {
    let env = Env::new().await;
    let (status, _) = env
        .call(
            Method::PATCH,
            &format!("/api/libraries/{}/source", env.library.0),
            Some(json!({ "poll_seconds": 60 })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    env.settled().await;
    let t0 = env.checked().await;

    // First failure: one minute.
    env.fake.fail_next(502);
    let t1 = t0 + seconds(60);
    assert_eq!(env.tick(t1).await, 1);
    let status = env.status().await;
    assert_eq!(status.state, SourceState::Failed);
    assert_eq!(status.retry_at, Some(t1 + seconds(60)));
    assert!(!status.last_error.unwrap().contains("something broke"));
    assert_eq!(env.tick(t1 + seconds(59)).await, 0);

    // Second: two minutes.
    env.fake.fail_next(502);
    let t2 = t1 + seconds(60);
    assert_eq!(env.tick(t2).await, 1);
    assert_eq!(env.status().await.retry_at, Some(t2 + seconds(120)));
    assert_eq!(env.tick(t2 + seconds(119)).await, 0);

    // Third: four minutes.
    env.fake.fail_next(502);
    let t3 = t2 + seconds(120);
    assert_eq!(env.tick(t3).await, 1);
    assert_eq!(env.status().await.retry_at, Some(t3 + seconds(240)));

    // GitHub answers again: in step, and the back-off starts over.
    let c2 = env.fake.commit("main", &files(&[("a.md", "recovered")]));
    let t4 = t3 + seconds(240);
    assert_eq!(env.tick(t4).await, 1);
    let status = env.status().await;
    assert_eq!(status.state, SourceState::Ok);
    assert_eq!(status.last_commit.as_deref(), Some(c2.sha.as_str()));
    assert_eq!(status.retry_at, None);
    env.fake.fail_next(502);
    let t5 = t4 + seconds(60);
    assert_eq!(env.tick(t5).await, 1);
    assert_eq!(env.status().await.retry_at, Some(t5 + seconds(60)));
}

#[tokio::test]
async fn the_back_off_stops_growing_at_an_hour() {
    let env = Env::new().await;
    let mut at = env.checked().await;
    for _ in 0..8 {
        let status = env.status().await;
        at = status
            .retry_at
            .unwrap_or(at)
            .max(status.last_checked_at.unwrap() + seconds(300));
        env.fake.fail_next(500);
        assert_eq!(env.tick(at).await, 1);
    }
    let status = env.status().await;
    assert_eq!(status.retry_at, Some(at + seconds(3600)));
}

#[tokio::test]
async fn wakes_for_one_library_are_coalesced() {
    let env = Env::new().await;
    let before = env.fake.count("commits");
    // Many wakes before the worker runs (this test's runtime has one thread): one sync.
    for _ in 0..10 {
        env.state.sources().wake(env.library);
    }
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while env.fake.count("commits") == before {
        assert!(tokio::time::Instant::now() < deadline, "the wake never ran");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    env.settled().await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(env.fake.count("commits"), before + 1);
}

#[tokio::test]
async fn dropping_the_worker_stops_running_and_waiting_syncs() {
    let env = Env::without_worker().await;
    let second = env.add_library("Mirror Two").await;
    let third = env.add_library("Mirror Three").await;
    env.fake.commit(
        "main",
        &files(&[("a.md", "first"), ("guide/b.md", "b"), ("c.md", "new")]),
    );
    let held = env.fake.hold_commits().await;
    let commits = env.fake.count("commits");
    let trees = env.fake.count("trees");
    let worker = server::source_sync::start_worker(env.state.clone());
    for library in [env.library, second, third] {
        env.state.sources().wake(library);
    }
    env.fake.wait_commits_held(2).await;

    drop(worker);
    drop(held);
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert_eq!(
        env.fake.count("commits"),
        commits + 2,
        "the permit-waiting sync must not start"
    );
    assert_eq!(
        env.fake.count("trees"),
        trees,
        "in-flight syncs must stop when their worker is dropped"
    );

    // Simulate the normal stale-sync recovery point, then start a fresh worker over the same
    // AppState. Dropping the first worker must not leave its in-memory running entry behind.
    let mut stale = env.status().await;
    stale.state = SourceState::Syncing;
    stale.last_checked_at = Some(OffsetDateTime::now_utc() - seconds(20 * 60));
    env.server
        .records_db()
        .set_source_status(env.library, &stale)
        .await
        .unwrap();
    let restarted = server::source_sync::start_worker(env.state.clone());
    env.state.sources().wake(env.library);
    let recovered = env.settled().await;
    assert_eq!(recovered.state, SourceState::Ok, "{recovered:?}");

    let latest = env.fake.commit(
        "main",
        &files(&[
            ("a.md", "first"),
            ("guide/b.md", "b"),
            ("d.md", "after restart"),
        ]),
    );
    env.state.sources().wake(env.library);
    env.wait_commit(&latest.sha).await;
    drop(restarted);

    dropping_the_worker_before_children_enter_clears_every_running_entry().await;
}

async fn dropping_the_worker_before_children_enter_clears_every_running_entry() {
    let env = Env::without_worker().await;
    let second = env.add_library("Mirror Two").await;
    let third = env.add_library("Mirror Three").await;
    let libraries = [env.library, second, third];
    let latest = env.fake.commit(
        "main",
        &files(&[
            ("a.md", "first"),
            ("guide/b.md", "b"),
            ("unpolled.md", "after restart"),
        ]),
    );
    let child_gate = env.state.sources().hold_child_starts().await;
    let worker = server::source_sync::start_worker(env.state.clone());
    for library in libraries {
        env.state.sources().wake(library);
    }
    env.state.sources().wait_child_start().await;

    // Every child exists but is paused before entering the body that used to construct its
    // RunningGuard. Cancelling the owner must still clear all three running-map entries.
    drop(worker);
    drop(child_gate);

    let restarted = server::source_sync::start_worker(env.state.clone());
    for library in libraries {
        env.state.sources().wake(library);
    }
    env.wait_libraries_commit(&libraries, &latest.sha).await;
    drop(restarted);
}

#[tokio::test]
async fn a_wake_while_a_sync_runs_is_tried_again_after_it() {
    let env = Env::new().await;
    let c2 = env
        .fake
        .commit("main", &files(&[("a.md", "first"), ("c.md", "two")]));
    let held = env.fake.hold_blobs().await;
    let running = {
        let state = env.state.clone();
        let library = env.library;
        tokio::spawn(async move { sync_once(&state, library, OffsetDateTime::now_utc()).await })
    };
    env.fake.wait_blob_held().await;
    // The running sync read c2; c3 is pushed meanwhile and the push wakes the worker, whose
    // sync finds the library busy (`Ok(None)`): it must try again once the first is done.
    let c3 = env.fake.commit(
        "main",
        &files(&[("a.md", "first"), ("c.md", "two"), ("d.md", "three")]),
    );
    env.state.sources().wake(env.library);
    tokio::time::sleep(Duration::from_millis(300)).await;
    drop(held);
    let report = running.await.unwrap().unwrap().unwrap();
    assert_eq!(report.commit, c2.sha);
    env.wait_commit(&c3.sha).await;
    assert_eq!(env.download("d.md").await.1, b"three");
}

#[tokio::test]
async fn libraries_of_archived_or_deleted_sites_are_not_synced() {
    let env = Env::new().await;
    let checked = env.checked().await;
    let before = env.fake.count("commits");
    let (status, answer) = env
        .call(
            Method::POST,
            &format!("/api/sites/{}/archive", env.site.0),
            None,
        )
        .await;
    assert!(status.is_success(), "{status} {answer}");
    assert_eq!(env.tick(checked + seconds(300)).await, 0);
    env.state.sources().wake(env.library);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(env.fake.count("commits"), before, "archived: nothing asked");

    let (status, answer) = env
        .call(
            Method::POST,
            &format!("/api/sites/{}/reactivate", env.site.0),
            None,
        )
        .await;
    assert!(status.is_success(), "{status} {answer}");
    // The site in the recycle bin: its libraries are not synced either.
    let mut sql = sqlx::PgConnection::connect(&env.server.db.url())
        .await
        .unwrap();
    sqlx::query("update nodes set deleted_at = now(), deleted_with = id where id = $1")
        .bind(env.site.0)
        .execute(&mut sql)
        .await
        .unwrap();
    assert_eq!(env.tick(checked + seconds(600)).await, 0);
    env.state.sources().wake(env.library);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(env.fake.count("commits"), before, "deleted: nothing asked");
    // Back from the bin: due again.
    sqlx::query("update nodes set deleted_at = null, deleted_with = null where id = $1")
        .bind(env.site.0)
        .execute(&mut sql)
        .await
        .unwrap();
    assert_eq!(env.tick(checked + seconds(900)).await, 1);

    library_archived_while_waiting_for_a_turn_is_not_synced().await;
}

async fn library_archived_while_waiting_for_a_turn_is_not_synced() {
    let env = Env::new().await;
    let second = env.add_library("Mirror Two").await;
    let third = env.add_library("Mirror Three").await;
    // `settled` observes the status write; let the worker also remove each from its in-memory
    // running set before this tick deliberately takes all three.
    tokio::time::sleep(Duration::from_millis(300)).await;
    env.fake.commit(
        "main",
        &files(&[("a.md", "first"), ("guide/b.md", "b"), ("c.md", "new")]),
    );
    let held = env.fake.hold_commits().await;
    let commits = env.fake.count("commits");
    for library in [env.library, second, third] {
        env.state.sources().wake(library);
    }
    env.fake.wait_commits_held(2).await;

    let archived = sqlx::query("update sites set archived_at = now() where id = $1")
        .bind(env.site.0)
        .execute(env.server.records_db().test_pool())
        .await;
    assert_eq!(archived.unwrap().rows_affected(), 1);
    drop(held);
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while env.fake.count("commits") < commits + 2 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "two syncs did not finish"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        env.fake.count("commits"),
        commits + 2,
        "the library waiting for a turn must be rechecked before GitHub"
    );
}

#[tokio::test]
async fn a_sync_whose_connection_changed_meanwhile_records_no_event() {
    let env = Env::new().await;
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while env.source_synced_events().await.is_empty() {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(env.source_synced_events().await.len(), 1);
    let c2 = env
        .fake
        .commit("main", &files(&[("a.md", "first"), ("c.md", "new")]));
    let held = env.fake.hold_blobs().await;
    let running = {
        let state = env.state.clone();
        let library = env.library;
        tokio::spawn(async move { sync_once(&state, library, OffsetDateTime::now_utc()).await })
    };
    env.fake.wait_blob_held().await;
    assert!(
        env.server
            .records_db()
            .bump_source_generation(env.library)
            .await
            .unwrap()
    );
    drop(held);
    running.await.unwrap().unwrap();
    // Its status was not recorded, and neither is its event; the next sync records both.
    sync_once(&env.state, env.library, OffsetDateTime::now_utc())
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while env.source_synced_events().await.len() < 2 {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let events = env.source_synced_events().await;
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(events[1]["commit"], c2.sha[..7]);
    assert_eq!(
        env.status().await.last_commit.as_deref(),
        Some(c2.sha.as_str())
    );
}

#[tokio::test]
async fn webhook_with_a_good_signature_syncs_within_the_timeout_without_waiting_for_the_poll() {
    let env = Env::new().await;
    let secret = env.webhook_secret().await;
    let c2 = env
        .fake
        .commit("main", &files(&[("a.md", "pushed"), ("guide/b.md", "b")]));
    let body = push("refs/heads/main");
    let status = env
        .hook("push", &body, Some(&signature(&secret, &body)))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    env.wait_commit(&c2.sha).await;
    assert_eq!(env.download("a.md").await.1, b"pushed");
    // The secret is never in the event log.
    let mut sql = sqlx::PgConnection::connect(&env.server.db.url())
        .await
        .unwrap();
    let details: Vec<String> = sqlx::query_scalar("select detail::text from events")
        .fetch_all(&mut sql)
        .await
        .unwrap();
    assert!(details.iter().all(|d| !d.contains(&secret)));
}

#[tokio::test]
async fn webhook_bad_signature_is_401_and_does_nothing() {
    let env = Env::new().await;
    let secret = env.webhook_secret().await;
    env.fake
        .commit("main", &files(&[("a.md", "pushed"), ("guide/b.md", "b")]));
    let before = env.fake.count("commits");
    let body = push("refs/heads/main");
    let wrong = signature("not the secret", &body);
    let altered = signature(&secret, &push("refs/heads/other"));
    let upper = signature(&secret, &body).to_uppercase();
    for sig in [
        Some(wrong.as_str()),
        Some(altered.as_str()),
        Some(upper.as_str()),
        Some("sha256="),
        Some("sha1=0123"),
        Some("garbage"),
        None,
    ] {
        assert_eq!(
            env.hook("push", &body, sig).await,
            StatusCode::UNAUTHORIZED,
            "{sig:?}"
        );
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(env.fake.count("commits"), before);
}

#[tokio::test]
async fn webhook_for_another_branch_is_204_and_does_nothing() {
    let env = Env::new().await;
    let secret = env.webhook_secret().await;
    let before = env.fake.count("commits");
    for (event, body) in [
        ("push", push("refs/heads/other")),
        ("push", push("refs/heads/main/x")),
        ("push", push("refs/tags/main")),
        ("push", b"not json".to_vec()),
        ("issues", push("refs/heads/main")),
    ] {
        let status = env
            .hook(event, &body, Some(&signature(&secret, &body)))
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{event}");
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(env.fake.count("commits"), before);
}

#[tokio::test]
async fn webhook_ping_is_200() {
    let env = Env::new().await;
    let secret = env.webhook_secret().await;
    let body = json!({ "zen": "Keep it logically awesome.", "hook_id": 1 })
        .to_string()
        .into_bytes();
    let status = env
        .hook("ping", &body, Some(&signature(&secret, &body)))
        .await;
    assert_eq!(status, StatusCode::OK);
    // A ping with a bad signature is refused as any delivery.
    let status = env
        .hook("ping", &body, Some(&signature("wrong", &body)))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn webhook_without_a_secret_set_is_404() {
    let env = Env::new().await;
    let body = push("refs/heads/main");
    // No secret yet: whatever it is signed with.
    let status = env
        .hook("push", &body, Some(&signature("anything", &body)))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // An unknown library, or not a library address at all.
    let client = reqwest::Client::new();
    for path in [
        format!("/hooks/github/{}", uuid::Uuid::new_v4()),
        "/hooks/github/not-a-uuid".to_owned(),
        format!("/hooks/github/{}", env.site.0),
    ] {
        let status = hook(
            &client,
            &env.server.url(&path),
            "push",
            &body,
            Some(&signature("anything", &body)),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
    }
}

#[tokio::test]
async fn webhook_failures_are_throttled() {
    let env = Env::new().await;
    let second = env.add_library("Mirror Two").await;
    let secret = env.webhook_secret_for(second).await;
    env.webhook_secret().await;
    let body = push("refs/heads/main");
    let bad = signature("wrong", &body);
    for i in 0..60 {
        assert_eq!(
            env.hook("push", &body, Some(&bad)).await,
            StatusCode::UNAUTHORIZED,
            "{i}"
        );
    }
    assert_eq!(
        env.hook("push", &body, Some(&bad)).await,
        StatusCode::TOO_MANY_REQUESTS
    );
    // A different library from the same address has its own failure bucket.
    assert_eq!(
        env.hook_for(second, "push", &body, Some(&signature(&secret, &body)))
            .await,
        StatusCode::ACCEPTED
    );
    // Anyone-link guesses use their original, separate bucket.
    let (status, _) = env.call(Method::GET, "/api/l/not-a-link-token", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    webhook_bodies_over_the_in_flight_limit_are_refused().await;
}

async fn webhook_bodies_over_the_in_flight_limit_are_refused() {
    use futures_util::StreamExt as _;

    let env = Env::new().await;
    let client = reqwest::Client::new();
    let url = env.server.url(&format!("/hooks/github/{}", env.library.0));
    let mut held = Vec::new();
    for _ in 0..8 {
        let client = client.clone();
        let url = url.clone();
        held.push(tokio::spawn(async move {
            let body =
                futures_util::stream::once(async { Ok::<Vec<u8>, std::io::Error>(b"{".to_vec()) })
                    .chain(futures_util::stream::pending());
            client
                .post(url)
                .header("x-github-event", "push")
                .body(reqwest::Body::wrap_stream(body))
                .send()
                .await
        }));
    }
    tokio::time::sleep(Duration::from_millis(300)).await;

    let status = client
        .post(url)
        .header("x-github-event", "push")
        .body("{}")
        .send()
        .await
        .unwrap()
        .status();
    for request in held {
        request.abort();
    }
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn webhook_handlers_keep_the_in_flight_permit_after_the_body_is_read() {
    let env = Env::new().await;
    let secret = env.webhook_secret().await;
    let body = b"{}";
    let signed = signature(&secret, body);
    let mut connection = sqlx::PgConnection::connect(&env.server.db.url())
        .await
        .unwrap();
    let mut blocker = connection.begin().await.unwrap();
    // The throttle middleware reads `library_sources` before extraction. Hold the handler at
    // its later secret read, after the delivery body and its in-flight permit were acquired.
    sqlx::query("lock table secrets in access exclusive mode")
        .execute(&mut *blocker)
        .await
        .unwrap();

    let client = reqwest::Client::new();
    let url = env.server.url(&format!("/hooks/github/{}", env.library.0));
    let mut held = Vec::new();
    for _ in 0..8 {
        let client = client.clone();
        let url = url.clone();
        let signed = signed.clone();
        held.push(tokio::spawn(async move {
            client
                .post(url)
                .header("x-github-event", "push")
                .header("x-hub-signature-256", signed)
                .body("{}")
                .send()
                .await
        }));
    }
    // Each small body has been sent in full; the handlers are now held at their first DB read.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let excess = tokio::time::timeout(
        Duration::from_millis(500),
        client
            .post(url)
            .header("x-github-event", "push")
            .header("x-hub-signature-256", signed)
            .body("{}")
            .send(),
    )
    .await
    .expect("an excess delivery was admitted while eight handlers were still running")
    .unwrap();
    assert_eq!(excess.status(), StatusCode::TOO_MANY_REQUESTS);

    blocker.rollback().await.unwrap();
    for request in held {
        request.abort();
    }
}

#[tokio::test]
async fn webhook_secret_replacement_invalidates_the_old_one() {
    let env = Env::new().await;
    let old = env.webhook_secret().await;
    let new = env.webhook_secret().await;
    assert_ne!(old, new);
    let body = push("refs/heads/main");
    assert_eq!(
        env.hook("push", &body, Some(&signature(&old, &body))).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        env.hook("push", &body, Some(&signature(&new, &body))).await,
        StatusCode::ACCEPTED
    );
}

#[tokio::test]
async fn an_oversized_body_is_refused() {
    let env = Env::new().await;
    let secret = env.webhook_secret().await;
    let before = env.fake.count("commits");
    let mut body = push("refs/heads/main");
    // Valid JSON padded past 5 MiB.
    body.pop();
    body.extend_from_slice(b", \"pad\": \"");
    body.extend(std::iter::repeat_n(b'x', 5 << 20));
    body.extend_from_slice(b"\"}");
    let status = env
        .hook("push", &body, Some(&signature(&secret, &body)))
        .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(env.fake.count("commits"), before);
    // Just under the limit is read and checked.
    let mut body = push("refs/heads/main");
    body.pop();
    body.extend_from_slice(b", \"pad\": \"");
    let room = (5 << 20) - body.len() - 2;
    body.extend(std::iter::repeat_n(b'x', room));
    body.extend_from_slice(b"\"}");
    assert_eq!(body.len(), 5 << 20);
    let status = env
        .hook("push", &body, Some(&signature(&secret, &body)))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
}

/// The public address production-mode tests are reached at.
const PUBLIC: &str = "https://share.example.com";

/// The headers a TLS-terminating proxy adds for GitHub's delivery: no `Origin`.
fn proxy_headers(proto: &'static str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("x-forwarded-proto", HeaderValue::from_static(proto));
    headers.insert(
        "x-forwarded-host",
        HeaderValue::from_static("share.example.com"),
    );
    headers.insert("x-forwarded-for", HeaderValue::from_static("140.82.115.7"));
    headers
}

#[tokio::test]
async fn webhook_works_through_the_trusted_proxy_in_production_mode() {
    let fake = start_fake_github(OWNER, REPO).await;
    fake.commit("main", &files(&[("a.md", "first")]));
    let origin = fake.origin();
    let server = spawn_server_config(move |c| {
        c.github_api = origin;
        c.deployment = Deployment::production_for_tests(PUBLIC, "127.0.0.1, ::1").unwrap();
    })
    .await;
    let state = server.app_state().await;
    let db = server.records_db();
    let library = common::Site::library(&server).await;
    let password = uuid::Uuid::new_v4().simple().to_string();
    let ada = db
        .create_user(
            None,
            records::NewUser {
                email: "ada@test.invalid",
                name: "Ada Admin",
                password: &password,
                is_admin: true,
            },
        )
        .await
        .unwrap();
    db.connect_source(&records::NewSource {
        library,
        kind: records::SourceKind::Github,
        owner: OWNER.to_owned(),
        repo: REPO.to_owned(),
        branch: "main".to_owned(),
        folder: String::new(),
        token: None,
        poll_seconds: 300,
        connected_by: ada.id,
    })
    .await
    .unwrap();
    let secret = "a3f1".repeat(16);
    let sealed = state
        .secrets()
        .seal(secret.as_bytes(), &webhook_context(library))
        .unwrap();
    let id = db
        .put_secret(&SealedRow {
            key_id: sealed.key_id,
            nonce: sealed.nonce,
            ciphertext: sealed.ciphertext,
        })
        .await
        .unwrap();
    db.change_source(
        library,
        &SourceChange {
            webhook_secret: Some(Some(id)),
            ..SourceChange::default()
        },
    )
    .await
    .unwrap();
    sync_once(&state, library, OffsetDateTime::now_utc())
        .await
        .unwrap();
    let c2 = fake.commit("main", &files(&[("a.md", "pushed")]));

    let url = server.url(&format!("/hooks/github/{}", library.0));
    let body = push("refs/heads/main");
    let signed = signature(&secret, &body);
    // Over plain HTTP the proxy's request is refused, as any non-page request is.
    let plain = reqwest::Client::builder()
        .default_headers(proxy_headers("http"))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    assert_eq!(
        hook(&plain, &url, "push", &body, Some(&signed)).await,
        StatusCode::FORBIDDEN
    );
    // Through the proxy over HTTPS, with no Origin, it is taken.
    let proxied = reqwest::Client::builder()
        .default_headers(proxy_headers("https"))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    assert_eq!(
        hook(&proxied, &url, "push", &body, Some(&signed)).await,
        StatusCode::ACCEPTED
    );
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        let status = db.source_of(library).await.unwrap().unwrap().1;
        if status.last_commit.as_deref() == Some(c2.sha.as_str()) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "never synced: {status:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
