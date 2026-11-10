//! Signing in, signing out and "who am I" over HTTP, with the same-origin guard and the
//! sign-in throttle.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use common::{TestServer, session_cookie, spawn_server};
use records::NewUser;
use reqwest::StatusCode;
use reqwest::header::{CONTENT_TYPE, COOKIE, HOST, ORIGIN, SET_COOKIE};
use serde_json::{Value, json};
use server::{Config, ConfigError};

const PASSWORD: &str = "correct horse battery";
const WRONG: &str = "not the password";

/// Creates `email` with [`PASSWORD`], without signing in.
async fn create(server: &TestServer, email: &str) -> collab_core::ids::UserId {
    let new = NewUser {
        email,
        name: "Ann",
        password: PASSWORD,
        is_admin: false,
    };
    server.db.db.create_user(None, new).await.unwrap().id
}

/// `POST /api/session` from this server's origin.
async fn sign_in(server: &TestServer, email: &str, password: &str) -> reqwest::Response {
    server
        .http(None)
        .post(server.url("/api/session"))
        .json(&json!({ "email": email, "password": password }))
        .send()
        .await
        .unwrap()
}

/// `GET /api/me` with `cookie`, if any.
async fn me(server: &TestServer, cookie: Option<&str>) -> reqwest::Response {
    let mut request = server.http(None).get(server.url("/api/me"));
    if let Some(cookie) = cookie {
        request = request.header(COOKIE, cookie);
    }
    request.send().await.unwrap()
}

async fn status_and_body(response: reqwest::Response) -> (StatusCode, Value) {
    let status = response.status();
    let body = response.json().await.unwrap();
    (status, body)
}

fn error(code: &str) -> Value {
    json!({ "error": code })
}

#[tokio::test]
async fn sign_in_sets_an_http_only_cookie_and_me_answers() {
    let server = spawn_server().await;
    let id = create(&server, "ann@x.test").await;

    let response = sign_in(&server, "Ann@X.test", PASSWORD).await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let set_cookie = response.headers()[SET_COOKIE].to_str().unwrap().to_owned();
    let attributes: Vec<&str> = set_cookie.split(';').map(str::trim).skip(1).collect();
    assert_eq!(
        attributes,
        ["HttpOnly", "SameSite=Lax", "Path=/", "Max-Age=2592000"]
    );
    let cookie = session_cookie(response.headers()).unwrap();

    // The session cookie is found among others in one header.
    let several = format!("theme=dark; {cookie}; other=1");
    let (status, body) = status_and_body(me(&server, Some(&several)).await).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({
            "id": id.0.to_string(),
            "email": "ann@x.test",
            "name": "Ann",
            "is_admin": false,
        })
    );
}

#[tokio::test]
async fn me_without_cookie_is_401() {
    let server = spawn_server().await;
    let (status, body) = status_and_body(me(&server, None).await).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body, error("not_signed_in"));

    for bad in [
        "sharecraft_session=nonsense",
        "sharecraft_session=",
        "sharecraft_session",
        &format!("sharecraft_session={}", "ab".repeat(32)),
    ] {
        let (status, body) = status_and_body(me(&server, Some(bad)).await).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{bad}");
        assert_eq!(body, error("not_signed_in"));
    }
}

#[tokio::test]
async fn wrong_password_and_unknown_email_look_the_same() {
    let server = spawn_server().await;
    create(&server, "ann@x.test").await;
    let wrong = status_and_body(sign_in(&server, "ann@x.test", WRONG).await).await;
    let unknown = status_and_body(sign_in(&server, "nobody@x.test", WRONG).await).await;
    assert_eq!(wrong, unknown);
    assert_eq!(
        wrong,
        (StatusCode::UNAUTHORIZED, error("invalid_credentials"))
    );
}

