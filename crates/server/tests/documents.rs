//! Documents, sharing and people over HTTP.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use common::{TestServer, TestUser, spawn_server};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

fn error(code: &str) -> Value {
    json!({ "error": code })
}

/// Sends one request as `user` (or unsigned) and returns the status and JSON body (`null` when
/// the body is empty).
async fn call(
    server: &TestServer,
    user: Option<&TestUser>,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = server.http(user).request(method, server.url(path));
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.unwrap();
    let status = response.status();
    let bytes = response.bytes().await.unwrap();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, body)
}

/// Creates a document owned by `user` and returns its id.
async fn create_doc(server: &TestServer, user: &TestUser, name: &str) -> String {
    let (status, body) = call(
        server,
        Some(user),
        Method::POST,
        "/api/docs",
        Some(json!({ "name": name })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    body["id"].as_str().unwrap().to_owned()
}

/// Shares `doc` with `email` as `role`, as `owner`.
async fn share(server: &TestServer, owner: &TestUser, doc: &str, email: &str, role: &str) {
    let (status, _) = call(
        server,
        Some(owner),
        Method::PUT,
        &format!("/api/docs/{doc}/access"),
        Some(json!({ "email": email, "role": role })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn creator_owns_and_lists_the_new_document() {
    let server = spawn_server().await;
    let ann = server.user("ann").await;

    let (status, body) = call(
        &server,
        Some(&ann),
        Method::POST,
        "/api/docs",
        Some(json!({ "name": "  Plans " })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["name"], "Plans");
    assert_eq!(body["role"], "owner");
    let id = body["id"].as_str().unwrap().to_owned();

    // The name is optional.
    let (status, body) = call(
        &server,
        Some(&ann),
        Method::POST,
        "/api/docs",
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["name"], "Untitled");

    let (status, list) = call(&server, Some(&ann), Method::GET, "/api/docs", None).await;
    assert_eq!(status, StatusCode::OK);
    let list = list.as_array().unwrap();
    assert_eq!(list.len(), 2);
    let plans = list.iter().find(|d| d["id"] == id.as_str()).unwrap();
    assert_eq!(plans["name"], "Plans");
    assert_eq!(plans["role"], "owner");
    assert_eq!(plans["revised_at"], Value::Null);

    let (status, doc) = call(
        &server,
        Some(&ann),
        Method::GET,
        &format!("/api/docs/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        doc,
        json!({ "id": id, "name": "Plans", "role": "owner", "revision": null })
    );

    // Names the records layer refuses, and ids that are not ids.
    let (status, body) = call(
        &server,
        Some(&ann),
        Method::POST,
        "/api/docs",
        Some(json!({ "name": "a/b" })),
    )
    .await;
    assert_eq!(
        (status, body),
        (StatusCode::UNPROCESSABLE_ENTITY, error("invalid_name"))
    );
    let (status, body) = call(
        &server,
        Some(&ann),
        Method::GET,
        "/api/docs/not-an-id",
        None,
    )
    .await;
    assert_eq!(
        (status, body),
        (StatusCode::UNPROCESSABLE_ENTITY, error("invalid_request"))
    );
}

#[tokio::test]
async fn stranger_gets_404_for_document_and_its_access_list() {
    let server = spawn_server().await;
    let ann = server.user("ann").await;
    let bob = server.user("bob").await;
    let id = create_doc(&server, &ann, "Secret").await;
    let unknown = uuid::Uuid::new_v4();

    let (status, body) = call(
        &server,
        Some(&bob),
        Method::GET,
        &format!("/api/docs/{id}"),
        None,
    )
    .await;
    assert_eq!((status, body), (StatusCode::NOT_FOUND, error("not_found")));
    let (status, body) = call(
        &server,
        Some(&bob),
        Method::GET,
        &format!("/api/docs/{unknown}"),
        None,
    )
    .await;
    assert_eq!((status, body), (StatusCode::NOT_FOUND, error("not_found")));

    let (status, body) = call(
        &server,
        Some(&bob),
        Method::GET,
        &format!("/api/docs/{id}/access"),
        None,
    )
    .await;
    assert_eq!((status, body), (StatusCode::NOT_FOUND, error("not_found")));
    let (status, body) = call(
        &server,
        Some(&bob),
        Method::PUT,
        &format!("/api/docs/{id}/access"),
        Some(json!({ "email": ann.email, "role": "viewer" })),
    )
    .await;
    assert_eq!((status, body), (StatusCode::NOT_FOUND, error("not_found")));

    let (_, list) = call(&server, Some(&bob), Method::GET, "/api/docs", None).await;
    assert_eq!(list, json!([]));
}

#[tokio::test]
async fn owner_shares_by_email_and_the_grantee_lists_the_document() {
    let server = spawn_server().await;
    let ann = server.user("ann").await;
    let bob = server.user("bob").await;
    let id = create_doc(&server, &ann, "Shared").await;

    share(&server, &ann, &id, &bob.email, "viewer").await;
    let (_, list) = call(&server, Some(&bob), Method::GET, "/api/docs", None).await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["id"], id.as_str());
    assert_eq!(list[0]["role"], "viewer");
    let (status, doc) = call(
        &server,
        Some(&bob),
        Method::GET,
        &format!("/api/docs/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(doc["role"], "viewer");

    // Changing the role replaces it.
    share(&server, &ann, &id, &bob.email, "editor").await;
    let (_, access) = call(
        &server,
        Some(&ann),
        Method::GET,
        &format!("/api/docs/{id}/access"),
        None,
    )
    .await;
    let access = access.as_array().unwrap();
    assert_eq!(access.len(), 2);
    let row = access
        .iter()
        .find(|r| r["user"]["email"] == bob.email.as_str())
        .unwrap();
    assert_eq!(row["role"], "editor");
    assert_eq!(row["user"]["id"], bob.id.0.to_string());
    assert!(row["user"]["name"].is_string());

    // Removing it takes the document away again.
    let (status, _) = call(
        &server,
        Some(&ann),
        Method::DELETE,
        &format!("/api/docs/{id}/access/{}", bob.id.0),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, list) = call(&server, Some(&bob), Method::GET, "/api/docs", None).await;
    assert_eq!(list, json!([]));

    // An unknown role is not a request we understand.
    let (status, body) = call(
        &server,
        Some(&ann),
        Method::PUT,
        &format!("/api/docs/{id}/access"),
        Some(json!({ "email": bob.email, "role": "boss" })),
    )
    .await;
    assert_eq!(
        (status, body),
        (StatusCode::UNPROCESSABLE_ENTITY, error("invalid_request"))
    );
}

#[tokio::test]
async fn editor_gets_403_managing_access() {
    let server = spawn_server().await;
    let ann = server.user("ann").await;
    let bob = server.user("bob").await;
    let cy = server.user("cy").await;
    let id = create_doc(&server, &ann, "Shared").await;
    share(&server, &ann, &id, &bob.email, "editor").await;

    let forbidden = (StatusCode::FORBIDDEN, error("forbidden"));
    let path = format!("/api/docs/{id}/access");
    let (status, body) = call(&server, Some(&bob), Method::GET, &path, None).await;
    assert_eq!((status, body), forbidden);
    let (status, body) = call(
        &server,
        Some(&bob),
        Method::PUT,
        &path,
        Some(json!({ "email": cy.email, "role": "viewer" })),
    )
    .await;
    assert_eq!((status, body), forbidden);
    // Not even to learn whether an email has an account.
    let (status, body) = call(
        &server,
        Some(&bob),
        Method::PUT,
        &path,
        Some(json!({ "email": "nobody@test.invalid", "role": "viewer" })),
    )
    .await;
    assert_eq!((status, body), forbidden);
    let (status, body) = call(
        &server,
        Some(&bob),
        Method::DELETE,
        &format!("{path}/{}", ann.id.0),
        None,
    )
    .await;
    assert_eq!((status, body), forbidden);
}

#[tokio::test]
async fn unknown_email_is_no_such_user() {
    let server = spawn_server().await;
    let ann = server.user("ann").await;
    let id = create_doc(&server, &ann, "Shared").await;
    let (status, body) = call(
        &server,
        Some(&ann),
        Method::PUT,
        &format!("/api/docs/{id}/access"),
        Some(json!({ "email": "nobody@test.invalid", "role": "viewer" })),
    )
    .await;
    assert_eq!(
        (status, body),
        (StatusCode::NOT_FOUND, error("no_such_user"))
    );
}

#[tokio::test]
async fn last_owner_cannot_be_removed() {
    let server = spawn_server().await;
    let ann = server.user("ann").await;
    let bob = server.user("bob").await;
    let id = create_doc(&server, &ann, "Mine").await;

    let remove_ann = format!("/api/docs/{id}/access/{}", ann.id.0);
    let (status, body) = call(&server, Some(&ann), Method::DELETE, &remove_ann, None).await;
    assert_eq!((status, body), (StatusCode::CONFLICT, error("last_owner")));
    // Nor can she demote herself.
    let (status, body) = call(
        &server,
        Some(&ann),
        Method::PUT,
        &format!("/api/docs/{id}/access"),
        Some(json!({ "email": ann.email, "role": "editor" })),
    )
    .await;
    assert_eq!((status, body), (StatusCode::CONFLICT, error("last_owner")));

    // With a second owner she can leave.
    share(&server, &ann, &id, &bob.email, "owner").await;
    let (status, _) = call(&server, Some(&ann), Method::DELETE, &remove_ann, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn admin_can_open_and_share_any_document() {
    let server = spawn_server().await;
    let ann = server.user("ann").await;
    let bob = server.user("bob").await;
    let root = server.admin("root").await;
    let id = create_doc(&server, &ann, "Private").await;

    let (status, doc) = call(
        &server,
        Some(&root),
        Method::GET,
        &format!("/api/docs/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(doc["role"], "owner");
    share(&server, &root, &id, &bob.email, "viewer").await;
    let (_, list) = call(&server, Some(&bob), Method::GET, "/api/docs", None).await;
    assert_eq!(list[0]["id"], id.as_str());
}

#[tokio::test]
async fn admin_adds_and_disables_a_person() {
    let server = spawn_server().await;
    let root = server.admin("root").await;

    let new = json!({
        "email": "dee@test.invalid",
        "name": "Dee",
        "password": "a long enough password",
        "is_admin": false,
    });
    let (status, body) = call(
        &server,
        Some(&root),
        Method::POST,
        "/api/admin/users",
        Some(new.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let dee = body["id"].as_str().unwrap().to_owned();

    let (status, body) = call(
        &server,
        Some(&root),
        Method::POST,
        "/api/admin/users",
        Some(new),
    )
    .await;
    assert_eq!((status, body), (StatusCode::CONFLICT, error("email_taken")));
    for (field, bad, code) in [
        ("email", "nope", "invalid_email"),
        ("name", "  ", "invalid_name"),
        ("password", "short", "weak_password"),
    ] {
        let mut person = json!({
            "email": "eve@test.invalid",
            "name": "Eve",
            "password": "a long enough password",
            "is_admin": false,
        });
        person[field] = json!(bad);
        let (status, body) = call(
            &server,
            Some(&root),
            Method::POST,
            "/api/admin/users",
            Some(person),
        )
        .await;
        assert_eq!(
            (status, body),
            (StatusCode::UNPROCESSABLE_ENTITY, error(code)),
            "{field}"
        );
    }

    let (status, users) = call(&server, Some(&root), Method::GET, "/api/admin/users", None).await;
    assert_eq!(status, StatusCode::OK);
    let row = users
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["id"] == dee.as_str())
        .unwrap();
    assert_eq!(
        *row,
        json!({ "id": dee, "email": "dee@test.invalid", "name": "Dee", "is_admin": false, "disabled": false })
    );

    // Dee signs in, is disabled, and her session stops working.
    let response = server
        .http(None)
        .post(server.url("/api/session"))
        .json(&json!({ "email": "dee@test.invalid", "password": "a long enough password" }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let dee_user = TestUser {
        id: collab_core::ids::UserId(dee.parse().unwrap()),
        email: "dee@test.invalid".to_owned(),
        cookie: common::session_cookie(response.headers()).unwrap(),
    };
    let (status, _) = call(&server, Some(&dee_user), Method::GET, "/api/me", None).await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = call(
        &server,
        Some(&root),
        Method::POST,
        &format!("/api/admin/users/{dee}/disable"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(&server, Some(&dee_user), Method::GET, "/api/me", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (_, users) = call(&server, Some(&root), Method::GET, "/api/admin/users", None).await;
    let row = users
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["id"] == dee.as_str())
        .unwrap();
    assert_eq!(row["disabled"], true);

    let (status, _) = call(
        &server,
        Some(&root),
        Method::POST,
        &format!("/api/admin/users/{dee}/enable"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // The only administrator cannot be disabled.
    let (status, body) = call(
        &server,
        Some(&root),
        Method::POST,
        &format!("/api/admin/users/{}/disable", root.id.0),
        None,
    )
    .await;
    assert_eq!((status, body), (StatusCode::CONFLICT, error("last_admin")));
    let (status, body) = call(
        &server,
        Some(&root),
        Method::POST,
        &format!("/api/admin/users/{}/disable", uuid::Uuid::new_v4()),
        None,
    )
    .await;
    assert_eq!((status, body), (StatusCode::NOT_FOUND, error("not_found")));
}

#[tokio::test]
async fn non_admin_gets_403_from_admin_routes() {
    let server = spawn_server().await;
    let ann = server.user("ann").await;
    let other = uuid::Uuid::new_v4();
    let person = json!({
        "email": "dee@test.invalid",
        "name": "Dee",
        "password": "a long enough password",
        "is_admin": false,
    });
    let routes = [
        (Method::GET, "/api/admin/users".to_owned(), None),
        (Method::POST, "/api/admin/users".to_owned(), Some(person)),
        (
            Method::POST,
            format!("/api/admin/users/{other}/disable"),
            None,
        ),
        (
            Method::POST,
            format!("/api/admin/users/{other}/enable"),
            None,
        ),
        // Not even a malformed id gets past the administrator check.
        (
            Method::POST,
            "/api/admin/users/not-an-id/disable".to_owned(),
            None,
        ),
    ];
    for (method, path, body) in routes {
        let (status, answer) = call(&server, Some(&ann), method.clone(), &path, body).await;
        assert_eq!(
            (status, answer),
            (StatusCode::FORBIDDEN, error("forbidden")),
            "{method} {path}"
        );
    }
}

#[tokio::test]
async fn unsigned_caller_gets_401_from_every_route() {
    let server = spawn_server().await;
    let id = uuid::Uuid::new_v4();
    let docs_access = format!("/api/docs/{id}/access");
    // One row per route; add new routes here.
    let routes: Vec<(Method, String, Option<Value>)> = vec![
        (Method::GET, "/api/me".into(), None),
        (Method::DELETE, "/api/session".into(), None),
        (Method::GET, "/api/docs".into(), None),
        (Method::POST, "/api/docs".into(), Some(json!({}))),
        (Method::GET, format!("/api/docs/{id}"), None),
        (Method::GET, docs_access.clone(), None),
        (Method::GET, format!("/api/docs/{id}/events"), None),
        (
            Method::PUT,
            docs_access,
            Some(json!({ "email": "a@test.invalid", "role": "viewer" })),
        ),
        (Method::DELETE, format!("/api/docs/{id}/access/{id}"), None),
        (Method::GET, "/api/admin/users".into(), None),
        (
            Method::POST,
            "/api/admin/users".into(),
            Some(
                json!({ "email": "a@test.invalid", "name": "A", "password": "a long enough password", "is_admin": false }),
            ),
        ),
        (Method::POST, format!("/api/admin/users/{id}/disable"), None),
        (Method::POST, format!("/api/admin/users/{id}/enable"), None),
    ];
    for (method, path, body) in routes {
        let (status, answer) = call(&server, None, method.clone(), &path, body).await;
        assert_eq!(
            (status, answer),
            (StatusCode::UNAUTHORIZED, error("not_signed_in")),
            "{method} {path}"
        );
    }
}
