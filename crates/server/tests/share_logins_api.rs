//! The stored login of a file server: one per server with an approved UNC root.
//!
//! A UNC root can only be used on Windows, so a login is stored only there; on other systems
//! the same request is answered `logins_windows_only` once every other rule has been checked.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use common::{TestServer, TestUser, spawn_server};
use records::{SHARE_ROOTS, SecretId};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

const PASSWORD: &str = "Tr1cky-pass-word";
const NAS_ROOT: &str = r"\\nas\docs";

struct Env {
    server: TestServer,
    admin: TestUser,
}

impl Env {
    async fn new() -> Self {
        let server = spawn_server().await;
        let admin = server.admin("Share admin").await;
        // Signing in is faked: no test reaches a real file server.
        common::fake_connector::FakeConnector::new().install(&server.app_state().await);
        server
            .records_db()
            .set_setting(admin.id, SHARE_ROOTS, json!([NAS_ROOT]))
            .await
            .unwrap();
        Self { server, admin }
    }

    async fn call(
        &self,
        user: &TestUser,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, String) {
        let mut request = self
            .server
            .http(Some(user))
            .request(method, self.server.url(path));
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        let status = response.status();
        (status, response.text().await.unwrap())
    }

    async fn put(&self, server: &str, body: Value) -> (StatusCode, String) {
        self.call(
            &self.admin,
            Method::PUT,
            &format!("/api/settings/shares/logins/{server}"),
            Some(body),
        )
        .await
    }

    async fn delete(&self, server: &str) -> (StatusCode, String) {
        self.call(
            &self.admin,
            Method::DELETE,
            &format!("/api/settings/shares/logins/{server}"),
            None,
        )
        .await
    }

