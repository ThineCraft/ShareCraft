//! The outside secret provider: secrets kept in a cloud key vault adapter that several front
//! ends share. An in-process fake serves the provider contract on a loopback port.
//!
//! Two stores over one fake provider and one database stand for two front ends.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use common::{TestServer, spawn_server_config_without_workers};
use hmac::{Hmac, KeyInit, Mac};
use records::test_support::TestDb;
use records::{NewSource, SecretId, SourceChange, SourceKind, SourceMode};
use reqwest::Method;
use server::secret_provider::{SecretProvider, SecretProviderSettings, provider_name};
use server::secret_store::SecretStore;
use server::secrets::{SecretBox, SecretError};
use sha2::Sha256;
use sqlx::Connection;

const TOKEN: &str = "provider-bearer-token-4711";
const VALUE: &[u8] = b"hunter2-but-longer-9921";

/// What the fake provider holds and has been asked.
#[derive(Default)]
struct Inner {
    /// Name to the base64 text the provider was given.
    values: HashMap<String, String>,
    /// Every request: method, name.
    log: Vec<(String, String)>,
    /// Answers every request 503.
    down: bool,
    /// Answers `DELETE` 503.
    delete_down: bool,
    /// Contract violations seen (a missing or wrong header, a bad name).
    violations: Vec<String>,
}

