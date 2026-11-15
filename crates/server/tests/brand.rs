//! The brand centre over HTTP: who may read and change it, its themes (names, limits and
//! contrast), brand images chosen from asset libraries (judged by content, within limits,
//! available), serving them to anyone signed in with safe headers, inputs that could run code,
//! pinning their content, and the records model's failure paths (a change refused by the
//! database taken back from Git, the reconcile at start rebuilding the mirror from Git).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::io::Cursor;

use common::{Site, StoppableServer, TestServer, TestUser, spawn_server};
use image::{DynamicImage, ImageFormat, Rgb, RgbImage};
use records::test_support::TestDb;
use reqwest::{Method, StatusCode};
use revisions::brand::{Colour, OrgBrand, OrgTheme};
use revisions::{Author, RevisionStore, SitesChange};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

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
    let response = request.send().await.unwrap();
    let status = response.status();
    let text = response.text().await.unwrap();
    let body = if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).unwrap_or(Value::String(text))
    };
    (status, body)
}

/// `GET /api/brand` as `user`; fails the test unless it answers.
async fn centre(site: &Site, user: &TestUser) -> Value {
    let (status, body) = call(site, Some(user), Method::GET, "/api/brand", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

/// A new site made by `admin`: its id, its `Documents` library and a new asset library.
async fn site_with_assets(site: &Site, admin: &TestUser, name: &str) -> (String, String, String) {
    let (status, body) = call(
        site,
        Some(admin),
        Method::POST,
        "/api/sites",
        Some(json!({ "name": name })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["id"].as_str().unwrap().to_owned();
    let (status, page) = call(
        site,
        Some(admin),
        Method::GET,
        &format!("/api/nodes/{id}/children"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let documents = page["items"][0]["id"].as_str().unwrap().to_owned();
    let (status, body) = call(
        site,
        Some(admin),
        Method::POST,
        &format!("/api/sites/{id}/libraries"),
        Some(json!({ "name": "Brand", "kind": "asset" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let assets = body["id"].as_str().unwrap().to_owned();
    (id, documents, assets)
}

/// Uploads `content` as `name` into `parent`; the new document's id.
async fn upload(site: &Site, user: &TestUser, parent: &str, name: &str, content: &[u8]) -> String {
    let response = site
        .http(Some(user))
        .post(site.url("/api/docs/upload"))
        .query(&[("parent", parent), ("name", name)])
        .body(content.to_vec())
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body: Value = response.json().await.unwrap();
    assert_eq!(status, StatusCode::CREATED, "{name}: {body}");
    body["id"].as_str().unwrap().to_owned()
}

/// Gives `user` the role `role` on `node`, as `admin`.
async fn grant(site: &Site, admin: &TestUser, node: &str, user: &TestUser, role: &str) {
    let (status, body) = call(
        site,
        Some(admin),
        Method::PUT,
        &format!("/api/nodes/{node}/access"),
        Some(json!({ "email": user.email, "role": role })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
}

/// Names `users` the brand managers, as `admin`.
async fn managers(site: &Site, admin: &TestUser, users: &[&TestUser]) {
    let ids: Vec<String> = users.iter().map(|u| u.id.0.to_string()).collect();
    let (status, body) = call(
        site,
        Some(admin),
        Method::PUT,
        "/api/brand/managers",
        Some(json!({ "users": ids })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
}

/// `POST /api/brand/themes` as `user`.
async fn new_theme(site: &Site, user: &TestUser, theme: Value) -> (StatusCode, Value) {
    call(
        site,
        Some(user),
        Method::POST,
        "/api/brand/themes",
        Some(theme),
    )
    .await
}

/// `POST /api/brand/images` as `user`.
async fn new_image(site: &Site, user: &TestUser, body: Value) -> (StatusCode, Value) {
    call(
        site,
        Some(user),
        Method::POST,
        "/api/brand/images",
        Some(body),
    )
    .await
}

/// `GET` of a brand image as `user` (signed out with `None`): status, headers and bytes.
async fn fetch(
    site: &Site,
    user: Option<&TestUser>,
    url: &str,
) -> (StatusCode, reqwest::header::HeaderMap, Vec<u8>) {
    let response = site.http(user).get(site.url(url)).send().await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    (status, headers, response.bytes().await.unwrap().to_vec())
}

/// An RGB image `width` × `height` in `format`.
fn picture(width: u32, height: u32, format: ImageFormat) -> Vec<u8> {
    let image = RgbImage::from_fn(width, height, |x, y| {
        Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8])
    });
    let mut out = Vec::new();
    DynamicImage::ImageRgb8(image)
        .write_to(&mut Cursor::new(&mut out), format)
        .unwrap();
    out
}

fn png(width: u32, height: u32) -> Vec<u8> {
    picture(width, height, ImageFormat::Png)
}

fn sha(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hash(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// The details of the events of `kind`, oldest first.
async fn events(site: &Site, kind: &str) -> Vec<Value> {
    sqlx::query_scalar("select detail from events where kind = $1 order by id")
        .bind(kind)
        .fetch_all(site.records_db().test_pool())
        .await
        .unwrap()
}

/// The brand centre as Git holds it.
fn in_git(server: &TestServer) -> OrgBrand {
    server.store().read_org_brand().unwrap().unwrap_or_default()
}

fn ocean() -> Value {
    json!({ "name": "Ocean", "primary": "#1F5F8B", "secondary": "#163447" })
}

#[tokio::test]
async fn brand_centre_needs_manager_or_admin() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let mia = server.user("Mia").await;
    let ned = server.user("Ned").await;

    // Anyone signed in reads it; nobody signed out does.
    let (status, _) = call(&server, None, Method::GET, "/api/brand", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let seen = centre(&server, &mia).await;
    assert_eq!(seen["can_manage"], false);
    assert!(seen.get("managers").is_none(), "{seen}");
    let themes = seen["themes"].as_array().unwrap();
    assert_eq!(themes.len(), 6);
    assert_eq!(themes[0]["id"], "builtin:sharecraft");
    assert_eq!(themes[0]["resolved"]["dark"]["accent"], "#748ddd");
    assert_eq!(seen["images"], json!([]));
    assert_eq!(centre(&server, &root).await["can_manage"], true);
    assert_eq!(centre(&server, &root).await["managers"], json!([]));

    // Nobody else changes it.
    let (status, body) = new_theme(&server, &mia, ocean()).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    managers(&server, &root, &[&mia]).await;
    assert_eq!(centre(&server, &mia).await["can_manage"], true);
    let (status, body) = new_theme(&server, &mia, ocean()).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["id"].as_str().unwrap().to_owned();

    for (method, path, body) in [
        (Method::POST, "/api/brand/themes".to_owned(), Some(ocean())),
        (
            Method::PATCH,
            format!("/api/brand/themes/{id}"),
            Some(json!({ "name": "Mine" })),
        ),
        (Method::DELETE, format!("/api/brand/themes/{id}"), None),
        (
            Method::POST,
            "/api/brand/images".to_owned(),
            Some(json!({ "doc": Uuid::new_v4(), "kind": "logo" })),
        ),
        (
            Method::DELETE,
            format!("/api/brand/images/{}", Uuid::new_v4()),
            None,
        ),
    ] {
        let (status, answer) = call(&server, Some(&ned), method.clone(), &path, body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}: {answer}");
    }

    // A manager whose account is disabled no longer changes it.
    let (status, _) = call(
        &server,
        Some(&root),
        Method::POST,
        &format!("/api/admin/users/{}/disable", mia.id.0),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let db = server.records_db();
    assert!(!db.can_manage_brand(mia.id).await.unwrap());
    let actor = db.user(mia.id).await.unwrap().unwrap();
    assert!(actor.disabled);
    assert_eq!(in_git(&server).themes.len(), 1);
}

#[tokio::test]
async fn managers_set_by_admins_only() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let mia = server.user("Mia").await;
    let ned = server.user("Ned").await;
    let gone = server.user("Gone").await;
    managers(&server, &root, &[&mia]).await;

    // A brand manager does not name managers.
    let (status, body) = call(
        &server,
        Some(&mia),
        Method::PUT,
        "/api/brand/managers",
        Some(json!({ "users": [ned.id.0] })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    let put = |users: Value| {
        let server = &server;
        let root = &root;
        async move {
            call(
                server,
                Some(root),
                Method::PUT,
                "/api/brand/managers",
                Some(json!({ "users": users })),
            )
            .await
        }
    };
    let (status, body) = put(json!([Uuid::new_v4()])).await;
    assert_eq!(
        (status, &body["error"]),
        (StatusCode::NOT_FOUND, &json!("no_such_user"))
    );
    let (status, _) = call(
        &server,
        Some(&root),
        Method::POST,
        &format!("/api/admin/users/{}/disable", gone.id.0),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, body) = put(json!([gone.id.0])).await;
    assert_eq!(
        (status, &body["error"]),
        (StatusCode::NOT_FOUND, &json!("no_such_user"))
    );
    let (status, body) = put(json!(["not an id"])).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let many: Vec<Uuid> = (0..21).map(|_| Uuid::new_v4()).collect();
    let (status, body) = put(json!(many)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body, json!({ "error": "too_many", "limit": 20 }));

    // Named twice counts once.
    let (status, body) = put(json!([ned.id.0, mia.id.0, ned.id.0])).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let seen = centre(&server, &root).await;
    let names: Vec<&str> = seen["managers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Ned", "Mia"]);
    assert_eq!(in_git(&server).managers, vec![ned.id.0, mia.id.0]);
    let changed = events(&server, "brand.managers_changed").await;
    assert_eq!(changed.len(), 2, "{changed:?}");
    assert_eq!(changed[1]["from"], json!([mia.id.0]));
    assert_eq!(changed[1]["to"], json!([ned.id.0, mia.id.0]));
    // The same managers again change nothing.
    let (status, _) = put(json!([ned.id.0, mia.id.0])).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(events(&server, "brand.managers_changed").await.len(), 2);
}

#[tokio::test]
async fn themes_created_renamed_deleted() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;

    let (status, body) = new_theme(&server, &root, ocean()).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["id"].as_str().unwrap().to_owned();
    assert_eq!(body["from"], format!("org:{id}"));
    assert_eq!(body["primary"], "#1f5f8b");
    assert_eq!(body["primary_dark"], Value::Null);
    assert!(body["resolved"]["dark"]["accent"].is_string());
    let seen = centre(&server, &root).await;
    assert_eq!(seen["themes"].as_array().unwrap().len(), 7);
    assert_eq!(seen["themes"][6]["name"], "Ocean");

    // Names are unique ignoring case, on create and on rename.
    let (status, body) = new_theme(
        &server,
        &root,
        json!({ "name": "OCEAN", "primary": "#3156c9", "secondary": "#233239" }),
    )
    .await;
    assert_eq!(
        (status, &body["error"]),
        (StatusCode::CONFLICT, &json!("name_taken"))
    );
    let (status, body) = new_theme(
        &server,
        &root,
        json!({ "name": "Forest floor", "primary": "#2f6b3a", "secondary": "#1f3324" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let forest = body["id"].as_str().unwrap().to_owned();
    let (status, body) = call(
        &server,
        Some(&root),
        Method::PATCH,
        &format!("/api/brand/themes/{forest}"),
        Some(json!({ "name": "ocean" })),
    )
    .await;
    assert_eq!(
        (status, &body["error"]),
        (StatusCode::CONFLICT, &json!("name_taken"))
    );
    let (status, body) = call(
        &server,
        Some(&root),
        Method::PATCH,
        &format!("/api/brand/themes/{id}"),
        Some(json!({ "name": "Sea", "primary_dark": "#9DB4FF" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        (&body["name"], &body["primary"], &body["primary_dark"]),
        (&json!("Sea"), &json!("#1f5f8b"), &json!("#9db4ff"))
    );
    // A dark colour set to null is derived again.
    let (status, body) = call(
        &server,
        Some(&root),
        Method::PATCH,
        &format!("/api/brand/themes/{id}"),
        Some(json!({ "primary_dark": null })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["primary_dark"], Value::Null);

    // Unreadable colours are refused, naming each failing pair with a suggestion.
    let (status, body) = new_theme(
        &server,
        &root,
        json!({ "name": "Pale", "primary": "#cccccc", "secondary": "#233239" }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"], "low_contrast");
    let pairs = body["pairs"].as_array().unwrap();
    assert_eq!(pairs[0]["check"], "primary_on_light");
    assert!(pairs[0]["ratio"].as_f64().unwrap() < 4.5);
    assert!(pairs[0]["suggestion"].is_string());
    let (status, body) = call(
        &server,
        Some(&root),
        Method::PATCH,
        &format!("/api/brand/themes/{id}"),
        Some(json!({ "secondary_dark": "#7b7b7b" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["pairs"][0]["check"], "header_on_secondary_dark");
    let (status, body) = new_theme(
        &server,
        &root,
        json!({ "name": " ", "primary": "#3156c9", "secondary": "#233239" }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"], "invalid_theme");
    assert!(body["reason"].is_string());

    // At most 50.
    for n in 2..50 {
        let (status, body) = new_theme(
            &server,
            &root,
            json!({ "name": format!("Theme {n}"), "primary": "#3156c9", "secondary": "#233239" }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    let (status, body) = new_theme(
        &server,
        &root,
        json!({ "name": "One too many", "primary": "#3156c9", "secondary": "#233239" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body, json!({ "error": "too_many", "limit": 50 }));

    // Deleted, then gone.
    let (status, _) = call(
        &server,
        Some(&root),
        Method::DELETE,
        &format!("/api/brand/themes/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(
        &server,
        Some(&root),
        Method::DELETE,
        &format!("/api/brand/themes/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let git = in_git(&server);
    assert_eq!(git.themes.len(), 49);
    assert!(git.themes.iter().all(|t| t.id.to_string() != id));
    let changed = events(&server, "brand.theme_changed").await;
    let actions: Vec<&str> = changed
        .iter()
        .map(|d| d["action"].as_str().unwrap())
        .collect();
    assert_eq!(actions.first(), Some(&"created"));
    assert_eq!(actions.iter().filter(|a| **a == "updated").count(), 2);
    assert_eq!(actions.last(), Some(&"deleted"));
}

#[tokio::test]
async fn image_from_asset_library_needs_view() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let mia = server.user("Mia").await;
    managers(&server, &root, &[&mia]).await;
    let (site, documents, assets) = site_with_assets(&server, &root, "Marketing").await;
    let logo = png(40, 20);
    let doc = upload(&server, &root, &assets, "Logo.png", &logo).await;

    // Mia cannot view it.
    let (status, body) = new_image(&server, &mia, json!({ "doc": doc, "kind": "logo" })).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    grant(&server, &root, &site, &mia, "viewer").await;
    let (status, body) = new_image(&server, &mia, json!({ "doc": doc, "kind": "logo" })).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["kind"], "logo");
    assert_eq!(body["name"], "Logo.png");
    assert_eq!(body["url"], format!("/api/brand/images/{}", sha(&logo)));
    let git = in_git(&server);
    let image = &git.images[0].image;
    assert_eq!(
        (image.width, image.height, image.bytes),
        (Some(40), Some(20), logo.len() as u64)
    );
    assert_eq!(image.from_doc.to_string(), doc);

    // Only from an asset library.
    let elsewhere = upload(&server, &root, &documents, "Other.png", &png(4, 4)).await;
    let (status, body) =
        new_image(&server, &mia, json!({ "doc": elsewhere, "kind": "logo" })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "wrong_place");
    let (status, _) = new_image(
        &server,
        &mia,
        json!({ "doc": Uuid::new_v4(), "kind": "logo" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Not from the recycle bin.
    let binned = upload(&server, &root, &assets, "Old.png", &png(5, 5)).await;
    let (status, _) = call(
        &server,
        Some(&root),
        Method::DELETE,
        &format!("/api/nodes/{binned}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = new_image(&server, &root, json!({ "doc": binned, "kind": "logo" })).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // A name may be given.
    let (status, body) = new_image(
        &server,
        &mia,
        json!({ "doc": doc, "kind": "icon", "name": "  Small mark " }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["name"], "Small mark");
    let added = events(&server, "brand.image_changed").await;
    assert_eq!(added.len(), 2);
    assert_eq!(added[0]["action"], "added");
}

#[tokio::test]
async fn image_type_judged_by_content() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let (_, _, assets) = site_with_assets(&server, &root, "Marketing").await;
    let cases: [(&str, Vec<u8>, &str); 4] = [
        ("Logo.svg", png(3, 3), "image/png"),
        ("Photo.png", picture(6, 4, ImageFormat::Jpeg), "image/jpeg"),
        (
            "Mark.png",
            b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>".to_vec(),
            "image/svg+xml",
        ),
        ("Moving.png", picture(2, 2, ImageFormat::Gif), "image/gif"),
    ];
    for (name, bytes, media_type) in cases {
        let doc = upload(&server, &root, &assets, name, &bytes).await;
        let (status, body) = new_image(&server, &root, json!({ "doc": doc, "kind": "logo" })).await;
        assert_eq!(status, StatusCode::CREATED, "{name}: {body}");
        let (status, headers, served) =
            fetch(&server, Some(&root), body["url"].as_str().unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers["content-type"], media_type, "{name}");
        assert_eq!(served, bytes);
    }
    let html = upload(
        &server,
        &root,
        &assets,
        "Page.png",
        b"<html><body>Hi</body></html>",
    )
    .await;
    let (status, body) = new_image(&server, &root, json!({ "doc": html, "kind": "logo" })).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"], "not_an_image");
}

#[tokio::test]
async fn image_limits_refused() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let (_, _, assets) = site_with_assets(&server, &root, "Marketing").await;
    // A real PNG a little over 1 MiB: bytes after its end are ignored by readers.
    let mut big = png(8, 8);
    big.resize(1024 * 1024 + 1, 0);
    let doc = upload(&server, &root, &assets, "Big.png", &big).await;
    for kind in ["logo", "icon"] {
        let (status, body) = new_image(&server, &root, json!({ "doc": doc, "kind": kind })).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
        assert_eq!(body["error"], "image_too_large");
    }
    let (status, body) =
        new_image(&server, &root, json!({ "doc": doc, "kind": "background" })).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let mut huge = png(8, 8);
    huge.resize(5 * 1024 * 1024 + 1, 0);
    let doc = upload(&server, &root, &assets, "Huge.png", &huge).await;
    let (status, _) = new_image(&server, &root, json!({ "doc": doc, "kind": "background" })).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    // Too many pixels on a side, read from the header.
    for (name, bytes) in [("Wide.png", png(8001, 1)), ("Tall.png", png(1, 8001))] {
        let doc = upload(&server, &root, &assets, name, &bytes).await;
        let (status, body) =
            new_image(&server, &root, json!({ "doc": doc, "kind": "background" })).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{name}: {body}");
    }
    let edge = upload(&server, &root, &assets, "Edge.png", &png(8000, 1)).await;
    let (status, body) =
        new_image(&server, &root, json!({ "doc": edge, "kind": "background" })).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    // At most 100 images.
    let small = upload(&server, &root, &assets, "Small.png", &png(2, 2)).await;
    for _ in 2..100 {
        let (status, body) =
            new_image(&server, &root, json!({ "doc": small, "kind": "icon" })).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    let (status, body) = new_image(&server, &root, json!({ "doc": small, "kind": "icon" })).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body, json!({ "error": "too_many", "limit": 100 }));
}

/// Sets the scan state of the content `bytes`, as a scan or rescan would.
async fn set_scan(site: &Site, bytes: &[u8], scan: &str) {
    sqlx::query("update blobs set scan = $1, scan_detail = $2 where sha256 = $3")
        .bind(scan)
        .bind((scan == "infected").then_some("Test-Signature"))
        .bind(sha(bytes))
        .execute(site.records_db().test_pool())
        .await
        .unwrap();
}

#[tokio::test]
async fn held_image_refused_and_served_as_held() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let (_, _, assets) = site_with_assets(&server, &root, "Marketing").await;
    let logo = png(10, 10);
    let doc = upload(&server, &root, &assets, "Logo.png", &logo).await;

    set_scan(&server, &logo, "infected").await;
    let (status, body) = new_image(&server, &root, json!({ "doc": doc, "kind": "logo" })).await;
    assert_eq!(status, StatusCode::LOCKED, "{body}");
    assert_eq!(body, json!({ "error": "held", "scan": "infected" }));
    assert!(in_git(&server).images.is_empty());

    set_scan(&server, &logo, "clean").await;
    let (status, body) = new_image(&server, &root, json!({ "doc": doc, "kind": "logo" })).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let url = body["url"].as_str().unwrap().to_owned();
    assert_eq!(fetch(&server, Some(&root), &url).await.0, StatusCode::OK);

    // A rescan finds it infected: it is held, not served.
    set_scan(&server, &logo, "infected").await;
    let (status, _, bytes) = fetch(&server, Some(&root), &url).await;
    assert_eq!(status, StatusCode::LOCKED);
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body, json!({ "error": "held", "scan": "infected" }));
}

#[tokio::test]
async fn brand_images_served_to_anyone_signed_in() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let ned = server.user("Ned").await;
    let (_, _, assets) = site_with_assets(&server, &root, "Marketing").await;
    let logo = png(12, 12);
    let doc = upload(&server, &root, &assets, "Logo.png", &logo).await;
    let other = png(13, 13);
    upload(&server, &root, &assets, "Other.png", &other).await;
    let (status, body) = new_image(&server, &root, json!({ "doc": doc, "kind": "logo" })).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let url = body["url"].as_str().unwrap().to_owned();

    // Ned cannot see the library, but sees the brand image.
    let (status, _) = call(
        &server,
        Some(&ned),
        Method::GET,
        &format!("/api/docs/{doc}/content"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, bytes) = fetch(&server, Some(&ned), &url).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, logo);
    // Signed out: nothing.
    assert_eq!(fetch(&server, None, &url).await.0, StatusCode::NOT_FOUND);
    // Content the brand does not name, stored or not, and addresses that name nothing.
    for path in [
        format!("/api/brand/images/{}", sha(&other)),
        format!("/api/brand/images/{}", "0".repeat(64)),
        format!("/api/brand/images/{}", sha(&logo).to_uppercase()),
        "/api/brand/images/nonsense".to_owned(),
    ] {
        assert_eq!(
            fetch(&server, Some(&ned), &path).await.0,
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
}

#[tokio::test]
async fn brand_image_headers() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let (_, _, assets) = site_with_assets(&server, &root, "Marketing").await;
    let svg = b"<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 4 4\"><rect width=\"4\" \
                height=\"4\"/></svg>";
    for (name, bytes, media_type) in [
        ("Logo.png", png(6, 6), "image/png"),
        ("Logo.svg", svg.to_vec(), "image/svg+xml"),
    ] {
        let doc = upload(&server, &root, &assets, name, &bytes).await;
        let (status, body) = new_image(&server, &root, json!({ "doc": doc, "kind": "logo" })).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let (status, headers, served) =
            fetch(&server, Some(&root), body["url"].as_str().unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(served, bytes);
        assert_eq!(headers["content-type"], media_type);
        assert_eq!(headers["x-content-type-options"], "nosniff");
        assert_eq!(
            headers["content-security-policy"],
            "sandbox; default-src 'none'; style-src 'unsafe-inline'"
        );
        assert_eq!(
            headers["cache-control"],
            "private, max-age=31536000, immutable"
        );
        assert!(headers.get("content-disposition").is_none());
        assert_eq!(headers["content-length"], bytes.len().to_string());
    }
}

#[tokio::test]
async fn brand_inputs_never_run() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let (_, _, assets) = site_with_assets(&server, &root, "Marketing").await;

    // Images that could run code or load from elsewhere are refused.
    let refused: [(&str, &[u8]); 6] = [
        (
            "Script.svg",
            b"<svg xmlns=\"http://www.w3.org/2000/svg\"><script>alert(1)</script></svg>",
        ),
        (
            "Onload.svg",
            b"<svg xmlns=\"http://www.w3.org/2000/svg\" onload=\"alert(1)\"/>",
        ),
        (
            "External.svg",
            b"<svg xmlns=\"http://www.w3.org/2000/svg\"><image href=\"https://evil.example/x.png\"/></svg>",
        ),
        (
            "Linked.svg",
            b"<svg xmlns=\"http://www.w3.org/2000/svg\" xmlns:xlink=\"http://www.w3.org/1999/xlink\"><use xlink:href=\"//evil.example/a.svg#x\"/></svg>",
        ),
        (
            "Styled.svg",
            b"<svg xmlns=\"http://www.w3.org/2000/svg\"><style>@import url(https://evil.example/a.css);</style></svg>",
        ),
        ("Page.png", b"<!DOCTYPE html><html><script>alert(1)</script></html>"),
    ];
    for (name, bytes) in refused {
        let doc = upload(&server, &root, &assets, name, bytes).await;
        let (status, body) = new_image(&server, &root, json!({ "doc": doc, "kind": "logo" })).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{name}: {body}");
        assert_eq!(body["error"], "not_an_image", "{name}");
    }
    assert!(in_git(&server).images.is_empty());

    // Colours are #rrggbb and nothing else.
    for theme in [
        json!({ "name": "X", "primary": "red;background:url(x)", "secondary": "#233239" }),
        json!({ "name": "X", "primary": "#3156c9", "secondary": "#233239;color:red" }),
        json!({ "name": "X", "primary": "#3156c9", "secondary": "#233239",
                "primary_dark": "url(javascript:alert(1))" }),
        json!({ "name": "X", "primary": "#3156c9", "secondary": "#233239", "css": "x" }),
    ] {
        let (status, body) = new_theme(&server, &root, theme.clone()).await;
        assert!(
            status == StatusCode::UNPROCESSABLE_ENTITY || status == StatusCode::BAD_REQUEST,
            "{theme}: {status} {body}"
        );
    }

    // HTML in names is kept as text, never markup.
    let marked = "<img src=x onerror=alert(1)>";
    let (status, body) = new_theme(
        &server,
        &root,
        json!({ "name": marked, "primary": "#3156c9", "secondary": "#233239" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let doc = upload(&server, &root, &assets, "Logo.png", &png(4, 4)).await;
    let (status, body) = new_image(
        &server,
        &root,
        json!({ "doc": doc, "kind": "logo", "name": "<b>Bold</b>" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let response = server
        .http(Some(&root))
        .get(server.url("/api/brand"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.headers()["content-type"], "application/json");
    let seen: Value = response.json().await.unwrap();
    assert_eq!(seen["themes"][6]["name"], marked);
    assert_eq!(seen["images"][0]["name"], "<b>Bold</b>");
    // Control and invisible characters are refused.
    let (status, body) = new_theme(
        &server,
        &root,
        json!({ "name": "Right\u{202e}left", "primary": "#3156c9", "secondary": "#233239" }),
    )
    .await;
    assert_eq!(
        (status, &body["error"]),
        (StatusCode::UNPROCESSABLE_ENTITY, &json!("invalid_theme"))
    );
}

#[tokio::test]
async fn brand_images_pinned_until_unreferenced() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let (_, _, assets) = site_with_assets(&server, &root, "Marketing").await;
    let logo = png(16, 16);
    let doc = upload(&server, &root, &assets, "Logo.png", &logo).await;
    let mut ids = Vec::new();
    for kind in ["logo", "icon"] {
        let (status, body) = new_image(&server, &root, json!({ "doc": doc, "kind": kind })).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        ids.push(body["id"].as_str().unwrap().to_owned());
    }
    let url = format!("/api/brand/images/{}", sha(&logo));
    let (status, body) = new_theme(&server, &root, ocean()).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let theme = body["id"].as_str().unwrap().to_owned();

    // The source file is deleted and its bin emptied: the image stays.
    let (status, _) = call(
        &server,
        Some(&root),
        Method::DELETE,
        &format!("/api/nodes/{doc}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    for _ in 0..2 {
        let (status, body) = call(
            &server,
            Some(&root),
            Method::DELETE,
            &format!("/api/bin/{doc}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    }
    let db = server.records_db();
    assert!(db.blob_refs_of(&hash(&logo)).await.unwrap().is_empty());
    assert!(server.blobs().contains(&hash(&logo)));
    assert_eq!(fetch(&server, Some(&root), &url).await.0, StatusCode::OK);
    // Nothing erases it while it is pinned.
    assert!(!db.release_blob_if_unreferenced(&hash(&logo)).await.unwrap());
    let old = time::OffsetDateTime::now_utc() + time::Duration::days(2);
    assert!(
        !db.unreferenced_blobs(old)
            .await
            .unwrap()
            .contains(&hash(&logo))
    );

    // A theme deleted changes nothing of the images.
    let (status, _) = call(
        &server,
        Some(&root),
        Method::DELETE,
        &format!("/api/brand/themes/{theme}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(fetch(&server, Some(&root), &url).await.0, StatusCode::OK);

    // One image of the two removed: still pinned by the other.
    let (status, _) = call(
        &server,
        Some(&root),
        Method::DELETE,
        &format!("/api/brand/images/{}", ids[0]),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(server.blobs().contains(&hash(&logo)));
    assert_eq!(fetch(&server, Some(&root), &url).await.0, StatusCode::OK);
    // The last removed: nothing names it, and it is erased.
    let (status, _) = call(
        &server,
        Some(&root),
        Method::DELETE,
        &format!("/api/brand/images/{}", ids[1]),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(!server.blobs().contains(&hash(&logo)));
    assert_eq!(
        fetch(&server, Some(&root), &url).await.0,
        StatusCode::NOT_FOUND
    );
    let removed = events(&server, "brand.image_changed").await;
    assert_eq!(removed.last().unwrap()["action"], "removed");
}

#[tokio::test]
async fn reconcile_rebuilds_brand_from_git() {
    let data_dir = tempfile::tempdir().unwrap();
    let db = TestDb::create().await;
    let mut first = StoppableServer::start(data_dir.path(), &db, |_| {});
    let root = first.admin("Root").await;
    let mia = first.user("Mia").await;
    managers(&first, &root, &[&mia]).await;
    let (status, body) = new_theme(&first, &root, ocean()).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (_, _, assets) = site_with_assets(&first, &root, "Marketing").await;
    let logo = png(9, 9);
    let doc = upload(&first, &root, &assets, "Logo.png", &logo).await;
    let (status, body) = new_image(&first, &root, json!({ "doc": doc, "kind": "logo" })).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let url = body["url"].as_str().unwrap().to_owned();
    first.stop();

    // The mirror is lost.
    let pool = db.db.test_pool();
    sqlx::query(
        "update org_brand set record = \
         '{\"schema\": 1, \"managers\": [], \"themes\": [], \"images\": []}'",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("delete from brand_refs")
        .execute(pool)
        .await
        .unwrap();
    // A change written to Git while the server was stopped.
    let store = RevisionStore::open(&server::library_path(data_dir.path())).unwrap();
    let mut brand = store.read_org_brand().unwrap().unwrap();
    brand.themes.push(OrgTheme {
        id: Uuid::new_v4(),
        name: "Written elsewhere".into(),
        primary: Colour::rgb(0x31, 0x56, 0xc9),
        secondary: Colour::rgb(0x23, 0x32, 0x39),
        primary_dark: None,
        secondary_dark: None,
        changed_at: 1_800_000_000,
    });
    let author = Author {
        id: "elsewhere".into(),
        name: "Elsewhere".into(),
    };
    let mut change = SitesChange::new(&author, "Change the brand centre");
    change.org_brand = Some(&brand);
    store.write_sites(&change).unwrap();

    let second = StoppableServer::start(data_dir.path(), &db, |_| {});
    let rita = second.admin("Rita").await;
    let seen = centre(&second, &rita).await;
    let names: Vec<&str> = seen["themes"]
        .as_array()
        .unwrap()
        .iter()
        .skip(6)
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Ocean", "Written elsewhere"]);
    assert_eq!(seen["images"][0]["url"], url);
    assert_eq!(seen["managers"][0]["id"], json!(mia.id.0));
    assert_eq!(fetch(&second, Some(&rita), &url).await.0, StatusCode::OK);
    assert_eq!(
        second.records_db().brand_pins("org").await.unwrap(),
        vec![(sha(&logo), "image/png".to_owned())]
    );
    for kind in [
        "brand.managers_changed",
        "brand.theme_changed",
        "brand.image_changed",
    ] {
        let found = events(&second, kind).await;
        assert!(
            found.iter().any(|d| d["repaired"] == true),
            "{kind}: {found:?}"
        );
    }
}

#[tokio::test]
async fn brand_change_refused_by_database_is_taken_back() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let (_, _, assets) = site_with_assets(&server, &root, "Marketing").await;
    let logo = png(7, 7);
    let doc = upload(&server, &root, &assets, "Logo.png", &logo).await;
    // The content's rows are gone, as after an erasure that ran while the image was chosen;
    // the file is still read from the store.
    let pool = server.records_db().test_pool().clone();
    sqlx::query("delete from blob_refs where sha256 = $1")
        .bind(sha(&logo))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("delete from blobs where sha256 = $1")
        .bind(sha(&logo))
        .execute(&pool)
        .await
        .unwrap();
    let tip = |server: &TestServer| {
        let repo = gix::open(server.library()).unwrap();
        repo.find_reference("refs/heads/main")
            .unwrap()
            .id()
            .detach()
    };
    let before = tip(&server);

    let (status, body) = new_image(&server, &root, json!({ "doc": doc, "kind": "logo" })).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "image_gone");
    // The change went into Git and a second commit took it back.
    let repo = gix::open(server.library()).unwrap();
    let undo = repo.find_commit(tip(&server)).unwrap();
    assert!(
        String::from_utf8_lossy(undo.message_raw().unwrap()).starts_with("Undo: "),
        "the last commit takes the change back"
    );
    let change = undo.parent_ids().next().unwrap().detach();
    assert_eq!(
        repo.find_commit(change)
            .unwrap()
            .parent_ids()
            .next()
            .unwrap()
            .detach(),
        before
    );
    assert!(in_git(&server).images.is_empty());
    assert_eq!(centre(&server, &root).await["images"], json!([]));
    assert!(events(&server, "brand.image_changed").await.is_empty());
    assert!(
        server
            .records_db()
            .brand_pins("org")
            .await
            .unwrap()
            .is_empty()
    );

    // Nothing is out of step: the next change goes through.
    let (status, body) = new_theme(&server, &root, ocean()).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(in_git(&server).themes.len(), 1);
}
