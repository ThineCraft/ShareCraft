//! Signing in to a file server with its stored login before reading a share.
//!
//! Every test sets a fake connector, so none reaches a real file server; only the Windows-only
//! `wnet_unreachable_host_maps_to_unreachable` asks Windows, for a host under `.invalid`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::sync::Arc;

use collab_core::ids::NodeId;
use common::fake_connector::FakeConnector;
use common::fake_share::FakeShare;
use common::{TestServer, TestUser, spawn_server_config_without_workers};
use records::{NewSource, SHARE_ROOTS, SecretId, SharePath, SourceKind, SourceMode, SourceState};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use server::AppState;
use server::share_login::LoginFailure;
use server::source_sync::{SyncError, sync_once};
use time::OffsetDateTime;

const PASSWORD: &str = "Tr1cky-pass-word";
const ROOT: &str = r"\\nas\docs";
const OTHER_ROOT: &str = r"\\nas\other";
const ELSEWHERE_ROOT: &str = r"\\files\team";
const REFUSED: &str = r"The stored login for \\nas was refused. An administrator can set it again under Settings › File shares.";
const DENIED: &str = "The server's account may not read this folder, so nothing was changed.";

struct Env {
    server: TestServer,
    admin: TestUser,
    state: AppState,
    fake: FakeShare,
    connector: FakeConnector,
    library: NodeId,
}

impl Env {
    async fn new() -> Self {
        Self::with(|_| {}).await
    }

    async fn with(change: impl FnOnce(&mut server::Config)) -> Self {
        let server = spawn_server_config_without_workers(change).await;
        let admin = server.admin("Share admin").await;
        let library = common::Site::library(&server).await;
        let state = server.app_state().await;
        let fake = FakeShare::new(ROOT);
        fake.put("report.txt", b"quarterly");
        state.shares().set_fs(Arc::new(fake.clone()));
        let connector = FakeConnector::new();
        connector.install(&state);
        let db = server.records_db();
        db.set_setting(
            admin.id,
            SHARE_ROOTS,
            json!([ROOT, OTHER_ROOT, ELSEWHERE_ROOT]),
        )
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
        Self {
            server,
            admin,
            state,
            fake,
            connector,
            library,
        }
    }

    /// Stores a login straight into the records, as an earlier PUT on Windows would have.
    async fn seed(&self, server: &str, username: &str, password: &str) -> SecretId {
        let id = self
            .state
            .secret_store()
            .put(
                self.state.db(),
                password.as_bytes(),
                format!("share-login:{server}").as_bytes(),
            )
            .await
            .unwrap();
        self.server
            .records_db()
            .set_share_login(self.admin.id, server, username, id)
            .await
            .unwrap();
        id
    }

    async fn sync(&self) -> Result<(), SyncError> {
        sync_once(&self.state, self.library, OffsetDateTime::now_utc())
            .await
            .map(|_| ())
    }

    async fn ensure(&self, root: &str) -> Result<(), LoginFailure> {
        self.state
            .share_logins()
            .ensure(&self.state, &SharePath::parse(root).unwrap())
            .await
    }

    async fn status(&self) -> records::SourceStatus {
        self.server
            .records_db()
            .source_of(self.library)
            .await
            .unwrap()
            .unwrap()
            .1
    }

    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> (StatusCode, String) {
        let mut request = self
            .server
            .http(Some(&self.admin))
            .request(method, self.server.url(path));
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        let status = response.status();
        (status, response.text().await.unwrap())
    }
}

