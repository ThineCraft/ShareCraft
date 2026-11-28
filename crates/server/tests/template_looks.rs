//! Looks in site templates and applying a look to many sites, over HTTP: saving a template from
//! a site keeps its look as references to brand-centre images and fonts (images from the site's
//! own libraries are dropped and listed); packages carry only references, and importing one
//! drops and lists those this organisation does not have; a site made from a template gets the
//! look in its first commit with its images and fonts pinned; and administrators apply a look
//! to many sites at once, one commit and one history entry per site, never limited by the
//! brand policy.

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
use revisions::TemplateRecord;
use revisions::brand::{FontRole, TemplateFontRef, TemplateImage};
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

/// `POST /api/sites {body}` as `user`.
async fn new_site(site: &Site, user: &TestUser, body: Value) -> (StatusCode, Value) {
    call(site, Some(user), Method::POST, "/api/sites", Some(body)).await
}

/// A new site made by `admin`: its id, its `Documents` library and a new asset library.
async fn site_with_assets(site: &Site, admin: &TestUser, name: &str) -> (String, String, String) {
    let (status, body) = new_site(site, admin, json!({ "name": name })).await;
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

/// A new site made by `admin` from nothing but its name: its id.
async fn plain_site(site: &Site, admin: &TestUser, name: &str) -> String {
    let (status, body) = new_site(site, admin, json!({ "name": name })).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["id"].as_str().unwrap().to_owned()
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

/// Every limit on, as `admin`.
async fn lock_down(site: &Site, admin: &TestUser) {
    let (status, body) = call(
        site,
        Some(admin),
        Method::PUT,
        "/api/brand/policy",
        Some(
            json!({ "hide_builtin_themes": true, "custom_colours": false,
                     "site_themes": false, "site_images": false, "site_fonts": false }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// Adds `doc` to the brand centre as an image of `kind`, as `admin`: its id.
async fn brand_image(site: &Site, admin: &TestUser, doc: &str, kind: &str) -> String {
    let (status, body) = call(
        site,
        Some(admin),
        Method::POST,
        "/api/brand/images",
        Some(json!({ "doc": doc, "kind": kind })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["id"].as_str().unwrap().to_owned()
}

/// Adds `doc` to the brand centre as a font, as `admin`: its id.
async fn brand_font(site: &Site, admin: &TestUser, doc: &str) -> String {
    let (status, body) = call(
        site,
        Some(admin),
        Method::POST,
        "/api/brand/fonts",
        Some(json!({ "doc": doc, "licence_confirmed": true })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["id"].as_str().unwrap().to_owned()
}

/// A brand-centre font package named `name` setting titles and headings in the font `font`:
/// its id.
async fn org_package(site: &Site, admin: &TestUser, name: &str, font: &str) -> String {
    let (status, body) = call(
        site,
        Some(admin),
        Method::POST,
        "/api/brand/font-packages",
        Some(json!({ "name": name, "display": { "brand_font": font },
                     "roles": roles("display", "display", "system", "system") })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["id"].as_str().unwrap().to_owned()
}

/// `PUT /api/sites/{id}/look {look}` as `user`, which must succeed.
async fn set_look(site: &Site, user: &TestUser, id: &str, look: Value) {
    let (status, body) = call(
        site,
        Some(user),
        Method::PUT,
        &format!("/api/sites/{id}/look"),
        Some(json!({ "look": look })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// `GET /api/sites/{id}/look` as `user`, which must succeed.
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

/// Saves the site `id` as the template `name`, as `admin`, which must succeed: the answer.
async fn save(site: &Site, admin: &TestUser, id: &str, name: &str) -> Value {
    let (status, body) = call(
        site,
        Some(admin),
        Method::POST,
        &format!("/api/sites/{id}/template"),
        Some(json!({ "name": name })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body
}

/// The package of the template `id`, as `admin`: its bytes.
async fn package(site: &Site, admin: &TestUser, id: &str) -> Vec<u8> {
    let response = site
        .http(Some(admin))
        .get(site.url(&format!("/api/templates/{id}/package")))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    response.bytes().await.unwrap().to_vec()
}

/// Imports the package `bytes` as `admin`.
async fn import(site: &Site, admin: &TestUser, bytes: Vec<u8>) -> (StatusCode, Value) {
    let response = site
        .http(Some(admin))
        .post(site.url("/api/templates/import"))
        .header("content-type", "application/json")
        .body(bytes)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let text = response.text().await.unwrap();
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

/// `POST /api/admin/looks/apply {body}` as `user` (signed out with `None`).
async fn apply(site: &Site, user: Option<&TestUser>, body: Value) -> (StatusCode, Value) {
    call(
        site,
        user,
        Method::POST,
        "/api/admin/looks/apply",
        Some(body),
    )
    .await
}

/// The saved template `id` as Git holds it.
fn template_in_git(server: &TestServer, id: &str) -> TemplateRecord {
    let id = Uuid::parse_str(id).unwrap();
    server
        .store()
        .read_templates()
        .unwrap()
        .into_iter()
        .find(|t| t.id == id)
        .unwrap()
}

/// The site record of `id` as Git holds it.
fn record(server: &TestServer, id: &str) -> revisions::SiteRecord {
    server
        .store()
        .read_site_record(Uuid::parse_str(id).unwrap())
        .unwrap()
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

/// The messages of the commits after `since` up to `main`, newest first.
fn commits_since(server: &TestServer, since: gix::ObjectId) -> Vec<String> {
    let repo = gix::open(server.library()).unwrap();
    let mut out = Vec::new();
    let mut at = tip(server);
    while at != since {
        let commit = repo.find_commit(at).unwrap();
        out.push(commit.message_raw().unwrap().to_string());
        at = commit.parent_ids().next().unwrap().detach();
    }
    out
}

/// The details of the events of `kind`, oldest first.
async fn events(site: &Site, kind: &str) -> Vec<Value> {
    sqlx::query_scalar("select detail from events where kind = $1 order by id")
        .bind(kind)
        .fetch_all(site.records_db().test_pool())
        .await
        .unwrap()
}

/// The pins `owner` holds, as `(sha256, media type)`, sorted.
async fn pins(site: &Site, owner: &str) -> Vec<(String, String)> {
    let mut pins = site.records_db().brand_pins(owner).await.unwrap();
    pins.sort();
    pins
}

fn png(width: u32, height: u32, seed: u8) -> Vec<u8> {
    let image = RgbImage::from_fn(width, height, |x, y| {
        Rgb([
            (x % 256) as u8,
            (y % 256) as u8,
            ((x + y) % 256) as u8 ^ seed,
        ])
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

fn roles(title: &str, heading: &str, body: &str, interactive: &str) -> Value {
    json!({ "title": title, "heading": heading, "body": body, "interactive": interactive })
}

/// A brand centre with a logo, an icon and a font in a package, and a site whose look uses
/// them with a thumbnail from its own library.
struct Branded {
    site: String,
    documents: String,
    logo: Vec<u8>,
    logo_id: String,
    icon: Vec<u8>,
    icon_id: String,
    thumb: Vec<u8>,
    font: Vec<u8>,
    font_id: String,
    package_id: String,
}

async fn branded(server: &TestServer, root: &TestUser) -> Branded {
    let (site, documents, assets) = site_with_assets(server, root, "Marketing").await;
    let logo = png(200, 80, 1);
    let logo_doc = upload(server, root, &assets, "Logo.png", &logo).await;
    let logo_id = brand_image(server, root, &logo_doc, "logo").await;
    let icon = png(64, 64, 2);
    let icon_doc = upload(server, root, &assets, "Icon.png", &icon).await;
    let icon_id = brand_image(server, root, &icon_doc, "icon").await;
    let thumb = png(48, 48, 3);
    let thumb_doc = upload(server, root, &documents, "Ours.png", &thumb).await;
    let font = woff2(21);
    let font_doc = upload(server, root, &assets, "Display.woff2", &font).await;
    let font_id = brand_font(server, root, &font_doc).await;
    let package_id = org_package(server, root, "Brand", &font_id).await;
    set_look(
        server,
        root,
        &site,
        json!({
            "theme": { "from": "builtin:plum" },
            "logo": { "brand_image": logo_id },
            "thumbnail": { "doc": thumb_doc },
            "icon": { "brand_image": icon_id },
            "logo_alt": "Marketing",
            "header": { "hide_title": true },
            "fonts": { "from": format!("org:{package_id}") },
            "menu": "mega",
            "page": "paper",
        }),
    )
    .await;
    Branded {
        site,
        documents,
        logo,
        logo_id,
        icon,
        icon_id,
        thumb,
        font,
        font_id,
        package_id,
    }
}

#[tokio::test]
async fn saving_a_template_keeps_the_look_as_references() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let b = branded(&server, &root).await;

    let saved = save(&server, &root, &b.site, "Marketing look").await;
    assert_eq!(
        saved["dropped"],
        json!([{ "slot": "thumbnail", "name": "Ours.png" }]),
        "{saved}"
    );
    let id = saved["id"].as_str().unwrap();
    let look = template_in_git(&server, id).body.look.unwrap();
    let logo_id = Uuid::parse_str(&b.logo_id).unwrap();
    assert_eq!(look.logo, Some(TemplateImage::BrandImage(logo_id)));
    assert_eq!(
        look.icon,
        Some(TemplateImage::BrandImage(
            Uuid::parse_str(&b.icon_id).unwrap()
        ))
    );
    assert_eq!(look.thumbnail, None);
    assert_eq!(look.from, "builtin:plum");
    assert!(look.header.hide_title);
    let fonts = look.fonts.unwrap();
    assert_eq!(fonts.from, format!("org:{}", b.package_id));
    assert_eq!(
        fonts.package.display,
        Some(TemplateFontRef::BrandFont(
            Uuid::parse_str(&b.font_id).unwrap()
        ))
    );
    assert_eq!(fonts.package.roles.title, FontRole::Display);
    // The event lists what was dropped.
    let saved_events = events(&server, "template.saved").await;
    assert_eq!(
        saved_events.last().unwrap()["dropped"],
        json!([{ "slot": "thumbnail", "name": "Ours.png" }])
    );

    // A site without a look saves a template without one, and drops nothing.
    let plain = plain_site(&server, &root, "Plain").await;
    let saved = save(&server, &root, &plain, "Plain").await;
    assert_eq!(saved["dropped"], json!([]), "{saved}");
    let id = saved["id"].as_str().unwrap();
    assert!(template_in_git(&server, id).body.look.is_none());
    let text = package(&server, &root, id).await;
    let text = String::from_utf8(text).unwrap();
    assert!(!text.contains("\"look\""), "{text}");
}

#[tokio::test]
async fn templates_carry_only_brand_centre_references() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let b = branded(&server, &root).await;

    // A theme only the site offers is kept as custom colours; a font only the site holds is
    // dropped, and the roles it set fall back to the system fonts.
    let (status, body) = call(
        &server,
        Some(&root),
        Method::POST,
        &format!("/api/sites/{}/themes", b.site),
        Some(json!({ "name": "Ours", "primary": "#1f5f8b", "secondary": "#163447" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let theme = body["id"].as_str().unwrap().to_owned();
    let own_font = woff2(33);
    let own_doc = upload(&server, &root, &b.documents, "Own.woff2", &own_font).await;
    let (status, body) = call(
        &server,
        Some(&root),
        Method::POST,
        &format!("/api/sites/{}/font-packages", b.site),
        Some(
            json!({ "name": "Ours", "content": { "doc": own_doc, "licence_confirmed": true },
                     "roles": roles("system", "content", "content", "system") }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let own_package = body["id"].as_str().unwrap().to_owned();
    set_look(
        &server,
        &root,
        &b.site,
        json!({
            "theme": { "from": format!("site:{theme}") },
            "logo": { "brand_image": b.logo_id },
            "thumbnail": { "keep": sha(&b.thumb) },
            "logo_alt": "Marketing",
            "fonts": { "from": format!("site:{own_package}") },
        }),
    )
    .await;
    let saved = save(&server, &root, &b.site, "Own things").await;
    assert_eq!(
        saved["dropped"],
        json!([
            { "slot": "thumbnail", "name": "Ours.png" },
            { "slot": "fonts.content", "name": "Own.woff2" },
        ]),
        "{saved}"
    );
    let id = saved["id"].as_str().unwrap().to_owned();
    let look = template_in_git(&server, &id).body.look.unwrap();
    assert_eq!(look.from, "custom");
    let fonts = look.fonts.unwrap();
    assert_eq!(fonts.package.content, None);
    assert_eq!(fonts.package.roles.heading, FontRole::System);
    assert_eq!(fonts.package.roles.body, FontRole::System);

    // The package holds no file and no content hash: only brand-centre identifiers.
    let text = String::from_utf8(package(&server, &root, &id).await).unwrap();
    for hash in [sha(&b.logo), sha(&b.thumb), sha(&b.font), sha(&own_font)] {
        assert!(!text.contains(&hash), "{text}");
    }
    assert!(!text.contains("sha256"), "{text}");
    assert!(!text.contains("from_doc"), "{text}");
    assert!(text.contains(&b.logo_id), "{text}");

    // A package whose look names a file, or anything but a brand-centre reference, is refused.
    let mut crafted: Value = serde_json::from_str(&text).unwrap();
    crafted["body"]["look"]["logo"] = json!({ "doc": Uuid::new_v4() });
    let (status, body) = import(&server, &root, serde_json::to_vec(&crafted).unwrap()).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"], "invalid_template", "{body}");
    let mut crafted: Value = serde_json::from_str(&text).unwrap();
    crafted["body"]["look"]["logo"] = json!({
        "sha256": sha(&b.logo), "media_type": "image/png", "bytes": 1, "name": "x",
        "from_doc": Uuid::new_v4(),
    });
    let (status, body) = import(&server, &root, serde_json::to_vec(&crafted).unwrap()).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // A site made from the template gets no site-only content of the first site: its theme
    // keeps the colours as custom ones, and its font package is its own copy.
    let (status, made) = new_site(&server, &root, json!({ "name": "Copy", "template": id })).await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    let copy = made["id"].as_str().unwrap().to_owned();
    let made_record = record(&server, &copy);
    assert!(made_record.themes.is_empty());
    let made_look = made_record.look.unwrap();
    assert_eq!(made_look.from, "custom");
    assert_eq!(made_look.thumbnail, None);
    assert_eq!(made_look.logo.unwrap().sha256, sha(&b.logo));
    let made_fonts = made_look.fonts.unwrap();
    assert_eq!(made_record.font_packages.len(), 1);
    let own = &made_record.font_packages[0];
    assert_ne!(own.id.to_string(), own_package);
    assert_eq!(made_fonts.from, format!("site:{}", own.id));
    assert_eq!(&made_fonts.package, own);
    assert_eq!(
        pins(&server, &copy).await,
        vec![(sha(&b.logo), "image/png".into())]
    );
}

#[tokio::test]
async fn package_import_reports_unknown_references() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let b = branded(&server, &root).await;
    let saved = save(&server, &root, &b.site, "Marketing look").await;
    let id = saved["id"].as_str().unwrap();
    let text = String::from_utf8(package(&server, &root, id).await).unwrap();

    // The same package imported here keeps every reference.
    let (status, body) = import(&server, &root, text.clone().into_bytes()).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["unresolved"], json!([]), "{body}");
    let kept = template_in_git(&server, body["id"].as_str().unwrap());
    assert_eq!(kept.body.look, template_in_git(&server, id).body.look);

    // From another organisation: identifiers this one does not have are left out and listed,
    // and the look stays one a site can hold.
    let unknown_logo = Uuid::new_v4();
    let unknown_font = Uuid::new_v4();
    let mut foreign: Value = serde_json::from_str(&text).unwrap();
    foreign["body"]["look"]["logo"] = json!({ "brand_image": unknown_logo });
    foreign["body"]["look"]["fonts"]["package"]["display"] = json!({ "brand_font": unknown_font });
    // An icon reference naming a logo-kind image is not a usable icon here either.
    foreign["body"]["look"]["icon"] = json!({ "brand_image": b.logo_id });
    let (status, body) = import(&server, &root, serde_json::to_vec(&foreign).unwrap()).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        body["unresolved"],
        json!([
            { "slot": "logo", "id": unknown_logo },
            { "slot": "icon", "id": Uuid::parse_str(&b.logo_id).unwrap() },
            { "slot": "fonts.display", "id": unknown_font },
        ]),
        "{body}"
    );
    let imported = template_in_git(&server, body["id"].as_str().unwrap());
    let look = imported.body.look.unwrap();
    assert_eq!(look.logo, None);
    assert_eq!(look.icon, None);
    // Hiding the title needs a logo, so it is shown again.
    assert!(!look.header.hide_title);
    let fonts = look.fonts.unwrap();
    assert_eq!(fonts.package.display, None);
    assert_eq!(fonts.package.roles.title, FontRole::System);
    let imported_events = events(&server, "template.imported").await;
    assert_eq!(
        imported_events.last().unwrap()["unresolved"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    // And a site made from it has a look without them.
    let (status, made) = new_site(
        &server,
        &root,
        json!({ "name": "Foreign", "template": imported.id }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    assert_eq!(made["unresolved"], json!([]), "{made}");
    let made_look = record(&server, made["id"].as_str().unwrap()).look.unwrap();
    assert!(made_look.logo.is_none());
    assert!(made_look.fonts.unwrap().package.display.is_none());
}

#[tokio::test]
async fn template_sites_pin_their_fonts() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let b = branded(&server, &root).await;
    let saved = save(&server, &root, &b.site, "Marketing look").await;
    let template = saved["id"].as_str().unwrap().to_owned();
    // A template pins nothing.
    assert!(pins(&server, &template).await.is_empty());

    let before = tip(&server);
    let (status, made) = new_site(
        &server,
        &root,
        json!({ "name": "Launch", "template": template }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    assert_eq!(made["unresolved"], json!([]), "{made}");
    let site = made["id"].as_str().unwrap().to_owned();
    // One commit makes the site with its look.
    assert_eq!(commits_since(&server, before).len(), 1);
    let look = record(&server, &site).look.unwrap();
    assert_eq!(look.logo.as_ref().unwrap().sha256, sha(&b.logo));
    assert_eq!(look.icon.as_ref().unwrap().sha256, sha(&b.icon));
    assert_eq!(look.menu, revisions::brand::MenuStyle::Mega);
    assert_eq!(look.page, revisions::brand::PageStyle::Paper);
    assert_eq!(
        look.fonts
            .as_ref()
            .unwrap()
            .package
            .display
            .as_ref()
            .unwrap()
            .sha256,
        sha(&b.font)
    );
    let mut wanted = vec![
        (sha(&b.logo), "image/png".to_owned()),
        (sha(&b.icon), "image/png".to_owned()),
        (sha(&b.font), "font/woff2".to_owned()),
    ];
    wanted.sort();
    assert_eq!(pins(&server, &site).await, wanted);
    let shown = effective(&server, &root, &site).await;
    assert_eq!(
        shown["fonts"]["title"]["url"],
        format!("/api/brand/fonts/{}", sha(&b.font))
    );
    assert_eq!(shown["navigation_menu"], "mega");
    assert_eq!(shown["page"], "paper");

    // The font and the logo leave the brand centre: the site keeps its snapshots pinned, and
    // a new site from the template drops and reports them.
    for path in [
        format!("/api/brand/font-packages/{}", b.package_id),
        format!("/api/brand/fonts/{}", b.font_id),
        format!("/api/brand/images/{}", b.logo_id),
    ] {
        let (status, body) = call(&server, Some(&root), Method::DELETE, &path, None).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{path}: {body}");
    }
    assert_eq!(pins(&server, &site).await, wanted);
    assert!(server.blobs().contains(&Sha256::digest(&b.font).into()));
    let (status, made) = new_site(
        &server,
        &root,
        json!({ "name": "Later", "template": template }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    assert_eq!(
        made["unresolved"],
        json!([
            { "slot": "logo", "id": Uuid::parse_str(&b.logo_id).unwrap() },
            { "slot": "fonts.display", "id": Uuid::parse_str(&b.font_id).unwrap() },
        ]),
        "{made}"
    );
    let later = made["id"].as_str().unwrap().to_owned();
    let look = record(&server, &later).look.unwrap();
    assert!(look.logo.is_none());
    assert!(!look.header.hide_title);
    assert_eq!(
        pins(&server, &later).await,
        vec![(sha(&b.icon), "image/png".to_owned())]
    );
    let _ = (&b.thumb, &b.icon_id);
}

#[tokio::test]
async fn template_create_respects_policy_for_owners() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let owner = server.user("Owner").await;
    let manager = server.user("Manager").await;
    let b = branded(&server, &root).await;
    // The template carries custom colours and a site-only font package.
    let (status, body) = call(
        &server,
        Some(&root),
        Method::POST,
        &format!("/api/sites/{}/font-packages", b.site),
        Some(
            json!({ "name": "Ours", "display": { "brand_font": b.font_id },
                     "roles": roles("display", "system", "system", "system") }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let own_package = body["id"].as_str().unwrap().to_owned();
    set_look(
        &server,
        &root,
        &b.site,
        json!({
            "theme": { "name": "Ours", "primary": "#1f5f8b", "secondary": "#163447" },
            "logo": { "brand_image": b.logo_id },
            "logo_alt": "Marketing",
            "fonts": { "from": format!("site:{own_package}") },
        }),
    )
    .await;
    let saved = save(&server, &root, &b.site, "Ours").await;
    let template = saved["id"].as_str().unwrap().to_owned();
    grant(&server, &root, &b.site, &owner, "owner").await;
    managers(&server, &root, &[&manager]).await;
    lock_down(&server, &root).await;

    // Site owners and brand managers do not create sites, from a template or otherwise; nothing
    // is written.
    let before = tip(&server);
    for user in [&owner, &manager] {
        let (status, body) = new_site(
            &server,
            user,
            json!({ "name": "Theirs", "template": template }),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    }
    assert_eq!(tip(&server), before);

    // An administrator is not limited: the look is written as the template has it.
    let (status, made) = new_site(
        &server,
        &root,
        json!({ "name": "Admin made", "template": template }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    let made_record = record(&server, made["id"].as_str().unwrap());
    let look = made_record.look.unwrap();
    assert_eq!(look.from, "custom");
    assert_eq!(look.theme.primary.to_string(), "#1f5f8b");
    assert!(look.fonts.unwrap().from.starts_with("site:"));
    assert_eq!(made_record.font_packages.len(), 1);

    // The site owner may keep working with that look under the limits only as the policy
    // allows: saving it unchanged is refused for them, as any look with custom colours is.
    grant(
        &server,
        &root,
        made["id"].as_str().unwrap(),
        &owner,
        "owner",
    )
    .await;
    let (status, body) = call(
        &server,
        Some(&owner),
        Method::PUT,
        &format!("/api/sites/{}/look", made["id"].as_str().unwrap()),
        Some(json!({ "look": {
            "theme": { "name": "Ours", "primary": "#1f5f8b", "secondary": "#163447" },
            "fonts": { "keep": true },
        } })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "not_allowed", "{body}");
}

#[tokio::test]
async fn bulk_apply_is_for_admins() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let owner = server.user("Owner").await;
    let manager = server.user("Manager").await;
    let first = plain_site(&server, &root, "First").await;
    let second = plain_site(&server, &root, "Second").await;
    grant(&server, &root, &first, &owner, "owner").await;
    grant(&server, &root, &second, &owner, "owner").await;
    managers(&server, &root, &[&manager]).await;
    let body = json!({
        "sites": [first, second],
        "source": { "look": { "theme": { "name": "Ours", "primary": "#1f5f8b",
                                          "secondary": "#163447" } } },
    });

    let before = tip(&server);
    let (status, _) = apply(&server, None, body.clone()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    for user in [&owner, &manager] {
        let (status, answer) = apply(&server, Some(user), body.clone()).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{answer}");
        assert_eq!(answer["error"], "forbidden", "{answer}");
    }
    assert_eq!(tip(&server), before);

    // Administrators are not limited by the brand policy.
    lock_down(&server, &root).await;
    let (status, answer) = apply(&server, Some(&root), body).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(
        answer["results"],
        json!([{ "site": first, "ok": true }, { "site": second, "ok": true }]),
        "{answer}"
    );
    for site in [&first, &second] {
        let look = record(&server, site).look.unwrap();
        assert_eq!(look.from, "custom");
        assert_eq!(look.theme.primary.to_string(), "#1f5f8b");
    }
}

#[tokio::test]
async fn bulk_apply_reports_per_site() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let b = branded(&server, &root).await;
    let live = plain_site(&server, &root, "Live").await;
    let archived = plain_site(&server, &root, "Old").await;
    let (status, body) = call(
        &server,
        Some(&root),
        Method::POST,
        &format!("/api/sites/{archived}/archive"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let missing = Uuid::new_v4().to_string();
    let archived_before = record(&server, &archived);

    // From another site's look.
    let (status, answer) = apply(
        &server,
        Some(&root),
        json!({ "sites": [live, archived, missing], "source": { "from_site": b.site } }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(
        answer["results"],
        json!([
            { "site": live, "ok": true },
            { "site": archived, "ok": false, "error": "archived" },
            { "site": missing, "ok": false, "error": "not_found" },
        ]),
        "{answer}"
    );
    assert_eq!(record(&server, &live).look, record(&server, &b.site).look);
    assert_eq!(record(&server, &archived), archived_before);

    // From a template: its references are resolved and pinned for each site.
    let saved = save(&server, &root, &b.site, "Marketing look").await;
    let template = saved["id"].as_str().unwrap().to_owned();
    let other = plain_site(&server, &root, "Other").await;
    let (status, answer) = apply(
        &server,
        Some(&root),
        json!({ "sites": [other], "source": { "template": template } }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(
        answer["results"],
        json!([{ "site": other, "ok": true }]),
        "{answer}"
    );
    let look = record(&server, &other).look.unwrap();
    assert_eq!(look.logo.unwrap().sha256, sha(&b.logo));
    assert!(look.thumbnail.is_none());
    assert!(
        pins(&server, &other)
            .await
            .contains(&(sha(&b.font), "font/woff2".to_owned()))
    );
    // A built-in template carries no look to apply.
    let (status, answer) = apply(
        &server,
        Some(&root),
        json!({ "sites": [other], "source": { "template": "team" } }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");

    // A look that a site's own rules refuse fails for that site only.
    let (status, answer) = apply(
        &server,
        Some(&root),
        json!({ "sites": [live, other],
                "source": { "look": { "theme": { "from": "builtin:nothing" } } } }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    for result in answer["results"].as_array().unwrap() {
        assert_eq!(result["ok"], false, "{answer}");
        assert_eq!(result["error"], "invalid_theme", "{answer}");
    }

    // At most 200 sites, and at least one.
    let many: Vec<String> = (0..201).map(|_| Uuid::new_v4().to_string()).collect();
    let before = tip(&server);
    for sites in [json!(many), json!([])] {
        let (status, answer) = apply(
            &server,
            Some(&root),
            json!({ "sites": sites, "source": { "from_site": b.site } }),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
        assert_eq!(answer["error"], "invalid_request", "{answer}");
    }
    assert_eq!(tip(&server), before);
    let two_hundred: Vec<String> = (0..200).map(|_| Uuid::new_v4().to_string()).collect();
    let (status, answer) = apply(
        &server,
        Some(&root),
        json!({ "sites": two_hundred, "source": { "from_site": b.site } }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["results"].as_array().unwrap().len(), 200);
}

#[tokio::test]
async fn bulk_theme_and_fonts_keep_each_sites_identity() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let b = branded(&server, &root).await;
    let second = plain_site(&server, &root, "Second").await;
    set_look(
        &server,
        &root,
        &second,
        json!({
            "theme": { "from": "builtin:sharecraft" },
            "logo_alt": "Second",
            "header": { "layout": "standard", "background": "soft" },
            "navigation": "vertical",
            "footer": { "enabled": true, "name": "Second Co" },
        }),
    )
    .await;
    let before_events = events(&server, "site.look_changed").await.len();
    let looks_before = [
        record(&server, &b.site).look.unwrap(),
        record(&server, &second).look.unwrap(),
    ];

    let (status, answer) = apply(
        &server,
        Some(&root),
        json!({ "sites": [b.site, second],
                "source": { "brand": { "theme": "builtin:forest",
                                       "fonts": "builtin:classic" } } }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(
        answer["results"],
        json!([{ "site": b.site, "ok": true }, { "site": second, "ok": true }]),
        "{answer}"
    );
    for (site, before) in [&b.site, &second].into_iter().zip(&looks_before) {
        let after = record(&server, site).look.unwrap();
        assert_eq!(after.from, "builtin:forest");
        assert_eq!(after.fonts.as_ref().unwrap().from, "builtin:classic");
        // Everything but the theme and fonts is the site's own, as it was.
        let mut expected = before.clone();
        expected.theme = after.theme.clone();
        expected.from = after.from.clone();
        expected.fonts = after.fonts.clone();
        assert_eq!(after, expected);
    }
    assert_eq!(
        record(&server, &b.site).look.unwrap().logo.unwrap().sha256,
        sha(&b.logo)
    );
    assert_eq!(
        record(&server, &b.site)
            .look
            .unwrap()
            .thumbnail
            .unwrap()
            .sha256,
        sha(&b.thumb)
    );

    // Each history entry changes only the theme and the fonts.
    let changed = events(&server, "site.look_changed").await;
    assert_eq!(changed.len(), before_events + 2);
    for detail in &changed[before_events..] {
        assert_eq!(detail["applied_by_admin"], true, "{detail}");
        let mut before = detail["before"].clone();
        let mut after = detail["after"].clone();
        for side in [&mut before, &mut after] {
            let map = side.as_object_mut().unwrap();
            for key in ["theme", "from", "fonts"] {
                map.remove(key);
            }
        }
        assert_eq!(before, after, "{detail}");
        assert_ne!(
            detail["before"]["from"], detail["after"]["from"],
            "{detail}"
        );
    }

    // Without a package the sites take the system fonts; an unknown source is refused.
    let (status, answer) = apply(
        &server,
        Some(&root),
        json!({ "sites": [second], "source": { "brand": { "theme": "builtin:plum" } } }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let look = record(&server, &second).look.unwrap();
    assert!(look.fonts.is_none());
    assert_eq!(look.footer.name, "Second Co");
    let (status, answer) = apply(
        &server,
        Some(&root),
        json!({ "sites": [second],
                "source": { "brand": { "theme": "builtin:plum", "fonts": "builtin:nothing" } } }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["results"][0]["ok"], false, "{answer}");
}

#[tokio::test]
async fn bulk_apply_records_history_per_site() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let first = plain_site(&server, &root, "First").await;
    let second = plain_site(&server, &root, "Second").await;
    let before_events = events(&server, "site.look_changed").await.len();
    let before = tip(&server);
    let (status, answer) = apply(
        &server,
        Some(&root),
        json!({ "sites": [first, second],
                "source": { "look": { "theme": { "from": "builtin:forest" },
                                      "navigation": "vertical" } } }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    // One commit per site.
    let commits = commits_since(&server, before);
    assert_eq!(commits.len(), 2, "{commits:?}");
    assert!(commits.iter().any(|m| m.contains("Second")), "{commits:?}");
    assert!(commits.iter().any(|m| m.contains("First")), "{commits:?}");
    // One history entry per site, marked as applied by an administrator.
    let changed = events(&server, "site.look_changed").await;
    assert_eq!(changed.len(), before_events + 2);
    for detail in &changed[before_events..] {
        assert_eq!(detail["applied_by_admin"], true, "{detail}");
        assert_eq!(detail["after"]["from"], "builtin:forest", "{detail}");
        assert!(detail["before"].is_null(), "{detail}");
    }
    for site in [&first, &second] {
        let (status, history) = call(
            &server,
            Some(&root),
            Method::GET,
            &format!("/api/sites/{site}/look/history"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{history}");
        let history = history.as_array().unwrap();
        assert_eq!(history.len(), 1, "{history:?}");
        assert_eq!(history[0]["look"]["navigation"], "vertical");
    }
    // The same look again changes nothing and records nothing.
    let before = tip(&server);
    let (status, answer) = apply(
        &server,
        Some(&root),
        json!({ "sites": [first],
                "source": { "look": { "theme": { "from": "builtin:forest" },
                                      "navigation": "vertical" } } }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["results"], json!([{ "site": first, "ok": true }]));
    assert_eq!(tip(&server), before);
}

#[tokio::test]
async fn built_in_templates_unchanged() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    for name in ["team", "communication"] {
        let text = String::from_utf8(package(&server, &root, name).await).unwrap();
        assert!(!text.contains("\"look\""), "{text}");
        let (status, made) = new_site(
            &server,
            &root,
            json!({ "name": format!("From {name}"), "template": name }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{made}");
        assert_eq!(made["unresolved"], json!([]), "{made}");
        let site = made["id"].as_str().unwrap();
        let site_record = record(&server, site);
        assert!(site_record.look.is_none());
        assert!(site_record.font_packages.is_empty());
        assert!(pins(&server, site).await.is_empty());
    }
}
