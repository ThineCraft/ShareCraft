//! Production mode, reached as a reverse proxy reaches it: only trusted proxies are answered,
//! only over HTTPS, the origin check compares with the public address, the session cookie is
//! `__Host-` and `Secure`, HSTS is sent, and links name the public address.
//!
//! The tests talk to the server on loopback, which the server is told to trust as its proxy,
//! and send the headers a proxy would add. Session cookies and link tokens are kept in
//! variables and never printed.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use collab_core::ids::DocId;
use common::{
    TestServer, handshake_status, install_stub_editor, spawn_server, spawn_server_config,
    ws_request,
};
use reqwest::header::{
    CONTENT_SECURITY_POLICY, CONTENT_TYPE, COOKIE, HeaderMap, HeaderName, HeaderValue, LOCATION,
    ORIGIN, REFERRER_POLICY, SET_COOKIE, STRICT_TRANSPORT_SECURITY, X_CONTENT_TYPE_OPTIONS,
};
use reqwest::{Response, StatusCode};
use serde_json::{Value, json};
use server::deployment::Deployment;
use uuid::Uuid;

/// The public address the test servers are told they are reached at.
const PUBLIC: &str = "https://share.example.com";
/// The public host.
const HOST: &str = "share.example.com";
/// The client address the proxy reports.
const CLIENT: &str = "198.51.100.7";
/// The production cookie's name.
const COOKIE_NAME: &str = "__Host-sharecraft_session";

/// A production-mode server trusting `proxies`, with a home page, a link landing page and a stub
/// editor installed.
async fn production_server(proxies: &str) -> TestServer {
    let proxies = proxies.to_owned();
    spawn_server_config(move |config| {
        config.deployment = Deployment::production_for_tests(PUBLIC, &proxies).unwrap();
        std::fs::create_dir_all(&config.web_dir).unwrap();
        std::fs::write(
            config.web_dir.join("index.html"),
            "<!doctype html><title>home</title>",
        )
        .unwrap();
        std::fs::write(
            config.web_dir.join("link.html"),
            "<!doctype html><title>link</title>",
        )
        .unwrap();
        install_stub_editor(config, "gridcraft");
    })
    .await
}

/// A production server whose proxy is on this computer.
async fn server() -> TestServer {
    production_server("127.0.0.1, ::1").await
}

/// The headers a TLS-terminating proxy adds for a browser on the public page.
fn proxy_headers(cookie: Option<&str>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));
    headers.insert("x-forwarded-host", HeaderValue::from_static(HOST));
    headers.insert("x-forwarded-for", HeaderValue::from_static(CLIENT));
    headers.insert(ORIGIN, HeaderValue::from_static(PUBLIC));
    if let Some(cookie) = cookie {
        headers.insert(COOKIE, HeaderValue::from_str(cookie).unwrap());
    }
    headers
}

/// A client sending `headers` on every request; redirects are not followed.
fn client_with(headers: HeaderMap) -> reqwest::Client {
    reqwest::Client::builder()
        .default_headers(headers)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(common::TIMEOUT)
        .build()
        .unwrap()
}

/// A browser on the public page, behind the proxy.
fn proxied(cookie: Option<&str>) -> reqwest::Client {
    client_with(proxy_headers(cookie))
}

/// Sends `request`; a failure panics without printing the address, which may hold a token.
async fn send(request: reqwest::RequestBuilder) -> Response {
    match request.send().await {
        Ok(response) => response,
        Err(_) => panic!("a request did not reach the server"),
    }
}

async fn status_and_json(response: Response) -> (StatusCode, Value) {
    let status = response.status();
    let bytes = response.bytes().await.unwrap();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, body)
}

/// The `Set-Cookie` values of a response.
fn set_cookies(response: &Response) -> Vec<String> {
    response
        .headers()
        .get_all(SET_COOKIE)
        .iter()
        .map(|v| v.to_str().unwrap().to_owned())
        .collect()
}

