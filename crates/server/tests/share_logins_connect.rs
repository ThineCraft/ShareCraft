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
use std::time::Duration;

use collab_core::ids::NodeId;
use common::fake_connector::FakeConnector;
use common::fake_share::FakeShare;
use common::{TestServer, TestUser, spawn_server_config_without_workers};
use records::{NewSource, SHARE_ROOTS, SecretId, SharePath, SourceKind, SourceMode, SourceState};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use server::AppState;
use server::share_login::{LoginFailure, ShareLogins};
use server::source_sync::{SyncError, sync_once};
use time::OffsetDateTime;

const PASSWORD: &str = "Tr1cky-pass-word";
/// The password after a change on the file server.
const CHANGED_PASSWORD: &str = "the-new-pass-word";
/// A new password the file server refuses.
const REFUSED_PASSWORD: &str = "new-pass-word";
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

/// The `error` of an answer.
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

    {
        let db = env.server.records_db();
        let before = db.share_login("nas").await.unwrap().unwrap();
        let (status, text) = env
            .call(
                Method::PUT,
                "/api/settings/shares/logins/nas",
                Some(json!({"username": "bob", "password": REFUSED_PASSWORD})),
            )
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{text}");
        assert_eq!(error_of(&text), "login_refused");
        assert!(text.contains(r"\\nas"), "{text}");
        assert!(!text.contains("bob") && !text.contains(REFUSED_PASSWORD));
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
    // Not forced: other reads of the share keep their open files.
    assert_eq!(
        env.connector.disconnect_calls(),
        vec![(ROOT.to_owned(), false)]
    );

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
    env.state.share_logins().invalidate(&env.state, ROOT).await;
    env.connector.always(Some(LoginFailure::Refused));
    let db = env.server.records_db();
    db.requeue_source(env.library, None).await.unwrap();
    assert!(env.sync().await.is_err());
    assert_eq!(env.status().await.last_error.as_deref(), Some(REFUSED));

    // A login stored again (another front end, or the settings page) is used at the next
    // read, without a restart: the secret it uses is new.
    env.connector.always(None);
    env.seed("nas", "ann", CHANGED_PASSWORD).await;
    env.ensure(ROOT).await.unwrap();
    let last = env.connector.connects().pop().unwrap();
    assert!(last.password.expose() == CHANGED_PASSWORD);

    {
        // From the settings page: the library is woken and syncs again.
        env.connector.always(Some(LoginFailure::Refused));
        env.state.share_logins().invalidate(&env.state, ROOT).await;
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
    let mut gone = env.connector.disconnect_calls();
    gone.sort();
    // Forced: the login was removed, so nothing may go on reading with it.
    assert_eq!(
        gone,
        vec![(ROOT.to_owned(), true), (OTHER_ROOT.to_owned(), true)]
    );

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

    env.state.share_logins().invalidate(&env.state, ROOT).await;
    // Refused last: a refused login is not tried again until it changes.
    for failure in [
        LoginFailure::Conflict,
        LoginFailure::Unreachable,
        LoginFailure::Other(1),
        LoginFailure::Refused,
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

/// A provider that answers and holds nothing: every request is a 404.
async fn empty_provider() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buffer = [0u8; 4096];
                let _ = stream.read(&mut buffer).await;
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await;
            });
        }
    });
    (addr, task)
}

#[tokio::test]
async fn a_stored_password_the_provider_lost_says_so_and_stops() {
    let (addr, task) = empty_provider().await;
    let dir = tempfile::tempdir().unwrap();
    let token = dir.path().join("provider.token");
    std::fs::write(&token, "provider-bearer-token\n").unwrap();
    let settings = server::secret_provider::SecretProviderSettings::from_values(
        Some(&format!("http://{addr}")),
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
        matches!(&answer, Err(SyncError::NotConnected(message)) if message == r"The stored password for \\nas is gone from the secret store. Set the login again."),
        "{answer:?}"
    );
    assert!(env.connector.connects().is_empty());
    assert_eq!(env.ensure(ROOT).await, Err(LoginFailure::SecretLost));
    task.abort();
}

