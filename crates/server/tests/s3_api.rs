//! Connecting and managing S3 libraries, and the administrator's S3 settings: the routes, the
//! order of their checks, and the secret key's whole life (it is never answered, logged as an
//! event or stored outside its ciphertext).

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
use records::{Principal, S3_ENDPOINTS, SourceKind, SourceState};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use server::AppState;
use server::secrets::Sealed;
use server::source_sync::{sync_once, token_context};
use time::OffsetDateTime;

const BUCKET: &str = "api-bucket-9f3";
const ENDPOINT: &str = "api-endpoint-2b7";
/// Approved, but its ranges do not take the fake's loopback address.
const BLOCKED: &str = "blocked-endpoint-6d1";
/// Approved, but nothing listens there.
const CLOSED: &str = "closed-endpoint-0c4";
const KEY_ID: &str = "AKIDAPIEXAMPLE4417";
const SECRET: &str = "planted-api-secret-8c1e-zz";
const OTHER_KEY_ID: &str = "AKIDOTHEREXAMPLE5528";
const OTHER_SECRET: &str = "planted-other-secret-3b7d-qq";
const WRONG_SECRET: &str = "planted-wrong-secret-9a2f-xx";

struct Env {
    server: TestServer,
    fake: FakeS3,
    admin: TestUser,
    site: NodeId,
    state: AppState,
    /// Every answer received, as text: status, headers and body.
    seen: Mutex<Vec<String>>,
}

/// An entry of the `s3_endpoints` setting pointing at `origin`.
fn endpoint_entry(name: &str, origin: &str, allow: &[&str]) -> Value {
    json!({
        "name": name,
        "url": origin,
        "region": "us-east-1",
        "path_style": true,
        "allow": allow,
    })
}

/// Writes the setting straight to the table: the plain-HTTP loopback form is refused by the
/// settings route and only the server's test switch lets it be used.
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

async fn count(server: &TestServer, sql: &str) -> i64 {
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_owned()))
        .fetch_one(server.records_db().test_pool())
        .await
        .unwrap()
}

/// Puts the fake in the state a probe-failure case needs.
type Arrange = Box<dyn Fn(&FakeS3)>;

fn id_of(answer: &Value) -> NodeId {
    NodeId(answer["id"].as_str().unwrap().parse().unwrap())
}

