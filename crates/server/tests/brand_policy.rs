//! The brand policy and copying looks over HTTP: only administrators set the limits on site
//! owners; the limits hold on every path that sets a look (changing it, copying one, rolling
//! back, and making site themes and font packages) for site owners only; a look already stored
//! survives a policy change; copying a look needs view on its source and `manage_site` on its
//! target, pins its images and fonts to the target in one commit and records where it came
//! from.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::io::Cursor;

use common::{Site, TestServer, TestUser, spawn_server};
use image::{DynamicImage, ImageFormat, Rgb, RgbImage};
use reqwest::{Method, StatusCode};
use revisions::brand::{BrandPolicy, Look};
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

/// `PUT /api/brand/policy` as `user`.
async fn put_policy(site: &Site, user: &TestUser, policy: Value) -> (StatusCode, Value) {
    call(
        site,
        Some(user),
        Method::PUT,
        "/api/brand/policy",
        Some(policy),
    )
    .await
}

/// Sets the policy as `admin`, which must succeed.
async fn set_policy(site: &Site, admin: &TestUser, policy: Value) {
    let (status, body) = put_policy(site, admin, policy).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// Every limit on.
fn locked() -> Value {
    json!({ "hide_builtin_themes": true, "custom_colours": false, "site_themes": false,
            "site_images": false, "site_fonts": false })
}

/// Every limit off.
fn open() -> Value {
    json!({ "hide_builtin_themes": false, "custom_colours": true, "site_themes": true,
            "site_images": true, "site_fonts": true })
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

/// `POST /api/sites/{id}/look/copy` as `user`.
async fn copy(site: &Site, user: Option<&TestUser>, id: &str, body: Value) -> (StatusCode, Value) {
    call(
        site,
        user,
        Method::POST,
        &format!("/api/sites/{id}/look/copy"),
        Some(body),
    )
    .await
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

/// `GET /api/sites/{id}/look/history` as `user`, which must succeed.
async fn history(site: &Site, user: &TestUser, id: &str) -> Vec<Value> {
    let (status, body) = call(
        site,
        Some(user),
        Method::GET,
        &format!("/api/sites/{id}/look/history"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body.as_array().unwrap().clone()
}

/// The problems of a `409 not_allowed` answer.
fn problems(status: StatusCode, body: &Value) -> Vec<String> {
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "not_allowed", "{body}");
    body["problems"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap().to_owned())
        .collect()
}

/// Whether one of `problems` mentions `words`.
fn mentions(problems: &[String], words: &str) -> bool {
    problems.iter().any(|p| p.contains(words))
}

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

fn woff2(seed: u8) -> Vec<u8> {
    let mut bytes = b"wOF2".to_vec();
    bytes.extend((0..200u16).map(|i| (i as u8).wrapping_mul(seed).wrapping_add(seed)));
    bytes
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

/// The details of the events of `kind`, oldest first.
async fn events(site: &Site, kind: &str) -> Vec<Value> {
    sqlx::query_scalar("select detail from events where kind = $1 order by id")
        .bind(kind)
        .fetch_all(site.records_db().test_pool())
        .await
        .unwrap()
}

/// The commit `main` points at.
fn tip(server: &TestServer) -> gix::ObjectId {
    let repo = gix::open(server.library()).unwrap();
    repo.find_reference("refs/heads/main")
        .unwrap()
        .id()
        .detach()
}

/// The bytes of the site record of `id` on `main`.
fn record_bytes(server: &TestServer, id: &str) -> Vec<u8> {
    let repo = gix::open(server.library()).unwrap();
    let tree = repo.find_commit(tip(server)).unwrap().tree().unwrap();
    let path = format!("sites/{}/{id}.json", &id[..2]);
    let entry = tree.lookup_entry_by_path(&path).unwrap().unwrap();
    entry.object().unwrap().data.clone()
}

/// The site record of `id` as Git holds it.
fn record(server: &TestServer, id: &str) -> revisions::SiteRecord {
    server
        .store()
        .read_site_record(Uuid::parse_str(id).unwrap())
        .unwrap()
        .unwrap()
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

fn custom() -> Value {
    json!({ "name": "Ours", "primary": "#1f5f8b", "secondary": "#163447" })
}

#[tokio::test]
async fn policy_set_by_admins_only() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let mia = server.user("Mia").await;
    let ned = server.user("Ned").await;
    managers(&server, &root, &[&mia]).await;

    // Every limit starts off.
    let (status, seen) = call(&server, Some(&ned), Method::GET, "/api/brand", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(seen["policy"], open());
    let in_git = server.store().read_org_brand().unwrap().unwrap_or_default();
    assert_eq!(in_git.policy, BrandPolicy::default());

    // Brand managers and everyone else are refused.
    for user in [&mia, &ned] {
        let (status, body) = put_policy(&server, user, locked()).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    }
    let (status, _) = call(
        &server,
        None,
        Method::PUT,
        "/api/brand/policy",
        Some(locked()),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, body) = put_policy(&server, &root, json!({ "everything": false })).await;
    assert!(status.is_client_error(), "{body}");

    // An administrator sets it; each field given is set.
    let (status, body) = put_policy(&server, &root, json!({ "custom_colours": false })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["custom_colours"], false);
    assert_eq!(body["site_images"], true);
    let (status, body) = put_policy(&server, &root, locked()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, locked());
    let in_git = server.store().read_org_brand().unwrap().unwrap();
    assert!(in_git.policy.hide_builtin_themes && !in_git.policy.site_fonts);
    let (_, seen) = call(&server, Some(&ned), Method::GET, "/api/brand", None).await;
    assert_eq!(seen["policy"], locked());
    let changed = events(&server, "brand.policy_changed").await;
    assert_eq!(changed.len(), 2);
    assert_eq!(changed[0]["before"], open());
    assert_eq!(changed[1]["after"], locked());
    // The same again changes nothing.
    set_policy(&server, &root, locked()).await;
    assert_eq!(events(&server, "brand.policy_changed").await.len(), 2);
}

#[tokio::test]
async fn policy_holds_on_every_path() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let mia = server.user("Mia").await;
    let olive = server.user("Olive").await;
    managers(&server, &root, &[&mia]).await;
    let (team, documents, assets) = site_with_assets(&server, &root, "Team").await;
    let (other, other_documents, _) = site_with_assets(&server, &root, "Other").await;
    for user in [&olive, &mia] {
        grant(&server, &root, &team, user, "owner").await;
        grant(&server, &root, &other, user, "viewer").await;
    }
    // The brand centre: a theme, a logo and a font package.
    let (status, body) = call(
        &server,
        Some(&root),
        Method::POST,
        "/api/brand/themes",
        Some(json!({ "name": "Ocean", "primary": "#1F5F8B", "secondary": "#163447" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let ocean = body["id"].as_str().unwrap().to_owned();
    let logo = png(10, 10);
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
    let brand_logo = body["id"].as_str().unwrap().to_owned();
    let (status, body) = call(
        &server,
        Some(&root),
        Method::POST,
        "/api/brand/font-packages",
        Some(json!({ "name": "Serif", "roles": roles("system-serif", "system-serif", "system", "system") })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let org_package = body["id"].as_str().unwrap().to_owned();
    let site_logo = upload(&server, &root, &documents, "Own.png", &png(11, 11)).await;
    let site_font = upload(&server, &root, &documents, "Own.woff2", &woff2(3)).await;
    // History to roll back to: a custom look set while nothing was limited.
    set_look(&server, &olive, &team, json!({ "theme": custom() })).await;
    let custom_entry = history(&server, &olive, &team).await[0]["event_id"]
        .as_i64()
        .unwrap();
    set_look(
        &server,
        &root,
        &other,
        json!({ "theme": custom(), "logo": { "doc": upload(&server, &root, &other_documents, "O.png", &png(12, 12)).await } }),
    )
    .await;

    set_policy(&server, &root, locked()).await;
    let org_look = json!({ "theme": { "from": format!("org:{ocean}") } });

    // PUT look.
    let (status, body) = put_look(
        &server,
        &olive,
        &team,
        json!({ "theme": { "from": "builtin:plum" } }),
    )
    .await;
    assert!(mentions(
        &problems(status, &body),
        "built-in themes are turned off for site owners"
    ));
    let (status, body) = put_look(&server, &olive, &team, json!({ "theme": custom() })).await;
    assert!(mentions(
        &problems(status, &body),
        "custom colours are turned off for site owners"
    ));
    let (status, body) = put_look(
        &server,
        &olive,
        &team,
        json!({ "theme": { "from": format!("org:{ocean}") }, "logo": { "doc": site_logo } }),
    )
    .await;
    assert!(mentions(&problems(status, &body), "logo"));
    set_look(
        &server,
        &olive,
        &team,
        json!({ "theme": { "from": format!("org:{ocean}") },
                "logo": { "brand_image": brand_logo },
                "fonts": { "from": format!("org:{org_package}") } }),
    )
    .await;
    set_look(
        &server,
        &olive,
        &team,
        json!({ "theme": { "from": format!("org:{ocean}") },
                                              "fonts": { "from": "builtin:classic" } }),
    )
    .await;
    // Brand managers and administrators are never limited.
    set_look(
        &server,
        &mia,
        &team,
        json!({ "theme": custom(), "logo": { "doc": site_logo } }),
    )
    .await;
    set_look(
        &server,
        &root,
        &team,
        json!({ "theme": { "from": "builtin:plum" } }),
    )
    .await;

    // Site themes and site font packages.
    let (status, body) = call(
        &server,
        Some(&olive),
        Method::POST,
        &format!("/api/sites/{team}/themes"),
        Some(custom()),
    )
    .await;
    assert!(mentions(
        &problems(status, &body),
        "site-only themes are turned off"
    ));
    let (status, body) = call(
        &server,
        Some(&olive),
        Method::POST,
        &format!("/api/sites/{team}/font-packages"),
        Some(json!({ "name": "Ours", "roles": roles("system", "system", "system", "system") })),
    )
    .await;
    assert!(mentions(
        &problems(status, &body),
        "site-only font packages are turned off"
    ));
    let (status, body) = call(
        &server,
        Some(&root),
        Method::POST,
        &format!("/api/sites/{team}/themes"),
        Some(custom()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let site_theme = body["id"].as_str().unwrap().to_owned();
    let (status, body) = call(
        &server,
        Some(&mia),
        Method::POST,
        &format!("/api/sites/{team}/font-packages"),
        Some(
            json!({ "name": "Ours", "content": { "doc": site_font, "licence_confirmed": true },
                     "roles": roles("system", "system", "content", "content") }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let site_package = body["id"].as_str().unwrap().to_owned();
    let (status, body) = call(
        &server,
        Some(&olive),
        Method::PATCH,
        &format!("/api/sites/{team}/themes/{site_theme}"),
        Some(json!({ "name": "Renamed" })),
    )
    .await;
    assert!(mentions(
        &problems(status, &body),
        "site-only themes are turned off"
    ));
    let (status, body) = call(
        &server,
        Some(&olive),
        Method::PATCH,
        &format!("/api/sites/{team}/font-packages/{site_package}"),
        Some(json!({ "name": "Renamed" })),
    )
    .await;
    assert!(mentions(
        &problems(status, &body),
        "site-only font packages are turned off"
    ));
    // An owner may not use them in a look either.
    let (status, body) = put_look(
        &server,
        &olive,
        &team,
        json!({ "theme": { "from": format!("site:{site_theme}") } }),
    )
    .await;
    assert!(mentions(
        &problems(status, &body),
        "site-only themes are turned off"
    ));
    let (status, body) = put_look(
        &server,
        &olive,
        &team,
        json!({ "theme": { "from": format!("org:{ocean}") },
                "fonts": { "from": format!("site:{site_package}") } }),
    )
    .await;
    assert!(mentions(
        &problems(status, &body),
        "site-only font packages are turned off"
    ));

    // Rollback to a look the limits refuse.
    let (status, body) = call(
        &server,
        Some(&olive),
        Method::POST,
        &format!("/api/sites/{team}/look/rollback"),
        Some(json!({ "event_id": custom_entry })),
    )
    .await;
    assert!(mentions(
        &problems(status, &body),
        "custom colours are turned off"
    ));
    let (status, body) = call(
        &server,
        Some(&root),
        Method::POST,
        &format!("/api/sites/{team}/look/rollback"),
        Some(json!({ "event_id": custom_entry })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Copying a look the limits refuse: custom colours, and an image from the other site.
    let (status, body) = copy(&server, Some(&olive), &team, json!({ "from": other })).await;
    let found = problems(status, &body);
    assert!(
        mentions(&found, "custom colours are turned off"),
        "{found:?}"
    );
    assert!(mentions(&found, "logo"), "{found:?}");
    set_look(&server, &root, &other, org_look.clone()).await;
    let (status, body) = copy(&server, Some(&olive), &team, json!({ "from": other })).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // A site theme and package copied in the same request are known, so a look that uses
    // them is not mistaken for custom colours or unknown fonts.
    set_policy(
        &server,
        &root,
        json!({ "hide_builtin_themes": true, "custom_colours": false, "site_themes": true,
                "site_images": true, "site_fonts": true }),
    )
    .await;
    let (status, body) = call(
        &server,
        Some(&root),
        Method::POST,
        &format!("/api/sites/{other}/themes"),
        Some(json!({ "name": "Theirs", "primary": "#2f6b3a", "secondary": "#1d3324" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let their_theme = body["id"].as_str().unwrap().to_owned();
    let (status, body) = call(
        &server,
        Some(&root),
        Method::POST,
        &format!("/api/sites/{other}/font-packages"),
        Some(json!({ "name": "Theirs", "roles": roles("system-serif", "system", "system", "system") })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let their_package = body["id"].as_str().unwrap().to_owned();
    set_look(
        &server,
        &root,
        &other,
        json!({ "theme": { "from": format!("site:{their_theme}") },
                "fonts": { "from": format!("site:{their_package}") } }),
    )
    .await;
    let (status, body) = copy(&server, Some(&olive), &team, json!({ "from": other })).await;
    assert!(mentions(
        &problems(status, &body),
        "custom colours are turned off"
    ));
    let (status, body) = copy(
        &server,
        Some(&olive),
        &team,
        json!({ "from": other, "themes": true, "fonts": true }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let copied = record(&server, &team);
    assert!(
        copied
            .themes
            .iter()
            .any(|t| t.id.to_string() == their_theme)
    );
    assert!(
        copied
            .font_packages
            .iter()
            .any(|p| p.id.to_string() == their_package)
    );
    assert_eq!(copied.look.unwrap().from, format!("site:{their_theme}"));
}

#[tokio::test]
async fn existing_looks_survive_a_policy_change() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let olive = server.user("Olive").await;
    let (team, documents, _) = site_with_assets(&server, &root, "Team").await;
    grant(&server, &root, &team, &olive, "owner").await;
    let logo = png(14, 14);
    let logo_doc = upload(&server, &root, &documents, "Logo.png", &logo).await;
    let font_doc = upload(&server, &root, &documents, "Own.woff2", &woff2(5)).await;
    let (status, body) = call(
        &server,
        Some(&olive),
        Method::POST,
        &format!("/api/sites/{team}/font-packages"),
        Some(
            json!({ "name": "Ours", "display": { "doc": font_doc, "licence_confirmed": true },
                     "roles": roles("display", "display", "system", "system") }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let package = body["id"].as_str().unwrap().to_owned();
    let (status, body) = call(
        &server,
        Some(&olive),
        Method::POST,
        &format!("/api/sites/{team}/themes"),
        Some(custom()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let theme = body["id"].as_str().unwrap().to_owned();
    set_look(
        &server,
        &olive,
        &team,
        json!({ "theme": custom(), "logo": { "doc": logo_doc },
                "fonts": { "from": format!("site:{package}") } }),
    )
    .await;
    let bytes = record_bytes(&server, &team);
    let before = effective(&server, &olive, &team).await;

    // Every limit on: the stored look is untouched and still shown.
    set_policy(&server, &root, locked()).await;
    assert_eq!(record_bytes(&server, &team), bytes);
    assert_eq!(effective(&server, &olive, &team).await, before);
    let stored: Look = record(&server, &team).look.unwrap();
    assert_eq!(stored.from, "custom");
    assert!(server.blobs().contains(&hash(&logo)));

    // The next change is limited.
    let (status, body) = put_look(
        &server,
        &olive,
        &team,
        json!({ "theme": custom(), "logo": { "keep": sha(&logo) } }),
    )
    .await;
    let found = problems(status, &body);
    assert!(mentions(&found, "custom colours"), "{found:?}");
    assert!(mentions(&found, "logo"), "{found:?}");
    assert_eq!(record_bytes(&server, &team), bytes);
    // Removing a site theme is never limited.
    let (status, body) = call(
        &server,
        Some(&olive),
        Method::DELETE,
        &format!("/api/sites/{team}/themes/{theme}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    // Limits off again: the owner may.
    set_policy(&server, &root, open()).await;
    set_look(
        &server,
        &olive,
        &team,
        json!({ "theme": custom(), "logo": { "keep": sha(&logo) } }),
    )
    .await;
}

#[tokio::test]
async fn copy_look_needs_view_and_manage() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let olive = server.user("Olive").await;
    let ed = server.user("Ed").await;
    let (source, _, _) = site_with_assets(&server, &root, "Source").await;
    let (target, _, _) = site_with_assets(&server, &root, "Target").await;
    grant(&server, &root, &target, &olive, "owner").await;
    grant(&server, &root, &target, &ed, "editor").await;
    grant(&server, &root, &source, &ed, "viewer").await;
    set_look(
        &server,
        &root,
        &source,
        json!({ "theme": { "from": "builtin:plum" }, "navigation": "vertical" }),
    )
    .await;

    // Signed out; a source the caller cannot see; a target that is not a site.
    let (status, _) = copy(&server, None, &target, json!({ "from": source })).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, body) = copy(&server, Some(&olive), &target, json!({ "from": source })).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, body) = copy(
        &server,
        Some(&root),
        &target,
        json!({ "from": Uuid::new_v4() }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, body) = copy(
        &server,
        Some(&root),
        &Uuid::new_v4().to_string(),
        json!({ "from": source }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, body) = copy(&server, Some(&root), &target, json!({ "from": "nonsense" })).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    // Viewing the source is not enough without manage_site on the target.
    let (status, body) = copy(&server, Some(&ed), &target, json!({ "from": source })).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(record(&server, &target).look, None);

    // View on the source and manage_site on the target: copied.
    grant(&server, &root, &source, &olive, "viewer").await;
    let (status, body) = copy(&server, Some(&olive), &target, json!({ "from": source })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["look"]["from"], "builtin:plum");
    assert_eq!(body["look"]["navigation"], "vertical");
    assert_eq!(record(&server, &target).look, record(&server, &source).look);

    // An archived target refuses; an archived source can still be copied from.
    let (status, body) = call(
        &server,
        Some(&root),
        Method::POST,
        &format!("/api/sites/{target}/archive"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = copy(&server, Some(&root), &target, json!({ "from": source })).await;
    assert_eq!(
        (status, &body["error"]),
        (StatusCode::FORBIDDEN, &json!("archived"))
    );
    let (status, body) = call(
        &server,
        Some(&root),
        Method::POST,
        &format!("/api/sites/{source}/archive"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (other, _, _) = site_with_assets(&server, &root, "Other").await;
    let (status, body) = copy(&server, Some(&root), &other, json!({ "from": source })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn copy_pins_snapshots_and_records_history() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let olive = server.user("Olive").await;
    let (source, documents, _) = site_with_assets(&server, &root, "Source").await;
    let (target, _, _) = site_with_assets(&server, &root, "Target").await;
    grant(&server, &root, &target, &olive, "owner").await;
    grant(&server, &root, &source, &olive, "viewer").await;
    let logo = png(15, 15);
    let logo_doc = upload(&server, &root, &documents, "Logo.png", &logo).await;
    let font = woff2(7);
    let font_doc = upload(&server, &root, &documents, "Own.woff2", &font).await;
    let (status, body) = call(
        &server,
        Some(&root),
        Method::POST,
        &format!("/api/sites/{source}/themes"),
        Some(custom()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let theme = body["id"].as_str().unwrap().to_owned();
    let (status, body) = call(
        &server,
        Some(&root),
        Method::POST,
        &format!("/api/sites/{source}/font-packages"),
        Some(
            json!({ "name": "Ours", "display": { "doc": font_doc, "licence_confirmed": true },
                     "roles": roles("display", "display", "system", "system") }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let package = body["id"].as_str().unwrap().to_owned();
    set_look(
        &server,
        &root,
        &source,
        json!({ "theme": { "from": format!("site:{theme}") }, "logo": { "doc": logo_doc },
                "fonts": { "from": format!("site:{package}") }, "menu": "mega" }),
    )
    .await;

    let before = tip(&server);
    let (status, body) = copy(
        &server,
        Some(&olive),
        &target,
        json!({ "from": source, "themes": true, "fonts": true }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // One commit.
    let repo = gix::open(server.library()).unwrap();
    let commit = repo.find_commit(tip(&server)).unwrap();
    assert_eq!(commit.parent_ids().next().unwrap().detach(), before);
    let copied = record(&server, &target);
    assert_eq!(copied.look, record(&server, &source).look);
    assert_eq!(copied.themes.len(), 1);
    assert_eq!(copied.font_packages.len(), 1);
    // Pinned to the target as snapshots.
    let mut wanted = vec![
        (sha(&logo), "image/png".to_owned()),
        (sha(&font), "font/woff2".to_owned()),
    ];
    wanted.sort();
    assert_eq!(
        server.records_db().brand_pins(&target).await.unwrap(),
        wanted
    );
    // Recorded with where it came from.
    let changed = events(&server, "site.look_changed").await;
    let last = changed.last().unwrap();
    assert_eq!(last["copied_from"], json!(source));
    assert!(last["before"].is_null(), "{last}");
    assert_eq!(last["after"]["menu"], "mega");
    let entries = history(&server, &olive, &target).await;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["by"]["id"], json!(olive.id.0));

    // Without themes or fonts asked for, the copy names no theme of this site's.
    let (other, _, _) = site_with_assets(&server, &root, "Other").await;
    let (status, body) = copy(&server, Some(&root), &other, json!({ "from": source })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let plain = record(&server, &other);
    assert!(plain.themes.is_empty() && plain.font_packages.is_empty());
    assert_eq!(plain.look.as_ref().unwrap().from, "custom");

    // The source lets go of everything: the target keeps its snapshots.
    set_look(
        &server,
        &root,
        &source,
        json!({ "theme": { "from": "builtin:sharecraft" } }),
    )
    .await;
    let (status, _) = call(
        &server,
        Some(&root),
        Method::DELETE,
        &format!("/api/sites/{source}/font-packages/{package}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    delete_for_good(&server, &root, &logo_doc).await;
    delete_for_good(&server, &root, &font_doc).await;
    assert!(server.blobs().contains(&hash(&logo)));
    assert!(server.blobs().contains(&hash(&font)));
    let seen = effective(&server, &olive, &target).await;
    assert_eq!(
        seen["logo_url"],
        format!("/api/brand/images/{}", sha(&logo))
    );
    assert_eq!(
        seen["fonts"]["title"]["url"],
        format!("/api/brand/fonts/{}", sha(&font))
    );
    assert_eq!(seen["navigation_menu"], "mega");
}