#[tokio::test]
async fn store_failure_after_good_sign_in_disconnects_the_remote() {
    // The secret store cannot be reached, so the new login cannot be stored after the file
    // server accepted it.
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
    let (status, text) = env
        .call(
            Method::PUT,
            "/api/settings/shares/logins/nas",
            Some(json!({"username": "ann", "password": PASSWORD})),
        )
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{text}");
    assert_eq!(error_of(&text), "secret_store_unavailable");
    assert_eq!(env.connector.remotes(), vec![ROOT.to_owned()]);
    assert!(env.connector.disconnects().contains(&ROOT.to_owned()));
    assert!(env.connector.live().is_empty());
    assert!(
        env.server
            .records_db()
            .share_logins()
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn timed_out_sign_in_is_undone_when_it_finishes() {
    let env = Env::new().await;
    env.seed("nas", "ann", PASSWORD).await;
    env.state
        .shares()
        .set_operation_timeout(Duration::from_millis(100));
    env.connector.delay(Duration::from_millis(400));
    assert_eq!(env.ensure(ROOT).await, Err(LoginFailure::Unreachable));
    // The sign-in finishes on its own thread after the wait ran out, and is undone then.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while env.connector.disconnects().is_empty() {
        assert!(tokio::time::Instant::now() < deadline, "never undone");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(env.connector.disconnects(), vec![ROOT.to_owned()]);
    assert!(env.connector.live().is_empty());
    // Nothing was remembered: the next read signs in again.
    env.connector.delay(Duration::ZERO);
    env.ensure(ROOT).await.unwrap();
    assert_eq!(env.connector.connects().len(), 2);
}

#[tokio::test]
async fn login_replaced_elsewhere_signs_out_of_every_remote_of_server() {
    let env = Env::new().await;
    env.seed("nas", "ann", PASSWORD).await;
    // Front end A (the server's own state) reads two shares of one server.
    env.ensure(ROOT).await.unwrap();
    env.ensure(OTHER_ROOT).await.unwrap();
    assert_eq!(
        env.connector.live(),
        vec![ROOT.to_owned(), OTHER_ROOT.to_owned()]
    );

    // Front end B, over the same database, replaces the login and reads with it.
    let other_connector = FakeConnector::new();
    let other = ShareLogins::default();
    other.set_connector_for_tests(Arc::new(other_connector.clone()));
    env.seed("nas", "bob", CHANGED_PASSWORD).await;
    other
        .ensure(&env.state, &SharePath::parse(ROOT).unwrap())
        .await
        .unwrap();
    assert_eq!(other_connector.connects()[0].username, "bob");

    // A's next read of either share drops both old connections first: no conflict.
    env.ensure(OTHER_ROOT).await.unwrap();
    let mut gone = env.connector.disconnects();
    gone.sort();
    assert_eq!(gone, vec![ROOT.to_owned(), OTHER_ROOT.to_owned()]);
    assert_eq!(env.connector.live(), vec![OTHER_ROOT.to_owned()]);
    let last = env.connector.connects().pop().unwrap();
    assert_eq!(last.username, "bob");
    // A first read of the other share uses the new login too.
    env.ensure(ROOT).await.unwrap();
    assert_eq!(
        env.connector.live(),
        vec![ROOT.to_owned(), OTHER_ROOT.to_owned()]
    );
}

#[tokio::test]
async fn removed_login_signs_out_at_the_next_read() {
    let env = Env::new().await;
    env.seed("nas", "ann", PASSWORD).await;
    env.ensure(ROOT).await.unwrap();
    env.ensure(OTHER_ROOT).await.unwrap();
    // Removed without this front end signing out (another front end, or a removal that raced
    // a sign-in).
    env.server
        .records_db()
        .remove_share_login("nas")
        .await
        .unwrap();
    env.ensure(ROOT).await.unwrap();
    assert!(env.connector.live().is_empty());
    let mut gone = env.connector.disconnects();
    gone.sort();
    assert_eq!(gone, vec![ROOT.to_owned(), OTHER_ROOT.to_owned()]);
    assert_eq!(env.connector.connects().len(), 2);
}

#[tokio::test]
async fn refused_login_is_not_tried_again_until_it_changes() {
    let env = Env::new().await;
    env.seed("nas", "ann", PASSWORD).await;
    env.connector.always(Some(LoginFailure::Refused));
    assert_eq!(env.ensure(ROOT).await, Err(LoginFailure::Refused));
    assert_eq!(env.connector.connects().len(), 1);
    // Every further read of the server, of any of its shares, and every sync answers refused
    // without asking the file server: each try could count toward locking the account.
    for _ in 0..3 {
        assert_eq!(env.ensure(ROOT).await, Err(LoginFailure::Refused));
        assert_eq!(env.ensure(OTHER_ROOT).await, Err(LoginFailure::Refused));
        env.server
            .records_db()
            .requeue_source(env.library, None)
            .await
            .unwrap();
        assert!(env.sync().await.is_err());
    }
    assert_eq!(env.connector.connects().len(), 1);

    // A new login is tried at once.
    env.connector.always(None);
    env.seed("nas", "ann", CHANGED_PASSWORD).await;
    env.ensure(ROOT).await.unwrap();
    assert_eq!(env.connector.connects().len(), 2);
}

#[tokio::test]
async fn forgetting_a_server_lets_its_refused_login_be_tried_again() {
    let env = Env::new().await;
    env.seed("nas", "ann", PASSWORD).await;
    env.connector.always(Some(LoginFailure::Refused));
    assert_eq!(env.ensure(ROOT).await, Err(LoginFailure::Refused));
    assert_eq!(env.ensure(ROOT).await, Err(LoginFailure::Refused));
    assert_eq!(env.connector.connects().len(), 1);
    // Signing out of the server (as removing or setting its login does) forgets the refusal.
    env.state
        .share_logins()
        .forget_server(&env.state, "nas")
        .await;
    env.connector.always(None);
    env.ensure(ROOT).await.unwrap();
    assert_eq!(env.connector.connects().len(), 2);
}

#[tokio::test]
async fn slow_sign_out_does_not_hold_up_other_servers() {
    let env = Env::new().await;
    env.seed("nas", "ann", PASSWORD).await;
    env.seed("files", "fay", PASSWORD).await;
    env.ensure(ROOT).await.unwrap();
    env.state
        .shares()
        .set_operation_timeout(Duration::from_millis(150));
    // Signing out hangs, as it does when the file server has gone.
    env.connector.disconnect_delay(Duration::from_secs(2));
    let started = tokio::time::Instant::now();
    // The login of nas is replaced: its next read signs out of the old connection first.
    env.seed("nas", "bob", CHANGED_PASSWORD).await;
    let state = env.state.clone();
    let stale = tokio::spawn(async move {
        state
            .share_logins()
            .ensure(&state, &SharePath::parse(ROOT).unwrap())
            .await
    });
    let deadline = started + Duration::from_secs(5);
    while env.connector.disconnects().is_empty() {
        assert!(tokio::time::Instant::now() < deadline, "never signed out");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // Another server's read waits no longer than the bounded sign-out.
    env.ensure(ELSEWHERE_ROOT).await.unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "the other server waited {:?}",
        started.elapsed()
    );
    // The read of nas itself is not held up beyond the bound either.
    let _ = stale.await.unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "the read of nas waited {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_read_while_a_login_is_set_waits_and_uses_the_new_one() {
    let env = Env::new().await;
    env.seed("nas", "ann", PASSWORD).await;
    env.ensure(ROOT).await.unwrap();
    // Signing in takes a while, so a read of another share of the server comes in while the
    // settings page signs in with the new login.
    env.connector.delay(Duration::from_millis(300));
    let (put, read) = tokio::join!(
        env.call(
            Method::PUT,
            "/api/settings/shares/logins/nas",
            Some(json!({"username": "bob", "password": CHANGED_PASSWORD})),
        ),
        async {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            while env.connector.connects().len() < 2 {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the PUT never signed in"
                );
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            env.ensure(OTHER_ROOT).await
        }
    );
    // Neither met the other's connection: the read waited, then signed in with the new login.
    assert_eq!(put.0, StatusCode::OK, "{}", put.1);
    assert_eq!(read, Ok(()));
    let connects = env.connector.connects();
    assert_eq!(connects.len(), 3);
    assert!(connects[1..].iter().all(|c| c.username == "bob"));
    assert_eq!(
        env.connector.live(),
        vec![ROOT.to_owned(), OTHER_ROOT.to_owned()]
    );
}
/// The node of the share item at `path`, after a sync.
#[cfg(windows)]
async fn node_of(env: &Env, path: &str) -> NodeId {
    env.server
        .records_db()
        .source_items(env.library)
        .await
        .unwrap()
        .into_iter()
        .find(|item| item.repository_path == path)
        .unwrap_or_else(|| panic!("no source item at {path}"))
        .node
}

/// The share's own paths are Windows paths, so reading one file needs Windows.
#[cfg(windows)]
#[tokio::test]
async fn a_denied_file_under_a_readable_root_keeps_the_connection() {
    let env = Env::new().await;
    env.fake.put("second.txt", b"second");
    env.seed("nas", "ann", PASSWORD).await;
    env.sync().await.unwrap();
    assert_eq!(env.connector.connects().len(), 1);

    // One file the account may not read, under a root it may: the connection is not the
    // fault, so it is kept (other reads of the share go on using it).
    let node = node_of(&env, "report.txt").await;
    env.fake.deny(r"\\nas\docs\report.txt");
    let answer = server::external::materialise(&env.state, node).await;
    assert!(
        matches!(answer, Err(server::external::ExternalError::Denied)),
        "not denied"
    );
    assert!(env.connector.disconnects().is_empty());
    assert_eq!(env.connector.connects().len(), 1);

    // The root refused too: the connection may have gone stale, so it is signed in again once.
    let node = node_of(&env, "second.txt").await;
    env.fake.deny(r"\\nas\docs\second.txt");
    env.fake.deny(ROOT);
    let answer = server::external::materialise(&env.state, node).await;
    assert!(
        matches!(answer, Err(server::external::ExternalError::Denied)),
        "not denied"
    );
    // Not forced: other reads of the share keep their open files.
    assert_eq!(
        env.connector.disconnect_calls(),
        vec![(ROOT.to_owned(), false)]
    );
    assert_eq!(env.connector.connects().len(), 2);
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
