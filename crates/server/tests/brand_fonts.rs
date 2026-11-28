//! Brand fonts over HTTP: font files added to the brand centre from asset libraries with the
//! licence confirmed, judged by their content and within their limits; served to anyone signed
//! in with safe headers; font packages in the brand centre and in sites; inputs that could run
//! code or inject style; pinning their content until nothing uses it, and the reconcile at
//! start rebuilding the pins; and the effective look carrying fonts, the tab icon and the menu
//! style, with the hub rule.

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
use revisions::brand::OrgBrand;
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

/// `POST /api/brand/fonts` as `user`.
async fn new_font(site: &Site, user: &TestUser, body: Value) -> (StatusCode, Value) {
    call(
        site,
        Some(user),
        Method::POST,
        "/api/brand/fonts",
        Some(body),
    )
    .await
}

/// Adds `doc` to the brand centre as a font, as `user`, which must succeed: its id.
async fn add_font(site: &Site, user: &TestUser, doc: &str) -> String {
    let (status, body) =
        new_font(site, user, json!({ "doc": doc, "licence_confirmed": true })).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["id"].as_str().unwrap().to_owned()
}

/// `POST /api/brand/font-packages` as `user`.
async fn new_package(site: &Site, user: &TestUser, body: Value) -> (StatusCode, Value) {
    call(
        site,
        Some(user),
        Method::POST,
        "/api/brand/font-packages",
        Some(body),
    )
    .await
}

/// `POST /api/sites/{id}/font-packages` as `user`.
async fn new_site_package(
    site: &Site,
    user: &TestUser,
    id: &str,
    body: Value,
) -> (StatusCode, Value) {
    call(
        site,
        Some(user),
        Method::POST,
        &format!("/api/sites/{id}/font-packages"),
        Some(body),
    )
    .await
}

/// `PUT /api/sites/{id}/look {look}` as `user`.
async fn put_look(site: &Site, user: &TestUser, id: &str, look: Value) -> (StatusCode, Value) {
    call(
        site,
        Some(user),
        Method::PUT,
        &format!("/api/sites/{id}/look"),
        Some(json!({ "look": look })),
    )
    .await
}

