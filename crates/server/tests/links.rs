//! Sharing links over HTTP: making, listing and revoking them; the landing page and what a
//! link's address reaches, signed in and anonymously; the headers on all of it; the sharing
//! settings; and the expiry sweeper. Link tokens are kept in variables and never printed.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::time::Duration;

use collab_core::ids::DocId;
use common::{
    Site, TestClient, TestServer, TestUser, handshake_status, spawn_server_config, wait_stored,
    ws_request,
};
use reqwest::header::{
    CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_SECURITY_POLICY, CONTENT_TYPE, COOKIE,
    REFERRER_POLICY, SET_COOKIE, X_CONTENT_TYPE_OPTIONS,
};
use reqwest::{Method, Response, StatusCode};
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

/// How soon a person must be out of a session once their access ended.
const PROMPTLY: Duration = Duration::from_secs(1);

/// The landing page the test server serves at `/l/{token}`.
const PAGE: &str = "<!doctype html><title>link</title>";

/// A server with a landing page installed.
async fn server() -> TestServer {
    spawn_server_config(|config| {
        std::fs::create_dir_all(&config.web_dir).unwrap();
        std::fs::write(config.web_dir.join("link.html"), PAGE).unwrap();
    })
    .await
}

/// Sends `request`; a failure panics without printing the address, which may hold a token.
async fn send(request: reqwest::RequestBuilder) -> Response {
    match request.send().await {
        Ok(response) => response,
        Err(_) => panic!("a request did not reach the server"),
    }
}

/// The status and JSON body (`null` when empty) of a response.
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

/// `method path` as `user` (or nobody), with this server's `Origin` and a JSON `body`.
async fn call(
    site: &Site,
    user: Option<&TestUser>,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = site.http(user).request(method, site.url(path));
    if let Some(body) = body {
        request = request.json(&body);
    }
    status_and_json(send(request).await).await
}

async fn get(site: &Site, user: Option<&TestUser>, path: &str) -> (StatusCode, Value) {
    call(site, user, Method::GET, path, None).await
}

/// The error code of a refusal.
fn code(body: &Value) -> &str {
    body["error"].as_str().unwrap_or("")
}

