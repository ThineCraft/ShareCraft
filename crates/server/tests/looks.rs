//! Site looks over HTTP: who may read and change a site's look and its own themes; a site with
//! no look reads the ShareCraft defaults and keeps its record's exact bytes; themes are copied
//! into the look and a changed source is marked; site themes stay in their site; every path
//! that sets colours refuses unreadable ones; the header and footer rules; image slots chosen
//! from a document, the brand centre or kept; the hub rule worked out whenever the look is read,
//! through every hub change; history and rollback; archived and purged sites; brand images
//! pinned until nothing uses them; the records model's failure paths; and site lists carrying
//! each site's thumbnail and accent.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::io::Cursor;

use collab_core::ids::NodeId;
use common::{Site, StoppableServer, TestServer, TestUser, spawn_server};
use image::{DynamicImage, ImageFormat, Rgb, RgbImage};
use records::test_support::TestDb;
use reqwest::{Method, StatusCode};
use revisions::brand::{BUILT_IN_THEMES, Colour, Look, check_theme};
use revisions::{Author, RevisionStore, SiteTheme, SitesChange};
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

fn node(id: &str) -> NodeId {
    NodeId(Uuid::parse_str(id).unwrap())
}

/// Creates the site `name` as `admin`: its id and its Documents library.
async fn create_site(site: &Site, admin: &TestUser, name: &str) -> (String, String) {
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
    (id, documents)
}