    /// Stores a login straight into the records, as an earlier PUT on Windows would have.
    async fn seed(&self, server: &str, username: &str, password: &[u8]) -> SecretId {
        let state = self.server.app_state().await;
        let id = state
            .secret_store()
            .put(
                state.db(),
                password,
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
}

fn error_of(text: &str) -> String {
    serde_json::from_str::<Value>(text).unwrap()["error"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn non_admin_gets_403() {
    let env = Env::new().await;
    let user = env.server.user("Plain user").await;
    let (status, _) = env
        .call(
            &user,
            Method::PUT,
            "/api/settings/shares/logins/nas",
            Some(json!({"username": "ann", "password": PASSWORD})),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = env
        .call(
            &user,
            Method::DELETE,
            "/api/settings/shares/logins/nas",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn login_needs_an_approved_root_on_server() {
    let env = Env::new().await;
    let (status, text) = env
        .put(
            "elsewhere",
            json!({"username": "ann", "password": PASSWORD}),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{text}");
    assert_eq!(error_of(&text), "no_root_on_server");
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
async fn bad_username_and_password_refused() {
    let env = Env::new().await;
    let long_user = "u".repeat(257);
    let long_password = "p".repeat(257);
    for body in [
        json!({"username": "", "password": PASSWORD}),
        json!({"username": long_user, "password": PASSWORD}),
        json!({"username": "a/b", "password": PASSWORD}),
        json!({"username": "a\nb", "password": PASSWORD}),
        json!({"username": "ann", "password": ""}),
        json!({"username": "ann", "password": long_password}),
        json!({"username": "ann", "password": "a\u{0}b"}),
        json!({"username": "ann"}),
    ] {
        let (status, text) = env.put("nas", body).await;
        assert_eq!(status.as_u16() / 100, 4, "{text}");
        assert_ne!(error_of(&text), "logins_windows_only");
        assert_ne!(status, StatusCode::FORBIDDEN);
        assert!(!text.contains(PASSWORD));
    }
    let (status, text) = env
        .put("nas", json!({"username": "", "password": PASSWORD}))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error_of(&text), "invalid_request");
    // The server in the route must be a lower-case host name.
    for server in ["NAS", "bad%20host", "a..b", "-nas"] {
        let (status, text) = env
            .put(server, json!({"username": "ann", "password": PASSWORD}))
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{server}: {text}");
        assert_eq!(error_of(&text), "invalid_request");
    }
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
async fn answer_never_contains_password() {
    let env = Env::new().await;
    let (status, text) = env
        .put("nas", json!({"username": "ann", "password": PASSWORD}))
        .await;
    if cfg!(windows) {
        assert_eq!(status, StatusCode::OK, "{text}");
        let answer: Value = serde_json::from_str(&text).unwrap();
        let logins = answer["logins"].as_array().unwrap();
        assert_eq!(logins.len(), 1);
        assert_eq!(logins[0]["server"], "nas");
        assert_eq!(logins[0]["username"], "ann");
        assert!(logins[0]["set_at"].is_string());
        assert!(logins[0].get("password").is_none());
    } else {
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{text}");
        assert_eq!(error_of(&text), "logins_windows_only");
    }
    assert!(!text.contains(PASSWORD));

    let (_, listed) = env
        .call(&env.admin, Method::GET, "/api/settings/shares", None)
        .await;
    assert!(!listed.contains(PASSWORD));
    assert!(listed.contains("\"logins\""));

    let db = env.server.records_db();
    for kind in ["share_login_set", "share_login_removed", "setting.changed"] {
        for event in db.events_of_kind(kind).await.unwrap() {
            assert!(!event.detail.to_string().contains(PASSWORD));
        }
    }
    if cfg!(windows) {
        let events = db.events_of_kind("share_login_set").await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].detail,
            json!({"server": "nas", "username": "ann"})
        );
    }
}

#[tokio::test]
async fn listing_shows_a_stored_login_without_its_password() {
    let env = Env::new().await;
    env.seed("nas", "ann", PASSWORD.as_bytes()).await;
    let (status, text) = env
        .call(&env.admin, Method::GET, "/api/settings/shares", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!text.contains(PASSWORD));
    let answer: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(answer["logins"][0]["server"], "nas");
    assert_eq!(answer["logins"][0]["username"], "ann");
}

#[tokio::test]
async fn removing_a_login_forgets_its_secret() {
    let env = Env::new().await;
    let id = env.seed("nas", "ann", PASSWORD.as_bytes()).await;
    let (status, text) = env.delete("nas").await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(
        serde_json::from_str::<Value>(&text).unwrap()["logins"],
        json!([])
    );
    let db = env.server.records_db();
    assert!(db.share_logins().await.unwrap().is_empty());
    assert!(db.get_stored_secret(id).await.unwrap().is_none());
    let events = db.events_of_kind("share_login_removed").await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].actor, Some(env.admin.id));
    assert_eq!(
        events[0].detail,
        json!({"server": "nas", "username": "ann"})
    );
    // Removing what is not there is not an error.
    let (status, _) = env.delete("nas").await;
    assert_eq!(status, StatusCode::OK);
}

#[cfg(windows)]
#[tokio::test]
async fn replacing_a_login_forgets_the_old_secret() {
    let env = Env::new().await;
    let (status, text) = env
        .put("nas", json!({"username": "ann", "password": PASSWORD}))
        .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let db = env.server.records_db();
    let old = db
        .share_login("nas")
        .await
        .unwrap()
        .unwrap()
        .password_secret;
    assert!(db.get_stored_secret(old).await.unwrap().is_some());

    let (status, text) = env
        .put(
            "nas",
            json!({"username": "bob", "password": "another-pass"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let new = db.share_login("nas").await.unwrap().unwrap();
    assert_eq!(new.username, "bob");
    assert_ne!(new.password_secret, old);
    assert!(db.get_stored_secret(old).await.unwrap().is_none());
    assert!(
        db.get_stored_secret(new.password_secret)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(db.share_logins().await.unwrap().len(), 1);
}

#[tokio::test]
async fn removing_last_root_removes_login() {
    let env = Env::new().await;
    let id = env.seed("nas", "ann", PASSWORD.as_bytes()).await;
    let (status, text) = env
        .call(
            &env.admin,
            Method::PUT,
            "/api/settings/shares",
            Some(json!({"roots": ["/other-approved"]})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(
        serde_json::from_str::<Value>(&text).unwrap()["logins"],
        json!([])
    );
    let db = env.server.records_db();
    assert!(db.share_logins().await.unwrap().is_empty());
    assert!(db.get_stored_secret(id).await.unwrap().is_none());
    let events = db.events_of_kind("share_login_removed").await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].detail,
        json!({"server": "nas", "username": "ann"})
    );
}

#[tokio::test]
async fn a_root_change_that_keeps_the_server_keeps_its_login() {
    let env = Env::new().await;
    env.seed("nas", "ann", PASSWORD.as_bytes()).await;
    let (status, text) = env
        .call(
            &env.admin,
            Method::PUT,
            "/api/settings/shares",
            Some(json!({"roots": [r"\\NAS\other", "/other-approved"]})),
        )
        .await;
    // A UNC root is refused off Windows; the login stays either way.
    assert!(
        status == StatusCode::OK || status == StatusCode::UNPROCESSABLE_ENTITY,
        "{text}"
    );
    assert_eq!(
        env.server.records_db().share_logins().await.unwrap().len(),
        1
    );
}