impl Env {
    async fn new() -> Env {
        let fake = FakeS3::start().await;
        fake.bucket(BUCKET);
        fake.require_keys(KEY_ID, SECRET);
        let server = spawn_server_config_without_workers(|config| {
            config.max_file_bytes = 512 * 1024 * 1024;
        })
        .await;
        let admin = server.admin("S3 admin").await;
        let library = common::Site::library(&server).await;
        let site = server
            .records_db()
            .node(library)
            .await
            .unwrap()
            .unwrap()
            .site;
        let state = server.app_state().await;
        state
            .s3()
            .set_retry_wait_for_tests(Duration::from_millis(5));
        state
            .s3()
            .set_timeouts_for_tests(Duration::from_secs(2), Duration::from_secs(2));
        let closed = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}", listener.local_addr().unwrap())
        };
        set_endpoints(
            &server,
            json!([
                endpoint_entry(ENDPOINT, &fake.origin(), &["127.0.0.0/8"]),
                endpoint_entry(BLOCKED, &fake.origin(), &[]),
                endpoint_entry(CLOSED, &closed, &["127.0.0.0/8"]),
            ]),
        )
        .await;
        Env {
            server,
            fake,
            admin,
            site,
            state,
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
        let headers = format!("{:?}", response.headers());
        let text = response.text().await.unwrap();
        self.seen
            .lock()
            .unwrap()
            .push(format!("{status} {headers} {text}"));
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    fn body(name: &str) -> Value {
        json!({
            "name": name,
            "endpoint": ENDPOINT,
            "bucket": BUCKET,
            "access_key_id": KEY_ID,
            "secret_access_key": SECRET,
        })
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

    async fn connect(&self, body: Value) -> (StatusCode, Value) {
        self.connect_as(&self.admin, body).await
    }

    async fn connected(&self, name: &str) -> NodeId {
        let (status, answer) = self.connect(Env::body(name)).await;
        assert_eq!(status, StatusCode::CREATED, "{answer}");
        id_of(&answer)
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

    async fn disconnect(&self, user: &TestUser, library: NodeId) -> (StatusCode, Value) {
        self.call(
            user,
            Method::DELETE,
            &format!("/api/libraries/{}/source", library.0),
            None,
        )
        .await
    }

    async fn sync(&self, library: NodeId) {
        sync_once(&self.state, library, OffsetDateTime::UNIX_EPOCH)
            .await
            .unwrap();
    }

    async fn secrets(&self) -> i64 {
        count(&self.server, "select count(*) from secrets").await
    }

    async fn sources(&self) -> i64 {
        count(&self.server, "select count(*) from library_sources").await
    }

    async fn libraries(&self) -> i64 {
        count(
            &self.server,
            "select count(*) from nodes where kind = 'library' and deleted_at is null",
        )
        .await
    }

    async fn with_role(&self, name: &str, role: &str, at: NodeId) -> TestUser {
        let user = self.server.user(name).await;
        self.server
            .records_db()
            .set_node_access(self.admin.id, at, Principal::User(user.id), Some(role))
            .await
            .unwrap();
        user
    }

    /// Every event's detail as text, once `kind` has been recorded (events are written by a
    /// background task, in order).
    async fn event_texts_after(&self, kind: &str) -> Vec<String> {
        let deadline = tokio::time::Instant::now() + common::TIMEOUT;
        loop {
            if !self
                .server
                .records_db()
                .events_of_kind(kind)
                .await
                .unwrap()
                .is_empty()
            {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "no {kind} event");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        sqlx::query_scalar("select kind || ' ' || detail::text from events")
            .fetch_all(self.server.records_db().test_pool())
            .await
            .unwrap()
    }

    async fn token(&self, library: NodeId) -> records::SecretId {
        self.server
            .records_db()
            .source_of(library)
            .await
            .unwrap()
            .unwrap()
            .0
            .token
            .unwrap()
    }
}

#[tokio::test]
async fn connect_creates_a_library_and_the_first_listing_fills_it() {
    let env = Env::new().await;
    env.fake.put(BUCKET, "docs/Guide.txt", b"guide");
    env.fake.put(BUCKET, "Readme.md", b"readme");
    let (status, answer) = env.connect(Env::body("From the bucket")).await;
    assert_eq!(status, StatusCode::CREATED, "{answer}");
    assert_eq!(answer["source"]["kind"], "s3");
    assert_eq!(answer["source"]["endpoint"], ENDPOINT);
    assert_eq!(answer["source"]["bucket"], BUCKET);
    let library = id_of(&answer);
    let db = env.server.records_db();
    let (config, status) = db.source_of(library).await.unwrap().unwrap();
    assert_eq!(config.kind, SourceKind::S3);
    assert_eq!(config.connected_by, env.admin.id);
    assert_eq!(status.state, SourceState::Connecting);
    let place = db.s3_place(library).await.unwrap().unwrap();
    assert_eq!(
        (
            place.endpoint.as_str(),
            place.bucket.as_str(),
            place.prefix.as_str()
        ),
        (ENDPOINT, BUCKET, "")
    );
    assert_eq!(place.access_key_id, KEY_ID);
    // The probe: one listing and the versioning question, nothing else.
    assert_eq!(env.fake.count("list"), 1);
    assert_eq!(env.fake.count("versioning"), 1);
    env.sync(library).await;
    assert_eq!(db.live_children(library).await.unwrap().len(), 2);
    assert_eq!(env.fake.count("get"), 0);
    assert_eq!(env.fake.count("write"), 0);
}

#[tokio::test]
async fn connecting_to_an_unapproved_endpoint_is_refused_and_makes_nothing() {
    let env = Env::new().await;
    let (sources, secrets, libraries) = (
        env.sources().await,
        env.secrets().await,
        env.libraries().await,
    );
    let mut body = Env::body("Unapproved");
    body["endpoint"] = json!("not-approved-endpoint");
    let (status, answer) = env.connect(body).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
    assert_eq!(answer["error"], "s3_endpoint_not_approved");
    assert_eq!(env.sources().await, sources);
    assert_eq!(env.secrets().await, secrets);
    assert_eq!(env.libraries().await, libraries);
    assert!(env.fake.requests().is_empty());
}

#[tokio::test]
async fn an_address_typed_as_the_endpoint_is_refused() {
    let env = Env::new().await;
    for typed in [
        env.fake.origin(),
        "https://s3.example.com".to_owned(),
        "127.0.0.1".to_owned(),
        "aws:not-a-region".to_owned(),
    ] {
        let mut body = Env::body("Typed");
        body["endpoint"] = json!(typed);
        let (status, answer) = env.connect(body).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{typed}: {answer}"
        );
        assert_eq!(answer["error"], "s3_endpoint_not_approved", "{typed}");
    }
    assert!(env.fake.requests().is_empty());
}

#[tokio::test]
async fn each_bad_bucket_and_prefix_is_refused_with_its_reason() {
    let env = Env::new().await;
    for bucket in [
        "",
        "ab",
        "UPPER-bucket",
        "-abc",
        "abc-",
        "a..b",
        "a/b",
        "a@b",
        "a:1",
        "a_b",
        "192.168.1.1",
    ] {
        let mut body = Env::body("Bad bucket");
        body["bucket"] = json!(bucket);
        let (status, answer) = env.connect(body).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{bucket}: {answer}"
        );
        assert_eq!(answer["error"], "invalid_s3_place", "{bucket}");
        assert_eq!(answer["reason"], "bucket", "{bucket}");
    }
    let long = "a".repeat(513);
    for prefix in [
        "/x",
        "a//b",
        "../x",
        "a/../b",
        "./a",
        "x\u{1}y",
        long.as_str(),
    ] {
        let mut body = Env::body("Bad prefix");
        body["prefix"] = json!(prefix);
        let (status, answer) = env.connect(body).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{prefix:?}: {answer}"
        );
        assert_eq!(answer["error"], "invalid_s3_place", "{prefix:?}");
        assert_eq!(answer["reason"], "prefix", "{prefix:?}");
    }
    assert!(env.fake.requests().is_empty());
}

