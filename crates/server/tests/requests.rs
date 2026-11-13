//! Requests for access over HTTP: asking (always the same answer), the managers' list and
//! badge, approving and declining, and the requester's own decided requests.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use common::{Site, TestServer, TestUser, spawn_server};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use uuid::Uuid;

/// The one answer to every accepted request.
const SENT: &str = "If this item exists, the people who manage it will see your request.";

/// `method path` as `user` (or nobody), with a JSON `body`: the status and the JSON answer.
async fn call(
    site: &Site,
    user: Option<&TestUser>,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let (status, bytes) = raw(site, user, method, path, body).await;
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, body)
}

/// As [`call`], answering the body's bytes.
async fn raw(
    site: &Site,
    user: Option<&TestUser>,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Vec<u8>) {
    let mut request = site.http(user).request(method, site.url(path));
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.unwrap();
    (response.status(), response.bytes().await.unwrap().to_vec())
}

fn code(body: &Value) -> &str {
    body["error"].as_str().unwrap_or("")
}

/// A plain-text document owned by `owner`, in their own folder.
async fn owned_doc(server: &TestServer, owner: &TestUser, name: &str) -> String {
    let parent = server.home_of(owner).await.0.to_string();
    let (status, body) = call(
        server,
        Some(owner),
        Method::POST,
        "/api/docs",
        Some(json!({ "parent": parent, "name": name, "editor": "text" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["id"].as_str().unwrap().to_owned()
}

/// `user` asks for access to `node`.
async fn ask(
    server: &TestServer,
    user: &TestUser,
    node: &str,
    message: &str,
) -> (StatusCode, Value) {
    call(
        server,
        Some(user),
        Method::POST,
        &format!("/api/nodes/{node}/requests"),
        Some(json!({ "message": message })),
    )
    .await
}

/// The ids of the open requests `user` may decide.
async fn open_ids(server: &TestServer, user: &TestUser) -> Vec<String> {
    let (status, body) = call(server, Some(user), Method::GET, "/api/requests", None).await;
    assert_eq!(status, StatusCode::OK);
    body.as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect()
}

async fn badge(server: &TestServer, user: &TestUser) -> i64 {
    let (status, me) = call(server, Some(user), Method::GET, "/api/me", None).await;
    assert_eq!(status, StatusCode::OK);
    me["open_requests"].as_i64().unwrap()
}

async fn can_open(server: &TestServer, user: &TestUser, node: &str) -> Option<Value> {
    let (status, body) = call(
        server,
        Some(user),
        Method::GET,
        &format!("/api/nodes/{node}"),
        None,
    )
    .await;
    (status == StatusCode::OK).then(|| body["access"].clone())
}

#[tokio::test]
async fn request_round_trip() {
    let server = spawn_server().await;
    let ann = server.user("Ann").await;
    let bob = server.user("Bob").await;
    let doc = owned_doc(&server, &ann, "Plan.txt").await;
    assert_eq!(badge(&server, &ann).await, 0);
    assert_eq!(can_open(&server, &bob, &doc).await, None);

    let (status, answer) = ask(&server, &bob, &doc, "May I see the plan?").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(answer, json!({ "message": SENT }));

    // Ann's badge counts it, and her list shows it.
    assert_eq!(badge(&server, &ann).await, 1);
    assert_eq!(badge(&server, &bob).await, 0);
    let (_, open) = call(&server, Some(&ann), Method::GET, "/api/requests", None).await;
    let open = open.as_array().unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0]["node"]["id"], doc.as_str());
    assert_eq!(open[0]["node"]["name"], "Plan.txt");
    assert_eq!(open[0]["requester"]["id"], bob.id.0.to_string());
    assert_eq!(open[0]["message"], "May I see the plan?");
    assert!(!open[0]["path"].as_array().unwrap().is_empty());
    let id = open[0]["id"].as_str().unwrap().to_owned();
    // Bob does not see his open request, nor can he decide it.
    let (_, mine) = call(&server, Some(&bob), Method::GET, "/api/requests/mine", None).await;
    assert_eq!(mine, json!([]));
    assert!(open_ids(&server, &bob).await.is_empty());
    let (status, _) = call(
        &server,
        Some(&bob),
        Method::POST,
        &format!("/api/requests/{id}/approve"),
        Some(json!({ "role": "owner" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Approved: Bob can open the item with the role.
    let (status, _) = call(
        &server,
        Some(&ann),
        Method::POST,
        &format!("/api/requests/{id}/approve"),
        Some(json!({ "role": "editor" })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let access = can_open(&server, &bob, &doc).await.unwrap();
    assert_eq!(access["roles"][0]["id"], "editor");
    assert_eq!(badge(&server, &ann).await, 0);
    let (_, mine) = call(&server, Some(&bob), Method::GET, "/api/requests/mine", None).await;
    let mine = mine.as_array().unwrap();
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0]["decision"], "approved");
    assert_eq!(mine[0]["role"]["id"], "editor");
    assert_eq!(mine[0]["node"]["id"], doc.as_str());
    let (status, body) = call(
        &server,
        Some(&ann),
        Method::POST,
        &format!("/api/requests/{id}/approve"),
        Some(json!({ "role": "viewer" })),
    )
    .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::CONFLICT, "already_decided")
    );
}

#[tokio::test]
async fn request_answer_identical() {
    let server = spawn_server().await;
    let ann = server.user("Ann").await;
    let bob = server.user("Bob").await;
    let existing = owned_doc(&server, &ann, "Plan.txt").await;
    let his_own = owned_doc(&server, &bob, "Mine.txt").await;
    let missing = Uuid::new_v4().to_string();
    let mut answers = Vec::new();
    for node in [&existing, &missing, &his_own] {
        let (status, bytes) = raw(
            &server,
            Some(&bob),
            Method::POST,
            &format!("/api/nodes/{node}/requests"),
            Some(json!({ "message": "please" })),
        )
        .await;
        answers.push((status, bytes));
    }
    assert_eq!(answers[0].0, StatusCode::ACCEPTED);
    assert!(answers.iter().all(|answer| *answer == answers[0]));
    // Without a message too.
    let (status, bytes) = raw(
        &server,
        Some(&bob),
        Method::POST,
        &format!("/api/nodes/{missing}/requests"),
        Some(json!({})),
    )
    .await;
    assert_eq!((status, bytes), answers[0]);
    assert_eq!(open_ids(&server, &ann).await.len(), 1);
}

#[tokio::test]
async fn request_refusals() {
    let server = spawn_server().await;
    let ann = server.user("Ann").await;
    let bob = server.user("Bob").await;
    let carl = server.user("Carl").await;
    let doc = owned_doc(&server, &ann, "Plan.txt").await;

    // Asking.
    let (status, body) = ask(&server, &bob, &doc, &"x".repeat(501)).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::BAD_REQUEST, "message_too_long")
    );
    let (status, body) = call(
        &server,
        None,
        Method::POST,
        &format!("/api/nodes/{doc}/requests"),
        Some(json!({})),
    )
    .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNAUTHORIZED, "not_signed_in")
    );
    let (status, _) = ask(&server, &bob, "not-an-id", "").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _) = ask(&server, &bob, &doc, "please").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let id = open_ids(&server, &ann).await.pop().unwrap();

    // Deciding: only someone who manages the item, with a known request id.
    let (status, _) = call(
        &server,
        Some(&carl),
        Method::POST,
        &format!("/api/requests/{id}/decline"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(
        &server,
        Some(&ann),
        Method::POST,
        &format!("/api/requests/{}/approve", Uuid::new_v4()),
        Some(json!({ "role": "viewer" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(
        &server,
        Some(&ann),
        Method::POST,
        "/api/requests/nope/approve",
        Some(json!({ "role": "viewer" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, body) = call(
        &server,
        Some(&ann),
        Method::POST,
        &format!("/api/requests/{id}/approve"),
        Some(json!({ "role": "nonesuch" })),
    )
    .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_request")
    );
    // Bob already has editing another way: approving Viewer would give him nothing.
    let (status, _) = call(
        &server,
        Some(&ann),
        Method::PUT,
        &format!("/api/nodes/{doc}/access"),
        Some(json!({ "email": bob.email, "role": "editor" })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, body) = call(
        &server,
        Some(&ann),
        Method::POST,
        &format!("/api/requests/{id}/approve"),
        Some(json!({ "role": "viewer" })),
    )
    .await;
    assert_eq!((status, code(&body)), (StatusCode::CONFLICT, "already_has"));
    // Declined, with a reply Bob sees.
    let (status, body) = call(
        &server,
        Some(&ann),
        Method::POST,
        &format!("/api/requests/{id}/decline"),
        Some(json!({ "reply": "y".repeat(501) })),
    )
    .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::BAD_REQUEST, "message_too_long")
    );
    let (status, _) = call(
        &server,
        Some(&ann),
        Method::POST,
        &format!("/api/requests/{id}/decline"),
        Some(json!({ "reply": "You have it already." })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, body) = call(
        &server,
        Some(&ann),
        Method::POST,
        &format!("/api/requests/{id}/decline"),
        Some(json!({})),
    )
    .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::CONFLICT, "already_decided")
    );
    let (_, mine) = call(&server, Some(&bob), Method::GET, "/api/requests/mine", None).await;
    assert_eq!(mine[0]["decision"], "declined");
    assert_eq!(mine[0]["reply"], "You have it already.");
    assert!(mine[0]["role"].is_null());

    // The daily limit counts every submission, whatever the item.
    for _ in 0..18 {
        let (status, _) = ask(&server, &carl, &Uuid::new_v4().to_string(), "").await;
        assert_eq!(status, StatusCode::ACCEPTED);
    }
    let (status, _) = ask(&server, &carl, &doc, "").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let (status, _) = ask(&server, &carl, &doc, "").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    for node in [doc.clone(), Uuid::new_v4().to_string()] {
        let (status, body) = ask(&server, &carl, &node, "").await;
        assert_eq!(
            (status, code(&body)),
            (StatusCode::TOO_MANY_REQUESTS, "too_many_requests")
        );
    }
}

#[tokio::test]
async fn own_request_events_stay_hidden() {
    let server = spawn_server().await;
    let ann = server.user("Ann").await;
    let bob = server.user("Bob").await;
    let doc = owned_doc(&server, &ann, "Plan.txt").await;
    let (status, _) = ask(&server, &bob, &doc, "please").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let id = open_ids(&server, &ann).await.pop().unwrap();
    // Ann sees that Bob asked.
    let (status, events) = call(
        &server,
        Some(&ann),
        Method::GET,
        &format!("/api/docs/{doc}/events"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        events
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "request.created")
    );
    // Bob, made an owner, may read the events, but never his own request's.
    let (status, _) = call(
        &server,
        Some(&ann),
        Method::POST,
        &format!("/api/requests/{id}/approve"),
        Some(json!({ "role": "owner" })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, events) = call(
        &server,
        Some(&bob),
        Method::GET,
        &format!("/api/docs/{doc}/events"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let events = events.as_array().unwrap();
    assert!(events.iter().any(|e| e["kind"] == "request.approved"));
    assert!(events.iter().all(|e| e["kind"] != "request.created"));
}