/// A fake provider on a loopback port. It stops when dropped.
struct FakeProvider {
    inner: Arc<Mutex<Inner>>,
    addr: SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeProvider {
    fn drop(&mut self) {
        self.task.abort();
    }
}

type Shared = Arc<Mutex<Inner>>;

fn contract_name(name: &str) -> bool {
    name.strip_prefix("sharecraft-").is_some_and(|hex| {
        hex.len() == 32
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

fn check(inner: &mut Inner, method: &str, name: &str, headers: &HeaderMap) -> Option<Response> {
    inner.log.push((method.to_owned(), name.to_owned()));
    if !contract_name(name) {
        inner.violations.push(format!("a bad name: {name}"));
    }
    let authorised = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == format!("Bearer {TOKEN}"));
    if !authorised {
        return Some(StatusCode::UNAUTHORIZED.into_response());
    }
    let json = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"));
    if method == "PUT" && !json {
        inner
            .violations
            .push("PUT without a JSON content type".to_owned());
    }
    if inner.down {
        return Some(StatusCode::SERVICE_UNAVAILABLE.into_response());
    }
    None
}

async fn put_value(
    State(shared): State<Shared>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let mut inner = shared.lock().unwrap();
    if let Some(answer) = check(&mut inner, "PUT", &name, &headers) {
        return answer;
    }
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Some(value) = json["value"].as_str() else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    inner.values.insert(name, value.to_owned());
    StatusCode::NO_CONTENT.into_response()
}

async fn get_value(
    State(shared): State<Shared>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    let mut inner = shared.lock().unwrap();
    if let Some(answer) = check(&mut inner, "GET", &name, &headers) {
        return answer;
    }
    match inner.values.get(&name) {
        Some(value) => axum::Json(serde_json::json!({ "value": value })).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn delete_value(
    State(shared): State<Shared>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    let mut inner = shared.lock().unwrap();
    if let Some(answer) = check(&mut inner, "DELETE", &name, &headers) {
        return answer;
    }
    if inner.delete_down {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    match inner.values.remove(&name) {
        Some(_) => StatusCode::NO_CONTENT.into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

impl FakeProvider {
    async fn start() -> FakeProvider {
        let inner: Shared = Arc::default();
        let app = Router::new()
            .route(
                "/v1/secrets/{name}",
                get(get_value).put(put_value).delete(delete_value),
            )
            .with_state(Arc::clone(&inner));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        FakeProvider { inner, addr, task }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn settings(&self) -> SecretProviderSettings {
        settings_for(&self.url())
    }

    fn provider(&self) -> SecretProvider {
        SecretProvider::new(&self.settings()).unwrap()
    }

    fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.inner.lock().unwrap().values.keys().cloned().collect();
        names.sort();
        names
    }

    fn requests(&self, method: &str) -> usize {
        self.inner
            .lock()
            .unwrap()
            .log
            .iter()
            .filter(|(m, _)| m == method)
            .count()
    }

    fn set_down(&self, down: bool) {
        self.inner.lock().unwrap().down = down;
    }

    fn set_delete_down(&self, down: bool) {
        self.inner.lock().unwrap().delete_down = down;
    }

    fn violations(&self) -> Vec<String> {
        self.inner.lock().unwrap().violations.clone()
    }
}

/// Settings for `url` with the right token, read from a token file as the server does.
fn settings_for(url: &str) -> SecretProviderSettings {
    settings_with_token(url, TOKEN)
}

fn settings_with_token(url: &str, token: &str) -> SecretProviderSettings {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("provider.token");
    std::fs::write(&file, format!("{token}\n")).unwrap();
    SecretProviderSettings::from_values(Some(url), Some(file.to_str().unwrap()))
        .unwrap()
        .expect("settings")
}

/// A front end: its own key file, sharing `db` and the provider.
fn front_end(provider: Option<SecretProvider>) -> (SecretStore, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let mut store = SecretStore::local(SecretBox::open_or_create(dir.path()).unwrap());
    if let Some(provider) = provider {
        store = store.with_provider(provider);
    }
    (store, dir)
}

async fn count_secrets(db: &TestDb) -> i64 {
    let mut conn = sqlx::PgConnection::connect(&db.url()).await.unwrap();
    sqlx::query_scalar("select count(*) from secrets")
        .fetch_one(&mut conn)
        .await
        .unwrap()
}

#[tokio::test]
async fn put_and_get_through_provider() {
    let fake = FakeProvider::start().await;
    let db = TestDb::create().await;
    let (store, _dir) = front_end(Some(fake.provider()));
    let id = store.put(&db.db, VALUE, b"ctx").await.unwrap();
    let got = store.get(&db.db, id, b"ctx").await.unwrap();
    assert_eq!(got.as_deref(), Some(VALUE));
    // The row holds no ciphertext: the vault protects the value.
    assert!(matches!(
        db.db.get_stored_secret(id).await.unwrap(),
        Some(records::StoredSecret::External)
    ));
    assert!(fake.violations().is_empty(), "{:?}", fake.violations());
}

#[tokio::test]
async fn values_reach_provider_under_contract_name() {
    let fake = FakeProvider::start().await;
    let db = TestDb::create().await;
    let (store, _dir) = front_end(Some(fake.provider()));
    let id = store.put(&db.db, VALUE, b"ctx").await.unwrap();
    let names = fake.names();
    assert_eq!(names, vec![provider_name(id)]);
    assert!(contract_name(&names[0]), "{}", names[0]);
    assert_eq!(names[0], format!("sharecraft-{}", id.0.simple()));
    let held = fake.inner.lock().unwrap().values[&names[0]].clone();
    assert_eq!(STANDARD.decode(held).unwrap(), VALUE);
    assert!(fake.violations().is_empty(), "{:?}", fake.violations());
}

#[tokio::test]
async fn second_store_reads_after_cache_expiry() {
    let fake = FakeProvider::start().await;
    let db = TestDb::create().await;
    let (first, _d1) = front_end(Some(fake.provider()));
    let (second, _d2) = front_end(Some(fake.provider()));
    let id = first.put(&db.db, VALUE, b"ctx").await.unwrap();

    // The other front end reads it through the provider, then from its cache.
    assert_eq!(
        second.get(&db.db, id, b"ctx").await.unwrap().as_deref(),
        Some(VALUE)
    );
    assert_eq!(fake.requests("GET"), 1);
    assert_eq!(
        second.get(&db.db, id, b"ctx").await.unwrap().as_deref(),
        Some(VALUE)
    );
    assert_eq!(
        fake.requests("GET"),
        1,
        "the second read came from the cache"
    );

    // The value changes at the provider; the cache holds until it expires.
    let name = provider_name(id);
    fake.inner
        .lock()
        .unwrap()
        .values
        .insert(name, STANDARD.encode(b"a-changed-value"));
    assert_eq!(
        second.get(&db.db, id, b"ctx").await.unwrap().as_deref(),
        Some(VALUE)
    );
    second.set_cache_ttl_for_tests(Duration::from_millis(40));
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_eq!(
        second.get(&db.db, id, b"ctx").await.unwrap().as_deref(),
        Some(&b"a-changed-value"[..])
    );
    assert_eq!(fake.requests("GET"), 2);

    // A forget made by this process drops its own entry at once.
    second.forget(&db.db, id).await;
    assert_eq!(second.get(&db.db, id, b"ctx").await, Ok(None));
}

#[tokio::test]
async fn provider_down_is_unavailable_and_put_leaves_no_row() {
    let fake = FakeProvider::start().await;
    let db = TestDb::create().await;
    let (store, _dir) = front_end(Some(fake.provider()));
    fake.set_down(true);
    let result = store.put(&db.db, VALUE, b"ctx").await;
    assert!(
        matches!(&result, Err(SecretError::Unavailable(s)) if s == "The secret store could not be reached."),
        "{result:?}"
    );
    assert_eq!(count_secrets(&db).await, 0);
    assert!(fake.names().is_empty());

    // A provider nobody listens on is the same.
    let closed = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap()
    };
    let (lonely, _d) = front_end(Some(
        SecretProvider::new(&settings_for(&format!("http://{closed}"))).unwrap(),
    ));
    let result = lonely.put(&db.db, VALUE, b"ctx").await;
    assert!(matches!(result, Err(SecretError::Unavailable(_))));
    assert_eq!(count_secrets(&db).await, 0);

    // A read while it is down is Unavailable, not "gone".
    fake.set_down(false);
    let id = store.put(&db.db, VALUE, b"ctx").await.unwrap();
    let (reader, _d2) = front_end(Some(fake.provider()));
    fake.set_down(true);
    assert!(matches!(
        reader.get(&db.db, id, b"ctx").await,
        Err(SecretError::Unavailable(_))
    ));
}

/// A server with the provider configured, and a library with a webhook secret kept there.
async fn webhook_env(
    fake: &FakeProvider,
) -> (
    TestServer,
    collab_core::ids::NodeId,
    String,
    common::TestUser,
) {
    let settings = fake.settings();
    let server = spawn_server_config_without_workers(|c| c.secret_provider = Some(settings)).await;
    let admin = server.admin("Ada Admin").await;
    let library = common::Site::library(&server).await;
    let state = server.app_state().await;
    let db = server.records_db();
    let secret = "whsec-test-secret";
    let id = state
        .secret_store()
        .put(&db, secret.as_bytes(), b"webhook")
        .await
        .unwrap();
    connect(&db, library, admin.id, None, Some(id)).await;
    (server, library, secret.to_owned(), admin)
}

async fn connect(
    db: &records::Db,
    library: collab_core::ids::NodeId,
    by: collab_core::ids::UserId,
    token: Option<SecretId>,
    webhook: Option<SecretId>,
) {
    db.connect_source(&NewSource {
        library,
        kind: SourceKind::Github,
        owner: "octo-org".to_owned(),
        repo: "docs".to_owned(),
        branch: "main".to_owned(),
        folder: String::new(),
        location: None,
        mode: SourceMode::ReadOnly,
        token,
        poll_seconds: 300,
        connected_by: by,
    })
    .await
    .unwrap();
    if webhook.is_some() {
        db.change_source(
            library,
            &SourceChange {
                branch: None,
                folder: None,
                poll_seconds: None,
                token: None,
                webhook_secret: Some(webhook),
            },
        )
        .await
        .unwrap();
    }
}

fn signature(secret: &str, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body);
    let hex: String = mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("sha256={hex}")
}

async fn deliver(
    server: &TestServer,
    library: collab_core::ids::NodeId,
    secret: &str,
) -> StatusCode {
    let body = b"{\"zen\":\"x\"}";
    let response = reqwest::Client::new()
        .post(server.url(&format!("/hooks/github/{}", library.0)))
        .header("x-github-event", "ping")
        .header("content-type", "application/json")
        .header("x-hub-signature-256", signature(secret, body))
        .body(body.to_vec())
        .send()
        .await
        .unwrap();
    response.status()
}

#[tokio::test]
async fn webhook_answers_503_when_provider_down() {
    let fake = FakeProvider::start().await;
    let (server, library, secret, _admin) = webhook_env(&fake).await;
    assert_eq!(deliver(&server, library, &secret).await, StatusCode::OK);

    // Nothing is cached by the put, and the provider stays down: GitHub should deliver again.
    // (A fresh server state has an empty cache; the first answer above cached the value, so
    // expire it.)
    server
        .app_state()
        .await
        .secret_store()
        .set_cache_ttl_for_tests(Duration::ZERO);
    fake.set_down(true);
    assert_eq!(
        deliver(&server, library, &secret).await,
        StatusCode::SERVICE_UNAVAILABLE
    );
    // Nothing was deleted: the provider back, the same delivery is accepted.
    fake.set_down(false);
    assert_eq!(deliver(&server, library, &secret).await, StatusCode::OK);
}

#[tokio::test]
async fn forget_marks_then_sweep_deletes_at_provider() {
    let fake = FakeProvider::start().await;
    let db = TestDb::create().await;
    let (store, _dir) = front_end(Some(fake.provider()));
    let id = store.put(&db.db, VALUE, b"ctx").await.unwrap();
    store.forget(&db.db, id).await;

    // Marked, not yet gone from the provider; reads already say gone.
    assert_eq!(db.db.forgotten_secrets(10).await.unwrap(), vec![id]);
    assert_eq!(fake.names(), vec![provider_name(id)]);
    assert_eq!(store.get(&db.db, id, b"ctx").await, Ok(None));
    let (other, _d) = front_end(Some(fake.provider()));
    assert_eq!(other.get(&db.db, id, b"ctx").await, Ok(None));

    assert_eq!(store.sweep_forgotten(&db.db).await, 1);
    assert!(fake.names().is_empty());
    assert_eq!(count_secrets(&db).await, 0);
    assert_eq!(store.sweep_forgotten(&db.db).await, 0);
}

#[tokio::test]
async fn sweep_retries_after_failed_delete() {
    let fake = FakeProvider::start().await;
    let db = TestDb::create().await;
    let (store, _dir) = front_end(Some(fake.provider()));
    let id = store.put(&db.db, VALUE, b"ctx").await.unwrap();
    store.forget(&db.db, id).await;

    fake.set_delete_down(true);
    assert_eq!(store.sweep_forgotten(&db.db).await, 0);
    assert_eq!(db.db.forgotten_secrets(10).await.unwrap(), vec![id]);
    assert_eq!(fake.names(), vec![provider_name(id)]);

    fake.set_delete_down(false);
    assert_eq!(store.sweep_forgotten(&db.db).await, 1);
    assert!(fake.names().is_empty());
    assert!(db.db.forgotten_secrets(10).await.unwrap().is_empty());

    // A value the provider has already lost (404) still lets the row go.
    let lost = store.put(&db.db, VALUE, b"ctx").await.unwrap();
    fake.inner.lock().unwrap().values.clear();
    store.forget(&db.db, lost).await;
    assert_eq!(store.sweep_forgotten(&db.db).await, 1);
    assert_eq!(count_secrets(&db).await, 0);
}

#[tokio::test]
async fn site_purge_forgets_external_secrets() {
    let fake = FakeProvider::start().await;
    let (server, library, _secret, admin) = webhook_env(&fake).await;
    let state = server.app_state().await;
    let db = server.records_db();
    let node = db.node(library).await.unwrap().unwrap();
    let site = node.site;
    let (config, _) = db.source_of(library).await.unwrap().unwrap();
    let hook = config.webhook_secret.unwrap();
    assert_eq!(fake.names(), vec![provider_name(hook)]);

    let http = server.http(Some(&admin));
    let response = http
        .request(
            Method::DELETE,
            server.url(&format!("/api/sites/{}", site.0)),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let response = http
        .request(
            Method::DELETE,
            server.url(&format!("/api/admin/sites/{}", site.0)),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    assert!(db.source_of(library).await.unwrap().is_none());
    // The row is marked, and the provider still holds the value until the sweep.
    assert_eq!(db.forgotten_secrets(10).await.unwrap(), vec![hook]);
    assert_eq!(fake.names(), vec![provider_name(hook)]);
    assert_eq!(state.secret_store().sweep_forgotten(&db).await, 1);
    assert!(fake.names().is_empty());
    assert!(db.forgotten_secrets(10).await.unwrap().is_empty());
}

#[tokio::test]
async fn old_local_rows_still_open_with_provider_configured() {
    let fake = FakeProvider::start().await;
    let db = TestDb::create().await;
    let dir = tempfile::tempdir().unwrap();
    let key = || SecretBox::open_or_create(dir.path()).unwrap();
    let old = SecretStore::local(key());
    let id = old.put(&db.db, VALUE, b"ctx").await.unwrap();

    let with_provider = SecretStore::local(key()).with_provider(fake.provider());
    assert_eq!(
        with_provider
            .get(&db.db, id, b"ctx")
            .await
            .unwrap()
            .as_deref(),
        Some(VALUE)
    );
    assert_eq!(
        with_provider.get(&db.db, id, b"other").await,
        Err(SecretError::WrongKey)
    );
    assert_eq!(
        fake.requests("GET"),
        0,
        "a local row never goes to the provider"
    );
    with_provider.forget(&db.db, id).await;
    assert_eq!(count_secrets(&db).await, 0);
    assert!(fake.requests("DELETE") == 0);

    // The other way: an outside row on a server with no provider is Unavailable.
    let outside = with_provider.put(&db.db, VALUE, b"ctx").await.unwrap();
    let (bare, _d) = front_end(None);
    let result = bare.get(&db.db, outside, b"ctx").await;
    assert_eq!(
        result,
        Err(SecretError::Unavailable(
            "this server has no secret provider configured".to_owned()
        ))
    );
}

fn token_file(dir: &tempfile::TempDir, text: &str) -> String {
    let file = dir.path().join("t.token");
    std::fs::write(&file, text).unwrap();
    file.to_str().unwrap().to_owned()
}

#[test]
fn from_env_refuses_plain_http_to_non_loopback() {
    let dir = tempfile::tempdir().unwrap();
    let file = token_file(&dir, "tok\n");
    for bad in [
        "http://vault.example.com",
        "http://10.0.0.5:8200",
        "http://192.168.1.2",
        "ftp://vault.example.com",
        "not a url",
        "https://user:pass@vault.example.com",
    ] {
        let result = SecretProviderSettings::from_values(Some(bad), Some(&file));
        let sentence = result.expect_err(bad);
        assert!(
            sentence.contains("SHARECRAFT_SECRET_PROVIDER"),
            "{sentence}"
        );
        assert!(!sentence.contains("user:pass"), "{sentence}");
    }
    for good in [
        "https://vault.example.com",
        "https://vault.example.com:8443/base",
        "http://127.0.0.1:8200",
        "http://[::1]:8200",
        "http://localhost:8200",
    ] {
        let settings = SecretProviderSettings::from_values(Some(good), Some(&file))
            .unwrap_or_else(|e| panic!("{good}: {e}"));
        assert!(settings.is_some(), "{good}");
    }
}

#[test]
fn from_env_refuses_url_without_token_file() {
    let dir = tempfile::tempdir().unwrap();
    let file = token_file(&dir, "tok\n");
    assert_eq!(
        SecretProviderSettings::from_values(None, None)
            .unwrap()
            .map(|_| ()),
        None
    );
    let no_file = SecretProviderSettings::from_values(Some("https://v.example.com"), None);
    assert!(
        no_file
            .unwrap_err()
            .contains("SHARECRAFT_SECRET_PROVIDER_TOKEN_FILE")
    );
    let no_url = SecretProviderSettings::from_values(None, Some(&file));
    assert!(no_url.unwrap_err().contains("SHARECRAFT_SECRET_PROVIDER"));
    // A missing or empty token file stops the start, too, without showing a token.
    let missing = dir.path().join("none.token");
    let result =
        SecretProviderSettings::from_values(Some("https://v.example.com"), missing.to_str());
    assert!(result.is_err());
    let empty = token_file(&dir, "  \n");
    let result = SecretProviderSettings::from_values(Some("https://v.example.com"), Some(&empty));
    assert!(result.is_err());
}

#[tokio::test]
async fn no_value_in_debug_or_errors() {
    let fake = FakeProvider::start().await;
    let db = TestDb::create().await;
    let provider = fake.provider();
    let settings = fake.settings();
    let (store, _dir) = front_end(Some(fake.provider()));
    let id = store.put(&db.db, VALUE, b"ctx").await.unwrap();

    let value = std::str::from_utf8(VALUE).unwrap();
    let mut shown = vec![
        format!("{provider:?}"),
        format!("{settings:?}"),
        format!("{store:?}"),
    ];

    // A wrong token is refused by the provider: Unavailable, naming nothing.
    let wrong = SecretProvider::new(&settings_with_token(&fake.url(), "wrong-token-0000")).unwrap();
    let put = wrong.put(&provider_name(id), VALUE).await;
    let get = wrong.get(&provider_name(id)).await;
    let delete = wrong.delete(&provider_name(id)).await;
    for result in [&put, &delete] {
        assert!(
            matches!(result, Err(SecretError::Unavailable(_))),
            "{result:?}"
        );
    }
    assert!(matches!(get, Err(SecretError::Unavailable(_))), "{get:?}");
    shown.push(format!("{put:?} {get:?} {delete:?}"));
    for e in [put.unwrap_err(), get.unwrap_err(), delete.unwrap_err()] {
        shown.push(e.to_string());
    }

    // Down: the same.
    fake.set_down(true);
    let (reader, _d) = front_end(Some(fake.provider()));
    let down = reader.get(&db.db, id, b"ctx").await.unwrap_err();
    shown.push(format!("{down:?} {down}"));
    let down = reader.put(&db.db, VALUE, b"ctx").await.unwrap_err();
    shown.push(format!("{down:?} {down}"));

    for text in &shown {
        assert!(!text.contains(value), "a value is shown: {text}");
        assert!(!text.contains(TOKEN), "the token is shown: {text}");
        assert!(
            !text.contains("wrong-token-0000"),
            "a token is shown: {text}"
        );
        assert!(
            !text.contains(&STANDARD.encode(VALUE)),
            "an encoding is shown"
        );
    }
    assert!(shown[1].contains("[hidden]"));
}