/// Signs `name` in through the proxy; the `Cookie` pair, the account's id and the full
/// `Set-Cookie` value.
async fn sign_in(server: &TestServer, name: &str) -> (String, collab_core::ids::UserId, String) {
    sign_in_as(server, name, false).await
}

/// Like [`sign_in`], for an administrator when `is_admin`.
async fn sign_in_as(
    server: &TestServer,
    name: &str,
    is_admin: bool,
) -> (String, collab_core::ids::UserId, String) {
    let (response, id, _) = server
        .sign_in_through(name, is_admin, &proxied(None), &server.url("/api/session"))
        .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let set = set_cookies(&response);
    assert_eq!(set.len(), 1);
    let pair = set[0].split(';').next().unwrap().trim().to_owned();
    (pair, id, set[0].clone())
}

#[tokio::test]
async fn untrusted_peer_refused_except_health() {
    // Only 10.9.9.9 is trusted, so this test's loopback connection is not.
    let server = production_server("10.9.9.9").await;
    let client = proxied(None);
    for path in ["/api/me", "/", "/index.html", "/l/whatever"] {
        let response = send(client.get(server.url(path))).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
        assert!(
            response.headers().get(STRICT_TRANSPORT_SECURITY).is_none(),
            "a refused peer never came over HTTPS"
        );
        assert_eq!(response.headers()[X_CONTENT_TYPE_OPTIONS], "nosniff");
        assert!(
            response.headers()[CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("text/plain")
        );
        assert!(response.text().await.unwrap().contains("reverse proxy"));
    }
    let post = send(client.post(server.url("/api/session")).json(&json!({}))).await;
    assert_eq!(post.status(), StatusCode::FORBIDDEN);
    // The Start shortcut's readiness check still works from this computer.
    let health = send(reqwest::Client::new().get(server.url("/healthz"))).await;
    assert_eq!(health.status(), StatusCode::OK);
    assert_eq!(health.text().await.unwrap(), "ok");
}

#[tokio::test]
async fn plain_http_refused() {
    let server = server().await;
    let mut headers = proxy_headers(None);
    headers.insert("x-forwarded-proto", HeaderValue::from_static("http"));
    let plain = client_with(headers);
    let mut missing = proxy_headers(None);
    missing.remove("x-forwarded-proto");
    for client in [plain, client_with(missing)] {
        let page = send(client.get(server.url("/folder/page.html?x=1&y=2"))).await;
        assert_eq!(page.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(
            page.headers()[LOCATION],
            "https://share.example.com/folder/page.html?x=1&y=2"
        );
        assert_eq!(
            page.headers()[STRICT_TRANSPORT_SECURITY],
            "max-age=31536000"
        );
        let root = send(client.head(server.url("/"))).await;
        assert_eq!(root.headers()[LOCATION], "https://share.example.com/");

        for request in [
            client.get(server.url("/api/me")),
            client.post(server.url("/api/session")).json(&json!({})),
        ] {
            let response = send(request).await;
            assert_eq!(
                response.headers()[STRICT_TRANSPORT_SECURITY],
                "max-age=31536000"
            );
            let (status, body) = status_and_json(response).await;
            assert_eq!(status, StatusCode::FORBIDDEN);
            assert_eq!(body, json!({"error": "https_required"}));
        }
        let socket = send(client.get(server.url("/ws/docs/x"))).await;
        assert_eq!(socket.status(), StatusCode::FORBIDDEN);
        let health = send(client.get(server.url("/healthz"))).await;
        assert_eq!(health.status(), StatusCode::OK);
    }
}

#[tokio::test]
async fn public_origin_check() {
    let server = server().await;
    // Through the proxy, from the public page: the API answers (401: nobody signed in).
    let (status, body) =
        status_and_json(send(proxied(None).get(server.url("/api/me"))).await).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body, json!({"error": "not_signed_in"}));
    // A GET without Origin is fine, as before.
    let mut no_origin = proxy_headers(None);
    no_origin.remove(ORIGIN);
    let response = send(client_with(no_origin.clone()).get(server.url("/api/me"))).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    // A change without Origin is refused, as before.
    let (status, body) = status_and_json(
        send(
            client_with(no_origin)
                .post(server.url("/api/session"))
                .json(&json!({})),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body, json!({"error": "forbidden"}));

    for origin in [
        "https://evil.example",
        "http://share.example.com",
        "https://share.example.com:8443",
        "https://share.example.com.evil.example",
        "null",
    ] {
        let mut headers = proxy_headers(None);
        headers.insert(ORIGIN, HeaderValue::from_str(origin).unwrap());
        let response = send(
            client_with(headers)
                .post(server.url("/api/session"))
                .json(&json!({})),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{origin}");
    }
    // Another host name (DNS rebinding), or the proxy forwarding the wrong host.
    for host in ["evil.example", "127.0.0.1"] {
        let mut headers = proxy_headers(None);
        headers.insert("x-forwarded-host", HeaderValue::from_str(host).unwrap());
        let response = send(client_with(headers).get(server.url("/api/me"))).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{host}");
    }
    // Without X-Forwarded-Host the Host header is used: here the loopback address, refused.
    let mut headers = proxy_headers(None);
    headers.remove("x-forwarded-host");
    let response = send(client_with(headers).get(server.url("/api/me"))).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn production_cookie_attributes() {
    let server = server().await;
    let (pair, _, full) = sign_in(&server, "Ann").await;
    assert!(pair.starts_with(&format!("{COOKIE_NAME}=")));
    let attributes: Vec<&str> = full.split(';').skip(1).map(str::trim).collect();
    for wanted in ["Secure", "HttpOnly", "SameSite=Lax", "Path=/"] {
        assert!(attributes.contains(&wanted), "{wanted} missing");
    }
    assert!(attributes.iter().any(|a| a.starts_with("Max-Age=")));
    assert!(
        !attributes
            .iter()
            .any(|a| a.to_ascii_lowercase().starts_with("domain"))
    );

    let me = send(proxied(Some(&pair)).get(server.url("/api/me"))).await;
    assert_eq!(me.status(), StatusCode::OK);

    let signed_out = send(proxied(Some(&pair)).delete(server.url("/api/session"))).await;
    assert_eq!(signed_out.status(), StatusCode::NO_CONTENT);
    let cleared = set_cookies(&signed_out);
    assert_eq!(cleared.len(), 1);
    assert!(cleared[0].starts_with(&format!("{COOKIE_NAME}=;")));
    assert!(cleared[0].contains("Secure"));
    assert!(cleared[0].contains("Max-Age=0"));
}

#[tokio::test]
async fn development_cookie_ignored_in_production() {
    let server = server().await;
    let (pair, _, _) = sign_in(&server, "Ann").await;
    let token = pair.split_once('=').unwrap().1;
    let development = format!("sharecraft_session={token}");
    let response = send(proxied(Some(&development)).get(server.url("/api/me"))).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn hsts_on_every_production_answer() {
    let server = server().await;
    let (pair, _, _) = sign_in(&server, "Ann").await;
    let signed = proxied(Some(&pair));
    for (what, request) in [
        ("page", proxied(None).get(server.url("/"))),
        (
            "missing file",
            proxied(None).get(server.url("/nothing-here.html")),
        ),
        ("api success", signed.get(server.url("/api/me"))),
        ("api error", proxied(None).get(server.url("/api/me"))),
        (
            "websocket refusal",
            proxied(None).get(server.url("/ws/docs/not-a-uuid")),
        ),
    ] {
        let response = send(request).await;
        assert_eq!(
            response
                .headers()
                .get(STRICT_TRANSPORT_SECURITY)
                .map(|v| v.to_str().unwrap()),
            Some("max-age=31536000"),
            "{what}"
        );
    }
    // Never in development mode.
    let dev = spawn_server().await;
    let response = send(reqwest::Client::new().get(dev.url("/healthz"))).await;
    assert!(response.headers().get(STRICT_TRANSPORT_SECURITY).is_none());
}

#[tokio::test]
async fn hsts_time_can_be_shortened() {
    let server = spawn_server_config(|config| {
        config.deployment = Deployment::from_values(Some(PUBLIC), None, Some("300")).unwrap();
    })
    .await;
    let response = send(proxied(None).get(server.url("/api/me"))).await;
    assert_eq!(response.headers()[STRICT_TRANSPORT_SECURITY], "max-age=300");
}

#[tokio::test]
async fn existing_headers_kept_in_production() {
    let server = server().await;
    let page = send(proxied(None).get(server.url("/"))).await;
    assert_eq!(page.status(), StatusCode::OK);
    assert_eq!(
        page.headers()[CONTENT_SECURITY_POLICY],
        "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; frame-src 'self'; frame-ancestors 'none'"
    );
    assert_eq!(page.headers()[X_CONTENT_TYPE_OPTIONS], "nosniff");
    assert_eq!(page.headers()[REFERRER_POLICY], "same-origin");
    // A link page keeps its own no-referrer.
    let link = send(proxied(None).get(server.url("/l/whatever"))).await;
    assert_eq!(link.headers()[REFERRER_POLICY], "no-referrer");
    assert!(link.headers().get(STRICT_TRANSPORT_SECURITY).is_some());
    // An editor's files keep the editor's own policy.
    let (pair, _, _) = sign_in(&server, "Ann").await;
    let editor = send(proxied(Some(&pair)).get(server.url("/editors/gridcraft/index.html"))).await;
    assert_eq!(editor.status(), StatusCode::OK);
    let policy = editor.headers()[CONTENT_SECURITY_POLICY].to_str().unwrap();
    assert_ne!(
        policy,
        "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; frame-src 'self'; frame-ancestors 'none'"
    );
    assert!(editor.headers().get(STRICT_TRANSPORT_SECURITY).is_some());
}

/// A WebSocket handshake for `doc` as the browser on the public page sends it, through the
/// proxy, with `origin` and `cookie`.
fn ws_handshake(
    server: &TestServer,
    doc: DocId,
    origin: &str,
    cookie: &str,
) -> tokio_tungstenite::tungstenite::handshake::client::Request {
    let mut request = ws_request(server.addr, &format!("/ws/docs/{}", doc.0), Some(cookie));
    let headers = request.headers_mut();
    headers.insert(ORIGIN, HeaderValue::from_str(origin).unwrap());
    for (name, value) in proxy_headers(None).iter() {
        if name != ORIGIN {
            headers.insert(
                HeaderName::from_bytes(name.as_ref()).unwrap(),
                value.clone(),
            );
        }
    }
    request
}

#[tokio::test]
async fn websocket_and_editors_at_public_address() {
    let server = server().await;
    let (pair, id, _) = sign_in(&server, "Ann").await;
    let user = common::TestUser {
        id,
        email: server.email_of("Ann").await,
        cookie: pair.clone(),
    };
    let doc = DocId(Uuid::new_v4());
    server.give_access(doc, &user).await;
    assert_eq!(
        handshake_status(ws_handshake(&server, doc, PUBLIC, &pair)).await,
        101
    );
    for origin in ["https://evil.example", "http://share.example.com"] {
        assert_eq!(
            handshake_status(ws_handshake(&server, doc, origin, &pair)).await,
            403,
            "{origin}"
        );
    }
    // The editors' files: through the proxy yes, with another site's Origin no.
    let editor = send(proxied(Some(&pair)).get(server.url("/editors/gridcraft/index.html"))).await;
    assert_eq!(editor.status(), StatusCode::OK);
    let mut evil = proxy_headers(Some(&pair));
    evil.insert(ORIGIN, HeaderValue::from_static("https://evil.example"));
    let refused = send(client_with(evil).get(server.url("/editors/gridcraft/index.html"))).await;
    assert_eq!(refused.status(), StatusCode::NOT_FOUND);
}

/// Signs `name` in through the proxy as a harness `TestUser` with the production cookie.
async fn user(server: &TestServer, name: &str) -> common::TestUser {
    let (pair, id, _) = sign_in(server, name).await;
    common::TestUser {
        id,
        email: server.email_of(name).await,
        cookie: pair,
    }
}

/// `method path` with `body` through the proxy as `who`; the status and JSON answer.
async fn call(
    server: &TestServer,
    cookie: Option<&str>,
    method: reqwest::Method,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let client = proxied(cookie);
    let mut request = client.request(method, server.url(path));
    if let Some(body) = body {
        request = request.json(&body);
    }
    status_and_json(send(request).await).await
}

/// A link on `node` made by `owner` through the proxy: its address.
async fn make_link(
    server: &TestServer,
    owner: &common::TestUser,
    node: &str,
    kind: &str,
) -> String {
    let mut body = json!({ "kind": kind, "role": "viewer" });
    if kind == "anyone" {
        let expiry = time::OffsetDateTime::now_utc() + time::Duration::days(1);
        body["expires_at"] = json!(
            expiry
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap()
        );
    }
    let (status, made) = call(
        server,
        Some(&owner.cookie),
        reqwest::Method::POST,
        &format!("/api/nodes/{node}/links"),
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    made["url"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn link_address_is_public() {
    let server = server().await;
    let ann = user(&server, "Ann").await;
    let home = server.home_of(&ann).await.0.to_string();
    let url = make_link(&server, &ann, &home, "signed_in").await;
    assert!(
        url.starts_with("https://share.example.com/l/"),
        "the link names the public address"
    );
    // A proxy forwarding some other host is refused before a link is made, so its host can
    // never end up in a link.
    let mut headers = proxy_headers(Some(&ann.cookie));
    headers.insert("x-forwarded-host", HeaderValue::from_static("evil.example"));
    let response = send(
        client_with(headers)
            .post(server.url(&format!("/api/nodes/{home}/links")))
            .json(&json!({ "kind": "signed_in", "role": "viewer" })),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn anyone_link_works_signed_out_at_public_address() {
    let server = server().await;
    let (admin, _, _) = sign_in_as(&server, "Settings Admin", true).await;
    let (status, _) = call(
        &server,
        Some(&admin),
        reqwest::Method::PUT,
        "/api/settings/sharing",
        Some(json!({ "anyone_links": true })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let ann = user(&server, "Ann").await;
    let home = server.home_of(&ann).await.0.to_string();
    let url = make_link(&server, &ann, &home, "anyone").await;
    let token = url.rsplit('/').next().unwrap().to_owned();

    // Signed out, through the proxy: the landing page and what the link reaches.
    let page = send(proxied(None).get(server.url(&format!("/l/{token}")))).await;
    assert_eq!(page.status(), StatusCode::OK);
    assert_eq!(page.headers()[REFERRER_POLICY], "no-referrer");
    assert!(page.headers().get(SET_COOKIE).is_none());
    let what = send(proxied(None).get(server.url(&format!("/api/l/{token}")))).await;
    assert_eq!(what.status(), StatusCode::OK);
    assert!(what.headers().get(SET_COOKIE).is_none());
    assert_eq!(what.headers()[REFERRER_POLICY], "no-referrer");
    let children =
        send(proxied(None).get(server.url(&format!("/api/l/{token}/children/{home}")))).await;
    assert_eq!(children.status(), StatusCode::OK);
}

#[tokio::test]
async fn signed_in_link_and_access_request_at_public_address() {
    let server = server().await;
    let ann = user(&server, "Ann").await;
    let bob = user(&server, "Bob").await;
    let home = server.home_of(&ann).await.0.to_string();
    let url = make_link(&server, &ann, &home, "signed_in").await;
    let token = url.rsplit('/').next().unwrap().to_owned();
    // Bob redeems the link with his production cookie.
    let (status, body) = call(
        &server,
        Some(&bob.cookie),
        reqwest::Method::POST,
        &format!("/api/l/{token}/redeem"),
        None,
    )
    .await;
    assert!(status.is_success(), "{status} {body}");

    // Carol has no access, asks for it, and Ann approves.
    let carol = user(&server, "Carol").await;
    let (status, body) = call(
        &server,
        Some(&carol.cookie),
        reqwest::Method::POST,
        &format!("/api/nodes/{home}/requests"),
        Some(json!({ "message": "May I see this?" })),
    )
    .await;
    assert!(status.is_success(), "{status} {body}");
    let (status, open) = call(
        &server,
        Some(&ann.cookie),
        reqwest::Method::GET,
        "/api/requests",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let id = open.as_array().unwrap()[0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (status, body) = call(
        &server,
        Some(&ann.cookie),
        reqwest::Method::POST,
        &format!("/api/requests/{id}/approve"),
        Some(json!({ "role": "viewer" })),
    )
    .await;
    assert!(status.is_success(), "{status} {body}");
    let (status, _) = call(
        &server,
        Some(&carol.cookie),
        reqwest::Method::GET,
        &format!("/api/nodes/{home}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

/// A browser behind the proxy at client address `from`.
fn proxied_from(from: &str) -> reqwest::Client {
    let mut headers = proxy_headers(None);
    headers.insert("x-forwarded-for", HeaderValue::from_str(from).unwrap());
    client_with(headers)
}

/// `POST /api/session` with `email` and a wrong password, from `from`.
async fn wrong_password(server: &TestServer, from: &str, email: &str) -> Response {
    send(
        proxied_from(from)
            .post(server.url("/api/session"))
            .json(&json!({ "email": email, "password": "not the password at all" })),
    )
    .await
}

/// The `Retry-After` seconds of a refusal, checked against its body.
async fn retry_after(response: Response) -> u64 {
    let header: u64 = response.headers()[reqwest::header::RETRY_AFTER]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    let (status, body) = status_and_json(response).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body["error"], "too_many_attempts");
    assert_eq!(body["retry_after"], header);
    header
}

#[tokio::test]
async fn limits_per_account_and_address() {
    let server = server().await;
    let ann = server.email_of("Ann").await;
    // Five failures for one account, each from a different address, lock the account.
    for i in 0..5 {
        let response = wrong_password(&server, &format!("203.0.113.{i}"), &ann).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    let wait = retry_after(wrong_password(&server, "203.0.113.99", &ann).await).await;
    assert!((14 * 60..=15 * 60).contains(&wait), "{wait}");

    // Thirty failures from one address, across accounts that exist or not, lock the address.
    let attempts = (0..30).map(|i| {
        let server = &server;
        async move {
            wrong_password(server, "198.51.100.20", &format!("nobody{i}@test.invalid"))
                .await
                .status()
        }
    });
    let statuses = futures_util::future::join_all(attempts).await;
    let checked = statuses
        .iter()
        .filter(|s| **s == StatusCode::UNAUTHORIZED)
        .count();
    let refused = statuses
        .iter()
        .filter(|s| **s == StatusCode::TOO_MANY_REQUESTS)
        .count();
    assert_eq!(checked + refused, 30, "{statuses:?}");
    // Finish the count one at a time, in case simultaneous attempts were refused.
    let mut extra = 0;
    while extra < 30 {
        let response = wrong_password(
            &server,
            "198.51.100.20",
            &format!("more{extra}@test.invalid"),
        )
        .await;
        if response.status() == StatusCode::TOO_MANY_REQUESTS {
            break;
        }
        extra += 1;
    }
    assert_eq!(
        checked + extra,
        30,
        "the address was locked after 30 failures"
    );
    retry_after(wrong_password(&server, "198.51.100.20", "fresh@test.invalid").await).await;
    // Another address is not affected, and nor is a right password from it.
    let (response, _, _) = server
        .sign_in_through(
            "Bob",
            false,
            &proxied_from("198.51.100.21"),
            &server.url("/api/session"),
        )
        .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn ipv6_addresses_share_their_64() {
    let server = server().await;
    for i in 0..30 {
        let from = format!("2001:db8:5:6::{:x}", i + 1);
        let response = wrong_password(&server, &from, &format!("v6-{i}@test.invalid")).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    // Another address in the same /64 is locked; one in another /64 is not.
    retry_after(wrong_password(&server, "2001:db8:5:6:ffff::1", "x@test.invalid").await).await;
    let other = wrong_password(&server, "2001:db8:5:7::1", "x@test.invalid").await;
    assert_eq!(other.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn locked_even_with_right_password() {
    let server = server().await;
    let ann = server.email_of("Ann").await;
    for _ in 0..5 {
        wrong_password(&server, CLIENT, &ann).await;
    }
    let (response, _, _) = server
        .sign_in_through("Ann", false, &proxied(None), &server.url("/api/session"))
        .await;
    retry_after(response).await;
}

#[tokio::test]
async fn lock_recorded_once() {
    let server = server().await;
    let ann = server.email_of("Ann").await;
    for _ in 0..8 {
        wrong_password(&server, CLIENT, &ann).await;
    }
    let db = server.records_db();
    let deadline = std::time::Instant::now() + common::TIMEOUT;
    loop {
        let locks = db.events_of_kind("sign_in.locked").await.unwrap();
        let failures = db.events_of_kind("sign_in.failed").await.unwrap().len();
        if failures == 5 && !locks.is_empty() {
            assert_eq!(locks.len(), 1, "one lock, recorded once");
            assert_eq!(locks[0].detail["by"], "account");
            assert_eq!(locks[0].detail["address"], CLIENT);
            assert!(locks[0].detail["until"].is_string());
            assert!(locks[0].subject.is_some(), "the account is named");
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{failures} failures recorded"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn password_checks_are_pooled() {
    let server = server().await;
    let state = server.app_state().await;
    state.set_password_wait(std::time::Duration::from_millis(300));
    let held = state.hold_password_checks().await.unwrap();
    let (response, _, _) = server
        .sign_in_through("Ann", false, &proxied(None), &server.url("/api/session"))
        .await;
    assert_eq!(response.headers()[reqwest::header::RETRY_AFTER], "10");
    let (status, body) = status_and_json(response).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body, json!({ "error": "busy" }));
    // A waiting sign-in goes ahead once a turn comes free within the wait.
    state.set_password_wait(std::time::Duration::from_secs(5));
    let release = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        drop(held);
    });
    let (response, _, _) = server
        .sign_in_through("Ann", false, &proxied(None), &server.url("/api/session"))
        .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    release.await.unwrap();
}

#[tokio::test]
async fn link_lookup_limit() {
    let server = server().await;
    let missing = "0".repeat(64);
    for _ in 0..60 {
        let response =
            send(proxied_from("192.0.2.9").get(server.url(&format!("/api/l/{missing}")))).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
    let refused =
        send(proxied_from("192.0.2.9").get(server.url(&format!("/api/l/{missing}")))).await;
    assert_eq!(refused.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(refused.headers()[REFERRER_POLICY], "no-referrer");
    assert_eq!(
        refused.headers()[reqwest::header::CACHE_CONTROL],
        "no-store"
    );
    // Another address still gets its answers.
    let other =
        send(proxied_from("192.0.2.10").get(server.url(&format!("/api/l/{missing}")))).await;
    assert_eq!(other.status(), StatusCode::NOT_FOUND);
}