#[tokio::test]
async fn bad_key_lengths_are_refused_before_the_store_is_asked() {
    let env = Env::new().await;
    let long_id = "A".repeat(129);
    let long_secret = "s".repeat(1001);
    for (id, secret) in [
        ("", SECRET),
        (long_id.as_str(), SECRET),
        ("AKID\nX", SECRET),
        (KEY_ID, ""),
        (KEY_ID, long_secret.as_str()),
        (KEY_ID, "with\u{7}bell"),
    ] {
        let mut body = Env::body("Bad key");
        body["access_key_id"] = json!(id);
        body["secret_access_key"] = json!(secret);
        let (status, answer) = env.connect(body).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
        assert_eq!(answer["error"], "invalid_request");
    }
    assert!(env.fake.requests().is_empty());
}

#[tokio::test]
async fn each_probe_failure_is_refused_plainly_and_leaves_no_library_and_no_secret_row() {
    let env = Env::new().await;
    env.fake.bucket("denied-bucket");
    env.fake.deny("denied-bucket", "");
    let (sources, secrets, libraries) = (
        env.sources().await,
        env.secrets().await,
        env.libraries().await,
    );
    let cases: Vec<(&str, Value, Arrange)> = vec![
        (
            "keys",
            json!({"secret_access_key": WRONG_SECRET}),
            Box::new(|_| {}),
        ),
        (
            "keys",
            json!({"access_key_id": OTHER_KEY_ID}),
            Box::new(|_| {}),
        ),
        (
            "bucket",
            json!({"bucket": "no-such-bucket"}),
            Box::new(|_| {}),
        ),
        (
            "denied",
            json!({"bucket": "denied-bucket"}),
            Box::new(|_| {}),
        ),
        (
            "region",
            json!({}),
            Box::new(|fake| fake.fail_next(400, "AuthorizationHeaderMalformed")),
        ),
        ("address", json!({"endpoint": BLOCKED}), Box::new(|_| {})),
        ("offline", json!({"endpoint": CLOSED}), Box::new(|_| {})),
        (
            "offline",
            json!({}),
            Box::new(|fake| fake.throttle_next(20)),
        ),
        ("clock", json!({}), Box::new(|fake| fake.skew_clock())),
    ];
    for (reason, change, arrange) in cases {
        arrange(&env.fake);
        let mut body = Env::body("Probed");
        for (field, value) in change.as_object().unwrap() {
            body[field] = value.clone();
        }
        let (status, answer) = env.connect(body).await;
        env.fake.throttle_next(0);
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{reason}: {answer}"
        );
        assert_eq!(answer["error"], "s3_unreachable", "{reason}");
        assert_eq!(answer["reason"], reason, "{answer}");
        let message = answer["message"].as_str().unwrap();
        assert!(!message.is_empty() && message.len() <= 300);
        let text = answer.to_string();
        assert!(!text.contains("fake S3"), "{text}");
        assert!(
            !text.contains(BUCKET) && !text.contains("127.0.0.1"),
            "{text}"
        );
        assert_eq!(env.sources().await, sources, "{reason}");
        assert_eq!(env.secrets().await, secrets, "{reason}");
        assert_eq!(env.libraries().await, libraries, "{reason}");
    }
    assert_eq!(env.fake.count("write"), 0);
}

#[tokio::test]
async fn template_and_asset_kinds_are_refused() {
    let env = Env::new().await;
    for kind in ["template", "asset", "picture"] {
        let mut body = Env::body("Kinds");
        body["kind"] = json!(kind);
        let (status, answer) = env.connect(body).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{kind}: {answer}");
        assert_eq!(answer["error"], "invalid_request");
    }
    let mut body = Env::body("Read and write");
    body["mode"] = json!("read_write");
    let (status, answer) = env.connect(body).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
    assert!(env.fake.requests().is_empty());
    let mut body = Env::body("Pictures");
    body["kind"] = json!("image");
    body["mode"] = json!("read_only");
    let (status, answer) = env.connect(body).await;
    assert_eq!(status, StatusCode::CREATED, "{answer}");
}