/// A folder named `name` made by `user` in `parent`.
async fn folder(site: &Site, user: &TestUser, parent: &str, name: &str) -> String {
    let (status, body) = call(
        site,
        Some(user),
        Method::POST,
        &format!("/api/nodes/{parent}/children"),
        Some(json!({ "kind": "folder", "name": name })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["id"].as_str().unwrap().to_owned()
}

/// A plain-text document named `name` made by `user` in `parent`.
async fn text_doc(site: &Site, user: &TestUser, parent: &str, name: &str) -> String {
    let (status, body) = call(
        site,
        Some(user),
        Method::POST,
        "/api/docs",
        Some(json!({ "parent": parent, "name": name, "editor": "text" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["id"].as_str().unwrap().to_owned()
}

/// A file uploaded by `user` into `parent` as `name` with `bytes`.
async fn file(site: &Site, user: &TestUser, parent: &str, name: &str, bytes: &[u8]) -> String {
    let response = send(
        site.http(Some(user))
            .post(site.url("/api/docs/upload"))
            .query(&[("parent", parent), ("name", name)])
            .body(bytes.to_vec()),
    )
    .await;
    let (status, body) = status_and_json(response).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["id"].as_str().unwrap().to_owned()
}

/// `user`'s own folder, as text.
async fn home(site: &Site, user: &TestUser) -> String {
    site.home_of(user).await.0.to_string()
}

fn rfc3339(at: OffsetDateTime) -> String {
    at.format(&Rfc3339).unwrap()
}

/// `POST /api/nodes/{node}/links` as `user`.
async fn make_link(
    site: &Site,
    user: &TestUser,
    node: &str,
    kind: &str,
    role: &str,
    expires_at: Option<OffsetDateTime>,
) -> (StatusCode, Value) {
    let mut body = json!({ "kind": kind, "role": role });
    if let Some(at) = expires_at {
        body["expires_at"] = json!(rfc3339(at));
    }
    call(
        site,
        Some(user),
        Method::POST,
        &format!("/api/nodes/{node}/links"),
        Some(body),
    )
    .await
}

/// A link made by `user` on `node`: its id and its token.
async fn link(
    site: &Site,
    user: &TestUser,
    node: &str,
    kind: &str,
    role: &str,
    expires_at: Option<OffsetDateTime>,
) -> (String, String) {
    let (status, body) = make_link(site, user, node, kind, role, expires_at).await;
    assert_eq!(status, StatusCode::CREATED, "{}", code(&body));
    let url = body["url"].as_str().unwrap();
    let token = url.rsplit('/').next().unwrap().to_owned();
    (body["id"].as_str().unwrap().to_owned(), token)
}

fn in_days(days: i64) -> OffsetDateTime {
    OffsetDateTime::now_utc() + time::Duration::days(days)
}

/// Switches anyone links on or off, as a new administrator.
async fn anyone_links(site: &Site, on: bool) {
    let admin = site.admin("Settings Admin").await;
    let (status, body) = call(
        site,
        Some(&admin),
        Method::PUT,
        "/api/settings/sharing",
        Some(json!({ "anyone_links": on })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// `POST /api/l/{token}/redeem` as `user`.
async fn redeem(site: &Site, user: &TestUser, token: &str) -> (StatusCode, Value) {
    call(
        site,
        Some(user),
        Method::POST,
        &format!("/api/l/{token}/redeem"),
        None,
    )
    .await
}

/// `user`'s access to `node` as `GET /api/nodes/{node}` answers it, `None` if refused.
async fn access_of(site: &Site, user: &TestUser, node: &str) -> Option<Value> {
    let (status, body) = get(site, Some(user), &format!("/api/nodes/{node}")).await;
    (status == StatusCode::OK).then(|| body["access"].clone())
}

/// The roles named by an access, by id.
fn roles(access: &Value) -> Vec<String> {
    access["roles"]
        .as_array()
        .map(|roles| {
            roles
                .iter()
                .map(|role| role["id"].as_str().unwrap_or("").to_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// A GET to a link's address without any cookie, and its response.
async fn anonymous(site: &Site, path: &str) -> Response {
    send(reqwest::Client::new().get(site.url(path))).await
}

/// Whether `response` is `404 link_not_found` and sets no cookie.
async fn link_not_found(response: Response) -> bool {
    let sets_cookie = response.headers().contains_key(SET_COOKIE);
    let (status, body) = status_and_json(response).await;
    !sets_cookie && status == StatusCode::NOT_FOUND && code(&body) == "link_not_found"
}

fn uuid_of(text: &str) -> Uuid {
    text.parse().unwrap()
}

/// Waits at most [`PROMPTLY`] for `c` to be removed.
async fn removed_promptly(c: &mut TestClient) {
    tokio::time::timeout(PROMPTLY, c.pump_until(|c| c.removed.is_some()))
        .await
        .expect("not removed within one second");
}

#[tokio::test]
async fn link_routes_list_create_and_revoke() {
    let server = server().await;
    let ann = server.user("Ann").await;
    let bob = server.user("Bob").await;
    let place = home(&server, &ann).await;
    let doc = text_doc(&server, &ann, &place, "Plan.txt").await;

    // Created: the address once, on the address Ann uses.
    let expires = in_days(7);
    let (status, made) = make_link(&server, &ann, &doc, "signed_in", "editor", Some(expires)).await;
    assert_eq!(status, StatusCode::CREATED);
    let url = made["url"].as_str().unwrap();
    let prefix = format!("{}/l/", server.origin());
    assert!(
        url.starts_with(&prefix),
        "the address is not on this server"
    );
    let token = &url[prefix.len()..];
    assert!(token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(made["kind"], "signed_in");
    assert_eq!(made["role"]["id"], "editor");
    assert_eq!(made["node"], doc.as_str());
    assert_eq!(made["members"], 0);
    assert!(made["revoked"].is_null());
    let link_id = made["id"].as_str().unwrap().to_owned();

    // Listed, without the address.
    let (status, listed) = get(&server, Some(&ann), &format!("/api/nodes/{doc}/links")).await;
    assert_eq!(status, StatusCode::OK);
    let listed = listed.as_array().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["id"], link_id.as_str());
    assert!(listed[0].get("url").is_none());
    assert!(!listed[0].to_string().contains(token), "a token was listed");
    let expiry =
        OffsetDateTime::parse(listed[0]["expires_at"].as_str().unwrap(), &Rfc3339).unwrap();
    assert!((expiry - expires).abs() < time::Duration::seconds(1));

    // Refusals while making one.
    let (status, body) = make_link(&server, &ann, &doc, "signed_in", "owner", None).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::BAD_REQUEST, "link_role_too_high")
    );
    let (status, body) = make_link(&server, &ann, &doc, "signed_in", "nonesuch", None).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_request")
    );
    let past = OffsetDateTime::now_utc() - time::Duration::minutes(1);
    let (status, body) = make_link(&server, &ann, &doc, "signed_in", "viewer", Some(past)).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::BAD_REQUEST, "expiry_in_past")
    );
    let (status, body) = call(
        &server,
        Some(&ann),
        Method::POST,
        &format!("/api/nodes/{doc}/links"),
        Some(json!({ "kind": "signed_in", "role": "viewer", "expires_at": "tomorrow" })),
    )
    .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_request")
    );
    let (status, _) = call(
        &server,
        Some(&ann),
        Method::POST,
        &format!("/api/nodes/{doc}/links"),
        Some(json!({ "kind": "secret", "role": "viewer" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, body) = make_link(&server, &ann, &doc, "anyone", "viewer", Some(in_days(1))).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::BAD_REQUEST, "anyone_links_off")
    );
    anyone_links(&server, true).await;
    let (status, body) = make_link(&server, &ann, &doc, "anyone", "viewer", None).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::BAD_REQUEST, "expiry_required")
    );
    let (status, body) =
        make_link(&server, &ann, &doc, "anyone", "viewer", Some(in_days(31))).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::BAD_REQUEST, "expiry_too_long")
    );
    let (status, body) = make_link(&server, &ann, &doc, "anyone", "editor", Some(in_days(1))).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::BAD_REQUEST, "link_role_too_high")
    );
    // Bob has no access: the item does not exist for him. As a Viewer he may not manage it.
    let (status, body) = make_link(&server, &bob, &doc, "signed_in", "viewer", None).await;
    assert_eq!((status, code(&body)), (StatusCode::NOT_FOUND, "not_found"));
    let (status, _) = get(&server, Some(&bob), &format!("/api/nodes/{doc}/links")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(
        &server,
        Some(&ann),
        Method::PUT,
        &format!("/api/nodes/{doc}/access"),
        Some(json!({ "email": bob.email, "role": "viewer" })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, body) = make_link(&server, &bob, &doc, "signed_in", "viewer", None).await;
    assert_eq!((status, code(&body)), (StatusCode::FORBIDDEN, "forbidden"));
    let (status, _) = get(&server, Some(&bob), &format!("/api/nodes/{doc}/links")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // Nobody makes a link above their own access: a manager who cannot edit.
    let admin = server.admin("Role Admin").await;
    let (status, made_role) = call(
        &server,
        Some(&admin),
        Method::POST,
        "/api/roles",
        Some(json!({ "name": "Gatekeeper", "perms": ["view", "manage_access"] })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{made_role}");
    let gatekeeper = made_role["id"].as_str().unwrap().to_owned();
    let (status, _) = call(
        &server,
        Some(&ann),
        Method::PUT,
        &format!("/api/nodes/{doc}/access"),
        Some(json!({ "email": bob.email, "role": gatekeeper })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, body) = make_link(&server, &bob, &doc, "signed_in", "editor", None).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::FORBIDDEN, "beyond_your_access")
    );
    // Signed out, and from another site.
    let (status, body) = call(
        &server,
        None,
        Method::POST,
        &format!("/api/nodes/{doc}/links"),
        Some(json!({ "kind": "signed_in", "role": "viewer" })),
    )
    .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNAUTHORIZED, "not_signed_in")
    );
    let response = send(
        reqwest::Client::new()
            .post(server.url(&format!("/api/nodes/{doc}/links")))
            .header(COOKIE, &ann.cookie)
            .header("origin", "http://evil.example")
            .json(&json!({ "kind": "signed_in", "role": "viewer" })),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    // Revoked: by a manager only (Carl cannot even see the item; a Viewer may not), at once,
    // and again without complaint.
    let carl = server.user("Carl").await;
    let (status, _) = call(
        &server,
        Some(&carl),
        Method::DELETE,
        &format!("/api/links/{link_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(
        &server,
        Some(&ann),
        Method::PUT,
        &format!("/api/nodes/{doc}/access"),
        Some(json!({ "email": carl.email, "role": "viewer" })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(
        &server,
        Some(&carl),
        Method::DELETE,
        &format!("/api/links/{link_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    for _ in 0..2 {
        let (status, _) = call(
            &server,
            Some(&ann),
            Method::DELETE,
            &format!("/api/links/{link_id}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }
    let (_, listed) = get(&server, Some(&ann), &format!("/api/nodes/{doc}/links")).await;
    let revoked = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["id"] == link_id.as_str())
        .unwrap();
    assert_eq!(revoked["revoked"]["reason"], "manager");
    assert_eq!(revoked["revoked"]["by"]["id"], ann.id.0.to_string());
    assert!(link_not_found(anonymous(&server, &format!("/api/l/{token}")).await).await);
    let (status, body) = redeem(&server, &bob, token).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::NOT_FOUND, "link_not_found")
    );
    let (status, _) = call(
        &server,
        Some(&ann),
        Method::DELETE,
        &format!("/api/links/{}", Uuid::new_v4()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(&server, Some(&ann), Method::DELETE, "/api/links/nope", None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn signed_in_link_round_trip() {
    let server = server().await;
    let ann = server.user("Ann").await;
    let bob = server.user("Bob").await;
    let place = home(&server, &ann).await;
    let doc = text_doc(&server, &ann, &place, "Plan.txt").await;
    assert_eq!(access_of(&server, &bob, &doc).await, None);
    let (link_id, token) = link(&server, &ann, &doc, "signed_in", "editor", None).await;

    // The page asks what the link is.
    let (status, what) = get(&server, Some(&bob), &format!("/api/l/{token}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(what["kind"], "signed_in");
    assert_eq!(what["role"]["id"], "editor");
    assert!(
        what.get("item").is_none(),
        "a signed-in link names its item"
    );

    // Bob confirms; the item opens with the link's role.
    let (status, opened) = redeem(&server, &bob, &token).await;
    assert_eq!(status, StatusCode::OK, "{}", code(&opened));
    assert_eq!(opened["node"], doc.as_str());
    assert_eq!(opened["url"], format!("/doc.html#{doc}"));
    let access = access_of(&server, &bob, &doc).await.unwrap();
    assert_eq!(roles(&access), vec!["editor"]);
    // Opening it again changes nothing.
    let (status, _) = redeem(&server, &bob, &token).await;
    assert_eq!(status, StatusCode::OK);
    let (_, listed) = get(&server, Some(&ann), &format!("/api/nodes/{doc}/links")).await;
    assert_eq!(listed[0]["members"], 1);

    // Ann, who already owns it, is not added.
    let (status, _) = redeem(&server, &ann, &token).await;
    assert_eq!(status, StatusCode::OK);
    let (_, listed) = get(&server, Some(&ann), &format!("/api/nodes/{doc}/links")).await;
    assert_eq!(listed[0]["members"], 1);

    // The access page and the explorer show the link, never its hidden group.
    let (status, access) = get(&server, Some(&ann), &format!("/api/nodes/{doc}/access")).await;
    assert_eq!(status, StatusCode::OK);
    let principals: Vec<&Value> = access["assignments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| &a["principal"])
        .collect();
    let shown = principals
        .iter()
        .find(|p| p["kind"] == "link")
        .expect("the link is not shown");
    assert_eq!(shown["id"], link_id.as_str());
    assert_eq!(shown["link_kind"], "signed_in");
    assert!(shown["expires_at"].is_null());
    let group: Uuid = sqlx::query_scalar("select group_id from links where id = $1")
        .bind(uuid_of(&link_id))
        .fetch_one(server.db.db.test_pool())
        .await
        .unwrap();
    assert!(!access.to_string().contains(&group.to_string()));
    let (status, explored) = get(&server, Some(&ann), &format!("/api/explorer/{place}")).await;
    assert_eq!(status, StatusCode::OK);
    let on_doc = explored["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["node"]["id"] == doc.as_str())
        .expect("the link's break is not in the explorer");
    assert!(
        on_doc["assignments"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["principal"]["kind"] == "link" && a["principal"]["id"] == link_id.as_str())
    );
    assert_eq!(on_doc["anyone_links"], 0);
    assert!(!explored.to_string().contains(&group.to_string()));
    let csv = send(
        server
            .http(Some(&ann))
            .get(server.url(&format!("/api/explorer/{place}/export.csv"))),
    )
    .await
    .text()
    .await
    .unwrap();
    assert!(csv.contains(&format!("link,signed-in link {link_id},Editor")));
    assert!(!csv.contains(&group.to_string()));

    // Its group is changed only by the link code.
    let (status, body) = call(
        &server,
        Some(&ann),
        Method::DELETE,
        &format!("/api/nodes/{doc}/access/group/{group}"),
        None,
    )
    .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::BAD_REQUEST, "link_group")
    );
}

#[tokio::test]
async fn visiting_never_joins() {
    let server = server().await;
    let ann = server.user("Ann").await;
    let bob = server.user("Bob").await;
    let place = home(&server, &ann).await;
    let doc = text_doc(&server, &ann, &place, "Plan.txt").await;
    let (_, token) = link(&server, &ann, &doc, "signed_in", "editor", None).await;

    // A visit to the address, signed in: the page, and nothing else.
    let response = send(
        reqwest::Client::new()
            .get(server.url(&format!("/l/{token}")))
            .header(COOKIE, &bob.cookie),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers()[CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    assert_eq!(response.text().await.unwrap(), PAGE);
    // The page is the same whatever the token.
    let response = anonymous(&server, &format!("/l/{}", "0".repeat(64))).await;
    assert_eq!(response.text().await.unwrap(), PAGE);
    // Asking what the link is joins nobody either.
    let (status, _) = get(&server, Some(&bob), &format!("/api/l/{token}")).await;
    assert_eq!(status, StatusCode::OK);
    // A redeem from another site, or without an origin, is refused.
    for origin in [Some("http://evil.example"), Some("null"), None] {
        let mut request = reqwest::Client::new()
            .post(server.url(&format!("/api/l/{token}/redeem")))
            .header(COOKIE, &bob.cookie);
        if let Some(origin) = origin {
            request = request.header("origin", origin);
        }
        let response = send(request).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
    // A GET of the redeem address is no redeem.
    let response = send(
        server
            .http(Some(&bob))
            .get(server.url(&format!("/api/l/{token}/redeem"))),
    )
    .await;
    assert_ne!(response.status(), StatusCode::OK);
    assert_eq!(access_of(&server, &bob, &doc).await, None);
    let (_, listed) = get(&server, Some(&ann), &format!("/api/nodes/{doc}/links")).await;
    assert_eq!(listed[0]["members"], 0);
}

#[tokio::test]
async fn not_signed_in_goes_to_sign_in_and_back() {
    let server = server().await;
    let ann = server.user("Ann").await;
    let place = home(&server, &ann).await;
    let doc = text_doc(&server, &ann, &place, "Plan.txt").await;
    let (_, token) = link(&server, &ann, &doc, "signed_in", "viewer", None).await;

    // Signed out: the page loads, and asking what the link is says to sign in.
    let response = anonymous(&server, &format!("/l/{token}")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let (status, body) =
        status_and_json(anonymous(&server, &format!("/api/l/{token}")).await).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNAUTHORIZED, "not_signed_in")
    );
    let forged = format!("sharecraft_session={}", "ab".repeat(32));
    let response = send(
        reqwest::Client::new()
            .get(server.url(&format!("/api/l/{token}")))
            .header(COOKIE, forged),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let (status, body) = call(
        &server,
        None,
        Method::POST,
        &format!("/api/l/{token}/redeem"),
        None,
    )
    .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNAUTHORIZED, "not_signed_in")
    );
    // A link that does not work says so first, signed in or not.
    let unknown = "0".repeat(64);
    assert!(link_not_found(anonymous(&server, &format!("/api/l/{unknown}")).await).await);
    assert!(link_not_found(anonymous(&server, "/api/l/not-a-token").await).await);

    // Signed in, back at the same address: the link is offered, then opened.
    let bob = server.user("Bob").await;
    let (status, what) = get(&server, Some(&bob), &format!("/api/l/{token}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(what["role"]["id"], "viewer");
    let (status, _) = redeem(&server, &bob, &token).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        roles(&access_of(&server, &bob, &doc).await.unwrap()),
        vec!["viewer"]
    );
}

/// Whether `response` carries the headers of every link answer, and sets no cookie.
fn has_link_headers(response: &Response) -> bool {
    let headers = response.headers();
    headers.get(CACHE_CONTROL).and_then(|v| v.to_str().ok()) == Some("no-store")
        && headers.get(REFERRER_POLICY).and_then(|v| v.to_str().ok()) == Some("no-referrer")
        && !headers.contains_key(SET_COOKIE)
}

#[tokio::test]
async fn link_headers() {
    let server = server().await;
    let ann = server.user("Ann").await;
    let bob = server.user("Bob").await;
    anyone_links(&server, true).await;
    let place = home(&server, &ann).await;
    let shared = folder(&server, &ann, &place, "Shared").await;
    let bytes = b"\x00\x01binary".to_vec();
    let doc = file(&server, &ann, &shared, "Budget.xlsx", &bytes).await;
    let note = text_doc(&server, &ann, &shared, "Note.txt").await;
    let (_, anyone) = link(&server, &ann, &shared, "anyone", "viewer", Some(in_days(1))).await;
    let (_, signed) = link(&server, &ann, &place, "signed_in", "viewer", None).await;
    let unknown = "0".repeat(64);

    let mut answers = Vec::new();
    for path in [
        format!("/l/{anyone}"),
        format!("/l/{signed}"),
        format!("/l/{unknown}"),
        "/l/not-a-token".to_owned(),
        format!("/api/l/{anyone}"),
        format!("/api/l/{signed}"),
        format!("/api/l/{unknown}"),
        format!("/api/l/{anyone}/children/{shared}"),
        format!("/api/l/{anyone}/children/{place}"),
        format!("/api/l/{anyone}/content/{doc}"),
        format!("/api/l/{anyone}/text/{note}"),
        format!("/api/l/{anyone}/nowhere"),
    ] {
        answers.push(anonymous(&server, &path).await);
        answers.push(send(server.http(Some(&bob)).get(server.url(&path))).await);
    }
    answers.push(
        send(
            server
                .http(Some(&bob))
                .post(server.url(&format!("/api/l/{signed}/redeem"))),
        )
        .await,
    );
    answers.push(
        send(
            reqwest::Client::new()
                .post(server.url(&format!("/api/l/{signed}/redeem")))
                .header("origin", "http://evil.example"),
        )
        .await,
    );
    for (i, response) in answers.iter().enumerate() {
        assert!(has_link_headers(response), "answer {i}");
    }

    // An anonymous download carries the signed-in download's headers.
    let anonymous_download = anonymous(&server, &format!("/api/l/{anyone}/content/{doc}")).await;
    let signed_download = send(
        server
            .http(Some(&ann))
            .get(server.url(&format!("/api/docs/{doc}/content"))),
    )
    .await;
    assert_eq!(anonymous_download.status(), StatusCode::OK);
    assert_eq!(signed_download.status(), StatusCode::OK);
    for name in [
        CONTENT_TYPE,
        CONTENT_DISPOSITION,
        CONTENT_SECURITY_POLICY,
        X_CONTENT_TYPE_OPTIONS,
    ] {
        assert_eq!(
            anonymous_download.headers().get(&name),
            signed_download.headers().get(&name),
            "{name}"
        );
    }
    assert_eq!(
        anonymous_download.headers()[CONTENT_TYPE],
        "application/octet-stream"
    );
    assert!(
        anonymous_download.headers()[CONTENT_DISPOSITION]
            .to_str()
            .unwrap()
            .starts_with("attachment;")
    );
    assert_eq!(
        anonymous_download.headers()[CONTENT_SECURITY_POLICY],
        "sandbox"
    );
    assert_eq!(
        anonymous_download.headers()[X_CONTENT_TYPE_OPTIONS],
        "nosniff"
    );
    assert_eq!(anonymous_download.bytes().await.unwrap().to_vec(), bytes);

    // Elsewhere the server's own defaults stay.
    let response = send(server.http(Some(&ann)).get(server.url("/api/me"))).await;
    assert_eq!(response.headers()[REFERRER_POLICY], "same-origin");
    assert_eq!(response.headers()[X_CONTENT_TYPE_OPTIONS], "nosniff");
    assert!(response.headers().contains_key(CONTENT_SECURITY_POLICY));
}

#[tokio::test]
async fn revoke_link_reaches_open_sessions() {
    let server = server().await;
    let ann = server.user("Ann").await;
    let bob = server.user("Bob").await;
    let carl = server.user("Carl").await;
    let admin = server.admin("Root").await;
    let place = home(&server, &ann).await;
    let shared = folder(&server, &ann, &place, "Shared").await;
    let doc = text_doc(&server, &ann, &shared, "Plan.txt").await;
    let (link_id, token) = link(&server, &ann, &shared, "signed_in", "editor", None).await;
    for user in [&bob, &carl] {
        let (status, _) = redeem(&server, user, &token).await;
        assert_eq!(status, StatusCode::OK);
    }
    // After the link was made, the file breaks inheritance: the link's assignment is copied.
    let (status, _) = call(
        &server,
        Some(&ann),
        Method::POST,
        &format!("/api/nodes/{doc}/inheritance"),
        Some(json!({ "inherit": false })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    // Carl can also view it through a group.
    let db = server.records_db();
    let group = db.create_group(admin.id, "Readers").await.unwrap();
    db.add_member(admin.id, group.id, carl.id).await.unwrap();
    db.set_node_access(
        admin.id,
        collab_core::ids::NodeId(uuid_of(&doc)),
        records::Principal::Group(group.id),
        Some("viewer"),
    )
    .await
    .unwrap();

    let doc_id = DocId(uuid_of(&doc));
    let mut bob_in = TestClient::connect_as(&server, doc_id, &bob).await;
    let mut carl_in = TestClient::connect_as(&server, doc_id, &carl).await;
    assert!(bob_in.can_edit && carl_in.can_edit);

    let (status, _) = call(
        &server,
        Some(&ann),
        Method::DELETE,
        &format!("/api/links/{link_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    // Bob was in through the link only: out at once.
    removed_promptly(&mut bob_in).await;
    // Carl stays, as a viewer.
    tokio::time::timeout(PROMPTLY, carl_in.pump_until(|c| !c.can_edit))
        .await
        .expect("Carl kept editing");
    assert!(carl_in.removed.is_none());
    assert_eq!(access_of(&server, &bob, &doc).await, None);
    assert_eq!(
        roles(&access_of(&server, &carl, &doc).await.unwrap()),
        vec!["viewer"]
    );
}

#[tokio::test]
async fn anyone_link_stays_inside_its_item() {
    let server = server().await;
    let ann = server.user("Ann").await;
    anyone_links(&server, true).await;
    let place = home(&server, &ann).await;
    let shared = folder(&server, &ann, &place, "Shared").await;
    let sub = folder(&server, &ann, &shared, "Sub").await;
    let own = folder(&server, &ann, &shared, "Own").await;
    let inside = file(&server, &ann, &sub, "Inside.xlsx", b"inside").await;
    let leaving = file(&server, &ann, &sub, "Leaving.xlsx", b"leaving").await;
    let doomed = file(&server, &ann, &sub, "Doomed.xlsx", b"doomed").await;
    let hidden = file(&server, &ann, &own, "Hidden.xlsx", b"hidden").await;
    let outside = file(&server, &ann, &place, "Outside.xlsx", b"outside").await;
    let note = text_doc(&server, &ann, &shared, "Note.txt").await;
    let (status, _) = call(
        &server,
        Some(&ann),
        Method::POST,
        &format!("/api/nodes/{own}/inheritance"),
        Some(json!({ "inherit": false })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, token) = link(&server, &ann, &shared, "anyone", "viewer", Some(in_days(2))).await;
    let reach = |path: String| {
        let server = &server;
        async move { anonymous(server, &path).await }
    };

    // The item and what is below it.
    let response = reach(format!("/api/l/{token}")).await;
    assert!(!response.headers().contains_key(SET_COOKIE));
    let (status, what) = status_and_json(response).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(what["kind"], "anyone");
    assert_eq!(what["role"]["id"], "viewer");
    assert_eq!(what["item"]["id"], shared.as_str());
    assert_eq!(what["item"]["name"], "Shared");
    let (status, listing) =
        status_and_json(reach(format!("/api/l/{token}/children/{shared}")).await).await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<&str> = listing["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec!["Sub", "Note.txt"],
        "Own has its own permissions"
    );
    assert!(
        listing["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| roles(&item["access"]) == vec!["viewer"])
    );
    // Paging.
    let (status, first) =
        status_and_json(reach(format!("/api/l/{token}/children/{shared}?limit=1")).await).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first["items"].as_array().unwrap().len(), 1);
    let next = first["next"].as_str().unwrap().to_owned();
    let (_, second) = status_and_json(
        reach(format!(
            "/api/l/{token}/children/{shared}?limit=1&after={next}"
        ))
        .await,
    )
    .await;
    assert_eq!(second["items"][0]["name"], "Note.txt");
    assert!(second["next"].is_null());
    let response = reach(format!("/api/l/{token}/children/{shared}?limit=0")).await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _) = status_and_json(reach(format!("/api/l/{token}/children/{sub}")).await).await;
    assert_eq!(status, StatusCode::OK);
    let response = reach(format!("/api/l/{token}/content/{inside}")).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.bytes().await.unwrap().as_ref(), b"inside");
    let (status, text) = status_and_json(reach(format!("/api/l/{token}/text/{note}")).await).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(text["text"], "");
    assert!(text["revision"].is_null());

    // Never outside it, nor below a break, nor by a crafted id, nor the wrong route.
    for path in [
        format!("/api/l/{token}/children/{place}"),
        format!("/api/l/{token}/children/{own}"),
        format!("/api/l/{token}/content/{hidden}"),
        format!("/api/l/{token}/text/{hidden}"),
        format!("/api/l/{token}/content/{outside}"),
        format!("/api/l/{token}/children/{}", Uuid::new_v4()),
        format!("/api/l/{token}/content/{}", Uuid::new_v4()),
        format!("/api/l/{token}/content/not-an-id"),
        format!("/api/l/{token}/content/{shared}"),
        format!("/api/l/{token}/content/{sub}"),
        format!("/api/l/{token}/text/{inside}"),
        format!("/api/l/{token}/children/{inside}/../{outside}"),
    ] {
        assert!(link_not_found(reach(path).await).await);
    }

    // A file moved out of the item, and a deleted file, are out of reach at once.
    let (status, _) = call(
        &server,
        Some(&ann),
        Method::PATCH,
        &format!("/api/nodes/{leaving}"),
        Some(json!({ "parent": place })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(link_not_found(reach(format!("/api/l/{token}/content/{leaving}")).await).await);
    let response = reach(format!("/api/l/{token}/content/{doomed}")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let (status, _) = call(
        &server,
        Some(&ann),
        Method::DELETE,
        &format!("/api/nodes/{doomed}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(link_not_found(reach(format!("/api/l/{token}/content/{doomed}")).await).await);

    // A signed-in link's token reaches nothing anonymously.
    let (_, signed) = link(&server, &ann, &shared, "signed_in", "viewer", None).await;
    for path in [
        format!("/api/l/{signed}/children/{shared}"),
        format!("/api/l/{signed}/content/{inside}"),
        format!("/api/l/{signed}/text/{note}"),
    ] {
        assert!(link_not_found(reach(path).await).await);
    }

    // A link stops at its expiry instant, before any sweep.
    let (expiring_id, expiring) =
        link(&server, &ann, &shared, "anyone", "viewer", Some(in_days(1))).await;
    let response = reach(format!("/api/l/{expiring}/children/{shared}")).await;
    assert_eq!(response.status(), StatusCode::OK);
    sqlx::query("update links set expires_at = now() where id = $1")
        .bind(uuid_of(&expiring_id))
        .execute(server.db.db.test_pool())
        .await
        .unwrap();
    for path in [
        format!("/api/l/{expiring}"),
        format!("/api/l/{expiring}/children/{shared}"),
        format!("/api/l/{expiring}/content/{inside}"),
    ] {
        assert!(link_not_found(reach(path).await).await);
    }

    // Switching anyone links off stops every one at once.
    anyone_links(&server, false).await;
    for path in [
        format!("/api/l/{token}"),
        format!("/api/l/{token}/children/{shared}"),
        format!("/api/l/{token}/content/{inside}"),
        format!("/api/l/{token}/text/{note}"),
    ] {
        assert!(link_not_found(reach(path).await).await);
    }
}

#[tokio::test]
async fn anyone_link_shows_the_last_saved_text() {
    let server = server().await;
    let ann = server.user("Ann").await;
    anyone_links(&server, true).await;
    let place = home(&server, &ann).await;
    let note = text_doc(&server, &ann, &place, "Note.txt").await;
    let (_, token) = link(&server, &ann, &note, "anyone", "viewer", Some(in_days(1))).await;
    let doc = DocId(uuid_of(&note));
    let mut writer = TestClient::connect_as(&server, doc, &ann).await;
    writer.type_at(0, "saved words").await;
    wait_stored(&server, doc, "saved words").await;
    let (status, what) =
        status_and_json(anonymous(&server, &format!("/api/l/{token}")).await).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(what["item"]["editor"], "text");
    let (status, text) =
        status_and_json(anonymous(&server, &format!("/api/l/{token}/text/{note}")).await).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(text["text"], "saved words");
    assert_eq!(text["name"], "Note.txt");
    assert!(text["revision"].is_string());
}

#[tokio::test]
async fn anyone_link_never_opens_editors_or_access_pages() {
    let server = server().await;
    let ann = server.user("Ann").await;
    let bob = server.user("Bob").await;
    anyone_links(&server, true).await;
    let place = home(&server, &ann).await;
    let shared = folder(&server, &ann, &place, "Shared").await;
    let doc = text_doc(&server, &ann, &shared, "Note.txt").await;
    let (_, token) = link(&server, &ann, &shared, "anyone", "viewer", Some(in_days(1))).await;
    // The token as a session cookie is no session.
    let as_cookie = format!("sharecraft_session={token}");
    for path in [
        format!("/api/nodes/{shared}"),
        format!("/api/nodes/{shared}/children"),
        format!("/api/nodes/{shared}/access"),
        format!("/api/nodes/{shared}/links"),
        format!("/api/explorer/{shared}"),
        format!("/api/docs/{doc}"),
        format!("/api/docs/{doc}/content"),
        format!("/api/docs/{doc}/events"),
        "/api/me".to_owned(),
        "/api/requests".to_owned(),
        "/api/settings/sharing".to_owned(),
    ] {
        for cookie in [None, Some(as_cookie.as_str())] {
            let mut request = server.http(None).get(server.url(&path));
            if let Some(cookie) = cookie {
                request = request.header(COOKIE, cookie);
            }
            let response = send(request).await;
            assert!(!response.headers().contains_key(SET_COOKIE));
            let (status, body) = status_and_json(response).await;
            assert_eq!(
                (status, code(&body)),
                (StatusCode::UNAUTHORIZED, "not_signed_in"),
                "{path}"
            );
        }
    }
    let ws = format!("/ws/docs/{doc}");
    assert_eq!(
        handshake_status(ws_request(server.addr, &ws, None)).await,
        401
    );
    assert_eq!(
        handshake_status(ws_request(server.addr, &ws, Some(&as_cookie))).await,
        401
    );
    // An anyone link joins nobody, signed in or not.
    let (status, body) = redeem(&server, &bob, &token).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::NOT_FOUND, "link_not_found")
    );
    assert_eq!(access_of(&server, &bob, &doc).await, None);
}

#[tokio::test]
async fn sweeper_revokes_and_rechecks() {
    let server = server().await;
    let ann = server.user("Ann").await;
    let bob = server.user("Bob").await;
    anyone_links(&server, true).await;
    let place = home(&server, &ann).await;
    let doc = text_doc(&server, &ann, &place, "Plan.txt").await;
    let soon = OffsetDateTime::now_utc() + time::Duration::hours(1);
    let (signed_id, token) = link(&server, &ann, &doc, "signed_in", "editor", Some(soon)).await;
    let (anyone_id, anyone) = link(&server, &ann, &doc, "anyone", "viewer", Some(soon)).await;
    let (lasting_id, _) = link(&server, &ann, &doc, "signed_in", "viewer", Some(in_days(3))).await;
    let (status, _) = redeem(&server, &bob, &token).await;
    assert_eq!(status, StatusCode::OK);
    let mut bob_in = TestClient::connect_as(&server, DocId(uuid_of(&doc)), &bob).await;
    let state = server.app_state().await;

    // Nothing has expired yet.
    assert_eq!(
        server::sweep_once(&state, OffsetDateTime::now_utc()).await,
        0
    );
    // Two hours on, by the test's clock: both links that expired go, one at a time.
    let later = OffsetDateTime::now_utc() + time::Duration::hours(2);
    assert_eq!(server::sweep_once(&state, later).await, 2);
    removed_promptly(&mut bob_in).await;
    assert_eq!(access_of(&server, &bob, &doc).await, None);
    let (_, listed) = get(&server, Some(&ann), &format!("/api/nodes/{doc}/links")).await;
    for id in [&signed_id, &anyone_id] {
        let swept = listed
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l["id"] == id.as_str())
            .unwrap();
        assert_eq!(swept["revoked"]["reason"], "expired");
        assert!(swept["revoked"]["by"].is_null());
    }
    let lasting = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["id"] == lasting_id.as_str())
        .unwrap();
    assert!(lasting["revoked"].is_null());
    assert!(link_not_found(anonymous(&server, &format!("/api/l/{anyone}")).await).await);
    // Swept once only.
    assert_eq!(server::sweep_once(&state, later).await, 0);
}

#[tokio::test]
async fn sharing_settings() {
    let server = server().await;
    let ann = server.user("Ann").await;
    let nobody = server.user("Nobody").await;
    let admin = server.admin("Admin").await;
    // Ann manages her own folder; Nobody manages nothing.
    home(&server, &ann).await;
    let (status, settings) = get(&server, Some(&ann), "/api/settings/sharing").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        settings,
        json!({ "anyone_links": false, "anyone_link_max_days": 30 })
    );
    let (status, _) = get(&server, Some(&nobody), "/api/settings/sharing").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = get(&server, None, "/api/settings/sharing").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = get(&server, Some(&admin), "/api/settings/sharing").await;
    assert_eq!(status, StatusCode::OK);

    let put = |user: &TestUser, body: Value| {
        let server = &server;
        let user = user.clone();
        async move {
            call(
                server,
                Some(&user),
                Method::PUT,
                "/api/settings/sharing",
                Some(body),
            )
            .await
        }
    };
    let (status, _) = put(&ann, json!({ "anyone_links": true })).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, settings) = put(
        &admin,
        json!({ "anyone_links": true, "anyone_link_max_days": 7 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        settings,
        json!({ "anyone_links": true, "anyone_link_max_days": 7 })
    );
    for bad in [
        json!({}),
        json!({ "anyone_links": "yes" }),
        json!({ "anyone_link_max_days": 0 }),
        json!({ "anyone_link_max_days": 366 }),
        json!({ "anyone_links": false, "anyone_link_max_days": 1.5 }),
    ] {
        let (status, body) = put(&admin, bad).await;
        assert_eq!(
            (status, code(&body)),
            (StatusCode::UNPROCESSABLE_ENTITY, "invalid_request")
        );
    }
    // Nothing of a refused change was kept.
    let (_, settings) = get(&server, Some(&admin), "/api/settings/sharing").await;
    assert_eq!(
        settings,
        json!({ "anyone_links": true, "anyone_link_max_days": 7 })
    );
    // The new maximum applies to new links.
    let place = home(&server, &ann).await;
    let (status, body) =
        make_link(&server, &ann, &place, "anyone", "viewer", Some(in_days(8))).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::BAD_REQUEST, "expiry_too_long")
    );
    let (status, _) = make_link(&server, &ann, &place, "anyone", "viewer", Some(in_days(6))).await;
    assert_eq!(status, StatusCode::CREATED);
}