#[tokio::test]
async fn sign_out_ends_the_session() {
    let server = spawn_server().await;
    let ann = server.user("Ann").await;
    assert_eq!(
        me(&server, Some(&ann.cookie)).await.status(),
        StatusCode::OK
    );

    let response = server
        .http(Some(&ann))
        .delete(server.url("/api/session"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let cleared = response.headers()[SET_COOKIE].to_str().unwrap();
    assert!(cleared.starts_with("sharecraft_session=;"), "{cleared}");
    assert!(cleared.contains("Max-Age=0"), "{cleared}");

    let (status, body) = status_and_body(me(&server, Some(&ann.cookie)).await).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body, error("not_signed_in"));
}

#[tokio::test]
async fn disabled_account_loses_its_session() {
    let server = spawn_server().await;
    let admin = server.admin("Ada").await;
    let ann = server.user("Ann").await;
    assert_eq!(
        me(&server, Some(&ann.cookie)).await.status(),
        StatusCode::OK
    );

    server
        .db
        .db
        .set_disabled(admin.id, ann.id, true)
        .await
        .unwrap();
    let (status, body) = status_and_body(me(&server, Some(&ann.cookie)).await).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body, error("not_signed_in"));
}

#[tokio::test]
async fn expired_session_is_401() {
    let server = spawn_server().await;
    let ann = server.user("Ann").await;
    sqlx::query("update sessions set expires_at = now() - interval '1 second' where user_id = $1")
        .bind(ann.id.0)
        .execute(server.db.db.test_pool())
        .await
        .unwrap();
    let (status, body) = status_and_body(me(&server, Some(&ann.cookie)).await).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body, error("not_signed_in"));
}