/// A new asset library in `site`, made by `admin`.
async fn asset_library(site: &Site, admin: &TestUser, id: &str) -> String {
    let (status, body) = call(
        site,
        Some(admin),
        Method::POST,
        &format!("/api/sites/{id}/libraries"),
        Some(json!({ "name": "Brand", "kind": "asset" })),
    )
    .await;
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

/// `PUT /api/sites/{id}/look`, which must succeed; the answer.
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

/// `POST /api/sites/{id}/look/rollback` as `user`.
async fn rollback(site: &Site, user: &TestUser, id: &str, body: Value) -> (StatusCode, Value) {
    call(
        site,
        Some(user),
        Method::POST,
        &format!("/api/sites/{id}/look/rollback"),
        Some(body),
    )
    .await
}

/// `POST /api/sites/{id}/themes` as `user`.
async fn new_site_theme(
    site: &Site,
    user: &TestUser,
    id: &str,
    theme: Value,
) -> (StatusCode, Value) {
    call(
        site,
        Some(user),
        Method::POST,
        &format!("/api/sites/{id}/themes"),
        Some(theme),
    )
    .await
}

/// The theme a built-in theme resolves to, as an answer shows it.
fn resolved(id: &str) -> Value {
    let theme = BUILT_IN_THEMES.iter().find(|t| t.id == id).unwrap().theme();
    serde_json::to_value(check_theme(&theme).unwrap()).unwrap()
}

/// A look built on `theme` with the defaults.
fn look_with(theme: Value) -> Value {
    json!({ "theme": theme })
}

fn builtin(id: &str) -> Value {
    look_with(json!({ "from": id }))
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

fn image_url(bytes: &[u8]) -> String {
    format!("/api/brand/images/{}", sha(bytes))
}

/// The status of fetching a brand image as `user`.
async fn fetch(site: &Site, user: &TestUser, url: &str) -> StatusCode {
    site.http(Some(user))
        .get(site.url(url))
        .send()
        .await
        .unwrap()
        .status()
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

/// The look of `id` as Git holds it.
fn look_in_git(server: &TestServer, id: &str) -> Option<Look> {
    server
        .store()
        .read_site_record(node(id).0)
        .unwrap()
        .unwrap()
        .look
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

#[tokio::test]
async fn look_needs_manage_site() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let olive = server.user("Olive").await;
    let ed = server.user("Ed").await;
    let vic = server.user("Vic").await;
    let out = server.user("Out").await;
    let (team, _) = create_site(&server, &root, "Team").await;
    grant(&server, &root, &team, &olive, "owner").await;
    grant(&server, &root, &team, &ed, "editor").await;
    grant(&server, &root, &team, &vic, "viewer").await;

    // Anyone who can see the site reads its effective look; nobody else does.
    for user in [&ed, &vic, &olive] {
        effective(&server, user, &team).await;
    }
    let (status, _) = call(
        &server,
        Some(&out),
        Method::GET,
        &format!("/api/sites/{team}/look"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(
        &server,
        None,
        Method::GET,
        &format!("/api/sites/{team}/look"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Only people who manage the site read its settings and history and change its look.
    let theme = Uuid::new_v4();
    for user in [&ed, &vic] {
        for (method, path, body) in [
            (
                Method::GET,
                format!("/api/sites/{team}/look/settings"),
                None,
            ),
            (Method::GET, format!("/api/sites/{team}/look/history"), None),
            (
                Method::PUT,
                format!("/api/sites/{team}/look"),
                Some(json!({ "look": builtin("builtin:plum") })),
            ),
            (
                Method::POST,
                format!("/api/sites/{team}/look/rollback"),
                Some(json!({ "event_id": 1 })),
            ),
            (
                Method::POST,
                format!("/api/sites/{team}/themes"),
                Some(json!({ "name": "Ours", "primary": "#1f5f8b", "secondary": "#163447" })),
            ),
            (
                Method::PATCH,
                format!("/api/sites/{team}/themes/{theme}"),
                Some(json!({ "name": "Mine" })),
            ),
            (
                Method::DELETE,
                format!("/api/sites/{team}/themes/{theme}"),
                None,
            ),
        ] {
            let (status, answer) = call(&server, Some(user), method.clone(), &path, body).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}: {answer}");
        }
    }
    let (status, _) = put_look(&server, &out, &team, builtin("builtin:plum")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(look_in_git(&server, &team), None);

    // An owner may.
    let answer = set_look(&server, &olive, &team, builtin("builtin:plum")).await;
    assert_eq!(answer["look"]["from"], "builtin:plum");
    assert_eq!(
        effective(&server, &vic, &team).await["theme"],
        resolved("builtin:plum")
    );
}

#[tokio::test]
async fn look_round_trip_and_defaults() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let (team, _) = create_site(&server, &root, "Team").await;
    let bytes = record_bytes(&server, &team);
    let text = String::from_utf8(bytes.clone()).unwrap();
    assert!(
        !text.contains("\"look\"") && !text.contains("\"themes\""),
        "{text}"
    );

    // A site with no look reads the ShareCraft defaults.
    let seen = effective(&server, &root, &team).await;
    assert_eq!(seen["theme"], resolved("builtin:sharecraft"));
    assert_eq!(seen["theme"]["light"]["accent"], "#3156c9");
    assert_eq!(seen["theme"]["dark"]["accent"], "#748ddd");
    assert_eq!(seen["theme_from"], "site");
    assert_eq!(seen["logo_alt"], "Team");
    assert!(
        seen.get("logo_url").is_none() && seen.get("hub").is_none(),
        "{seen}"
    );
    assert_eq!(
        seen["header"],
        json!({ "layout": "compact", "background": "none", "align": "left", "hide_title": false,
                "bleed": { "enabled": true, "distance": 240, "blur": 48, "transparency": 30,
                           "tint_light": "#ffffff", "tint_dark": "#000000" }, "frame": null })
    );
    assert_eq!(seen["navigation"], "horizontal");
    assert_eq!(seen["footer"]["enabled"], false);
    let shown = settings(&server, &root, &team).await;
    assert_eq!(
        shown["look"],
        serde_json::to_value(Look::default()).unwrap()
    );
    assert_eq!(shown["themes"], json!([]));
    assert_eq!(shown["theme_source_changed"], false);
    assert!(shown.get("in_hub").is_none(), "{shown}");
    // Reading changes nothing, and a change of other settings leaves no look in the record.
    assert_eq!(record_bytes(&server, &team), bytes);
    let (status, _) = call(
        &server,
        Some(&root),
        Method::PATCH,
        &format!("/api/sites/{team}"),
        Some(json!({ "description": "Ours" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8(record_bytes(&server, &team)).unwrap();
    assert!(
        !text.contains("\"look\"") && !text.contains("\"themes\""),
        "{text}"
    );

    // A look round-trips, text trimmed and colours in lower case.
    let wanted = json!({
        "theme": { "name": " Bright ", "primary": "#1F5F8B", "secondary": "#163447" },
        "logo_alt": "  Our mark ",
        "header": { "layout": "standard", "background": "strong", "align": "left",
                    "hide_title": false },
        "navigation": "vertical",
        "footer": {
            "enabled": true, "layout": "extended", "name": " Team Ltd ", "background": "soft",
            "groups": [
                { "heading": " About ", "links": [
                    { "label": " Home ", "url": "/sites" },
                    { "label": "Web", "url": "https://example.invalid/a" }
                ] },
                { "heading": "", "links": [] }
            ]
        }
    });
    let answer = set_look(&server, &root, &team, wanted).await;
    let stored = &answer["look"];
    assert_eq!(stored["from"], "custom");
    assert_eq!(
        stored["theme"],
        json!({ "name": "Bright", "primary": "#1f5f8b", "secondary": "#163447" })
    );
    assert_eq!(stored["logo_alt"], "Our mark");
    assert_eq!(stored["footer"]["name"], "Team Ltd");
    assert_eq!(stored["footer"]["groups"][0]["heading"], "About");
    assert_eq!(stored["footer"]["groups"][0]["links"][0]["label"], "Home");
    assert_eq!(settings(&server, &root, &team).await["look"], *stored);
    let in_git = look_in_git(&server, &team).unwrap();
    assert_eq!(serde_json::to_value(&in_git).unwrap(), *stored);
    let seen = effective(&server, &root, &team).await;
    assert_eq!(seen["logo_alt"], "Our mark");
    assert_eq!(seen["navigation"], "vertical");
    assert_eq!(seen["header"]["layout"], "standard");
    assert_eq!(
        seen["footer"]["groups"][0]["links"][1]["url"],
        "https://example.invalid/a"
    );
    assert_eq!(seen["theme"]["light"]["accent"], "#1f5f8b");
    let changed = events(&server, "site.look_changed").await;
    assert_eq!(changed.len(), 1);
    assert_eq!(changed[0]["before"], Value::Null);
    assert_eq!(changed[0]["after"], *stored);

    // The same look again writes nothing.
    let before = tip(&server);
    let again = json!({
        "theme": { "name": "Bright", "primary": "#1f5f8b", "secondary": "#163447" },
        "logo_alt": "Our mark",
        "header": stored["header"].clone(),
        "navigation": "vertical",
        "footer": stored["footer"].clone(),
    });
    set_look(&server, &root, &team, again).await;
    assert_eq!(tip(&server), before);
    assert_eq!(events(&server, "site.look_changed").await.len(), 1);
}

#[tokio::test]
async fn theme_copied_and_source_change_marked() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let (team, _) = create_site(&server, &root, "Team").await;
    let (status, body) = call(
        &server,
        Some(&root),
        Method::POST,
        "/api/brand/themes",
        Some(json!({ "name": "Ocean", "primary": "#1f5f8b", "secondary": "#163447" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let ocean = body["id"].as_str().unwrap().to_owned();

    let answer = set_look(
        &server,
        &root,
        &team,
        look_with(json!({ "from": format!("org:{ocean}") })),
    )
    .await;
    assert_eq!(answer["look"]["from"], format!("org:{ocean}"));
    assert_eq!(
        answer["look"]["theme"],
        json!({ "name": "Ocean", "primary": "#1f5f8b", "secondary": "#163447" })
    );
    assert_eq!(answer["theme_source_changed"], false);
    let before = effective(&server, &root, &team).await["theme"].clone();

    // The brand centre's theme changes: the site keeps its copy, and the change is marked.
    let (status, body) = call(
        &server,
        Some(&root),
        Method::PATCH,
        &format!("/api/brand/themes/{ocean}"),
        Some(json!({ "primary": "#3156c9" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(effective(&server, &root, &team).await["theme"], before);
    assert_eq!(
        settings(&server, &root, &team).await["theme_source_changed"],
        true
    );
    // Deleted: the site still keeps its copy.
    let (status, _) = call(
        &server,
        Some(&root),
        Method::DELETE,
        &format!("/api/brand/themes/{ocean}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(effective(&server, &root, &team).await["theme"], before);
    assert_eq!(
        settings(&server, &root, &team).await["theme_source_changed"],
        true
    );
    // A theme that no longer exists cannot be chosen again.
    let (status, body) = put_look(
        &server,
        &root,
        &team,
        look_with(json!({ "from": format!("org:{ocean}") })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"], "invalid_theme");

    // Built-in and custom themes never change under the site.
    for theme in [
        json!({ "from": "builtin:forest" }),
        json!({ "name": "Mine", "primary": "#7a3e8f", "secondary": "#3b2244" }),
    ] {
        let answer = set_look(&server, &root, &team, look_with(theme)).await;
        assert_eq!(answer["theme_source_changed"], false);
    }
    // An unknown source is refused.
    for from in ["builtin:nothing", "org:nonsense", "elsewhere"] {
        let (status, body) = put_look(&server, &root, &team, builtin(from)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{from}: {body}");
        assert_eq!(body["error"], "invalid_theme", "{from}");
    }
}

#[tokio::test]
async fn site_themes_stay_in_their_site() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let (team, _) = create_site(&server, &root, "Team").await;
    let (other, _) = create_site(&server, &root, "Other").await;
    let ours = json!({ "name": "Ours", "primary": "#1F5F8B", "secondary": "#163447" });

    let (status, body) = new_site_theme(&server, &root, &team, ours.clone()).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["id"].as_str().unwrap().to_owned();
    assert_eq!(body["from"], format!("site:{id}"));
    assert_eq!(body["primary"], "#1f5f8b");
    assert!(body["resolved"]["dark"]["accent"].is_string());
    let shown = settings(&server, &root, &team).await;
    assert_eq!(shown["themes"].as_array().unwrap().len(), 1);
    assert_eq!(shown["themes"][0]["name"], "Ours");
    // Only its own site offers it: not the other site, not the brand centre.
    assert_eq!(settings(&server, &root, &other).await["themes"], json!([]));
    let (_, centre) = call(&server, Some(&root), Method::GET, "/api/brand", None).await;
    assert_eq!(centre["themes"].as_array().unwrap().len(), 7);
    let from = look_with(json!({ "from": format!("site:{id}") }));
    let (status, body) = put_look(&server, &root, &other, from.clone()).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"], "invalid_theme");
    let answer = set_look(&server, &root, &team, from).await;
    assert_eq!(answer["look"]["theme"]["name"], "Ours");
    assert_eq!(answer["theme_source_changed"], false);

    // Names are unique in the site, ignoring case; another site may use the same name.
    let (status, body) = new_site_theme(
        &server,
        &root,
        &team,
        json!({ "name": "OURS", "primary": "#3156c9", "secondary": "#233239" }),
    )
    .await;
    assert_eq!(
        (status, &body["error"]),
        (StatusCode::CONFLICT, &json!("name_taken"))
    );
    let (status, body) = new_site_theme(&server, &root, &other, ours.clone()).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // Renamed and recoloured: the look keeps its copy, marked changed.
    let (status, body) = call(
        &server,
        Some(&root),
        Method::PATCH,
        &format!("/api/sites/{team}/themes/{id}"),
        Some(json!({ "name": "Renamed", "primary": "#2f6b3a" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        (&body["name"], &body["primary"]),
        (&json!("Renamed"), &json!("#2f6b3a"))
    );
    assert_eq!(
        settings(&server, &root, &team).await["theme_source_changed"],
        true
    );
    assert_eq!(
        effective(&server, &root, &team).await["theme"]["light"]["accent"],
        "#1f5f8b"
    );
    // Not changed through another site.
    let (status, _) = call(
        &server,
        Some(&root),
        Method::PATCH,
        &format!("/api/sites/{other}/themes/{id}"),
        Some(json!({ "name": "Stolen" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(
        &server,
        Some(&root),
        Method::DELETE,
        &format!("/api/sites/{other}/themes/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Deleted: the look keeps its copy.
    let (status, _) = call(
        &server,
        Some(&root),
        Method::DELETE,
        &format!("/api/sites/{team}/themes/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(settings(&server, &root, &team).await["themes"], json!([]));
    assert_eq!(
        effective(&server, &root, &team).await["theme"]["light"]["accent"],
        "#1f5f8b"
    );

    // At most 20.
    for n in 0..20 {
        let (status, body) = new_site_theme(
            &server,
            &root,
            &team,
            json!({ "name": format!("Theme {n}"), "primary": "#3156c9", "secondary": "#233239" }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    let (status, body) = new_site_theme(
        &server,
        &root,
        &team,
        json!({ "name": "One too many", "primary": "#3156c9", "secondary": "#233239" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body, json!({ "error": "too_many", "limit": 20 }));
    let actions: Vec<String> = events(&server, "site.theme_changed")
        .await
        .iter()
        .map(|d| d["action"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(&actions[..4], ["created", "created", "updated", "deleted"]);
}

#[tokio::test]
async fn custom_and_site_themes_refuse_low_contrast() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let (team, _) = create_site(&server, &root, "Team").await;
    let pale = json!({ "name": "Pale", "primary": "#cccccc", "secondary": "#233239" });

    let (status, body) = put_look(&server, &root, &team, look_with(pale.clone())).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"], "low_contrast");
    assert_eq!(body["pairs"][0]["check"], "primary_on_light");
    assert!(body["pairs"][0]["ratio"].as_f64().unwrap() < 4.5);
    assert!(body["pairs"][0]["suggestion"].is_string());
    let (status, body) = put_look(
        &server,
        &root,
        &team,
        look_with(
            json!({ "name": "Dark", "primary": "#3156c9", "secondary": "#233239",
                          "secondary_dark": "#7b7b7b" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["pairs"][0]["check"], "header_on_secondary_dark");
    // Colours are #rrggbb and nothing else.
    let (status, body) = put_look(
        &server,
        &root,
        &team,
        look_with(json!({ "name": "X", "primary": "red;background:url(x)",
                          "secondary": "#233239" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"], "invalid_theme");

    let (status, body) = new_site_theme(&server, &root, &team, pale).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"], "low_contrast");
    let (status, body) = new_site_theme(
        &server,
        &root,
        &team,
        json!({ "name": "Fine", "primary": "#3156c9", "secondary": "#233239" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["id"].as_str().unwrap().to_owned();
    let (status, body) = call(
        &server,
        Some(&root),
        Method::PATCH,
        &format!("/api/sites/{team}/themes/{id}"),
        Some(json!({ "primary": "#dddddd" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"], "low_contrast");

    // Nothing refused reached Git.
    let record = server
        .store()
        .read_site_record(node(&team).0)
        .unwrap()
        .unwrap();
    assert_eq!(record.look, None);
    assert_eq!(record.themes.len(), 1);
    assert_eq!(record.themes[0].primary, Colour::rgb(0x31, 0x56, 0xc9));
}

#[tokio::test]
async fn hide_title_needs_logo() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let (team, documents) = create_site(&server, &root, "Team").await;
    let header = json!({ "layout": "compact", "background": "none", "align": "left",
                         "hide_title": true });

    let (status, body) = put_look(
        &server,
        &root,
        &team,
        json!({ "theme": { "from": "builtin:sharecraft" }, "header": header }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"], "invalid_look");
    assert!(body["reason"].as_str().unwrap().contains("logo"), "{body}");

    let doc = upload(&server, &root, &documents, "Logo.png", &png(20, 10)).await;
    let answer = set_look(
        &server,
        &root,
        &team,
        json!({ "theme": { "from": "builtin:sharecraft" }, "header": header,
                "logo": { "doc": doc } }),
    )
    .await;
    assert_eq!(answer["look"]["header"]["hide_title"], true);
    assert_eq!(
        effective(&server, &root, &team).await["header"]["hide_title"],
        true
    );

    // Other header rules: an image and an alignment need the extended header.
    for header in [
        json!({ "layout": "compact", "background": "none", "align": "centre",
                "hide_title": false }),
        json!({ "layout": "extended", "background": "image", "align": "left",
                "hide_title": false }),
    ] {
        let (status, body) = put_look(
            &server,
            &root,
            &team,
            json!({ "theme": { "from": "builtin:sharecraft" }, "header": header }),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{header}: {body}");
        assert_eq!(body["error"], "invalid_look");
    }
}

#[tokio::test]
async fn image_slots_resolved() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let olive = server.user("Olive").await;
    let ned = server.user("Ned").await;
    let (team, documents) = create_site(&server, &root, "Team").await;
    let (other, other_docs) = create_site(&server, &root, "Other").await;
    let (brand_site, _) = create_site(&server, &root, "Brand").await;
    let assets = asset_library(&server, &root, &brand_site).await;
    grant(&server, &root, &team, &olive, "owner").await;
    grant(&server, &root, &other, &olive, "owner").await;

    // {doc}: an image in the site's own libraries, or in an asset library the owner can view.
    let logo = png(30, 10);
    let own = upload(&server, &root, &documents, "Logo.png", &logo).await;
    let answer = set_look(
        &server,
        &olive,
        &team,
        json!({ "theme": { "from": "builtin:sharecraft" }, "logo": { "doc": own } }),
    )
    .await;
    assert_eq!(answer["look"]["logo"]["sha256"], sha(&logo));
    assert_eq!(answer["look"]["logo"]["width"], 30);
    assert_eq!(answer["look"]["logo"]["from_doc"], own);
    let seen = effective(&server, &olive, &team).await;
    assert_eq!(seen["logo_url"], image_url(&logo));
    assert_eq!(seen["logo_alt"], "Team");
    // Anyone signed in sees it, even without access to the library.
    assert_eq!(
        fetch(&server, &ned, &image_url(&logo)).await,
        StatusCode::OK
    );
    // Not for another site's look.
    let (status, body) = put_look(
        &server,
        &olive,
        &other,
        json!({ "theme": { "from": "builtin:sharecraft" }, "logo": { "doc": own } }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "wrong_place");
    let mark = png(8, 8);
    let in_assets = upload(&server, &root, &assets, "Mark.png", &mark).await;
    let (status, _) = put_look(
        &server,
        &olive,
        &other,
        json!({ "theme": { "from": "builtin:sharecraft" }, "thumbnail": { "doc": in_assets } }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "Olive cannot view the asset library"
    );
    grant(&server, &root, &brand_site, &olive, "viewer").await;
    set_look(
        &server,
        &olive,
        &other,
        json!({ "theme": { "from": "builtin:sharecraft" }, "thumbnail": { "doc": in_assets } }),
    )
    .await;
    assert_eq!(
        effective(&server, &olive, &other).await["thumbnail_url"],
        image_url(&mark)
    );

    // {brand_image}: an image of the brand centre.
    let banner = png(60, 20);
    let banner_doc = upload(&server, &root, &assets, "Banner.png", &banner).await;
    let (status, body) = call(
        &server,
        Some(&root),
        Method::POST,
        "/api/brand/images",
        Some(json!({ "doc": banner_doc, "kind": "background" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let banner_id = body["id"].as_str().unwrap().to_owned();
    let extended = json!({ "layout": "extended", "background": "image", "align": "centre",
                           "hide_title": false, "image": { "brand_image": banner_id } });
    let answer = set_look(
        &server,
        &olive,
        &team,
        json!({ "theme": { "from": "builtin:sharecraft" }, "logo": { "keep": sha(&logo) },
                "header": extended }),
    )
    .await;
    assert_eq!(answer["look"]["logo"]["sha256"], sha(&logo));
    assert_eq!(answer["look"]["header"]["image"]["sha256"], sha(&banner));
    let seen = effective(&server, &olive, &team).await;
    assert_eq!(seen["logo_url"], image_url(&logo));
    assert_eq!(seen["header"]["image_url"], image_url(&banner));
    assert!(seen["header"].get("image").is_none(), "{seen}");

    // {keep}: only an image the look holds now.
    let (status, body) = put_look(
        &server,
        &olive,
        &team,
        json!({ "theme": { "from": "builtin:sharecraft" }, "logo": { "keep": sha(&mark) } }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"], "invalid_look");
    // An unknown brand image, two ways at once, or none at all.
    for slot in [
        json!({ "brand_image": Uuid::new_v4() }),
        json!({ "doc": own, "keep": sha(&logo) }),
        json!({}),
    ] {
        let (status, body) = put_look(
            &server,
            &olive,
            &team,
            json!({ "theme": { "from": "builtin:sharecraft" }, "logo": slot }),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{slot}: {body}");
        assert_eq!(body["error"], "invalid_look", "{slot}");
    }
    // A logo over 1 MiB.
    let mut big = png(8, 8);
    big.resize(1024 * 1024 + 1, 0);
    let big_doc = upload(&server, &root, &documents, "Big.png", &big).await;
    let (status, body) = put_look(
        &server,
        &olive,
        &team,
        json!({ "theme": { "from": "builtin:sharecraft" }, "logo": { "doc": big_doc } }),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    // A document that is not an image.
    let text = upload(&server, &root, &other_docs, "Notes.png", b"just text").await;
    let (status, body) = put_look(
        &server,
        &olive,
        &other,
        json!({ "theme": { "from": "builtin:sharecraft" }, "logo": { "doc": text } }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"], "not_an_image");
}

#[tokio::test]
async fn unsafe_footer_links_refused() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let (team, _) = create_site(&server, &root, "Team").await;
    let footer = |url: &str| {
        json!({
            "theme": { "from": "builtin:sharecraft" },
            "footer": { "enabled": true, "layout": "simple", "name": "", "background": "none",
                        "groups": [{ "heading": "", "links": [{ "label": "Go", "url": url }] }] }
        })
    };
    for url in [
        "javascript:alert(1)",
        "JavaScript:alert(1)",
        "data:text/html,<b>x</b>",
        "//evil.example",
        "/\\evil",
        "ftp://example.invalid",
        "relative",
    ] {
        let (status, body) = put_look(&server, &root, &team, footer(url)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{url}: {body}");
        assert_eq!(body["error"], "invalid_look", "{url}");
    }
    assert_eq!(look_in_git(&server, &team), None);
    // Labels holding HTML are kept as text.
    let marked = json!({
        "theme": { "from": "builtin:sharecraft" },
        "footer": { "enabled": true, "layout": "simple", "name": "<b>Us</b>",
                    "background": "none",
                    "groups": [{ "heading": "", "links": [
                        { "label": "<img src=x onerror=alert(1)>", "url": "https://example.invalid/" }
                    ] }] }
    });
    set_look(&server, &root, &team, marked).await;
    let seen = effective(&server, &root, &team).await;
    assert_eq!(seen["footer"]["name"], "<b>Us</b>");
    assert_eq!(
        seen["footer"]["groups"][0]["links"][0]["label"],
        "<img src=x onerror=alert(1)>"
    );
    // A simple footer holds one group of at most eight links, with no heading.
    let links: Vec<Value> = (0..9)
        .map(|n| json!({ "label": format!("L{n}"), "url": "/x" }))
        .collect();
    for footer in [
        json!({ "enabled": true, "layout": "simple", "name": "", "background": "none",
                "groups": [{ "heading": "", "links": links }] }),
        json!({ "enabled": true, "layout": "simple", "name": "", "background": "none",
                "groups": [{ "heading": "Head", "links": [] }] }),
        json!({ "enabled": true, "layout": "simple", "name": "Bad\u{202e}", "background": "none",
                "groups": [] }),
    ] {
        let (status, body) = put_look(
            &server,
            &root,
            &team,
            json!({ "theme": { "from": "builtin:sharecraft" }, "footer": footer }),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{footer}: {body}");
        assert_eq!(body["error"], "invalid_look");
    }
}

#[tokio::test]
async fn hub_rule_through_every_change() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let (hub, _) = create_site(&server, &root, "Hub").await;
    let (team, documents) = create_site(&server, &root, "Team").await;
    let set_hub = |navigation: Value| {
        let server = &server;
        let root = &root;
        let hub = hub.clone();
        async move {
            let (status, body) = call(
                server,
                Some(root),
                Method::PUT,
                &format!("/api/sites/{hub}/hub"),
                Some(json!({ "navigation": navigation })),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }
    };
    let join = |to: Option<String>| {
        let server = &server;
        let root = &root;
        let team = team.clone();
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
    set_hub(json!([])).await;
    let thumb = png(12, 12);
    let doc = upload(&server, &root, &documents, "Mark.png", &thumb).await;
    set_look(&server, &root, &hub, builtin("builtin:plum")).await;
    set_look(
        &server,
        &root,
        &team,
        json!({ "theme": { "from": "builtin:forest" }, "logo": { "doc": doc },
                "navigation": "vertical" }),
    )
    .await;
    let own = look_in_git(&server, &team).unwrap();
    let own_effective = effective(&server, &root, &team).await;
    assert_eq!(own_effective["theme"], resolved("builtin:forest"));
    assert_eq!(own_effective["theme_from"], "site");

    // Joining: the hub's theme; everything else stays the site's own.
    join(Some(hub.clone())).await;
    let seen = effective(&server, &root, &team).await;
    assert_eq!(seen["theme"], resolved("builtin:plum"));
    assert_eq!(seen["theme_from"], "hub");
    assert_eq!(seen["hub"]["id"], hub);
    assert_eq!(seen["hub"]["name"], "Hub");
    assert_eq!(seen["logo_url"], image_url(&thumb));
    assert_eq!(seen["navigation"], "vertical");
    let shown = settings(&server, &root, &team).await;
    assert_eq!(shown["in_hub"], json!({ "id": hub, "name": "Hub" }));
    assert_eq!(shown["look"]["from"], "builtin:forest");
    assert_eq!(look_in_git(&server, &team).as_ref(), Some(&own));

    // The hub changes its theme: the member follows.
    set_look(&server, &root, &hub, builtin("builtin:rust")).await;
    assert_eq!(
        effective(&server, &root, &team).await["theme"],
        resolved("builtin:rust")
    );
    // The member changes its own theme while in the hub: kept, unused.
    set_look(
        &server,
        &root,
        &team,
        json!({ "theme": { "from": "builtin:harbour" }, "logo": { "keep": sha(&thumb) },
                "navigation": "vertical" }),
    )
    .await;
    let own = look_in_git(&server, &team).unwrap();
    assert_eq!(own.from, "builtin:harbour");
    assert_eq!(
        effective(&server, &root, &team).await["theme"],
        resolved("builtin:rust")
    );

    // Leaving: its own theme comes back.
    join(None).await;
    let seen = effective(&server, &root, &team).await;
    assert_eq!(seen["theme"], resolved("builtin:harbour"));
    assert_eq!(seen["theme_from"], "site");
    assert!(seen.get("hub").is_none(), "{seen}");
    assert_eq!(look_in_git(&server, &team).as_ref(), Some(&own));

    // An archived hub is still the hub.
    join(Some(hub.clone())).await;
    let (status, _) = call(
        &server,
        Some(&root),
        Method::POST,
        &format!("/api/sites/{hub}/archive"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        effective(&server, &root, &team).await["theme"],
        resolved("builtin:rust")
    );
    let (status, _) = call(
        &server,
        Some(&root),
        Method::POST,
        &format!("/api/sites/{hub}/reactivate"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Unregistered: the member is released and its own theme comes back.
    let (status, body) = call(
        &server,
        Some(&root),
        Method::DELETE,
        &format!("/api/sites/{hub}/hub"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        effective(&server, &root, &team).await["theme"],
        resolved("builtin:harbour")
    );
    assert_eq!(look_in_git(&server, &team).as_ref(), Some(&own));

    // A deleted hub no longer lends its theme; a purged one releases its members.
    set_hub(json!([])).await;
    join(Some(hub.clone())).await;
    assert_eq!(
        effective(&server, &root, &team).await["theme"],
        resolved("builtin:rust")
    );
    let (status, body) = call(
        &server,
        Some(&root),
        Method::DELETE,
        &format!("/api/sites/{hub}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let seen = effective(&server, &root, &team).await;
    assert_eq!(seen["theme"], resolved("builtin:harbour"));
    assert_eq!(seen["theme_from"], "site");
    let (status, body) = call(
        &server,
        Some(&root),
        Method::DELETE,
        &format!("/api/admin/sites/{hub}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(
        effective(&server, &root, &team).await["theme"],
        resolved("builtin:harbour")
    );
    assert_eq!(look_in_git(&server, &team).as_ref(), Some(&own));
}

/// Sets the look of `id` to one whose logo description is `alt`.
async fn look_numbered(site: &Site, user: &TestUser, id: &str, alt: &str) {
    set_look(
        site,
        user,
        id,
        json!({ "theme": { "from": "builtin:sharecraft" }, "logo_alt": alt }),
    )
    .await;
}

#[tokio::test]
async fn history_lists_last_fifty() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let (team, _) = create_site(&server, &root, "Team").await;
    let (other, _) = create_site(&server, &root, "Other").await;
    assert!(history(&server, &root, &team).await.is_empty());
    look_numbered(&server, &root, &other, "elsewhere").await;
    for n in 1..=52 {
        look_numbered(&server, &root, &team, &format!("v{n}")).await;
    }
    let listed = history(&server, &root, &team).await;
    assert_eq!(listed.len(), 50);
    assert_eq!(listed[0]["look"]["logo_alt"], "v52");
    assert_eq!(listed[49]["look"]["logo_alt"], "v3");
    assert!(listed[0]["event_id"].as_i64().unwrap() > listed[1]["event_id"].as_i64().unwrap());
    assert!(listed[0]["at"].is_string());
    assert_eq!(listed[0]["by"], json!({ "id": root.id.0, "name": "Root" }));
    assert!(
        listed
            .iter()
            .all(|entry| entry["look"]["logo_alt"] != "elsewhere")
    );
}

#[tokio::test]
async fn rollback_restores_and_records() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let (team, _) = create_site(&server, &root, "Team").await;
    let (other, _) = create_site(&server, &root, "Other").await;
    set_look(&server, &root, &team, builtin("builtin:harbour")).await;
    set_look(&server, &root, &team, builtin("builtin:plum")).await;
    look_numbered(&server, &root, &other, "elsewhere").await;
    let listed = history(&server, &root, &team).await;
    assert_eq!(listed.len(), 2);
    let harbour = listed[1]["event_id"].clone();
    let plum = listed[0]["event_id"].clone();

    // An entry's look applied again as a new change.
    let (status, body) = rollback(&server, &root, &team, json!({ "event_id": harbour })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["look"]["from"], "builtin:harbour");
    assert_eq!(
        effective(&server, &root, &team).await["theme"],
        resolved("builtin:harbour")
    );
    assert_eq!(look_in_git(&server, &team).unwrap().from, "builtin:harbour");
    let listed = history(&server, &root, &team).await;
    assert_eq!(listed.len(), 3);
    assert_eq!(listed[0]["look"]["from"], "builtin:harbour");
    let changed = events(&server, "site.look_changed").await;
    let last = changed.last().unwrap();
    assert_eq!(last["rollback"], harbour);
    assert_eq!(last["before"]["from"], "builtin:plum");

    // An entry's `before`: the look before the first change was none, the defaults.
    let first = listed[2]["event_id"].clone();
    let (status, body) = rollback(
        &server,
        &root,
        &team,
        json!({ "event_id": first, "which": "before" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(look_in_git(&server, &team), None);
    assert_eq!(
        effective(&server, &root, &team).await["theme"],
        resolved("builtin:sharecraft")
    );
    let (status, body) = rollback(
        &server,
        &root,
        &team,
        json!({ "event_id": plum, "which": "after" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(look_in_git(&server, &team).unwrap().from, "builtin:plum");

    // Another site's entry, or none, is not found.
    let elsewhere = history(&server, &root, &other).await[0]["event_id"].clone();
    for id in [elsewhere, json!(i64::MAX)] {
        let (status, body) = rollback(&server, &root, &team, json!({ "event_id": id })).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    }
    let (status, _) = rollback(
        &server,
        &root,
        &team,
        json!({ "event_id": plum, "which": "sideways" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn rollback_rechecks_contrast() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let (team, _) = create_site(&server, &root, "Team").await;
    set_look(&server, &root, &team, builtin("builtin:harbour")).await;
    // An entry recorded under older rules, whose colours no longer pass.
    let mut old = Look {
        from: "custom".into(),
        ..Look::default()
    };
    old.theme.name = "Old".into();
    old.theme.primary = Colour::rgb(0xcc, 0xcc, 0xcc);
    let id: i64 = sqlx::query_scalar(
        "insert into events (kind, actor, node_id, detail) values ('site.look_changed', $1, $2, \
         jsonb_build_object('before', null, 'after', $3::jsonb)) returning id",
    )
    .bind(root.id.0)
    .bind(node(&team).0)
    .bind(serde_json::to_value(&old).unwrap())
    .fetch_one(server.records_db().test_pool())
    .await
    .unwrap();
    let before = tip(&server);

    let (status, body) = rollback(&server, &root, &team, json!({ "event_id": id })).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"], "low_contrast");
    assert_eq!(body["pairs"][0]["check"], "primary_on_light");
    assert_eq!(tip(&server), before);
    assert_eq!(look_in_git(&server, &team).unwrap().from, "builtin:harbour");
}

#[tokio::test]
async fn rollback_with_missing_image_is_image_gone() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let (team, documents) = create_site(&server, &root, "Team").await;
    let first = png(10, 10);
    let second = png(11, 11);
    let first_doc = upload(&server, &root, &documents, "First.png", &first).await;
    let second_doc = upload(&server, &root, &documents, "Second.png", &second).await;
    set_look(
        &server,
        &root,
        &team,
        json!({ "theme": { "from": "builtin:sharecraft" }, "logo": { "doc": first_doc },
                "thumbnail": { "doc": second_doc } }),
    )
    .await;
    // The first logo's file is deleted for good, then the look moves on: nothing names its
    // content any more, and it is erased.
    delete_for_good(&server, &root, &first_doc).await;
    set_look(
        &server,
        &root,
        &team,
        json!({ "theme": { "from": "builtin:sharecraft" }, "logo": { "doc": second_doc } }),
    )
    .await;
    assert!(!server.blobs().contains(&hash(&first)));
    let listed = history(&server, &root, &team).await;
    let before = tip(&server);

    let (status, body) = rollback(
        &server,
        &root,
        &team,
        json!({ "event_id": listed[1]["event_id"] }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body, json!({ "error": "image_gone", "slots": ["logo"] }));
    assert_eq!(tip(&server), before);
    assert_eq!(
        look_in_git(&server, &team).unwrap().logo.unwrap().sha256,
        sha(&second)
    );
}

#[tokio::test]
async fn archived_site_refuses_look_changes_but_shows_look() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let olive = server.user("Olive").await;
    let (team, _) = create_site(&server, &root, "Team").await;
    grant(&server, &root, &team, &olive, "owner").await;
    set_look(&server, &olive, &team, builtin("builtin:plum")).await;
    let (status, body) = new_site_theme(
        &server,
        &olive,
        &team,
        json!({ "name": "Ours", "primary": "#1f5f8b", "secondary": "#163447" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let theme = body["id"].as_str().unwrap().to_owned();
    let event = history(&server, &olive, &team).await[0]["event_id"].clone();
    let (status, _) = call(
        &server,
        Some(&root),
        Method::POST,
        &format!("/api/sites/{team}/archive"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let before = tip(&server);

    for user in [&root, &olive] {
        for (method, path, body) in [
            (
                Method::PUT,
                format!("/api/sites/{team}/look"),
                Some(json!({ "look": builtin("builtin:rust") })),
            ),
            (
                Method::POST,
                format!("/api/sites/{team}/look/rollback"),
                Some(json!({ "event_id": event })),
            ),
            (
                Method::POST,
                format!("/api/sites/{team}/themes"),
                Some(json!({ "name": "More", "primary": "#1f5f8b", "secondary": "#163447" })),
            ),
            (
                Method::PATCH,
                format!("/api/sites/{team}/themes/{theme}"),
                Some(json!({ "name": "Renamed" })),
            ),
            (
                Method::DELETE,
                format!("/api/sites/{team}/themes/{theme}"),
                None,
            ),
        ] {
            let (status, answer) = call(&server, Some(user), method.clone(), &path, body).await;
            assert_eq!(
                (status, answer["error"].as_str()),
                (StatusCode::FORBIDDEN, Some("archived")),
                "{method} {path}: {answer}"
            );
        }
        // Still shown.
        assert_eq!(
            effective(&server, user, &team).await["theme"],
            resolved("builtin:plum")
        );
        assert_eq!(
            settings(&server, user, &team).await["look"]["from"],
            "builtin:plum"
        );
        assert_eq!(history(&server, user, &team).await.len(), 1);
    }
    assert_eq!(tip(&server), before);
}

#[tokio::test]
async fn purged_site_releases_its_images() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let (team, documents) = create_site(&server, &root, "Team").await;
    let logo = png(14, 14);
    let doc = upload(&server, &root, &documents, "Logo.png", &logo).await;
    set_look(
        &server,
        &root,
        &team,
        json!({ "theme": { "from": "builtin:sharecraft" }, "logo": { "doc": doc } }),
    )
    .await;
    let db = server.records_db();
    assert_eq!(
        db.brand_pins(&team).await.unwrap(),
        vec![(sha(&logo), "image/png".to_owned())]
    );
    // Deleted: kept, as the site can be restored.
    let (status, _) = call(
        &server,
        Some(&root),
        Method::DELETE,
        &format!("/api/sites/{team}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(db.brand_pins(&team).await.unwrap().len(), 1);
    assert_eq!(
        fetch(&server, &root, &image_url(&logo)).await,
        StatusCode::OK
    );
    // Purged: the pins go, and the content with them.
    let (status, body) = call(
        &server,
        Some(&root),
        Method::DELETE,
        &format!("/api/admin/sites/{team}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert!(db.brand_pins(&team).await.unwrap().is_empty());
    assert!(!server.blobs().contains(&hash(&logo)));
    assert_eq!(
        fetch(&server, &root, &image_url(&logo)).await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn look_images_pinned_until_unreferenced() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let (team, _) = create_site(&server, &root, "Team").await;
    let (other, _) = create_site(&server, &root, "Other").await;
    let (brand_site, _) = create_site(&server, &root, "Brand").await;
    let assets = asset_library(&server, &root, &brand_site).await;
    let first = png(21, 21);
    let second = png(22, 22);
    let third = png(23, 23);
    let first_doc = upload(&server, &root, &assets, "First.png", &first).await;
    let second_doc = upload(&server, &root, &assets, "Second.png", &second).await;
    let third_doc = upload(&server, &root, &assets, "Third.png", &third).await;
    set_look(
        &server,
        &root,
        &team,
        json!({ "theme": { "from": "builtin:sharecraft" }, "logo": { "doc": first_doc } }),
    )
    .await;
    set_look(
        &server,
        &root,
        &other,
        json!({ "theme": { "from": "builtin:sharecraft" }, "logo": { "doc": third_doc },
                "footer": { "enabled": true, "layout": "simple", "name": "",
                            "background": "none", "groups": [],
                            "logo": { "doc": third_doc } } }),
    )
    .await;

    // The source file is deleted and its bin emptied while the look uses it: kept.
    delete_for_good(&server, &root, &first_doc).await;
    let db = server.records_db();
    assert!(db.blob_refs_of(&hash(&first)).await.unwrap().is_empty());
    assert!(server.blobs().contains(&hash(&first)));
    assert_eq!(
        fetch(&server, &root, &image_url(&first)).await,
        StatusCode::OK
    );
    assert!(
        !db.release_blob_if_unreferenced(&hash(&first))
            .await
            .unwrap()
    );

    // The look switches logos: the first is no longer named by anything and is erased.
    set_look(
        &server,
        &root,
        &team,
        json!({ "theme": { "from": "builtin:sharecraft" }, "logo": { "doc": second_doc } }),
    )
    .await;
    assert!(!server.blobs().contains(&hash(&first)));
    assert_eq!(
        fetch(&server, &root, &image_url(&first)).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        fetch(&server, &root, &image_url(&second)).await,
        StatusCode::OK
    );
    // Its file still names the second: switching away keeps it.
    set_look(&server, &root, &team, builtin("builtin:sharecraft")).await;
    assert!(server.blobs().contains(&hash(&second)));
    assert!(db.brand_pins(&team).await.unwrap().is_empty());

    // A site whose look uses an image is purged after the file went: the image goes with it.
    delete_for_good(&server, &root, &third_doc).await;
    assert_eq!(
        fetch(&server, &root, &image_url(&third)).await,
        StatusCode::OK
    );
    for (method, path) in [
        (Method::DELETE, format!("/api/sites/{other}")),
        (Method::DELETE, format!("/api/admin/sites/{other}")),
    ] {
        let (status, body) = call(&server, Some(&root), method, &path, None).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{path}: {body}");
    }
    assert!(!server.blobs().contains(&hash(&third)));
}

#[tokio::test]
async fn look_change_refused_by_database_is_taken_back() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let (team, documents) = create_site(&server, &root, "Team").await;
    set_look(&server, &root, &team, builtin("builtin:plum")).await;
    let logo = png(7, 7);
    let doc = upload(&server, &root, &documents, "Logo.png", &logo).await;
    // The content's rows are gone, as after an erasure that ran while the image was chosen;
    // the file is still read from the store.
    let pool = server.records_db().test_pool().clone();
    for table in ["blob_refs", "blobs"] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "delete from {table} where sha256 = $1"
        )))
        .bind(sha(&logo))
        .execute(&pool)
        .await
        .unwrap();
    }
    let before = tip(&server);
    let record = look_in_git(&server, &team);

    let (status, body) = put_look(
        &server,
        &root,
        &team,
        json!({ "theme": { "from": "builtin:rust" }, "logo": { "doc": doc } }),
    )
    .await;
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
    assert_eq!(look_in_git(&server, &team), record);
    assert_eq!(
        effective(&server, &root, &team).await["theme"],
        resolved("builtin:plum")
    );
    assert_eq!(events(&server, "site.look_changed").await.len(), 1);
    assert!(
        server
            .records_db()
            .brand_pins(&team)
            .await
            .unwrap()
            .is_empty()
    );

    // Nothing is out of step: the next change goes through.
    set_look(&server, &root, &team, builtin("builtin:forest")).await;
    assert_eq!(look_in_git(&server, &team).unwrap().from, "builtin:forest");
}

#[tokio::test]
async fn reconcile_rebuilds_looks_from_git() {
    let data_dir = tempfile::tempdir().unwrap();
    let db = TestDb::create().await;
    let mut first = StoppableServer::start(data_dir.path(), &db, |_| {});
    let root = first.admin("Root").await;
    let (team, documents) = create_site(&first, &root, "Team").await;
    let (other, _) = create_site(&first, &root, "Other").await;
    let logo = png(9, 9);
    let doc = upload(&first, &root, &documents, "Logo.png", &logo).await;
    set_look(
        &first,
        &root,
        &team,
        json!({ "theme": { "from": "builtin:plum" }, "logo": { "doc": doc } }),
    )
    .await;
    first.stop();

    // The mirror is lost.
    let pool = db.db.test_pool();
    sqlx::query("update sites set look = null, themes = '[]'")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("delete from brand_refs")
        .execute(pool)
        .await
        .unwrap();
    // A change written to Git while the server was stopped.
    let store = RevisionStore::open(&server::library_path(data_dir.path())).unwrap();
    let mut record = store.read_site_record(node(&other).0).unwrap().unwrap();
    record.themes.push(SiteTheme {
        id: Uuid::new_v4(),
        name: "Written elsewhere".into(),
        primary: Colour::rgb(0x1f, 0x5f, 0x8b),
        secondary: Colour::rgb(0x16, 0x34, 0x47),
        primary_dark: None,
        secondary_dark: None,
        changed_at: 1_800_000_000,
    });
    let author = Author {
        id: "elsewhere".into(),
        name: "Elsewhere".into(),
    };
    let records = [record];
    let mut change = SitesChange::new(&author, "Change site Other");
    change.sites = &records;
    store.write_sites(&change).unwrap();

    let second = StoppableServer::start(data_dir.path(), &db, |_| {});
    let rita = second.admin("Rita").await;
    let seen = effective(&second, &rita, &team).await;
    assert_eq!(seen["theme"], resolved("builtin:plum"));
    assert_eq!(seen["logo_url"], image_url(&logo));
    assert_eq!(
        fetch(&second, &rita, &image_url(&logo)).await,
        StatusCode::OK
    );
    assert_eq!(
        second.records_db().brand_pins(&team).await.unwrap(),
        vec![(sha(&logo), "image/png".to_owned())]
    );
    let shown = settings(&second, &rita, &other).await;
    assert_eq!(shown["themes"][0]["name"], "Written elsewhere");
    let repaired = events(&second, "site.changed").await;
    assert!(
        repaired
            .iter()
            .filter(|detail| detail["repaired"] == true)
            .count()
            >= 2,
        "{repaired:?}"
    );
}

#[tokio::test]
async fn branding_never_changes_access() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let olive = server.user("Olive").await;
    let vic = server.user("Vic").await;
    let out = server.user("Out").await;
    let (hub, _) = create_site(&server, &root, "Hub").await;
    let (team, documents) = create_site(&server, &root, "Team").await;
    grant(&server, &root, &team, &olive, "owner").await;
    grant(&server, &root, &team, &vic, "viewer").await;
    let db = server.records_db();
    let access = |user: &TestUser, at: &str| {
        let db = db.clone();
        let id = user.id;
        let at = node(at);
        async move { db.effective_access(id, at).await.unwrap() }
    };
    let mut before = Vec::new();
    for user in [&root, &olive, &vic, &out] {
        for at in [&team, &documents, &hub] {
            before.push(access(user, at).await);
        }
    }

    let logo = png(5, 5);
    let doc = upload(&server, &root, &documents, "Logo.png", &logo).await;
    set_look(
        &server,
        &olive,
        &team,
        json!({ "theme": { "from": "builtin:plum" }, "logo": { "doc": doc } }),
    )
    .await;
    let (status, body) = new_site_theme(
        &server,
        &olive,
        &team,
        json!({ "name": "Ours", "primary": "#1f5f8b", "secondary": "#163447" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, _) = call(
        &server,
        Some(&root),
        Method::PUT,
        &format!("/api/sites/{hub}/hub"),
        Some(json!({ "navigation": [] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    set_look(&server, &root, &hub, builtin("builtin:rust")).await;
    let (status, _) = call(
        &server,
        Some(&root),
        Method::PUT,
        &format!("/api/sites/{team}/joined-hub"),
        Some(json!({ "hub": hub })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let mut after = Vec::new();
    for user in [&root, &olive, &vic, &out] {
        for at in [&team, &documents, &hub] {
            after.push(access(user, at).await);
        }
    }
    assert_eq!(before, after);
    // The logo is shown to anyone signed in, but its file stays as private as it was.
    assert_eq!(
        fetch(&server, &out, &image_url(&logo)).await,
        StatusCode::OK
    );
    let (status, _) = call(
        &server,
        Some(&out),
        Method::GET,
        &format!("/api/docs/{doc}/content"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(
        &server,
        Some(&out),
        Method::GET,
        &format!("/api/sites/{team}/look"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn site_lists_carry_thumbnail_and_accent() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let (hub, _) = create_site(&server, &root, "Hub").await;
    let (team, documents) = create_site(&server, &root, "Team").await;
    let (plain, _) = create_site(&server, &root, "Plain").await;
    let thumb = png(16, 16);
    let doc = upload(&server, &root, &documents, "Thumb.png", &thumb).await;
    set_look(
        &server,
        &root,
        &team,
        json!({ "theme": { "from": "builtin:harbour" }, "thumbnail": { "doc": doc } }),
    )
    .await;
    let row = |listed: &Value, id: &str| {
        listed
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == id)
            .cloned()
            .unwrap()
    };
    let (status, listed) = call(&server, Some(&root), Method::GET, "/api/sites", None).await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let seen = row(&listed, &team);
    assert_eq!(seen["thumbnail_url"], image_url(&thumb));
    assert_eq!(seen["accent"], "#1f5f8b");
    let seen = row(&listed, &plain);
    assert!(seen.get("thumbnail_url").is_none(), "{seen}");
    assert_eq!(seen["accent"], "#3156c9");

    // In a hub, the hub's accent; the thumbnail stays the site's own.
    let (status, _) = call(
        &server,
        Some(&root),
        Method::PUT,
        &format!("/api/sites/{hub}/hub"),
        Some(json!({ "navigation": [] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    set_look(&server, &root, &hub, builtin("builtin:plum")).await;
    let (status, _) = call(
        &server,
        Some(&root),
        Method::PUT,
        &format!("/api/sites/{team}/joined-hub"),
        Some(json!({ "hub": hub })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let plum = resolved("builtin:plum")["light"]["accent"].clone();
    let (_, listed) = call(&server, Some(&root), Method::GET, "/api/sites", None).await;
    let seen = row(&listed, &team);
    assert_eq!(seen["accent"], plum);
    assert_eq!(seen["thumbnail_url"], image_url(&thumb));
    let (status, members) = call(
        &server,
        Some(&root),
        Method::GET,
        &format!("/api/hubs/{hub}/sites"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{members}");
    let seen = row(&members, &team);
    assert_eq!(seen["accent"], plum);
    assert_eq!(seen["thumbnail_url"], image_url(&thumb));
}

/// Whether `site`'s look pins the content `bytes`.
async fn pinned_by(server: &Site, site: &str, bytes: &[u8]) -> bool {
    let found: Option<String> =
        sqlx::query_scalar("select sha256 from brand_refs where owner = $1 and sha256 = $2")
            .bind(site)
            .bind(sha(bytes))
            .fetch_optional(server.records_db().test_pool())
            .await
            .unwrap();
    found.is_some()
}

#[tokio::test]
async fn keep_accepts_history_snapshots() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let (team, documents) = create_site(&server, &root, "Team").await;
    let first = png(12, 12);
    let mark = png(13, 13);
    let second = png(14, 14);
    let first_doc = upload(&server, &root, &documents, "First.png", &first).await;
    let mark_doc = upload(&server, &root, &documents, "Mark.png", &mark).await;
    let second_doc = upload(&server, &root, &documents, "Second.png", &second).await;
    set_look(
        &server,
        &root,
        &team,
        json!({ "theme": { "from": "builtin:sharecraft" }, "logo": { "doc": first_doc },
                "thumbnail": { "doc": mark_doc } }),
    )
    .await;
    // The look moves on: the first logo and the mark are no longer used by it.
    set_look(
        &server,
        &root,
        &team,
        json!({ "theme": { "from": "builtin:sharecraft" }, "logo": { "doc": second_doc } }),
    )
    .await;
    assert!(!pinned_by(&server, &team, &first).await);
    // Their files change since: a snapshot is kept, never taken again from its document.
    let (status, body) = call(
        &server,
        Some(&root),
        Method::DELETE,
        &format!("/api/nodes/{first_doc}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    // A look holding images named only by its history keeps those exact snapshots.
    let answer = set_look(
        &server,
        &root,
        &team,
        json!({ "theme": { "from": "builtin:plum" },
                "logo": { "keep": sha(&first) }, "thumbnail": { "keep": sha(&mark) } }),
    )
    .await;
    assert_eq!(answer["look"]["logo"]["sha256"], sha(&first));
    assert_eq!(answer["look"]["logo"]["name"], "First.png");
    assert_eq!(answer["look"]["thumbnail"]["sha256"], sha(&mark));
    let git = look_in_git(&server, &team).unwrap();
    assert_eq!(git.logo.unwrap().sha256, sha(&first));
    assert_eq!(git.thumbnail.unwrap().sha256, sha(&mark));
    // Pinned again with the look change.
    assert!(pinned_by(&server, &team, &first).await);
    assert!(pinned_by(&server, &team, &mark).await);
    assert_eq!(
        fetch(&server, &root, &image_url(&first)).await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn keep_refuses_unknown_and_gone_snapshots() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let (team, documents) = create_site(&server, &root, "Team").await;
    let (other, other_documents) = create_site(&server, &root, "Other").await;
    let first = png(15, 15);
    let mark = png(16, 16);
    let second = png(17, 17);
    let elsewhere = png(18, 18);
    let first_doc = upload(&server, &root, &documents, "First.png", &first).await;
    let mark_doc = upload(&server, &root, &documents, "Mark.png", &mark).await;
    let second_doc = upload(&server, &root, &documents, "Second.png", &second).await;
    let elsewhere_doc = upload(&server, &root, &other_documents, "Else.png", &elsewhere).await;
    set_look(
        &server,
        &root,
        &other,
        json!({ "theme": { "from": "builtin:sharecraft" }, "logo": { "doc": elsewhere_doc } }),
    )
    .await;
    set_look(
        &server,
        &root,
        &team,
        json!({ "theme": { "from": "builtin:sharecraft" }, "logo": { "doc": first_doc },
                "thumbnail": { "doc": mark_doc } }),
    )
    .await;
    // The first logo's file is deleted for good and the look moves on: its content is erased.
    delete_for_good(&server, &root, &first_doc).await;
    set_look(
        &server,
        &root,
        &team,
        json!({ "theme": { "from": "builtin:sharecraft" }, "logo": { "doc": second_doc } }),
    )
    .await;
    assert!(!server.blobs().contains(&hash(&first)));
    let before = tip(&server);

    // Content no history entry of this site names: not even another site's look.
    for unknown in [sha(&elsewhere), "0".repeat(64)] {
        let (status, body) = put_look(
            &server,
            &root,
            &team,
            json!({ "theme": { "from": "builtin:sharecraft" }, "logo": { "keep": unknown } }),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(body["error"], "invalid_look");
    }
    // A snapshot whose content is gone.
    let (status, body) = put_look(
        &server,
        &root,
        &team,
        json!({ "theme": { "from": "builtin:sharecraft" }, "logo": { "keep": sha(&first) } }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body, json!({ "error": "image_gone", "slots": ["logo"] }));
    // A snapshot a rescan found infected.
    sqlx::query(
        "update blobs set scan = 'infected', scan_detail = 'Test-Signature' where sha256 = $1",
    )
    .bind(sha(&mark))
    .execute(server.records_db().test_pool())
    .await
    .unwrap();
    let (status, body) = put_look(
        &server,
        &root,
        &team,
        json!({ "theme": { "from": "builtin:sharecraft" }, "logo": { "keep": sha(&first) },
                "thumbnail": { "keep": sha(&mark) } }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(
        body,
        json!({ "error": "image_gone", "slots": ["logo", "thumbnail"] })
    );
    assert_eq!(tip(&server), before);
    assert_eq!(
        look_in_git(&server, &team).unwrap().logo.unwrap().sha256,
        sha(&second)
    );
}

#[tokio::test]
async fn rollback_refuses_held_images() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let (team, documents) = create_site(&server, &root, "Team").await;
    let first = png(24, 24);
    let second = png(25, 25);
    let first_doc = upload(&server, &root, &documents, "First.png", &first).await;
    let second_doc = upload(&server, &root, &documents, "Second.png", &second).await;
    set_look(
        &server,
        &root,
        &team,
        json!({ "theme": { "from": "builtin:sharecraft" }, "logo": { "doc": first_doc } }),
    )
    .await;
    set_look(
        &server,
        &root,
        &team,
        json!({ "theme": { "from": "builtin:sharecraft" }, "logo": { "doc": second_doc } }),
    )
    .await;
    // The first logo's content is still stored (its file names it), but a rescan finds it
    // infected: it is held.
    sqlx::query(
        "update blobs set scan = 'infected', scan_detail = 'Test-Signature' where sha256 = $1",
    )
    .bind(sha(&first))
    .execute(server.records_db().test_pool())
    .await
    .unwrap();
    let listed = history(&server, &root, &team).await;
    let before = tip(&server);

    let (status, body) = rollback(
        &server,
        &root,
        &team,
        json!({ "event_id": listed[1]["event_id"] }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body, json!({ "error": "image_gone", "slots": ["logo"] }));
    assert_eq!(tip(&server), before);
    assert_eq!(
        look_in_git(&server, &team).unwrap().logo.unwrap().sha256,
        sha(&second)
    );
    // The held content is not pinned again.
    let pins = server.records_db().brand_pins(&team).await.unwrap();
    assert!(
        pins.iter().all(|(pinned, _)| *pinned != sha(&first)),
        "{pins:?}"
    );
}

#[tokio::test]
async fn look_settings_hide_unseen_hub() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let olive = server.user("Olive").await;
    let (hub, _) = create_site(&server, &root, "Secret hub").await;
    let (team, _) = create_site(&server, &root, "Team").await;
    grant(&server, &root, &team, &olive, "owner").await;
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

    // Root can see the hub: named.
    let shown = settings(&server, &root, &team).await;
    assert_eq!(shown["in_hub"], json!({ "id": hub, "name": "Secret hub" }));
    // Olive manages the site but cannot see the hub: told only that a hub's theme is in use.
    let shown = settings(&server, &olive, &team).await;
    assert_eq!(shown["in_hub"], json!({ "hidden": true }), "{shown}");
    assert!(!shown.to_string().contains("Secret hub"), "{shown}");
    assert!(!shown.to_string().contains(&hub), "{shown}");
    // A look change answers the settings the same way.
    let answer = set_look(&server, &olive, &team, builtin("builtin:plum")).await;
    assert_eq!(answer["in_hub"], json!({ "hidden": true }), "{answer}");
    // Once she can see the hub, it is named.
    grant(&server, &root, &hub, &olive, "viewer").await;
    let shown = settings(&server, &olive, &team).await;
    assert_eq!(shown["in_hub"], json!({ "id": hub, "name": "Secret hub" }));
}

/// Marks the content `bytes` infected, as a rescan would.
async fn mark_infected(server: &Site, bytes: &[u8]) {
    sqlx::query(
        "update blobs set scan = 'infected', scan_detail = 'Test-Signature' where sha256 = $1",
    )
    .bind(sha(bytes))
    .execute(server.records_db().test_pool())
    .await
    .unwrap();
}

#[tokio::test]
async fn quarantine_delete_reports_brand_pins() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let (team, documents) = create_site(&server, &root, "Team").await;
    let (brand_site, _) = create_site(&server, &root, "Brand").await;
    let assets = asset_library(&server, &root, &brand_site).await;
    let logo = png(40, 20);
    let doc = upload(&server, &root, &assets, "Logo.png", &logo).await;
    let (status, body) = call(
        &server,
        Some(&root),
        Method::POST,
        "/api/brand/images",
        Some(json!({ "doc": doc, "kind": "logo" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    set_look(
        &server,
        &root,
        &team,
        json!({ "theme": { "from": "builtin:sharecraft" }, "logo": { "doc": doc } }),
    )
    .await;
    mark_infected(&server, &logo).await;

    let (status, body) = call(
        &server,
        Some(&root),
        Method::POST,
        &format!("/api/admin/quarantine/{}/delete", sha(&logo)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["documents"], 1);
    assert_eq!(body["erased"], false);
    assert_eq!(
        body["pinned_by_brand"],
        json!([{ "owner": "org" }, { "owner": "site", "id": team, "name": "Team" }])
    );
    // The content stays, held: never served; the pins are left for the brand to change.
    assert!(server.blobs().contains(&hash(&logo)));
    assert_eq!(
        fetch(&server, &root, &image_url(&logo)).await,
        StatusCode::LOCKED
    );
    let db = server.records_db();
    assert_eq!(db.brand_pins("org").await.unwrap().len(), 1);
    assert_eq!(db.brand_pins(&team).await.unwrap().len(), 1);

    // Content nothing pins is erased, with an empty list.
    let plain = png(41, 21);
    upload(&server, &root, &documents, "Plain.png", &plain).await;
    mark_infected(&server, &plain).await;
    let (status, body) = call(
        &server,
        Some(&root),
        Method::POST,
        &format!("/api/admin/quarantine/{}/delete", sha(&plain)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["erased"], true, "{body}");
    assert_eq!(body["pinned_by_brand"], json!([]));
}

#[tokio::test]
async fn rollback_to_before_the_first_change() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let (team, _) = create_site(&server, &root, "Team").await;
    set_look(&server, &root, &team, builtin("builtin:harbour")).await;
    set_look(&server, &root, &team, builtin("builtin:plum")).await;
    let listed = history(&server, &root, &team).await;
    // Each entry says what the look was before it: none before the oldest, the older entry's
    // look before the newer.
    assert_eq!(listed[1]["before"], Value::Null);
    assert_eq!(listed[0]["before"], listed[1]["look"]);
    let first = listed[1]["event_id"].clone();
    let (status, body) = rollback(
        &server,
        &root,
        &team,
        json!({ "event_id": first, "which": "before" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(look_in_git(&server, &team), None);
    assert_eq!(
        effective(&server, &root, &team).await["theme"],
        resolved("builtin:sharecraft")
    );
}

#[tokio::test]
async fn banner_bleed_round_trips_and_is_checked() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let (team, _) = create_site(&server, &root, "Team").await;
    let header = |bleed: Value| {
        json!({ "layout": "compact", "background": "none", "align": "left",
                "hide_title": false, "bleed": bleed })
    };

    // Settings are kept as sent (colours in lower case) and shown on the effective look.
    let bleed = json!({ "enabled": false, "distance": 120, "blur": 16, "transparency": 80,
                        "tint_light": "#F0E0D0", "tint_dark": "#102030" });
    let answer = set_look(
        &server,
        &root,
        &team,
        json!({ "theme": { "from": "builtin:sharecraft" }, "header": header(bleed) }),
    )
    .await;
    let kept = json!({ "enabled": false, "distance": 120, "blur": 16, "transparency": 80,
                       "tint_light": "#f0e0d0", "tint_dark": "#102030" });
    assert_eq!(answer["look"]["header"]["bleed"], kept);
    assert_eq!(
        effective(&server, &root, &team).await["header"]["bleed"],
        kept
    );
    let text = String::from_utf8(record_bytes(&server, &team)).unwrap();
    assert!(text.contains("\"bleed\""), "{text}");

    // A bleed left out, or sent in part, takes the defaults; at the defaults the file leaves it out.
    let answer = set_look(
        &server,
        &root,
        &team,
        json!({ "theme": { "from": "builtin:sharecraft" }, "header": header(json!({ "blur": 8 })) }),
    )
    .await;
    assert_eq!(answer["look"]["header"]["bleed"]["blur"], 8);
    assert_eq!(answer["look"]["header"]["bleed"]["distance"], 240);
    set_look(
        &server,
        &root,
        &team,
        json!({ "theme": { "from": "builtin:sharecraft" } }),
    )
    .await;
    let text = String::from_utf8(record_bytes(&server, &team)).unwrap();
    assert!(!text.contains("\"bleed\""), "{text}");
    assert_eq!(
        effective(&server, &root, &team).await["header"]["bleed"]["enabled"],
        true
    );

    // Out of range is refused.
    for bleed in [
        json!({ "distance": 401 }),
        json!({ "blur": 97 }),
        json!({ "transparency": 101 }),
    ] {
        let (status, body) = put_look(
            &server,
            &root,
            &team,
            json!({ "theme": { "from": "builtin:sharecraft" }, "header": header(bleed) }),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(body["error"], "invalid_look", "{body}");
    }
    // As are a colour that is not #rrggbb and a field it does not have.
    for bleed in [json!({ "tint_dark": "navy" }), json!({ "glow": 3 })] {
        let (status, body) = put_look(
            &server,
            &root,
            &team,
            json!({ "theme": { "from": "builtin:sharecraft" }, "header": header(bleed) }),
        )
        .await;
        assert!(status.is_client_error(), "{status} {body}");
    }
}

#[tokio::test]
async fn header_frame_goes_with_the_image_and_is_checked() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let (team, documents) = create_site(&server, &root, "Team").await;
    let doc = upload(&server, &root, &documents, "Band.png", &png(400, 100)).await;
    let header = |frame: Value| {
        json!({ "layout": "extended", "background": "image", "align": "left",
                "hide_title": false, "image": { "doc": doc }, "frame": frame })
    };
    let answer = set_look(
        &server,
        &root,
        &team,
        json!({ "theme": { "from": "builtin:sharecraft" },
                "header": header(json!({ "x": 250, "y": 900, "zoom": 150 })) }),
    )
    .await;
    let frame = json!({ "x": 250, "y": 900, "zoom": 150 });
    assert_eq!(answer["look"]["header"]["frame"], frame);
    assert_eq!(
        effective(&server, &root, &team).await["header"]["frame"],
        frame
    );

    // Out of range, or without an image, is refused.
    for (body, why) in [
        (header(json!({ "x": 1001, "y": 0, "zoom": 100 })), "frame"),
        (header(json!({ "x": 0, "y": 0, "zoom": 99 })), "frame"),
        (
            json!({ "layout": "extended", "background": "none", "align": "left",
                    "hide_title": false, "frame": { "x": 0, "y": 0, "zoom": 100 } }),
            "image",
        ),
    ] {
        let (status, answer) = put_look(
            &server,
            &root,
            &team,
            json!({ "theme": { "from": "builtin:sharecraft" }, "header": body }),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
        assert_eq!(answer["error"], "invalid_look", "{answer}");
        assert!(answer["reason"].as_str().unwrap().contains(why), "{answer}");
    }
}