#[tokio::test]
async fn a_second_library_on_an_overlapping_prefix_is_refused() {
    let env = Env::new().await;
    let mut body = Env::body("Team");
    body["prefix"] = json!("team");
    let (status, answer) = env.connect(body).await;
    assert_eq!(status, StatusCode::CREATED, "{answer}");
    let team = id_of(&answer);
    assert_eq!(
        env.server
            .records_db()
            .s3_place(team)
            .await
            .unwrap()
            .unwrap()
            .prefix,
        "team/"
    );
    let (secrets, libraries) = (env.secrets().await, env.libraries().await);
    for (name, prefix) in [("Inner", "team/docs/"), ("Whole", "")] {
        let mut body = Env::body(name);
        body["prefix"] = json!(prefix);
        let (status, answer) = env.connect(body).await;
        assert_eq!(status, StatusCode::CONFLICT, "{prefix}: {answer}");
        assert_eq!(answer["error"], "s3_place_in_use");
        assert_eq!(env.secrets().await, secrets);
        assert_eq!(env.libraries().await, libraries);
    }
    let mut body = Env::body("Teammates");
    body["prefix"] = json!("teammates/");
    let (status, answer) = env.connect(body).await;
    assert_eq!(status, StatusCode::CREATED, "{answer}");
}

#[tokio::test]
async fn only_site_creators_may_connect_and_only_owners_may_change_or_disconnect() {
    let env = Env::new().await;
    let viewer = env.with_role("Site viewer", "viewer", env.site).await;
    let owner = env.with_role("Site owner", "owner", env.site).await;
    let (status, answer) = env.connect_as(&viewer, Env::body("Not yours")).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{answer}");
    assert!(env.fake.requests().is_empty());

    let (status, answer) = env.connect_as(&owner, Env::body("Owned")).await;
    assert_eq!(status, StatusCode::CREATED, "{answer}");
    let library = id_of(&answer);
    let route = format!("/api/libraries/{}/source", library.0);
    let (status, _) = env
        .patch(&viewer, library, json!({"poll_seconds": 600}))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = env
        .call(&viewer, Method::POST, &format!("{route}/sync"), None)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = env.disconnect(&viewer, library).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = env.source(&viewer, library).await;
    assert_eq!(status, StatusCode::OK);

    let (status, answer) = env
        .patch(&owner, library, json!({"poll_seconds": 600}))
        .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let (status, _) = env
        .call(&owner, Method::POST, &format!("{route}/sync"), None)
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let (status, answer) = env.disconnect(&owner, library).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{answer}");
}

#[tokio::test]
async fn show_gives_place_and_usage_to_owners_and_not_to_viewers() {
    let env = Env::new().await;
    env.fake.enable_versioning(BUCKET);
    let mut body = Env::body("Shown");
    body["prefix"] = json!("shown-prefix/");
    let (status, answer) = env.connect(body).await;
    assert_eq!(status, StatusCode::CREATED, "{answer}");
    let library = id_of(&answer);
    let viewer = env.with_role("Library viewer", "viewer", library).await;

    let (status, owner_view) = env.source(&env.admin, library).await;
    assert_eq!(status, StatusCode::OK, "{owner_view}");
    assert_eq!(owner_view["kind"], "s3");
    assert_eq!(owner_view["mode"], "read_only");
    assert_eq!(owner_view["endpoint"], ENDPOINT);
    assert_eq!(owner_view["bucket"], BUCKET);
    assert_eq!(owner_view["prefix"], "shown-prefix/");
    assert_eq!(owner_view["key"]["id_ends"], &KEY_ID[KEY_ID.len() - 4..]);
    assert_eq!(owner_view["key"]["secret"], "set");
    assert_eq!(owner_view["versioning"], "enabled");
    for field in [
        "list",
        "read",
        "write",
        "bytes_down",
        "bytes_up",
        "cache_hits",
    ] {
        assert!(owner_view["usage"][field].is_u64(), "{field}: {owner_view}");
    }
    assert!(owner_view["usage"]["month"].is_string());
    assert!(owner_view["budget"]["list_requests"].is_u64());
    assert!(owner_view["budget"]["read_requests"].is_u64());
    assert!(owner_view["paused"].is_null());
    assert!(owner_view["poll_seconds"].is_u64());
    for field in [
        "state",
        "files",
        "bytes",
        "hidden",
        "last_error",
        "next_check_at",
    ] {
        assert!(owner_view.get(field).is_some(), "{field}");
    }
    let text = owner_view.to_string();
    assert!(!text.contains(&env.fake.origin()) && !text.contains("127.0.0.1"));
    assert!(!text.contains(KEY_ID));

    let (status, viewer_view) = env.source(&viewer, library).await;
    assert_eq!(status, StatusCode::OK, "{viewer_view}");
    assert_eq!(viewer_view["kind"], "s3");
    for field in [
        "endpoint",
        "bucket",
        "prefix",
        "key",
        "versioning",
        "usage",
        "budget",
        "paused",
    ] {
        assert!(viewer_view[field].is_null(), "{field}: {viewer_view}");
    }
    let text = viewer_view.to_string();
    for hidden in [
        ENDPOINT,
        BUCKET,
        "shown-prefix",
        &KEY_ID[KEY_ID.len() - 4..],
    ] {
        assert!(!text.contains(hidden), "{hidden} in {text}");
    }
}