#[tokio::test]
async fn sixth_failed_attempt_is_refused() {
    let server = spawn_server().await;
    create(&server, "ann@x.test").await;
    // Case and surrounding spaces do not make a different email.
    for email in [
        "ann@x.test",
        "ANN@x.test",
        " ann@X.TEST",
        "ann@x.test",
        "Ann@x.test",
    ] {
        let answer = status_and_body(sign_in(&server, email, WRONG).await).await;
        assert_eq!(
            answer,
            (StatusCode::UNAUTHORIZED, error("invalid_credentials"))
        );
    }
    let answer = status_and_body(sign_in(&server, "ann@x.test", PASSWORD).await).await;
    assert_eq!(
        answer,
        (StatusCode::TOO_MANY_REQUESTS, error("too_many_attempts"))
    );

    // Other emails are not affected.
    create(&server, "bob@x.test").await;
    let response = sign_in(&server, "bob@x.test", PASSWORD).await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn simultaneous_attempts_cannot_pass_the_limit() {
    let server = spawn_server().await;
    create(&server, "ann@x.test").await;
    let attempts = (0..10).map(|_| {
        let server = &server;
        async move { sign_in(server, "ann@x.test", WRONG).await.status() }
    });
    let statuses = futures_util::future::join_all(attempts).await;
    let refused = statuses
        .iter()
        .filter(|s| **s == StatusCode::TOO_MANY_REQUESTS)
        .count();
    let checked = statuses
        .iter()
        .filter(|s| **s == StatusCode::UNAUTHORIZED)
        .count();
    assert!(checked <= 5, "{statuses:?}");
    assert_eq!(checked + refused, 10, "{statuses:?}");
    let answer = status_and_body(sign_in(&server, "ann@x.test", PASSWORD).await).await;
    assert_eq!(
        answer,
        (StatusCode::TOO_MANY_REQUESTS, error("too_many_attempts"))
    );
}

#[tokio::test]
async fn unknown_api_path_is_a_json_404() {
    let server = spawn_server().await;
    let answer = status_and_body(
        server
            .http(None)
            .get(server.url("/api/nothing-here"))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(answer, (StatusCode::NOT_FOUND, error("not_found")));

    // It passes the same guard as every other API path.
    let response = reqwest::Client::new()
        .post(server.url("/api/nothing-here"))
        .send()
        .await
        .unwrap();
    let answer = status_and_body(response).await;
    assert_eq!(answer, (StatusCode::FORBIDDEN, error("forbidden")));
}

#[tokio::test]
async fn success_clears_failed_attempts() {
    let server = spawn_server().await;
    create(&server, "ann@x.test").await;
    for _ in 0..2 {
        for _ in 0..4 {
            let response = sign_in(&server, "ann@x.test", WRONG).await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
        let response = sign_in(&server, "ann@x.test", PASSWORD).await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }
}

#[tokio::test]
async fn post_without_origin_is_refused() {
    let server = spawn_server().await;
    create(&server, "ann@x.test").await;
    let response = reqwest::Client::new()
        .post(server.url("/api/session"))
        .json(&json!({ "email": "ann@x.test", "password": PASSWORD }))
        .send()
        .await
        .unwrap();
    let answer = status_and_body(response).await;
    assert_eq!(answer, (StatusCode::FORBIDDEN, error("forbidden")));

    // A `GET` needs no `Origin`.
    let ann = server.user("Ann").await;
    let response = reqwest::Client::new()
        .get(server.url("/api/me"))
        .header(COOKIE, &ann.cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn cross_origin_post_is_refused() {
    let server = spawn_server().await;
    let ann = server.user("Ann").await;
    let response = reqwest::Client::new()
        .post(server.url("/api/session"))
        .header(ORIGIN, "http://evil.example")
        .json(&json!({ "email": ann.email, "password": PASSWORD }))
        .send()
        .await
        .unwrap();
    let answer = status_and_body(response).await;
    assert_eq!(answer, (StatusCode::FORBIDDEN, error("forbidden")));

    let response = reqwest::Client::new()
        .delete(server.url("/api/session"))
        .header(ORIGIN, "http://evil.example")
        .header(COOKIE, &ann.cookie)
        .send()
        .await
        .unwrap();
    let answer = status_and_body(response).await;
    assert_eq!(answer, (StatusCode::FORBIDDEN, error("forbidden")));

    let response = reqwest::Client::new()
        .get(server.url("/api/me"))
        .header(ORIGIN, "http://evil.example")
        .header(COOKIE, &ann.cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    // The session is still valid.
    assert_eq!(
        me(&server, Some(&ann.cookie)).await.status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn origin_host_case_is_ignored() {
    let server = spawn_server().await;
    create(&server, "ann@x.test").await;
    let port = server.addr.port();
    let response = reqwest::Client::new()
        .post(server.url("/api/session"))
        .header(HOST, format!("LocalHost:{port}"))
        .header(ORIGIN, format!("HTTP://localhost:{port}"))
        .json(&json!({ "email": "ann@x.test", "password": PASSWORD }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn oversized_or_non_json_body_is_refused() {
    let server = spawn_server().await;
    create(&server, "ann@x.test").await;
    let client = server.http(None);

    let huge = json!({ "email": "ann@x.test", "password": "x".repeat(17 * 1024) });
    let response = client
        .post(server.url("/api/session"))
        .json(&huge)
        .send()
        .await
        .unwrap();
    let answer = status_and_body(response).await;
    assert_eq!(answer, (StatusCode::PAYLOAD_TOO_LARGE, error("too_large")));

    let body = json!({ "email": "ann@x.test", "password": PASSWORD }).to_string();
    let response = client
        .post(server.url("/api/session"))
        .header(CONTENT_TYPE, "text/plain")
        .body(body)
        .send()
        .await
        .unwrap();
    let answer = status_and_body(response).await;
    assert_eq!(
        answer,
        (StatusCode::UNSUPPORTED_MEDIA_TYPE, error("not_json"))
    );

    let response = client
        .post(server.url("/api/session"))
        .header(CONTENT_TYPE, "application/json")
        .body("{not json")
        .send()
        .await
        .unwrap();
    let answer = status_and_body(response).await;
    assert_eq!(answer, (StatusCode::BAD_REQUEST, error("bad_request")));
}

#[test]
fn missing_database_url_is_a_config_error() {
    let err = Config::from_values(None, None, None, None, None, None).unwrap_err();
    assert_eq!(err, ConfigError::NoDatabase);
    assert_eq!(err.to_string(), "DATABASE_URL is not set");
    let blank = Config::from_values(None, None, None, Some("  "), None, None);
    assert_eq!(blank, Err(ConfigError::NoDatabase));
}

#[test]
fn config_debug_hides_the_database_url() {
    let config = Config::from_values(
        None,
        None,
        None,
        Some("postgres://ann:s3cret@db.invalid/x"),
        None,
        None,
    )
    .unwrap();
    let shown = format!("{config:?}");
    assert!(!shown.contains("s3cret"), "{shown}");
    assert!(!shown.contains("db.invalid"), "{shown}");
    assert!(shown.contains("[hidden]"), "{shown}");
    assert_eq!(
        config.database_url.expose(),
        "postgres://ann:s3cret@db.invalid/x"
    );
}