/// The `error` of an answer; only the Windows-only PUT checks read it.
#[cfg(windows)]
fn error_of(text: &str) -> String {
    serde_json::from_str::<Value>(text).unwrap()["error"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn no_login_means_no_connect() {
    let env = Env::new().await;
    env.ensure(ROOT).await.unwrap();
    env.sync().await.unwrap();
    assert!(env.connector.connects().is_empty());
    // A local or POSIX root never signs in either.
    env.ensure("/srv/share").await.unwrap();
    assert!(env.connector.connects().is_empty());
}

#[tokio::test]
async fn first_read_connects_once_per_remote() {
    let env = Env::new().await;
    env.seed("nas", "ann", PASSWORD).await;
    env.ensure(ROOT).await.unwrap();
    env.ensure(r"\\nas\docs\reports").await.unwrap();
    env.ensure(OTHER_ROOT).await.unwrap();
    env.sync().await.unwrap();
    env.sync().await.unwrap();
    assert_eq!(
        env.connector.remotes(),
        vec![ROOT.to_owned(), OTHER_ROOT.to_owned()]
    );
    let connects = env.connector.connects();
    assert!(connects.iter().all(|c| c.username == "ann"));
    assert!(connects.iter().all(|c| c.password.expose() == PASSWORD));
    assert!(env.connector.disconnects().is_empty());
    assert_eq!(env.status().await.state, SourceState::Ok);
}

#[tokio::test]
async fn refused_login_is_not_stored() {
    let env = Env::new().await;
    // A refused stored login stops the library with a plain sentence naming the server.
    env.seed("nas", "ann", PASSWORD).await;
    env.connector.always(Some(LoginFailure::Refused));
    let answer = env.sync().await;
    assert!(
        matches!(&answer, Err(SyncError::NotConnected(message)) if message == REFUSED),
        "{answer:?}"
    );
    let status = env.status().await;
    assert_eq!(status.state, SourceState::NotConnected);
    assert_eq!(status.last_error.as_deref(), Some(REFUSED));
    // Not retried in a loop: one connect, no reconnect.
    assert_eq!(env.connector.connects().len(), 1);

    #[cfg(windows)]
    {
        let db = env.server.records_db();
        let before = db.share_login("nas").await.unwrap().unwrap();
        let (status, text) = env
            .call(
                Method::PUT,
                "/api/settings/shares/logins/nas",
                Some(json!({"username": "bob", "password": "new-pass-word"})),
            )
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{text}");
        assert_eq!(error_of(&text), "login_refused");
        assert!(text.contains(r"\\nas"), "{text}");
        assert!(!text.contains("bob") && !text.contains("new-pass-word"));
        // Nothing was stored: the old login stays as it was.
        let after = db.share_login("nas").await.unwrap().unwrap();
        assert_eq!(after, before);
        assert!(
            db.events_of_kind("share_login_set")
                .await
                .unwrap()
                .iter()
                .all(|event| event.detail["username"] != "bob")
        );
        // The connect was tried with the new login, on an approved root of that server.
        let last = env.connector.connects().pop().unwrap();
        assert!(last.remote == ROOT || last.remote == OTHER_ROOT);
        assert_eq!(last.username, "bob");
    }
}

#[cfg(windows)]
#[tokio::test]
async fn conflict_answers_409_with_server_named() {
    let env = Env::new().await;
    env.connector.answer([Err(LoginFailure::Conflict)]);
    let (status, text) = env
        .call(
            Method::PUT,
            "/api/settings/shares/logins/nas",
            Some(json!({"username": "ann", "password": PASSWORD})),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{text}");
    assert_eq!(error_of(&text), "login_conflict");
    assert!(text.contains(r"\\nas"), "{text}");
    assert!(!text.contains("ann") && !text.contains(PASSWORD));
    assert!(
        env.server
            .records_db()
            .share_logins()
            .await
            .unwrap()
            .is_empty()
    );

    env.connector.answer([Err(LoginFailure::Unreachable)]);
    let (status, text) = env
        .call(
            Method::PUT,
            "/api/settings/shares/logins/nas",
            Some(json!({"username": "ann", "password": PASSWORD})),
        )
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{text}");
    assert_eq!(error_of(&text), "server_unreachable");
    assert!(text.contains(r"\\nas"), "{text}");
}

#[tokio::test]
async fn access_denied_reconnects_once_then_reports() {
    let env = Env::new().await;
    env.seed("nas", "ann", PASSWORD).await;
    env.fake.deny(ROOT);
    let answer = env.sync().await;
    assert!(
        matches!(&answer, Err(SyncError::Failed(message)) if message == DENIED),
        "{answer:?}"
    );
    // One sign-in, one forgetting of it, one sign-in again; then the refusal is reported.
    assert_eq!(
        env.connector.remotes(),
        vec![ROOT.to_owned(), ROOT.to_owned()]
    );
    assert_eq!(env.connector.disconnects(), vec![ROOT.to_owned()]);

    // Without a stored login a refusal is reported at once, with no sign-in.
    let plain = Env::new().await;
    plain.fake.deny(ROOT);
    assert!(plain.sync().await.is_err());
    assert!(plain.connector.connects().is_empty());
    assert!(plain.connector.disconnects().is_empty());
}

#[tokio::test]
async fn new_login_after_password_change_heals_without_restart() {
    let env = Env::new().await;
    env.seed("nas", "ann", PASSWORD).await;
    env.sync().await.unwrap();
    assert_eq!(env.connector.connects().len(), 1);

    // The password changes on the file server: the connection is lost and the stored login
    // is refused from then on.
    env.state.share_logins().invalidate(ROOT);
    env.connector.always(Some(LoginFailure::Refused));
    let db = env.server.records_db();
    db.requeue_source(env.library, None).await.unwrap();
    assert!(env.sync().await.is_err());
    assert_eq!(env.status().await.last_error.as_deref(), Some(REFUSED));

    // A login stored again (another front end, or the settings page) is used at the next
    // read, without a restart: the secret it uses is new.
    env.connector.always(None);
    env.seed("nas", "ann", "the-new-pass-word").await;
    env.ensure(ROOT).await.unwrap();
    let last = env.connector.connects().pop().unwrap();
    assert!(last.password.expose() == "the-new-pass-word");

    #[cfg(windows)]
    {
        // From the settings page: the library is woken and syncs again.
        env.connector.always(Some(LoginFailure::Refused));
        env.state.share_logins().invalidate(ROOT);
        db.requeue_source(env.library, None).await.unwrap();
        assert!(env.sync().await.is_err());
        assert_eq!(env.status().await.state, SourceState::NotConnected);
        env.connector.always(None);
        let (status, text) = env
            .call(
                Method::PUT,
                "/api/settings/shares/logins/nas",
                Some(json!({"username": "bob", "password": "fixed-pass-word"})),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{text}");
        assert_eq!(env.status().await.state, SourceState::Connecting);
        env.sync().await.unwrap();
        let status = env.status().await;
        assert_eq!(status.state, SourceState::Ok);
        assert_eq!(status.last_error, None);
        let last = env.connector.connects().pop().unwrap();
        assert_eq!(last.username, "bob");
    }
}

#[tokio::test]
async fn delete_disconnects_every_remote_of_server() {
    let env = Env::new().await;
    env.seed("nas", "ann", PASSWORD).await;
    env.seed("files", "fay", PASSWORD).await;
    env.ensure(ROOT).await.unwrap();
    env.ensure(OTHER_ROOT).await.unwrap();
    env.ensure(ELSEWHERE_ROOT).await.unwrap();
    assert_eq!(env.connector.connects().len(), 3);

    let (status, text) = env
        .call(Method::DELETE, "/api/settings/shares/logins/nas", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let mut gone = env.connector.disconnects();
    gone.sort();
    assert_eq!(gone, vec![ROOT.to_owned(), OTHER_ROOT.to_owned()]);

    // Removing the last root of a server removes its login and disconnects it too.
    let (status, text) = env
        .call(
            Method::PUT,
            "/api/settings/shares",
            Some(json!({"roots": ["/other-approved"]})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert!(
        env.connector
            .disconnects()
            .contains(&ELSEWHERE_ROOT.to_owned())
    );
    // With no login, nothing signs in again.
    env.ensure(ROOT).await.unwrap();
    assert_eq!(env.connector.connects().len(), 3);
}

#[tokio::test]
async fn password_reaches_connector_only() {
    let env = Env::new().await;
    env.seed("nas", "ann", PASSWORD).await;
    env.ensure(ROOT).await.unwrap();
    let connects = env.connector.connects();
    assert_eq!(connects.len(), 1);
    assert!(connects[0].password.expose() == PASSWORD);
    assert_eq!(format!("{:?}", connects[0].password), "[hidden]");

    env.state.share_logins().invalidate(ROOT);
    for failure in [
        LoginFailure::Refused,
        LoginFailure::Conflict,
        LoginFailure::Unreachable,
        LoginFailure::Other(1),
    ] {
        env.connector.always(Some(failure));
        env.server
            .records_db()
            .requeue_source(env.library, None)
            .await
            .unwrap();
        let answer = env.sync().await;
        let message = match answer {
            Err(SyncError::NotConnected(message) | SyncError::Failed(message)) => message,
            other => panic!("the sync did not fail: {}", other.is_ok()),
        };
        assert!(!message.contains(PASSWORD) && !message.contains("ann"));
        assert!(message.contains(r"\\nas"), "{message}");
        let status = env.status().await;
        assert!(!status.last_error.unwrap_or_default().contains(PASSWORD));
    }

    let (_, listed) = env.call(Method::GET, "/api/settings/shares", None).await;
    assert!(!listed.contains(PASSWORD));
    for kind in ["share_login_set", "share_login_removed", "source.synced"] {
        for event in env.server.records_db().events_of_kind(kind).await.unwrap() {
            assert!(!event.detail.to_string().contains(PASSWORD));
        }
    }

    #[cfg(windows)]
    {
        env.connector.always(Some(LoginFailure::Refused));
        let (_, text) = env
            .call(
                Method::PUT,
                "/api/settings/shares/logins/nas",
                Some(json!({"username": "ann", "password": PASSWORD})),
            )
            .await;
        assert!(!text.contains(PASSWORD));
    }
}

#[tokio::test]
async fn secret_store_failure_is_a_plain_error_and_a_retry() {
    // The secrets live with an outside provider that cannot be reached (nothing listens on the
    // discard port of the loopback address).
    let dir = tempfile::tempdir().unwrap();
    let token = dir.path().join("provider.token");
    std::fs::write(&token, "provider-bearer-token\n").unwrap();
    let settings = server::secret_provider::SecretProviderSettings::from_values(
        Some("http://127.0.0.1:9"),
        Some(token.to_str().unwrap()),
    )
    .unwrap()
    .expect("settings");
    let env = Env::with(move |config| config.secret_provider = Some(settings)).await;
    let db = env.server.records_db();
    let id = db.put_external_secret().await.unwrap();
    db.set_share_login(env.admin.id, "nas", "ann", id)
        .await
        .unwrap();

    let answer = env.sync().await;
    assert!(
        matches!(&answer, Err(SyncError::Failed(message)) if message == "The secret store could not be reached."),
        "{answer:?}"
    );
    assert!(env.connector.connects().is_empty());
    let status = env.status().await;
    // Tried again later, not stopped.
    assert_eq!(status.state, SourceState::Failed);
    assert_eq!(
        env.ensure(ROOT).await,
        Err(LoginFailure::SecretStoreUnavailable)
    );
}

#[cfg(windows)]
#[test]
fn wnet_unreachable_host_maps_to_unreachable() {
    use server::secrets::Secret;
    use server::share_login::{ShareConnector, WNetConnector};
    let answer = WNetConnector.connect(
        r"\\sharecraft-test-host.invalid\share",
        "nobody",
        &Secret::new("not-a-real-password"),
    );
    assert_eq!(answer, Err(LoginFailure::Unreachable));
}