#[tokio::test]
async fn the_secret_is_stored_only_as_ciphertext() {
    let env = Env::new().await;
    let library = env.connected("Sealed").await;
    let token = env.token(library).await;
    let row = env
        .server
        .records_db()
        .get_secret(token)
        .await
        .unwrap()
        .unwrap();
    assert!(
        !row.ciphertext
            .windows(SECRET.len())
            .any(|window| window == SECRET.as_bytes())
    );
    let sealed = Sealed {
        key_id: row.key_id,
        nonce: row.nonce,
        ciphertext: row.ciphertext,
    };
    let opened = env
        .state
        .secrets()
        .unseal(&sealed, &token_context(library))
        .unwrap();
    assert_eq!(opened, SECRET.as_bytes());
    // Another library's context does not open it.
    assert!(
        env.state
            .secrets()
            .unseal(&sealed, &token_context(env.site))
            .is_err()
    );
}

#[tokio::test]
async fn the_planted_secret_appears_in_no_answer_and_no_event() {
    let env = Env::new().await;
    env.fake.put(BUCKET, "Planted.txt", b"planted");
    let library = env.connected("Planted").await;
    let route = format!("/api/libraries/{}/source", library.0);
    env.source(&env.admin, library).await;
    env.patch(&env.admin, library, json!({"poll_seconds": 900}))
        .await;
    env.fake.require_keys(OTHER_KEY_ID, OTHER_SECRET);
    let (status, answer) = env
        .patch(
            &env.admin,
            library,
            json!({"access_key_id": OTHER_KEY_ID, "secret_access_key": OTHER_SECRET}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    // A failed probe with a wrong secret.
    let (status, _) = env
        .patch(
            &env.admin,
            library,
            json!({"access_key_id": OTHER_KEY_ID, "secret_access_key": WRONG_SECRET}),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let mut failed = Env::body("Failed probe");
    failed["secret_access_key"] = json!(WRONG_SECRET);
    env.connect(failed).await;
    env.call(&env.admin, Method::POST, &format!("{route}/sync"), None)
        .await;
    env.sync(library).await;
    env.source(&env.admin, library).await;

    // Every text column of the connection's rows.
    let rows: Vec<String> = sqlx::query_scalar(
        "select row_to_json(s)::text from library_sources s \
         union all select row_to_json(b)::text from source_s3 b",
    )
    .fetch_all(env.server.records_db().test_pool())
    .await
    .unwrap();
    assert!(!rows.is_empty());

    let (status, answer) = env.disconnect(&env.admin, library).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{answer}");
    let events = env.event_texts_after("source.disconnected").await;
    let seen = env.seen.lock().unwrap().clone();
    assert!(seen.len() >= 8);
    for planted in [SECRET, OTHER_SECRET, WRONG_SECRET] {
        for text in seen.iter().chain(&events).chain(&rows) {
            assert!(!text.contains(planted), "the secret leaked into {text}");
        }
    }
}

#[tokio::test]
async fn replacing_the_key_probes_first_and_a_bad_one_changes_nothing() {
    let env = Env::new().await;
    let library = env.connected("Kept key").await;
    let token = env.token(library).await;
    let secrets = env.secrets().await;
    let requests = env.fake.requests().len();
    // Both or neither.
    for half in [
        json!({"access_key_id": OTHER_KEY_ID}),
        json!({"secret_access_key": OTHER_SECRET}),
    ] {
        let (status, answer) = env.patch(&env.admin, library, half).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
        assert_eq!(answer["error"], "invalid_request");
    }
    assert_eq!(env.fake.requests().len(), requests);
    let (status, answer) = env
        .patch(
            &env.admin,
            library,
            json!({"access_key_id": KEY_ID, "secret_access_key": WRONG_SECRET}),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
    assert_eq!(answer["error"], "s3_unreachable");
    assert_eq!(answer["reason"], "keys");
    assert_eq!(env.token(library).await, token);
    assert_eq!(env.secrets().await, secrets);
    let place = env
        .server
        .records_db()
        .s3_place(library)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(place.access_key_id, KEY_ID);
}

#[tokio::test]
async fn replacing_the_key_deletes_the_old_secret_row_and_reconnects_a_not_connected_library() {
    let env = Env::new().await;
    env.fake.put(BUCKET, "Rekeyed.txt", b"rekeyed");
    let library = env.connected("Rekeyed").await;
    let old = env.token(library).await;
    let db = env.server.records_db();
    sqlx::query("update library_sources set state = 'not_connected' where library_id = $1")
        .bind(library.0)
        .execute(db.test_pool())
        .await
        .unwrap();
    env.fake.require_keys(OTHER_KEY_ID, OTHER_SECRET);
    let (status, answer) = env
        .patch(
            &env.admin,
            library,
            json!({"access_key_id": OTHER_KEY_ID, "secret_access_key": OTHER_SECRET}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(
        answer["key"]["id_ends"],
        &OTHER_KEY_ID[OTHER_KEY_ID.len() - 4..]
    );
    assert_eq!(answer["key"]["secret"], "set");
    assert!(db.get_secret(old).await.unwrap().is_none());
    let new = env.token(library).await;
    assert_ne!(new, old);
    let (_, status) = db.source_of(library).await.unwrap().unwrap();
    assert_eq!(status.state, SourceState::Connecting);
    assert_eq!(
        db.s3_place(library).await.unwrap().unwrap().access_key_id,
        OTHER_KEY_ID
    );
    env.sync(library).await;
    assert_eq!(db.live_children(library).await.unwrap().len(), 1);
}

#[tokio::test]
async fn patch_changes_the_poll_interval_and_the_budget() {
    let env = Env::new().await;
    let library = env.connected("Budgeted").await;
    let (status, answer) = env
        .patch(
            &env.admin,
            library,
            json!({
                "poll_seconds": 600,
                "budget": {
                    "list_requests": 5000,
                    "read_requests": 20000,
                    "bytes_down": 2_147_483_648_u64,
                },
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["poll_seconds"], 600);
    assert_eq!(answer["budget"]["list_requests"], 5000);
    assert_eq!(answer["budget"]["read_requests"], 20000);
    assert_eq!(answer["budget"]["bytes_down"], 2_147_483_648_u64);
    let budget = env.server.records_db().budget(library).await.unwrap();
    assert_eq!(
        (
            budget.list_requests,
            budget.read_requests,
            budget.bytes_down
        ),
        (5000, 20000, Some(2_147_483_648))
    );
    let (config, _) = env
        .server
        .records_db()
        .source_of(library)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(config.poll_seconds, 600);

    for refused in [
        json!({"poll_seconds": 5}),
        json!({"budget": {"list_requests": 0, "read_requests": 10, "bytes_down": null}}),
        json!({"budget": {"list_requests": 10}}),
        json!({"branch": "main"}),
        json!({"folder": "docs"}),
        json!({"token": "ghp_x"}),
        json!({"clear_token": true}),
        json!({"bucket": "other-bucket"}),
        json!({"prefix": "other/"}),
        json!({"endpoint": BLOCKED}),
    ] {
        let (status, answer) = env.patch(&env.admin, library, refused.clone()).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{refused}: {answer}"
        );
    }
    let place = env
        .server
        .records_db()
        .s3_place(library)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (place.endpoint.as_str(), place.bucket.as_str()),
        (ENDPOINT, BUCKET)
    );
}

#[tokio::test]
async fn the_webhook_route_is_refused_for_s3() {
    let env = Env::new().await;
    let library = env.connected("No webhook").await;
    let secrets = env.secrets().await;
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
    assert_eq!(env.secrets().await, secrets);
}

#[tokio::test]
async fn disconnect_empties_the_library_deletes_the_secret_and_sends_no_write() {
    let env = Env::new().await;
    env.fake.put(BUCKET, "One.txt", b"one");
    env.fake.put(BUCKET, "Two.txt", b"two");
    let library = env.connected("Disconnected").await;
    env.sync(library).await;
    let token = env.token(library).await;
    let db = env.server.records_db();
    assert_eq!(db.live_children(library).await.unwrap().len(), 2);
    let (status, answer) = env.disconnect(&env.admin, library).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{answer}");
    assert!(db.source_of(library).await.unwrap().is_none());
    assert!(db.s3_place(library).await.unwrap().is_none());
    assert!(db.live_children(library).await.unwrap().is_empty());
    assert!(db.get_secret(token).await.unwrap().is_none());
    assert_eq!(env.fake.count("write"), 0);
    env.event_texts_after("source.disconnected").await;
    let event = db
        .events_of_kind("source.disconnected")
        .await
        .unwrap()
        .remove(0);
    assert_eq!(event.detail, json!({"kind": "s3", "removed": 2}));
}

#[tokio::test]
async fn disconnect_during_a_long_first_listing_stops_it_and_succeeds() {
    let env = Env::new().await;
    env.fake.put(BUCKET, "Slow.txt", b"slow");
    let library = env.connected("Slow bucket").await;
    let probed = env.fake.count("list");
    env.fake.slow_list(Duration::from_secs(1));
    let syncing = tokio::spawn({
        let state = env.state.clone();
        async move { sync_once(&state, library, OffsetDateTime::UNIX_EPOCH).await }
    });
    let deadline = tokio::time::Instant::now() + common::TIMEOUT;
    while env.fake.count("list") <= probed {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let (status, answer) = env.disconnect(&env.admin, library).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{answer}");
    let _ = syncing.await.unwrap();
    let db = env.server.records_db();
    assert!(db.source_of(library).await.unwrap().is_none());
    assert!(db.live_children(library).await.unwrap().is_empty());
}

#[tokio::test]
async fn only_administrators_read_and_change_the_s3_settings() {
    let env = Env::new().await;
    let library = env.connected("In use").await;
    let someone = env.with_role("Site owner", "owner", env.site).await;
    let (status, _) = env
        .call(&someone, Method::GET, "/api/settings/s3", None)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let wanted = json!({
        "endpoints": [{
            "name": "r2",
            "url": "https://account.r2.cloudflarestorage.com:443",
            "region": "auto",
            "path_style": true,
            "allow": [],
        }],
        "list_requests": 5000,
        "read_requests": 50000,
        "bytes_down": null,
    });
    let (status, _) = env
        .call(
            &someone,
            Method::PUT,
            "/api/settings/s3",
            Some(wanted.clone()),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, answer) = env
        .call(&env.admin, Method::GET, "/api/settings/s3", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["in_use"][ENDPOINT], 1);
    assert!(answer["list_requests"].is_u64() && answer["read_requests"].is_u64());

    let (status, answer) = env
        .call(&env.admin, Method::PUT, "/api/settings/s3", Some(wanted))
        .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    // A default port is not kept: the stored address is what is signed.
    assert_eq!(
        answer["endpoints"][0]["url"],
        "https://account.r2.cloudflarestorage.com"
    );
    let (status, answer) = env
        .call(&env.admin, Method::GET, "/api/settings/s3", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["endpoints"].as_array().unwrap().len(), 1);
    assert_eq!(answer["endpoints"][0]["name"], "r2");
    assert_eq!(
        answer["endpoints"][0]["url"],
        "https://account.r2.cloudflarestorage.com"
    );
    assert_eq!(answer["endpoints"][0]["region"], "auto");
    assert_eq!(answer["endpoints"][0]["path_style"], true);
    assert_eq!(answer["list_requests"], 5000);
    assert_eq!(answer["read_requests"], 50000);
    assert!(answer["bytes_down"].is_null());
    // Removing the endpoint does not disconnect the library that uses it.
    assert_eq!(answer["in_use"][ENDPOINT], 1);
    assert!(
        env.server
            .records_db()
            .source_of(library)
            .await
            .unwrap()
            .is_some()
    );
    let events = env.event_texts_after("setting.changed").await;
    for text in events
        .iter()
        .filter(|text| text.starts_with("setting.changed"))
    {
        assert!(!text.contains("cloudflarestorage"), "{text}");
    }
}

#[tokio::test]
async fn a_bad_endpoint_is_refused_and_no_part_of_the_put_is_applied() {
    let env = Env::new().await;
    let before = env.server.records_db().setting(S3_ENDPOINTS).await.unwrap();
    let defaults = env.server.records_db().s3_settings().await.unwrap();
    let (status, answer) = env
        .call(
            &env.admin,
            Method::PUT,
            "/api/settings/s3",
            Some(json!({
                "endpoints": [
                    {"name": "good", "url": "https://s3.example.com", "region": "auto",
                     "path_style": false, "allow": []},
                    {"name": "bad", "url": "http://s3.example.org", "region": "auto",
                     "path_style": false, "allow": []},
                ],
                "list_requests": 7000,
                "read_requests": 70000,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
    assert_eq!(answer["error"], "invalid_s3_endpoint");
    assert_eq!(answer["entry"], 1);
    assert_eq!(answer["reason"], "not_https");
    let db = env.server.records_db();
    assert_eq!(db.setting(S3_ENDPOINTS).await.unwrap(), before);
    assert_eq!(db.s3_settings().await.unwrap(), defaults);

    // A bad budget after good endpoints changes nothing either.
    let (status, answer) = env
        .call(
            &env.admin,
            Method::PUT,
            "/api/settings/s3",
            Some(json!({
                "endpoints": [],
                "list_requests": 1,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
    assert_eq!(db.setting(S3_ENDPOINTS).await.unwrap(), before);

    // Two entries of one name.
    let twice = json!({"name": "twice", "url": "https://s3.example.com", "region": "auto",
                       "path_style": false, "allow": []});
    let (status, answer) = env
        .call(
            &env.admin,
            Method::PUT,
            "/api/settings/s3",
            Some(json!({"endpoints": [twice.clone(), twice]})),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
    assert_eq!(answer["entry"], 1);
    assert_eq!(answer["reason"], "name_taken");
    assert_eq!(db.setting(S3_ENDPOINTS).await.unwrap(), before);
}

#[tokio::test]
async fn http_and_private_addresses_are_refused_as_endpoints() {
    let env = Env::new().await;
    for (url, allow, reason) in [
        ("http://s3.example.com", json!([]), "not_https"),
        ("https://10.0.0.5", json!([]), "address_literal"),
        ("https://169.254.169.254", json!([]), "address_literal"),
        ("https://127.0.0.1:9000", json!([]), "address_literal"),
        ("https://[::ffff:10.0.0.1]", json!([]), "address_literal"),
        ("https://0x7f000001", json!([]), "address_literal"),
        (
            "https://user:pw@s3.example.com",
            json!([]),
            "has_credentials",
        ),
        ("https://s3.example.com/path", json!([]), "has_path"),
        ("https://s3.example.com?x=1", json!([]), "has_query"),
        ("ftp://s3.example.com", json!([]), "not_https"),
        ("https://s3.example.com", json!(["10.0.0.0/4"]), "bad_range"),
    ] {
        let (status, answer) = env
            .call(
                &env.admin,
                Method::PUT,
                "/api/settings/s3",
                Some(json!({"endpoints": [{
                    "name": "private", "url": url, "region": "auto",
                    "path_style": true, "allow": allow,
                }]})),
            )
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{url}: {answer}");
        assert_eq!(answer["error"], "invalid_s3_endpoint", "{url}");
        assert_eq!(answer["entry"], 0, "{url}");
        assert_eq!(answer["reason"], reason, "{url}");
        assert!(!answer.to_string().contains(url), "{answer}");
    }
    // An administrator may allow a private range for an address in it.
    let (status, answer) = env
        .call(
            &env.admin,
            Method::PUT,
            "/api/settings/s3",
            Some(json!({"endpoints": [{
                "name": "minio", "url": "https://10.0.0.5:9000", "region": "us-east-1",
                "path_style": true, "allow": ["10.0.0.0/8"],
            }]})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["endpoints"][0]["allow"], json!(["10.0.0.0/8"]));
}

#[tokio::test]
async fn removing_an_endpoint_stops_reads_at_once_and_the_next_listing_is_not_connected() {
    let env = Env::new().await;
    env.fake.put(BUCKET, "Withdrawn.txt", b"withdrawn");
    let library = env.connected("Withdrawn").await;
    env.sync(library).await;
    let db = env.server.records_db();
    let node = db.source_items(library).await.unwrap()[0].node;
    let (status, answer) = env
        .call(
            &env.admin,
            Method::PUT,
            "/api/settings/s3",
            Some(json!({"endpoints": []})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let (status, answer) = env
        .call(
            &env.admin,
            Method::GET,
            &format!("/api/docs/{}/content", node.0),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{answer}");
    assert_eq!(answer["reason"], "not_approved");
    assert_eq!(env.fake.count("get"), 0);
    assert_eq!(env.fake.count("head"), 0);
    let lists = env.fake.count("list");
    let _ = sync_once(&env.state, library, OffsetDateTime::UNIX_EPOCH).await;
    assert_eq!(env.fake.count("list"), lists);
    let (_, status) = db.source_of(library).await.unwrap().unwrap();
    assert_eq!(status.state, SourceState::NotConnected);
    // The library and its items stay.
    assert_eq!(db.live_children(library).await.unwrap().len(), 1);
}

#[tokio::test]
async fn no_event_holds_the_endpoint_bucket_prefix_or_key_id() {
    let env = Env::new().await;
    env.fake.put(BUCKET, "evt-prefix-77/Inside.txt", b"inside");
    let mut body = Env::body("Quiet");
    body["prefix"] = json!("evt-prefix-77/");
    let (status, answer) = env.connect(body).await;
    assert_eq!(status, StatusCode::CREATED, "{answer}");
    let library = id_of(&answer);
    env.sync(library).await;
    env.patch(&env.admin, library, json!({"poll_seconds": 900}))
        .await;
    env.fake.require_keys(OTHER_KEY_ID, OTHER_SECRET);
    let (status, answer) = env
        .patch(
            &env.admin,
            library,
            json!({"access_key_id": OTHER_KEY_ID, "secret_access_key": OTHER_SECRET}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let (status, _) = env.disconnect(&env.admin, library).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let events = env.event_texts_after("source.disconnected").await;
    assert!(
        events
            .iter()
            .any(|text| text.starts_with("source.connected"))
    );
    assert!(events.iter().any(|text| text.starts_with("source.changed")));
    for text in &events {
        for hidden in [
            ENDPOINT,
            BUCKET,
            "evt-prefix-77",
            KEY_ID,
            OTHER_KEY_ID,
            "127.0.0.1",
        ] {
            assert!(!text.contains(hidden), "{hidden} in {text}");
        }
    }
}

#[tokio::test]
async fn signed_in_users_are_given_only_the_names_of_the_approved_endpoints() {
    let env = Env::new().await;
    let someone = env.server.user("Plain user").await;
    let named = "names-endpoint-7a3";
    set_endpoints(
        &env.server,
        json!([endpoint_entry(named, "https://s3.example.test:443", &["10.0.0.0/8"])]),
    )
    .await;
    let (status, answer) = env
        .call(&someone, Method::GET, "/api/s3/endpoints", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer, json!([{ "name": named }]));
    let entries = answer.as_array().unwrap();
    for entry in entries {
        let keys: Vec<&str> = entry
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, vec!["name"], "{answer}");
    }
    let text = answer.to_string();
    for hidden in ["url", "region", "path_style", "allow", "127.0.0.1", "http"] {
        assert!(!text.contains(hidden), "{hidden} in {answer}");
    }
    let signed_out = env
        .server
        .http(None)
        .request(Method::GET, env.server.url("/api/s3/endpoints"))
        .send()
        .await
        .unwrap();
    assert_eq!(signed_out.status(), StatusCode::UNAUTHORIZED);
}