/// `PUT /api/sites/{id}/look`, which must succeed.
async fn set_look(site: &Site, user: &TestUser, id: &str, look: Value) -> Value {
    let (status, body) = put_look(site, user, id, look).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

/// `GET /api/sites/{id}/look` as `user`, which must succeed: the effective look.
async fn effective(site: &Site, user: &TestUser, id: &str) -> Value {
    let (status, body) = call(
        site,
        Some(user),
        Method::GET,
        &format!("/api/sites/{id}/look"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

/// `GET /api/sites/{id}/look/settings` as `user`, which must succeed.
async fn settings(site: &Site, user: &TestUser, id: &str) -> Value {
    let (status, body) = call(
        site,
        Some(user),
        Method::GET,
        &format!("/api/sites/{id}/look/settings"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

/// `GET` of a brand file as `user` (signed out with `None`): status, headers and bytes.
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

/// A font file of the kind its first four bytes `magic` say, made distinct by `seed`.
fn font(magic: &[u8; 4], seed: u8) -> Vec<u8> {
    let mut bytes = magic.to_vec();
    bytes.extend((0..200u16).map(|i| (i as u8).wrapping_mul(seed).wrapping_add(seed)));
    bytes
}

fn woff2(seed: u8) -> Vec<u8> {
    font(b"wOF2", seed)
}

/// An RGB PNG `width` × `height`.
fn png(width: u32, height: u32) -> Vec<u8> {
    let image = RgbImage::from_fn(width, height, |x, y| {
        Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8])
    });
    let mut out = Vec::new();
    DynamicImage::ImageRgb8(image)
        .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
        .unwrap();
    out
}

fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn hash(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn font_url(bytes: &[u8]) -> String {
    format!("/api/brand/fonts/{}", sha(bytes))
}

fn family(bytes: &[u8]) -> String {
    format!("sc-{}", &sha(bytes)[..12])
}

/// The details of the events of `kind`, oldest first.
async fn events(site: &Site, kind: &str) -> Vec<Value> {
    sqlx::query_scalar("select detail from events where kind = $1 order by id")
        .bind(kind)
        .fetch_all(site.records_db().test_pool())
        .await
        .unwrap()
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

/// The brand centre as Git holds it.
fn in_git(server: &TestServer) -> OrgBrand {
    server.store().read_org_brand().unwrap().unwrap_or_default()
}

/// Deletes `doc` and empties it from both stages of the recycle bin, as `admin`.
async fn delete_for_good(site: &Site, admin: &TestUser, doc: &str) {
    let (status, _) = call(
        site,
        Some(admin),
        Method::DELETE,
        &format!("/api/nodes/{doc}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    for _ in 0..2 {
        let (status, body) = call(
            site,
            Some(admin),
            Method::DELETE,
            &format!("/api/bin/{doc}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    }
}

fn roles(title: &str, heading: &str, body: &str, interactive: &str) -> Value {
    json!({ "title": title, "heading": heading, "body": body, "interactive": interactive })
}

fn system_fonts() -> Value {
    json!({
        "title": { "family": "system" },
        "heading": { "family": "system" },
        "body": { "family": "system" },
        "interactive": { "family": "system" },
    })
}

#[tokio::test]
async fn fonts_added_from_asset_libraries_need_licence_confirmation() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let mia = server.user("Mia").await;
    let ned = server.user("Ned").await;
    managers(&server, &root, &[&mia]).await;
    let (_, documents, assets) = site_with_assets(&server, &root, "Marketing").await;
    let bytes = woff2(3);
    let doc = upload(&server, &root, &assets, "Brand Sans.woff2", &bytes).await;
    let elsewhere = upload(&server, &root, &documents, "Other.woff2", &woff2(4)).await;

    // Only brand managers and administrators add fonts.
    let (status, body) = new_font(
        &server,
        &ned,
        json!({ "doc": doc, "licence_confirmed": true }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    // The licence must be confirmed.
    for body in [
        json!({ "doc": doc }),
        json!({ "doc": doc, "licence_confirmed": false }),
    ] {
        let (status, answer) = new_font(&server, &root, body).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
        assert_eq!(answer, json!({ "error": "licence_not_confirmed" }));
    }
    // Only from an asset library.
    let (status, body) = new_font(
        &server,
        &root,
        json!({ "doc": elsewhere, "licence_confirmed": true }),
    )
    .await;
    assert_eq!(
        (status, &body["error"]),
        (StatusCode::BAD_REQUEST, &json!("wrong_place"))
    );
    assert!(in_git(&server).fonts.is_empty());

    let (status, body) = new_font(
        &server,
        &root,
        json!({ "doc": doc, "licence_confirmed": true }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["name"], "Brand Sans.woff2");
    assert_eq!(body["media_type"], "font/woff2");
    assert_eq!(body["family"], family(&bytes));
    assert_eq!(body["url"], font_url(&bytes));
    assert_eq!(body["licence"]["confirmed_by"], json!(root.id.0));
    assert!(body["licence"]["confirmed_at"].as_i64().unwrap() > 0);
    let id = body["id"].as_str().unwrap().to_owned();

    // A brand manager who cannot view the document does not reach it; once they can, they add it.
    let (status, _) = new_font(
        &server,
        &mia,
        json!({ "doc": doc, "licence_confirmed": true, "name": "Mia's copy" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    grant(&server, &root, &assets, &mia, "viewer").await;
    let (status, body) = new_font(
        &server,
        &mia,
        json!({ "doc": doc, "licence_confirmed": true, "name": "Mia's copy" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["name"], "Mia's copy");

    // The record notes who confirmed the licence, and when.
    let fonts = in_git(&server).fonts;
    assert_eq!(fonts.len(), 2);
    assert_eq!(fonts[0].font.licence.confirmed_by, root.id.0);
    assert_eq!(fonts[1].font.licence.confirmed_by, mia.id.0);
    assert_eq!(fonts[0].font.sha256, sha(&bytes));
    let added = events(&server, "brand.font_changed").await;
    assert_eq!(added.len(), 2);
    assert_eq!(added[0]["action"], "added");
    let seen = centre(&server, &ned).await;
    assert_eq!(seen["fonts"][0]["id"], json!(id));
    assert_eq!(seen["fonts"][0]["family"], family(&bytes));

    // At most 30.
    for _ in 0..28 {
        add_font(&server, &root, &doc).await;
    }
    let (status, body) = new_font(
        &server,
        &root,
        json!({ "doc": doc, "licence_confirmed": true }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body, json!({ "error": "too_many", "limit": 30 }));

    // Removing one: managers only.
    let (status, _) = call(
        &server,
        Some(&ned),
        Method::DELETE,
        &format!("/api/brand/fonts/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(
        &server,
        Some(&root),
        Method::DELETE,
        &format!("/api/brand/fonts/{}", Uuid::new_v4()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(
        &server,
        Some(&root),
        Method::DELETE,
        &format!("/api/brand/fonts/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(in_git(&server).fonts.len(), 29);
    let removed = events(&server, "brand.font_changed").await;
    assert_eq!(removed.last().unwrap()["action"], "removed");
}

#[tokio::test]
async fn font_type_judged_by_content() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let (_, _, assets) = site_with_assets(&server, &root, "Marketing").await;

    // The kind comes from the content, never the name.
    for (name, bytes, media_type) in [
        ("A.woff2", woff2(1), "font/woff2"),
        ("B.ttf", font(b"wOFF", 2), "font/woff"),
        ("C.woff2", font(&[0, 1, 0, 0], 3), "font/ttf"),
        ("D.otf", font(b"true", 4), "font/ttf"),
        ("E.ttf", font(b"OTTO", 5), "font/otf"),
    ] {
        let doc = upload(&server, &root, &assets, name, &bytes).await;
        let (status, body) = new_font(
            &server,
            &root,
            json!({ "doc": doc, "licence_confirmed": true }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{name}: {body}");
        assert_eq!(body["media_type"], media_type, "{name}");
    }
    // Anything else is not a font.
    let refused: [(&str, Vec<u8>); 4] = [
        (
            "Page.woff2",
            b"<!DOCTYPE html><html><body>x</body></html>".to_vec(),
        ),
        ("Picture.woff2", png(4, 4)),
        ("Short.woff2", b"wOF".to_vec()),
        ("Text.ttf", b"just some text".to_vec()),
    ];
    for (name, bytes) in refused {
        let doc = upload(&server, &root, &assets, name, &bytes).await;
        let (status, body) = new_font(
            &server,
            &root,
            json!({ "doc": doc, "licence_confirmed": true }),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{name}: {body}");
        assert_eq!(body, json!({ "error": "not_a_font" }), "{name}");
    }
    // At most 2 MiB.
    let mut large = b"wOF2".to_vec();
    large.resize(2 * 1024 * 1024 + 1, 7);
    let doc = upload(&server, &root, &assets, "Large.woff2", &large).await;
    let (status, body) = new_font(
        &server,
        &root,
        json!({ "doc": doc, "licence_confirmed": true }),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    assert_eq!(body, json!({ "error": "font_too_large" }));
    large.truncate(2 * 1024 * 1024);
    let doc = upload(&server, &root, &assets, "Largest.woff2", &large).await;
    add_font(&server, &root, &doc).await;
    // The content must be available.
    let held = woff2(9);
    let doc = upload(&server, &root, &assets, "Held.woff2", &held).await;
    set_scan(&server, &held, "infected").await;
    let (status, body) = new_font(
        &server,
        &root,
        json!({ "doc": doc, "licence_confirmed": true }),
    )
    .await;
    assert_eq!(status, StatusCode::LOCKED, "{body}");
    assert_eq!(body, json!({ "error": "held", "scan": "infected" }));
    assert_eq!(in_git(&server).fonts.len(), 6);
}

#[tokio::test]
async fn font_route_headers() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let ned = server.user("Ned").await;
    let (_, _, assets) = site_with_assets(&server, &root, "Marketing").await;
    let bytes = woff2(5);
    let doc = upload(&server, &root, &assets, "Sans.woff2", &bytes).await;
    add_font(&server, &root, &doc).await;
    let other = woff2(6);
    upload(&server, &root, &assets, "Other.woff2", &other).await;
    let logo = png(8, 8);
    let logo_doc = upload(&server, &root, &assets, "Logo.png", &logo).await;
    let (status, body) = call(
        &server,
        Some(&root),
        Method::POST,
        "/api/brand/images",
        Some(json!({ "doc": logo_doc, "kind": "logo" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // Ned cannot see the library but loads the font, with safe headers.
    let (status, headers, served) = fetch(&server, Some(&ned), &font_url(&bytes)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(served, bytes);
    assert_eq!(headers["content-type"], "font/woff2");
    assert_eq!(headers["x-content-type-options"], "nosniff");
    assert_eq!(
        headers["content-security-policy"],
        "sandbox; default-src 'none'"
    );
    assert_eq!(
        headers["cache-control"],
        "private, max-age=31536000, immutable"
    );
    assert_eq!(headers["cross-origin-resource-policy"], "same-origin");
    assert_eq!(headers["content-length"], bytes.len().to_string());
    assert!(headers.get("content-disposition").is_none());

    // Signed out, content no font names, an image, and addresses that name nothing: 404.
    assert_eq!(
        fetch(&server, None, &font_url(&bytes)).await.0,
        StatusCode::NOT_FOUND
    );
    for path in [
        font_url(&other),
        font_url(&logo),
        format!("/api/brand/fonts/{}", "0".repeat(64)),
        format!("/api/brand/fonts/{}", sha(&bytes).to_uppercase()),
        "/api/brand/fonts/nonsense".to_owned(),
        // A font is never served as an image either.
        format!("/api/brand/images/{}", sha(&bytes)),
    ] {
        assert_eq!(
            fetch(&server, Some(&ned), &path).await.0,
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
    // A rescan finds it infected: held, not served.
    set_scan(&server, &bytes, "infected").await;
    let (status, _, body) = fetch(&server, Some(&ned), &font_url(&bytes)).await;
    assert_eq!(status, StatusCode::LOCKED);
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body, json!({ "error": "held", "scan": "infected" }));
}

#[tokio::test]
async fn font_inputs_never_run() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let (team, documents, assets) = site_with_assets(&server, &root, "Marketing").await;

    // Web pages and pictures renamed as fonts are refused.
    let refused: [(&str, &[u8]); 3] = [
        (
            "Page.woff2",
            b"<!DOCTYPE html><html><script>alert(1)</script></html>",
        ),
        (
            "Picture.woff2",
            b"<svg xmlns=\"http://www.w3.org/2000/svg\" onload=\"alert(1)\"/>",
        ),
        ("Style.woff", b"body { display: none }"),
    ];
    for (name, bytes) in refused {
        let doc = upload(&server, &root, &assets, name, bytes).await;
        let (status, body) = new_font(
            &server,
            &root,
            json!({ "doc": doc, "licence_confirmed": true }),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{name}: {body}");
        assert_eq!(body["error"], "not_a_font", "{name}");
    }
    assert!(in_git(&server).fonts.is_empty());

    // A name that looks like style is kept as text; the family is generated from the hash.
    let bytes = woff2(8);
    let doc = upload(&server, &root, &assets, "Sans.woff2", &bytes).await;
    let styled = "\"; } body { display:none";
    let (status, body) = new_font(
        &server,
        &root,
        json!({ "doc": doc, "licence_confirmed": true, "name": styled }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["name"], styled);
    assert_eq!(body["family"], family(&bytes));
    let font_id = body["id"].as_str().unwrap().to_owned();
    // Control and invisible characters are refused.
    let (status, body) = new_font(
        &server,
        &root,
        json!({ "doc": doc, "licence_confirmed": true, "name": "Right\u{202e}left" }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // A package name with markup is kept as text.
    let marked = "<img src=x onerror=alert(1)>";
    let (status, body) = new_package(
        &server,
        &root,
        json!({ "name": marked, "display": { "brand_font": font_id },
                "roles": roles("display", "display", "system", "system") }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["name"], marked);
    let package = body["id"].as_str().unwrap().to_owned();
    let response = server
        .http(Some(&root))
        .get(server.url("/api/brand"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.headers()["content-type"], "application/json");
    let seen: Value = response.json().await.unwrap();
    assert_eq!(seen["fonts"][0]["name"], styled);
    assert_eq!(seen["font_packages"][2]["name"], marked);

    // A look takes a package by its source only: no family or stack can be sent.
    for fonts in [
        json!({ "from": format!("org:{package}"), "family": "evil" }),
        json!({ "from": format!("org:{package}"), "title": "x; color: red" }),
        json!({ "from": "custom" }),
        json!({ "from": format!("org:{}", Uuid::new_v4()) }),
    ] {
        let (status, body) = put_look(
            &server,
            &root,
            &team,
            json!({ "theme": { "from": "builtin:sharecraft" }, "fonts": fonts }),
        )
        .await;
        assert!(
            status == StatusCode::UNPROCESSABLE_ENTITY || status == StatusCode::BAD_REQUEST,
            "{fonts}: {status} {body}"
        );
    }
    // Every family the page is given is generated or a fixed system name.
    set_look(
        &server,
        &root,
        &team,
        json!({ "theme": { "from": "builtin:sharecraft" },
                "fonts": { "from": format!("org:{package}") } }),
    )
    .await;
    let seen = effective(&server, &root, &team).await;
    for role in ["title", "heading", "body", "interactive"] {
        let family = seen["fonts"][role]["family"].as_str().unwrap();
        let generated = family.len() == 15
            && family.starts_with("sc-")
            && family[3..]
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        assert!(
            generated || family == "system" || family == "system-serif",
            "{role}: {family}"
        );
    }
    assert_eq!(seen["fonts"]["title"]["family"], family(&bytes));
    // A site package name with markup is text too.
    let site_font = woff2(9);
    let site_doc = upload(&server, &root, &documents, "Own.woff2", &site_font).await;
    let (status, body) = new_site_package(
        &server,
        &root,
        &team,
        json!({ "name": marked,
                "content": { "doc": site_doc, "licence_confirmed": true },
                "roles": roles("system", "system", "content", "content") }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["name"], marked);
    assert_eq!(body["content"]["family"], family(&site_font));
}

#[tokio::test]
async fn fonts_pinned_until_unreferenced() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let (team, documents, assets) = site_with_assets(&server, &root, "Marketing").await;
    let bytes = woff2(11);
    let doc = upload(&server, &root, &assets, "Sans.woff2", &bytes).await;
    let font_id = add_font(&server, &root, &doc).await;
    let (status, body) = new_package(
        &server,
        &root,
        json!({ "name": "Brand", "display": { "brand_font": font_id },
                "roles": roles("display", "display", "system", "system") }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let package = body["id"].as_str().unwrap().to_owned();
    set_look(
        &server,
        &root,
        &team,
        json!({ "theme": { "from": "builtin:sharecraft" },
                "fonts": { "from": format!("org:{package}") } }),
    )
    .await;
    let db = server.records_db();
    assert_eq!(
        db.brand_pins("org").await.unwrap(),
        vec![(sha(&bytes), "font/woff2".to_owned())]
    );
    assert_eq!(
        db.brand_pins(&team).await.unwrap(),
        vec![(sha(&bytes), "font/woff2".to_owned())]
    );

    // The source file is deleted and its bin emptied: the font stays.
    delete_for_good(&server, &root, &doc).await;
    assert!(db.blob_refs_of(&hash(&bytes)).await.unwrap().is_empty());
    assert!(server.blobs().contains(&hash(&bytes)));
    assert_eq!(
        fetch(&server, Some(&root), &font_url(&bytes)).await.0,
        StatusCode::OK
    );
    assert!(
        !db.release_blob_if_unreferenced(&hash(&bytes))
            .await
            .unwrap()
    );

    // The font is removed from the brand centre: the package keeps its copy.
    let (status, _) = call(
        &server,
        Some(&root),
        Method::DELETE,
        &format!("/api/brand/fonts/{font_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(server.blobs().contains(&hash(&bytes)));
    // The package is deleted while a look uses its fonts: the look keeps its copy.
    let (status, _) = call(
        &server,
        Some(&root),
        Method::DELETE,
        &format!("/api/brand/font-packages/{package}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(db.brand_pins("org").await.unwrap().is_empty());
    assert!(server.blobs().contains(&hash(&bytes)));
    assert_eq!(
        fetch(&server, Some(&root), &font_url(&bytes)).await.0,
        StatusCode::OK
    );
    // The look stops using it: nothing names it, and it is erased.
    set_look(
        &server,
        &root,
        &team,
        json!({ "theme": { "from": "builtin:sharecraft" },
                "fonts": { "from": "builtin:classic" } }),
    )
    .await;
    assert!(db.brand_pins(&team).await.unwrap().is_empty());
    assert!(!server.blobs().contains(&hash(&bytes)));
    assert_eq!(
        fetch(&server, Some(&root), &font_url(&bytes)).await.0,
        StatusCode::NOT_FOUND
    );

    // A site's own package pins its font; the site purged releases it.
    let own = woff2(12);
    let own_doc = upload(&server, &root, &documents, "Own.woff2", &own).await;
    let (status, body) = new_site_package(
        &server,
        &root,
        &team,
        json!({ "name": "Ours", "content": { "doc": own_doc, "licence_confirmed": true },
                "roles": roles("system", "system", "content", "content") }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        db.brand_pins(&team).await.unwrap(),
        vec![(sha(&own), "font/woff2".to_owned())]
    );
    delete_for_good(&server, &root, &own_doc).await;
    assert!(server.blobs().contains(&hash(&own)));
    for path in [
        format!("/api/sites/{team}"),
        format!("/api/admin/sites/{team}"),
    ] {
        let (status, body) = call(&server, Some(&root), Method::DELETE, &path, None).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{path}: {body}");
    }
    assert!(!server.blobs().contains(&hash(&own)));
}

#[tokio::test]
async fn font_packages_org_and_site() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let ned = server.user("Ned").await;
    let olive = server.user("Olive").await;
    let ed = server.user("Ed").await;
    let (team, documents, assets) = site_with_assets(&server, &root, "Marketing").await;
    grant(&server, &root, &team, &olive, "owner").await;
    grant(&server, &root, &team, &ed, "editor").await;
    let display = woff2(21);
    let content = font(b"OTTO", 22);
    let display_id = add_font(
        &server,
        &root,
        &upload(&server, &root, &assets, "Display.woff2", &display).await,
    )
    .await;
    let content_id = add_font(
        &server,
        &root,
        &upload(&server, &root, &assets, "Content.otf", &content).await,
    )
    .await;

    // The built-in packages come first.
    let seen = centre(&server, &ned).await;
    assert_eq!(seen["font_packages"][0]["id"], "builtin:system");
    assert_eq!(seen["font_packages"][0]["builtin"], true);
    assert_eq!(seen["font_packages"][1]["id"], "builtin:classic");
    assert_eq!(
        seen["font_packages"][1]["roles"],
        roles("system-serif", "system-serif", "system", "system")
    );

    // Brand managers and administrators make the organisation's packages.
    let brand = json!({
        "name": "Brand",
        "display": { "brand_font": display_id },
        "content": { "brand_font": content_id },
        "roles": roles("display", "display", "content", "system"),
    });
    let (status, _) = new_package(&server, &ned, brand.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, body) = new_package(&server, &root, brand.clone()).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["id"].as_str().unwrap().to_owned();
    assert_eq!(body["from"], format!("org:{id}"));
    assert_eq!(body["display"]["family"], family(&display));
    assert_eq!(body["content"]["url"], font_url(&content));
    let changed = events(&server, "brand.font_package_changed").await;
    assert_eq!(changed[0]["action"], "created");

    // Names are unique ignoring case; roles name only fonts the package has; fonts exist.
    let (status, body) = new_package(
        &server,
        &root,
        json!({ "name": "BRAND", "roles": roles("system", "system", "system", "system") }),
    )
    .await;
    assert_eq!(
        (status, &body["error"]),
        (StatusCode::CONFLICT, &json!("name_taken"))
    );
    for (what, bad) in [
        (
            "role without font",
            json!({ "name": "Half", "display": { "brand_font": display_id },
                    "roles": roles("display", "display", "content", "system") }),
        ),
        (
            "unknown font",
            json!({ "name": "Gone", "display": { "brand_font": Uuid::new_v4() },
                    "roles": roles("display", "system", "system", "system") }),
        ),
        (
            "empty name",
            json!({ "name": " ", "roles": roles("system", "system", "system", "system") }),
        ),
        (
            "long name",
            json!({ "name": "x".repeat(61),
                    "roles": roles("system", "system", "system", "system") }),
        ),
        (
            "document in the brand centre",
            json!({ "name": "Doc", "display": { "doc": Uuid::new_v4(), "licence_confirmed": true },
                    "roles": roles("display", "system", "system", "system") }),
        ),
    ] {
        let (status, body) = new_package(&server, &root, bad).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{what}: {body}");
        assert_eq!(body["error"], "invalid_font_package", "{what}");
        assert!(body["reason"].is_string(), "{what}: {body}");
    }
    let (status, body) = new_package(
        &server,
        &root,
        json!({ "name": "Bad role", "roles": roles("fancy", "system", "system", "system") }),
    )
    .await;
    assert!(status.is_client_error(), "{body}");

    // Changing one: each field given is set.
    let (status, body) = call(
        &server,
        Some(&root),
        Method::PATCH,
        &format!("/api/brand/font-packages/{id}"),
        Some(json!({ "name": "Brand 2", "content": null,
                     "roles": roles("display", "display", "system", "system") })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["name"], "Brand 2");
    assert!(body["content"].is_null(), "{body}");
    let (status, body) = call(
        &server,
        Some(&root),
        Method::PATCH,
        &format!("/api/brand/font-packages/{id}"),
        Some(json!({ "roles": roles("content", "display", "system", "system") })),
    )
    .await;
    assert_eq!(
        (status, &body["error"]),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            &json!("invalid_font_package")
        )
    );
    assert_eq!(in_git(&server).font_packages[0].name, "Brand 2");

    // At most 20.
    for n in 1..20 {
        let (status, body) = new_package(
            &server,
            &root,
            json!({ "name": format!("P{n}"), "roles": roles("system", "system", "system", "system") }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    let (status, body) = new_package(
        &server,
        &root,
        json!({ "name": "P20", "roles": roles("system", "system", "system", "system") }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body, json!({ "error": "too_many", "limit": 20 }));
    let (status, _) = call(
        &server,
        Some(&ned),
        Method::DELETE,
        &format!("/api/brand/font-packages/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(
        &server,
        Some(&root),
        Method::DELETE,
        &format!("/api/brand/font-packages/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(in_git(&server).font_packages.len(), 19);

    // A site's own packages: people who manage the site, fonts from the site's own libraries
    // or the brand centre, the licence confirmed.
    let own = woff2(23);
    let own_doc = upload(&server, &root, &documents, "Own.woff2", &own).await;
    let ours = json!({
        "name": "Ours",
        "display": { "doc": own_doc, "licence_confirmed": true },
        "content": { "brand_font": content_id },
        "roles": roles("display", "display", "content", "content"),
    });
    let (status, _) = new_site_package(&server, &ed, &team, ours.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, body) = new_site_package(
        &server,
        &olive,
        &team,
        json!({ "name": "Ours", "display": { "doc": own_doc },
                "roles": roles("display", "system", "system", "system") }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"], "licence_not_confirmed");
    let (status, body) = new_site_package(&server, &olive, &team, ours.clone()).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let site_package = body["id"].as_str().unwrap().to_owned();
    assert_eq!(body["from"], format!("site:{site_package}"));
    let (status, body) = new_site_package(&server, &olive, &team, ours.clone()).await;
    assert_eq!(
        (status, &body["error"]),
        (StatusCode::CONFLICT, &json!("name_taken"))
    );
    let shown = settings(&server, &olive, &team).await;
    assert_eq!(shown["font_packages"][0]["id"], json!(site_package));
    assert_eq!(shown["font_packages"][0]["display"]["family"], family(&own));
    let record = server
        .store()
        .read_site_record(Uuid::parse_str(&team).unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(record.font_packages.len(), 1);
    assert_eq!(
        record.font_packages[0]
            .display
            .as_ref()
            .unwrap()
            .licence
            .confirmed_by,
        olive.id.0
    );
    let changed = events(&server, "site.font_package_changed").await;
    assert_eq!(changed[0]["action"], "created");
    // A package of another site's is not this site's.
    let (other, _, _) = site_with_assets(&server, &root, "Other").await;
    let (status, body) = put_look(
        &server,
        &root,
        &other,
        json!({ "theme": { "from": "builtin:sharecraft" },
                "fonts": { "from": format!("site:{site_package}") } }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    // The site's look uses it.
    set_look(
        &server,
        &olive,
        &team,
        json!({ "theme": { "from": "builtin:sharecraft" },
                "fonts": { "from": format!("site:{site_package}") } }),
    )
    .await;
    let seen = effective(&server, &ed, &team).await;
    assert_eq!(seen["fonts"]["title"]["url"], font_url(&own));
    assert_eq!(seen["fonts"]["body"]["family"], family(&content));
    // At most 10.
    for n in 1..10 {
        let (status, body) = new_site_package(
            &server,
            &olive,
            &team,
            json!({ "name": format!("S{n}"), "roles": roles("system", "system", "system", "system") }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    let (status, body) = new_site_package(
        &server,
        &olive,
        &team,
        json!({ "name": "S10", "roles": roles("system", "system", "system", "system") }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body, json!({ "error": "too_many", "limit": 10 }));
    // Changed and deleted; the look keeps its copy.
    let (status, body) = call(
        &server,
        Some(&olive),
        Method::PATCH,
        &format!("/api/sites/{team}/font-packages/{site_package}"),
        Some(json!({ "name": "Ours, renamed" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["name"], "Ours, renamed");
    let (status, _) = call(
        &server,
        Some(&olive),
        Method::DELETE,
        &format!("/api/sites/{team}/font-packages/{site_package}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(
        &server,
        Some(&olive),
        Method::DELETE,
        &format!("/api/sites/{team}/font-packages/{site_package}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        effective(&server, &ed, &team).await["fonts"]["title"]["url"],
        font_url(&own)
    );
    assert!(server.blobs().contains(&hash(&own)));
}

#[tokio::test]
async fn effective_look_carries_fonts_icon_and_menu() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let vic = server.user("Vic").await;
    let (hub, _, assets) = site_with_assets(&server, &root, "Hub").await;
    let (team, documents, _) = site_with_assets(&server, &root, "Team").await;
    grant(&server, &root, &team, &vic, "viewer").await;

    // The defaults: system fonts, no icon of its own, cascading menus.
    let seen = effective(&server, &vic, &team).await;
    assert_eq!(seen["fonts"], system_fonts());
    assert_eq!(seen["icon_url"], Value::Null);
    assert_eq!(seen["navigation_menu"], "cascading");

    // A built-in package, a mega menu and an icon.
    let icon = png(32, 32);
    let icon_doc = upload(&server, &root, &documents, "Icon.png", &icon).await;
    set_look(
        &server,
        &root,
        &team,
        json!({ "theme": { "from": "builtin:forest" },
                "fonts": { "from": "builtin:classic" },
                "icon": { "doc": icon_doc },
                "menu": "mega" }),
    )
    .await;
    let seen = effective(&server, &vic, &team).await;
    assert_eq!(seen["fonts"]["title"], json!({ "family": "system-serif" }));
    assert_eq!(
        seen["fonts"]["heading"],
        json!({ "family": "system-serif" })
    );
    assert_eq!(seen["fonts"]["body"], json!({ "family": "system" }));
    assert_eq!(seen["navigation_menu"], "mega");
    assert_eq!(
        seen["icon_url"],
        format!("/api/brand/images/{}", sha(&icon))
    );
    // A mega menu goes with horizontal navigation only; an icon is square and large enough.
    let (status, body) = put_look(
        &server,
        &root,
        &team,
        json!({ "theme": { "from": "builtin:forest" }, "menu": "mega",
                "navigation": "vertical" }),
    )
    .await;
    assert_eq!(
        (status, &body["error"]),
        (StatusCode::UNPROCESSABLE_ENTITY, &json!("invalid_look"))
    );
    for (name, bytes) in [("Small.png", png(16, 16)), ("Wide.png", png(64, 32))] {
        let doc = upload(&server, &root, &documents, name, &bytes).await;
        let (status, body) = put_look(
            &server,
            &root,
            &team,
            json!({ "theme": { "from": "builtin:forest" }, "icon": { "doc": doc } }),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{name}: {body}");
        assert_eq!(body["error"], "not_an_icon", "{name}");
    }

    // An organisation package: each role its font, with its address; a system role has none.
    let display = woff2(31);
    let content = woff2(32);
    let display_id = add_font(
        &server,
        &root,
        &upload(&server, &root, &assets, "Display.woff2", &display).await,
    )
    .await;
    let content_id = add_font(
        &server,
        &root,
        &upload(&server, &root, &assets, "Content.woff2", &content).await,
    )
    .await;
    let (status, body) = new_package(
        &server,
        &root,
        json!({ "name": "Brand", "display": { "brand_font": display_id },
                "content": { "brand_font": content_id },
                "roles": roles("display", "display", "content", "system") }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let package = body["id"].as_str().unwrap().to_owned();
    set_look(
        &server,
        &root,
        &hub,
        json!({ "theme": { "from": "builtin:plum" },
                "fonts": { "from": format!("org:{package}") } }),
    )
    .await;
    let seen = effective(&server, &root, &hub).await;
    assert_eq!(
        seen["fonts"]["title"],
        json!({ "family": family(&display), "url": font_url(&display) })
    );
    assert_eq!(
        seen["fonts"]["body"],
        json!({ "family": family(&content), "url": font_url(&content) })
    );
    assert_eq!(seen["fonts"]["interactive"], json!({ "family": "system" }));

    // The hub rule: a site in a hub takes the hub's fonts with its theme, and keeps its own
    // icon and menu; leaving brings its own fonts back.
    let (status, body) = call(
        &server,
        Some(&root),
        Method::PUT,
        &format!("/api/sites/{hub}/hub"),
        Some(json!({ "navigation": [] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let join = |to: Option<&str>| {
        let server = &server;
        let root = &root;
        let team = team.clone();
        let to = to.map(str::to_owned);
        async move {
            let (status, body) = call(
                server,
                Some(root),
                Method::PUT,
                &format!("/api/sites/{team}/joined-hub"),
                Some(json!({ "hub": to })),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }
    };
    join(Some(&hub)).await;
    let seen = effective(&server, &vic, &team).await;
    assert_eq!(seen["theme_from"], "hub");
    assert_eq!(seen["fonts"]["title"]["family"], family(&display));
    assert_eq!(seen["navigation_menu"], "mega");
    assert_eq!(
        seen["icon_url"],
        format!("/api/brand/images/{}", sha(&icon))
    );
    join(None).await;
    let seen = effective(&server, &vic, &team).await;
    assert_eq!(seen["theme_from"], "site");
    assert_eq!(seen["fonts"]["title"], json!({ "family": "system-serif" }));
}

#[tokio::test]
async fn icon_falls_back_thumbnail_then_org_default() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let mia = server.user("Mia").await;
    let ned = server.user("Ned").await;
    managers(&server, &root, &[&mia]).await;
    let (team, documents, assets) = site_with_assets(&server, &root, "Team").await;
    grant(&server, &root, &assets, &mia, "viewer").await;

    // Nothing: ShareCraft's own icon (no address).
    assert_eq!(
        effective(&server, &root, &team).await["icon_url"],
        Value::Null
    );
    assert_eq!(centre(&server, &ned).await["default_icon_url"], Value::Null);

    // The organisation's default icon: a brand image of kind icon.
    let org_icon = png(48, 48);
    let org_doc = upload(&server, &root, &assets, "Org.png", &org_icon).await;
    let mut images = Vec::new();
    for kind in ["icon", "logo"] {
        let (status, body) = call(
            &server,
            Some(&mia),
            Method::POST,
            "/api/brand/images",
            Some(json!({ "doc": org_doc, "kind": kind })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        images.push(body["id"].as_str().unwrap().to_owned());
    }
    let put_default = |user: &TestUser, image: Value| {
        let server = &server;
        let user = user.clone();
        async move {
            call(
                server,
                Some(&user),
                Method::PUT,
                "/api/brand/default-icon",
                Some(json!({ "image": image })),
            )
            .await
        }
    };
    let (status, _) = put_default(&ned, json!(images[0])).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, body) = put_default(&mia, json!(images[1])).await;
    assert_eq!(
        (status, &body["error"]),
        (StatusCode::UNPROCESSABLE_ENTITY, &json!("not_an_icon"))
    );
    let (status, body) = put_default(&mia, json!(Uuid::new_v4())).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, body) = put_default(&mia, json!(images[0])).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let org_url = format!("/api/brand/images/{}", sha(&org_icon));
    assert_eq!(centre(&server, &ned).await["default_icon_url"], org_url);
    assert_eq!(in_git(&server).default_icon.unwrap().to_string(), images[0]);
    assert_eq!(effective(&server, &root, &team).await["icon_url"], org_url);

    // The site's thumbnail comes before it.
    let thumb = png(20, 20);
    let thumb_doc = upload(&server, &root, &documents, "Thumb.png", &thumb).await;
    set_look(
        &server,
        &root,
        &team,
        json!({ "theme": { "from": "builtin:sharecraft" }, "thumbnail": { "doc": thumb_doc } }),
    )
    .await;
    let thumb_url = format!("/api/brand/images/{}", sha(&thumb));
    assert_eq!(
        effective(&server, &root, &team).await["icon_url"],
        thumb_url
    );
    // Its own icon before both.
    let icon = png(40, 40);
    let icon_doc = upload(&server, &root, &documents, "Icon.png", &icon).await;
    set_look(
        &server,
        &root,
        &team,
        json!({ "theme": { "from": "builtin:sharecraft" }, "thumbnail": { "doc": thumb_doc },
                "icon": { "doc": icon_doc } }),
    )
    .await;
    assert_eq!(
        effective(&server, &root, &team).await["icon_url"],
        format!("/api/brand/images/{}", sha(&icon))
    );

    // The default icon's image removed: there is no default any more.
    set_look(
        &server,
        &root,
        &team,
        json!({ "theme": { "from": "builtin:sharecraft" } }),
    )
    .await;
    let (status, _) = call(
        &server,
        Some(&mia),
        Method::DELETE,
        &format!("/api/brand/images/{}", images[0]),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(centre(&server, &ned).await["default_icon_url"], Value::Null);
    assert!(in_git(&server).default_icon.is_none());
    assert_eq!(
        effective(&server, &root, &team).await["icon_url"],
        Value::Null
    );
    // Cleared on purpose.
    let (status, body) = call(
        &server,
        Some(&mia),
        Method::POST,
        "/api/brand/images",
        Some(json!({ "doc": org_doc, "kind": "icon" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let again = body["id"].as_str().unwrap().to_owned();
    let (status, _) = put_default(&mia, json!(again)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = put_default(&mia, Value::Null).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(in_git(&server).default_icon.is_none());
}

#[tokio::test]
async fn reconcile_rebuilds_font_pins() {
    let data_dir = tempfile::tempdir().unwrap();
    let db = TestDb::create().await;
    let mut first = StoppableServer::start(data_dir.path(), &db, |_| {});
    let root = first.admin("Root").await;
    let (team, documents, assets) = site_with_assets(&first, &root, "Team").await;
    let org_font = woff2(41);
    let org_id = add_font(
        &first,
        &root,
        &upload(&first, &root, &assets, "Org.woff2", &org_font).await,
    )
    .await;
    let (status, body) = new_package(
        &first,
        &root,
        json!({ "name": "Brand", "display": { "brand_font": org_id },
                "roles": roles("display", "display", "system", "system") }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let package = body["id"].as_str().unwrap().to_owned();
    let own = woff2(42);
    let own_doc = upload(&first, &root, &documents, "Own.woff2", &own).await;
    let (status, body) = new_site_package(
        &first,
        &root,
        &team,
        json!({ "name": "Ours", "content": { "doc": own_doc, "licence_confirmed": true },
                "roles": roles("system", "system", "content", "content") }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    set_look(
        &first,
        &root,
        &team,
        json!({ "theme": { "from": "builtin:sharecraft" },
                "fonts": { "from": format!("org:{package}") } }),
    )
    .await;
    first.stop();

    // The mirror is lost.
    let pool = db.db.test_pool();
    sqlx::query("delete from brand_refs")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("update sites set font_packages = '[]', look = null")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "update org_brand set record = \
         '{\"schema\": 1, \"managers\": [], \"themes\": [], \"images\": []}'",
    )
    .execute(pool)
    .await
    .unwrap();

    let second = StoppableServer::start(data_dir.path(), &db, |_| {});
    let rita = second.admin("Rita").await;
    let records = second.records_db();
    assert_eq!(
        records.brand_pins("org").await.unwrap(),
        vec![(sha(&org_font), "font/woff2".to_owned())]
    );
    let mut wanted = vec![
        (sha(&org_font), "font/woff2".to_owned()),
        (sha(&own), "font/woff2".to_owned()),
    ];
    wanted.sort();
    assert_eq!(records.brand_pins(&team).await.unwrap(), wanted);
    let seen = centre(&second, &rita).await;
    assert_eq!(seen["fonts"][0]["id"], json!(org_id));
    assert_eq!(seen["font_packages"][2]["id"], json!(package));
    assert_eq!(
        settings(&second, &rita, &team).await["font_packages"][0]["name"],
        "Ours"
    );
    assert_eq!(
        effective(&second, &rita, &team).await["fonts"]["title"]["url"],
        font_url(&org_font)
    );
    for bytes in [&org_font, &own] {
        assert_eq!(
            fetch(&second, Some(&rita), &font_url(bytes)).await.0,
            StatusCode::OK
        );
    }
}

#[tokio::test]
async fn fonts_keep_after_source_package_deleted() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let olive = server.user("Olive").await;
    let (team, documents, assets) = site_with_assets(&server, &root, "Team").await;
    grant(&server, &root, &team, &olive, "owner").await;
    let bytes = woff2(51);
    let font_id = add_font(
        &server,
        &root,
        &upload(&server, &root, &assets, "Brand.woff2", &bytes).await,
    )
    .await;
    let (status, body) = new_package(
        &server,
        &root,
        json!({ "name": "Brand", "display": { "brand_font": font_id },
                "roles": roles("display", "display", "system", "system") }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let package = body["id"].as_str().unwrap().to_owned();
    set_look(
        &server,
        &olive,
        &team,
        json!({ "theme": { "from": "builtin:plum" },
                "fonts": { "from": format!("org:{package}") } }),
    )
    .await;
    let kept_fonts = settings(&server, &olive, &team).await["look"]["fonts"].clone();

    // The source package is deleted: naming it fails, keeping the copy works.
    let (status, _) = call(
        &server,
        Some(&root),
        Method::DELETE,
        &format!("/api/brand/font-packages/{package}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, body) = put_look(
        &server,
        &olive,
        &team,
        json!({ "theme": { "from": "builtin:forest" },
                "fonts": { "from": format!("org:{package}") } }),
    )
    .await;
    assert_eq!(
        (status, &body["error"]),
        (StatusCode::UNPROCESSABLE_ENTITY, &json!("invalid_look"))
    );
    let answer = set_look(
        &server,
        &olive,
        &team,
        json!({ "theme": { "from": "builtin:forest" }, "fonts": { "keep": true } }),
    )
    .await;
    assert_eq!(answer["look"]["fonts"], kept_fonts);
    assert_eq!(answer["look"]["from"], "builtin:forest");
    assert_eq!(
        effective(&server, &olive, &team).await["fonts"]["title"]["url"],
        font_url(&bytes)
    );
    assert_eq!(
        server.records_db().brand_pins(&team).await.unwrap(),
        vec![(sha(&bytes), "font/woff2".to_owned())]
    );
    // Both ways at once, or neither, are refused.
    for fonts in [
        json!({ "keep": true, "from": "builtin:system" }),
        json!({}),
        json!({ "keep": false }),
    ] {
        let (status, body) = put_look(
            &server,
            &olive,
            &team,
            json!({ "theme": { "from": "builtin:forest" }, "fonts": fonts }),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{fonts}: {body}");
    }

    // A kept copy is the look already stored, so a policy change does not stand in its way:
    // its source is gone, yet an owner keeps it while site-only fonts are turned off.
    let (status, body) = call(
        &server,
        Some(&root),
        Method::PUT,
        "/api/brand/policy",
        Some(json!({ "site_fonts": false })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let answer = set_look(
        &server,
        &olive,
        &team,
        json!({ "theme": { "from": "builtin:plum" }, "fonts": { "keep": true } }),
    )
    .await;
    assert_eq!(answer["look"]["fonts"], kept_fonts);
    set_look(
        &server,
        &root,
        &team,
        json!({ "theme": { "from": "builtin:plum" }, "fonts": { "keep": true } }),
    )
    .await;

    // A site package's copy is kept the same way once the package is gone.
    let own = woff2(52);
    let own_doc = upload(&server, &root, &documents, "Own.woff2", &own).await;
    let (status, body) = new_site_package(
        &server,
        &root,
        &team,
        json!({ "name": "Ours", "content": { "doc": own_doc, "licence_confirmed": true },
                "roles": roles("system", "system", "content", "content") }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let site_package = body["id"].as_str().unwrap().to_owned();
    set_look(
        &server,
        &root,
        &team,
        json!({ "theme": { "from": "builtin:plum" },
                "fonts": { "from": format!("site:{site_package}") } }),
    )
    .await;
    let (status, _) = call(
        &server,
        Some(&root),
        Method::DELETE,
        &format!("/api/sites/{team}/font-packages/{site_package}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let answer = set_look(
        &server,
        &root,
        &team,
        json!({ "theme": { "from": "builtin:sharecraft" }, "fonts": { "keep": true } }),
    )
    .await;
    assert_eq!(
        answer["look"]["fonts"]["from"],
        format!("site:{site_package}")
    );
    assert!(server.blobs().contains(&hash(&own)));
    assert_eq!(
        effective(&server, &root, &team).await["fonts"]["body"]["url"],
        font_url(&own)
    );
}

#[tokio::test]
async fn kept_font_copy_survives_a_brand_centre_edit() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let olive = server.user("Olive").await;
    let (team, _, assets) = site_with_assets(&server, &root, "Team").await;
    grant(&server, &root, &team, &olive, "owner").await;
    let font_id = add_font(
        &server,
        &root,
        &upload(&server, &root, &assets, "Brand.woff2", &woff2(61)).await,
    )
    .await;
    let (status, body) = new_package(
        &server,
        &root,
        json!({ "name": "Brand", "display": { "brand_font": font_id },
                "roles": roles("display", "display", "system", "system") }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let package = body["id"].as_str().unwrap().to_owned();
    set_look(
        &server,
        &olive,
        &team,
        json!({ "theme": { "from": "builtin:plum" },
                "fonts": { "from": format!("org:{package}") } }),
    )
    .await;
    let kept_fonts = settings(&server, &olive, &team).await["look"]["fonts"].clone();
    assert_eq!(
        settings(&server, &olive, &team).await["fonts_source_changed"],
        false
    );

    // Site-only font packages are turned off, then the brand centre edits the package.
    let (status, body) = call(
        &server,
        Some(&root),
        Method::PUT,
        "/api/brand/policy",
        Some(json!({ "site_fonts": false })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = call(
        &server,
        Some(&root),
        Method::PATCH,
        &format!("/api/brand/font-packages/{package}"),
        Some(json!({ "name": "Brand 2",
                     "roles": roles("display", "system", "system", "system") })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        settings(&server, &olive, &team).await["fonts_source_changed"],
        true
    );

    // The owner still saves a header change, keeping the copy unchanged.
    let answer = set_look(
        &server,
        &olive,
        &team,
        json!({ "theme": { "from": "builtin:plum" }, "fonts": { "keep": true },
                "header": { "layout": "extended" } }),
    )
    .await;
    assert_eq!(answer["look"]["fonts"], kept_fonts);
    assert_eq!(answer["look"]["header"]["layout"], "extended");

    // Taking the package again takes the new version, which the policy allows.
    let answer = set_look(
        &server,
        &olive,
        &team,
        json!({ "theme": { "from": "builtin:plum" },
                "fonts": { "from": format!("org:{package}") } }),
    )
    .await;
    assert_eq!(answer["look"]["fonts"]["package"]["name"], "Brand 2");
    assert_eq!(answer["fonts_source_changed"], false);
}

#[tokio::test]
async fn effective_look_gives_a_hub_members_own_theme_and_fonts() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let vic = server.user("Vic").await;
    let (hub, _, _) = site_with_assets(&server, &root, "Hub").await;
    let (team, _, _) = site_with_assets(&server, &root, "Team").await;
    grant(&server, &root, &team, &vic, "viewer").await;
    set_look(
        &server,
        &root,
        &hub,
        json!({ "theme": { "from": "builtin:plum" }, "fonts": { "from": "builtin:classic" } }),
    )
    .await;
    set_look(
        &server,
        &root,
        &team,
        json!({ "theme": { "from": "builtin:forest" } }),
    )
    .await;
    // Outside a hub the effective look is the site's own, and `own` is left out.
    let alone = effective(&server, &vic, &team).await;
    assert_eq!(alone["theme_from"], "site");
    assert!(alone.get("own").is_none(), "{alone}");

    let (status, body) = call(
        &server,
        Some(&root),
        Method::PUT,
        &format!("/api/sites/{hub}/hub"),
        Some(json!({ "navigation": [] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = call(
        &server,
        Some(&root),
        Method::PUT,
        &format!("/api/sites/{team}/joined-hub"),
        Some(json!({ "hub": hub })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // In the hub the page shows the hub's theme and fonts; `own` gives the site's own, resolved
    // as the effective look resolves them, which is what copying the site writes.
    let joined = effective(&server, &vic, &team).await;
    assert_eq!(joined["theme_from"], "hub");
    assert_eq!(
        joined["fonts"]["title"],
        json!({ "family": "system-serif" })
    );
    assert_eq!(joined["own"]["theme"], alone["theme"]);
    assert_eq!(joined["own"]["fonts"], alone["fonts"]);
    assert_eq!(joined["own"]["fonts"], system_fonts());
    assert_ne!(joined["own"]["theme"], joined["theme"]);
}

#[tokio::test]
async fn effective_look_carries_the_page_and_keeps_it_from_the_hub() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let vic = server.user("Vic").await;
    let (hub, _, _) = site_with_assets(&server, &root, "Hub").await;
    let (team, _, _) = site_with_assets(&server, &root, "Team").await;
    grant(&server, &root, &team, &vic, "viewer").await;

    // A plain page by default; paper when the look says so, and it is recorded.
    assert_eq!(effective(&server, &vic, &team).await["page"], "plain");
    set_look(
        &server,
        &root,
        &team,
        json!({ "theme": { "from": "builtin:stationery" }, "page": "paper" }),
    )
    .await;
    assert_eq!(effective(&server, &vic, &team).await["page"], "paper");
    let changed = events(&server, "site.look_changed").await;
    assert_eq!(changed.last().unwrap()["after"]["page"], "paper");
    // Nothing but plain or paper.
    let (status, body) = put_look(
        &server,
        &root,
        &team,
        json!({ "theme": { "from": "builtin:stationery" }, "page": "linen" }),
    )
    .await;
    assert!(status.is_client_error(), "{status}: {body}");
    assert_eq!(effective(&server, &vic, &team).await["page"], "paper");

    // The page goes with the site: a plain member of a paper hub stays plain.
    set_look(&server, &root, &hub, json!({ "page": "paper" })).await;
    set_look(&server, &root, &team, json!({ "theme": { "from": "builtin:forest" } })).await;
    let (status, body) = call(
        &server,
        Some(&root),
        Method::PUT,
        &format!("/api/sites/{hub}/hub"),
        Some(json!({ "navigation": [] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = call(
        &server,
        Some(&root),
        Method::PUT,
        &format!("/api/sites/{team}/joined-hub"),
        Some(json!({ "hub": hub })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let seen = effective(&server, &vic, &team).await;
    assert_eq!(seen["theme_from"], "hub");
    assert_eq!(seen["page"], "plain");
    assert_eq!(effective(&server, &root, &hub).await["page"], "paper");
}