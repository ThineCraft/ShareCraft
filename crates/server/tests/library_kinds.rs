//! Template and image libraries over HTTP: the "New from template" choices of a site (the
//! files people can view, whose content is available, in its template libraries and nowhere
//! else), copying one into a library through the copy path (its latest version only, with the
//! quota, the file policies and `add` holding), image libraries refusing other types, and the
//! grid listing of an image library with its thumbnail marks.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;
#[path = "../../../xtask/src/fake_icap.rs"]
#[allow(dead_code)]
mod fake_icap;

use std::io::Cursor;
use std::net::SocketAddr;
use std::time::Duration;

use collab_core::ids::DocId;
use common::{Site, TestUser, spawn_server};
use fake_icap::{Behaviour, FakeIcap, eicar};
use image::{DynamicImage, ImageFormat, Rgb, RgbImage};
use records::ScanState;
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// The longest a test waits for the scanner.
const SCAN_WAIT: Duration = Duration::from_secs(20);

async fn call(
    site: &Site,
    user: &TestUser,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = site.http(Some(user)).request(method, site.url(path));
    if let Some(body) = body {
        request = request.json(&body);
    }
    answer(request.send().await.unwrap()).await
}

async fn answer(response: reqwest::Response) -> (StatusCode, Value) {
    let status = response.status();
    let text = response.text().await.unwrap();
    let body = if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).unwrap_or(Value::String(text))
    };
    (status, body)
}

/// A new site made by `admin`, and the id of its first library (`Documents`).
async fn site_and_library(site: &Site, admin: &TestUser, name: &str) -> (String, String) {
    let (status, body) = call(
        site,
        admin,
        Method::POST,
        "/api/sites",
        Some(json!({ "name": name })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["id"].as_str().unwrap().to_owned();
    let (status, page) = call(
        site,
        admin,
        Method::GET,
        &format!("/api/nodes/{id}/children"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let library = page["items"][0]["id"].as_str().unwrap().to_owned();
    (id, library)
}

/// A new library of `kind` named `name` in the site `site_id`; its id.
async fn new_library(
    site: &Site,
    admin: &TestUser,
    site_id: &str,
    name: &str,
    kind: &str,
) -> String {
    let (status, body) = call(
        site,
        admin,
        Method::POST,
        &format!("/api/sites/{site_id}/libraries"),
        Some(json!({ "name": name, "kind": kind })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["id"].as_str().unwrap().to_owned()
}

/// A new folder named `name` in `parent`; its id.
async fn folder(site: &Site, user: &TestUser, parent: &str, name: &str) -> String {
    let (status, body) = call(
        site,
        user,
        Method::POST,
        &format!("/api/nodes/{parent}/children"),
        Some(json!({ "kind": "folder", "name": name })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["id"].as_str().unwrap().to_owned()
}

/// Changes the library's settings; fails the test unless they are taken.
async fn set_library(site: &Site, admin: &TestUser, library: &str, settings: Value) {
    let (status, body) = call(
        site,
        admin,
        Method::PATCH,
        &format!("/api/libraries/{library}"),
        Some(settings),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// Gives `user` the role `role` on `node`, as `admin`.
async fn grant(site: &Site, admin: &TestUser, node: &str, user: &TestUser, role: &str) {
    let (status, body) = call(
        site,
        admin,
        Method::PUT,
        &format!("/api/nodes/{node}/access"),
        Some(json!({ "email": user.email, "role": role })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
}

/// Uploads `content` as `name` into `parent` the one-request way; the status and answer.
async fn try_upload(
    site: &Site,
    user: &TestUser,
    parent: &str,
    name: &str,
    content: &[u8],
    replace: bool,
) -> (StatusCode, Value) {
    let mut query = vec![("parent", parent), ("name", name)];
    if replace {
        query.push(("on_conflict", "replace"));
    }
    let response = site
        .http(Some(user))
        .post(site.url("/api/docs/upload"))
        .query(&query)
        .body(content.to_vec())
        .send()
        .await
        .unwrap();
    answer(response).await
}

/// Uploads `content` as `name` into `parent`; the new document's id.
async fn upload(site: &Site, user: &TestUser, parent: &str, name: &str, content: &[u8]) -> String {
    let (status, body) = try_upload(site, user, parent, name, content, false).await;
    assert_eq!(status, StatusCode::CREATED, "{name}: {body}");
    body["id"].as_str().unwrap().to_owned()
}

/// The bytes of the document `doc`'s latest version, as `user` downloads them.
async fn download(site: &Site, user: &TestUser, doc: &str) -> (StatusCode, Vec<u8>) {
    let response = site
        .http(Some(user))
        .get(site.url(&format!("/api/docs/{doc}/content")))
        .send()
        .await
        .unwrap();
    let status = response.status();
    (status, response.bytes().await.unwrap().to_vec())
}

/// The template choices of `library` for `user`: the status and answer.
async fn templates(site: &Site, user: &TestUser, library: &str) -> (StatusCode, Value) {
    call(
        site,
        user,
        Method::GET,
        &format!("/api/libraries/{library}/templates"),
        None,
    )
    .await
}

/// The names of the template choices of `library` for `user`, as listed.
async fn template_names(site: &Site, user: &TestUser, library: &str) -> Vec<String> {
    let (status, body) = templates(site, user, library).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body.as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_owned())
        .collect()
}

/// `POST /api/libraries/{library}/from-template` as `user` with `body`.
async fn from_template(
    site: &Site,
    user: &TestUser,
    library: &str,
    body: Value,
) -> (StatusCode, Value) {
    call(
        site,
        user,
        Method::POST,
        &format!("/api/libraries/{library}/from-template"),
        Some(body),
    )
    .await
}

/// The names of the children of `parent` that `user` sees.
async fn names(site: &Site, user: &TestUser, parent: &str) -> Vec<String> {
    let (status, page) = call(
        site,
        user,
        Method::GET,
        &format!("/api/nodes/{parent}/children"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["name"].as_str().unwrap().to_owned())
        .collect()
}

fn doc_id(id: &str) -> DocId {
    DocId(Uuid::parse_str(id).unwrap())
}

fn sha(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn local() -> SocketAddr {
    "127.0.0.1:0".parse().unwrap()
}

/// Bytes the fake scanner finds infected: `label`, then the test string, never first.
fn infected(label: &str) -> Vec<u8> {
    let mut bytes = format!("Quarterly template {label}\n").into_bytes();
    bytes.extend_from_slice(&eicar());
    bytes.extend_from_slice(b"\nend of template\n");
    bytes
}

/// Waits until the content `bytes` reaches a scan state `done` accepts.
async fn wait_scan(site: &Site, bytes: &[u8], done: impl Fn(ScanState) -> bool) {
    let sha = sha(bytes);
    let deadline = tokio::time::Instant::now() + SCAN_WAIT;
    loop {
        let info = site.records_db().scan_state(&sha).await.unwrap();
        if let Some(info) = &info
            && done(info.state)
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the content's scan never finished: {info:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
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

#[tokio::test]
async fn templates_listed_from_the_sites_template_libraries() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let vera = server.user("Vera").await;
    let (site, documents) = site_and_library(&server, &root, "Team").await;
    let templates_lib = new_library(&server, &root, &site, "Templates", "template").await;
    let forms = new_library(&server, &root, &site, "Forms", "template").await;
    let images = new_library(&server, &root, &site, "Pictures", "image").await;
    grant(&server, &root, &site, &vera, "viewer").await;

    let budget = upload(&server, &root, &templates_lib, "Budget.xlsx", b"budget").await;
    let letters = folder(&server, &root, &templates_lib, "Letters").await;
    upload(&server, &root, &letters, "Letter.docx", b"dear").await;
    upload(&server, &root, &forms, "Claim.pdf", b"%PDF claim").await;
    // Files in libraries of other kinds are not templates.
    upload(&server, &root, &documents, "Notes.txt", b"notes").await;
    upload(&server, &root, &images, "Logo.png", &png(4, 4)).await;
    // A file in the bin is no choice.
    let gone = upload(&server, &root, &templates_lib, "Old.xlsx", b"old").await;
    let (status, _) = call(
        &server,
        &root,
        Method::DELETE,
        &format!("/api/nodes/{gone}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    // A file Vera cannot view is never listed to her.
    let secret = upload(&server, &root, &templates_lib, "Secret.xlsx", b"secret").await;
    let (status, body) = call(
        &server,
        &root,
        Method::POST,
        &format!("/api/nodes/{secret}/inheritance"),
        Some(json!({ "inherit": false })),
    )
    .await;
    assert!(status.is_success(), "{status} {body}");
    let (status, body) = call(
        &server,
        &root,
        Method::DELETE,
        &format!("/api/nodes/{secret}/access/user/{}", vera.id.0),
        None,
    )
    .await;
    assert!(status.is_success(), "{status} {body}");

    // The same choices from every library of the site, by name.
    for library in [&documents, &templates_lib, &forms, &images] {
        assert_eq!(
            template_names(&server, &root, library).await,
            vec!["Budget.xlsx", "Claim.pdf", "Letter.docx", "Secret.xlsx"],
        );
        assert_eq!(
            template_names(&server, &vera, library).await,
            vec!["Budget.xlsx", "Claim.pdf", "Letter.docx"],
        );
    }
    let (_, listed) = templates(&server, &vera, &documents).await;
    let first = &listed[0];
    assert_eq!(first["id"], budget.as_str(), "{listed}");
    assert_eq!(first["library"], templates_lib.as_str(), "{listed}");
    assert_eq!(first["library_name"], "Templates", "{listed}");
    assert_eq!(first["editor"], "gridcraft", "{listed}");

    // A library that becomes another kind offers its files no more; its files stay.
    set_library(&server, &root, &forms, json!({ "kind": "document" })).await;
    assert_eq!(
        template_names(&server, &vera, &documents).await,
        vec!["Budget.xlsx", "Letter.docx"],
    );
    assert_eq!(names(&server, &root, &forms).await, vec!["Claim.pdf"]);

    // Someone who cannot see the library is told nothing.
    let stranger = server.user("Stranger").await;
    let (status, _) = templates(&server, &stranger, &documents).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Nor is anyone asking of something that is not a library.
    let (status, _) = templates(&server, &root, &letters).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn new_from_template_copies_latest_version_only() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let olive = server.user("Olive").await;
    let (site, documents) = site_and_library(&server, &root, "Team").await;
    let templates_lib = new_library(&server, &root, &site, "Templates", "template").await;
    grant(&server, &root, &site, &olive, "editor").await;

    let budget = upload(
        &server,
        &root,
        &templates_lib,
        "Budget.xlsx",
        b"first draft",
    )
    .await;
    let (status, body) = try_upload(
        &server,
        &root,
        &templates_lib,
        "Budget.xlsx",
        b"the final template",
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(server.records_revisions(doc_id(&budget)).await.len(), 2);

    let (status, body) =
        from_template(&server, &olive, &documents, json!({ "template": budget })).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let copy = body["node"]["id"].as_str().unwrap().to_owned();
    assert_eq!(body["node"]["name"], "Budget.xlsx");
    assert_eq!(body["node"]["parent"], documents.as_str());
    assert!(body["renamed"].is_null(), "{body}");
    let (status, bytes) = download(&server, &olive, &copy).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, b"the final template");
    // Only the latest version, no history.
    assert_eq!(server.records_revisions(doc_id(&copy)).await.len(), 1);
    // The template itself is untouched.
    assert_eq!(server.records_revisions(doc_id(&budget)).await.len(), 2);

    // Into a folder, with a name of one's own; and a name taken is made unique.
    let plans = folder(&server, &olive, &documents, "Plans").await;
    let (status, body) = from_template(
        &server,
        &olive,
        &documents,
        json!({ "template": budget, "parent": plans, "name": "2027 budget" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["node"]["name"], "2027 budget.xlsx", "{body}");
    assert_eq!(body["node"]["parent"], plans.as_str());
    let (status, body) =
        from_template(&server, &olive, &documents, json!({ "template": budget })).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["node"]["name"], "Budget (2).xlsx", "{body}");
    assert_eq!(body["renamed"]["from"], "Budget.xlsx", "{body}");
    assert_eq!(
        names(&server, &olive, &documents).await,
        vec!["Plans", "Budget (2).xlsx", "Budget.xlsx"]
    );

    // The parent must be in the library named.
    let (status, body) = from_template(
        &server,
        &olive,
        &templates_lib,
        json!({ "template": budget, "parent": plans }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "wrong_place");
    // Only a template is copied this way.
    let plain = upload(&server, &root, &documents, "Plain.xlsx", b"plain").await;
    let (status, _) =
        from_template(&server, &olive, &documents, json!({ "template": plain })).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = from_template(
        &server,
        &olive,
        &documents,
        json!({ "template": Uuid::new_v4() }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) =
        from_template(&server, &olive, &documents, json!({ "template": "nope" })).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn new_from_template_obeys_quota_policies_and_add() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let vera = server.user("Vera").await;
    let olive = server.user("Olive").await;
    let (site, documents) = site_and_library(&server, &root, "Team").await;
    let templates_lib = new_library(&server, &root, &site, "Templates", "template").await;
    grant(&server, &root, &site, &vera, "viewer").await;
    grant(&server, &root, &site, &olive, "editor").await;
    let budget = upload(&server, &root, &templates_lib, "Budget.xlsx", &[7u8; 1000]).await;

    // `add` is needed where the copy goes.
    let (status, body) =
        from_template(&server, &vera, &documents, json!({ "template": budget })).await;
    assert_eq!(
        (status, body["error"].as_str()),
        (StatusCode::FORBIDDEN, Some("forbidden")),
        "{body}"
    );

    // The library's policies.
    set_library(
        &server,
        &root,
        &documents,
        json!({ "allowed_types": [".docx"] }),
    )
    .await;
    let (status, body) =
        from_template(&server, &olive, &documents, json!({ "template": budget })).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"], "blocked_file_type");
    assert_eq!(body["extension"], ".xlsx");
    set_library(
        &server,
        &root,
        &documents,
        json!({ "allowed_types": null, "max_file_bytes": 500 }),
    )
    .await;
    let (status, body) =
        from_template(&server, &olive, &documents, json!({ "template": budget })).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    assert_eq!(body["error"], "file_too_large");
    assert_eq!(body["limit"], 500);
    set_library(
        &server,
        &root,
        &documents,
        json!({ "max_file_bytes": null }),
    )
    .await;

    // The organisation's block list.
    let (status, body) = call(
        &server,
        &root,
        Method::PUT,
        "/api/settings/files",
        Some(json!({ "blocked_file_types": [".xlsx"] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) =
        from_template(&server, &olive, &documents, json!({ "template": budget })).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"], "blocked_file_type");
    let (status, _) = call(
        &server,
        &root,
        Method::PUT,
        "/api/settings/files",
        Some(json!({ "blocked_file_types": [] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // The site's quota.
    let used = server
        .records_db()
        .site_usage(collab_core::ids::NodeId(Uuid::parse_str(&site).unwrap()))
        .await
        .unwrap()
        .used;
    let (status, body) = call(
        &server,
        &root,
        Method::PUT,
        &format!("/api/sites/{site}/quota"),
        Some(json!({ "bytes": used + 500 })),
    )
    .await;
    assert!(status.is_success(), "{status} {body}");
    let (status, body) =
        from_template(&server, &olive, &documents, json!({ "template": budget })).await;
    assert_eq!(status, StatusCode::INSUFFICIENT_STORAGE, "{body}");
    assert_eq!(body["error"], "quota_exceeded");

    // Nothing was made by any refusal.
    assert!(names(&server, &root, &documents).await.is_empty());

    // With room, it is made.
    let (status, _) = call(
        &server,
        &root,
        Method::PUT,
        &format!("/api/sites/{site}/quota"),
        Some(json!({ "bytes": null })),
    )
    .await;
    assert!(status.is_success());
    let (status, body) =
        from_template(&server, &olive, &documents, json!({ "template": budget })).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(names(&server, &root, &documents).await, vec!["Budget.xlsx"]);
}

#[tokio::test]
async fn held_templates_not_offered() {
    let server = spawn_server().await;
    let fake = FakeIcap::start(local(), Behaviour::default()).unwrap();
    let root = server.admin("Root").await;
    let (status, body) = call(
        &server,
        &root,
        Method::PUT,
        "/api/settings/scanner",
        Some(json!({ "scanner_url": fake.url() })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let olive = server.user("Olive").await;
    let (site, documents) = site_and_library(&server, &root, "Team").await;
    let templates_lib = new_library(&server, &root, &site, "Templates", "template").await;
    grant(&server, &root, &site, &olive, "editor").await;

    let good = b"a clean template".to_vec();
    let clean = upload(&server, &root, &templates_lib, "Clean.xlsx", &good).await;
    wait_scan(&server, &good, |s| s == ScanState::Clean).await;
    let bad = infected("quarantined");
    let quarantined = upload(&server, &root, &templates_lib, "Infected.xlsx", &bad).await;
    wait_scan(&server, &bad, |s| s == ScanState::Infected).await;
    fake.set(Behaviour {
        fail: true,
        ..Behaviour::default()
    });
    let waiting = b"held until the scanner answers".to_vec();
    let held = upload(&server, &root, &templates_lib, "Held.xlsx", &waiting).await;
    wait_scan(&server, &waiting, |s| s == ScanState::Failed).await;

    // Only the available one is offered, to anyone.
    for user in [&root, &olive] {
        assert_eq!(
            template_names(&server, user, &documents).await,
            vec!["Clean.xlsx"]
        );
    }
    // Asked for all the same, held content is not copied.
    for (doc, scan) in [(&quarantined, "infected"), (&held, "failed")] {
        let (status, body) =
            from_template(&server, &olive, &documents, json!({ "template": doc })).await;
        assert_eq!(status, StatusCode::LOCKED, "{body}");
        assert_eq!(body["error"], "held");
        assert_eq!(body["scan"], scan);
    }
    assert!(names(&server, &olive, &documents).await.is_empty());
    let (status, body) =
        from_template(&server, &olive, &documents, json!({ "template": clean })).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // Released content is offered again.
    let hex: String = sha(&bad).iter().map(|b| format!("{b:02x}")).collect();
    let (status, body) = call(
        &server,
        &root,
        Method::POST,
        &format!("/api/admin/quarantine/{hex}/release"),
        Some(json!({ "reason": "a false alarm" })),
    )
    .await;
    assert!(status.is_success(), "{status} {body}");
    assert_eq!(
        template_names(&server, &olive, &documents).await,
        vec!["Clean.xlsx", "Infected.xlsx"]
    );
}

#[tokio::test]
async fn templates_from_other_sites_never_offered() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let olive = server.user("Olive").await;
    let (team, documents) = site_and_library(&server, &root, "Team").await;
    let (other, _) = site_and_library(&server, &root, "Other").await;
    let theirs = new_library(&server, &root, &other, "Their templates", "template").await;
    let ours = new_library(&server, &root, &team, "Templates", "template").await;
    grant(&server, &root, &team, &olive, "editor").await;
    grant(&server, &root, &other, &olive, "editor").await;
    let foreign = upload(&server, &root, &theirs, "Foreign.xlsx", b"foreign").await;
    upload(&server, &root, &ours, "Ours.xlsx", b"ours").await;

    for user in [&root, &olive] {
        assert_eq!(
            template_names(&server, user, &documents).await,
            vec!["Ours.xlsx"]
        );
        // Even someone who can view and copy it gets no copy of another site's template.
        let (status, body) =
            from_template(&server, user, &documents, json!({ "template": foreign })).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    }
    assert!(names(&server, &root, &documents).await.is_empty());

    // A deleted site has no templates and takes no new files.
    let (status, _) = call(
        &server,
        &root,
        Method::DELETE,
        &format!("/api/sites/{other}"),
        None,
    )
    .await;
    assert!(status.is_success());
    let (status, _) = templates(&server, &root, &theirs).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = from_template(&server, &root, &theirs, json!({ "template": foreign })).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn image_library_refuses_other_types() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let (site, documents) = site_and_library(&server, &root, "Team").await;
    let images = new_library(&server, &root, &site, "Pictures", "image").await;
    let templates_lib = new_library(&server, &root, &site, "Templates", "template").await;

    let picture = upload(&server, &root, &images, "Photo.png", &png(8, 8)).await;
    upload(&server, &root, &images, "Logo.svg", b"<svg/>").await;
    for name in ["Notes.txt", "Sheet.xlsx", "Setup.exe", "Archive.tar.gz"] {
        let (status, body) = try_upload(&server, &root, &images, name, b"x", false).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{name}: {body}");
        assert_eq!(body["error"], "blocked_file_type", "{name}");
    }
    // Rename, copy and new-from-template hold the image list too.
    let (status, body) = call(
        &server,
        &root,
        Method::PATCH,
        &format!("/api/nodes/{picture}"),
        Some(json!({ "name": "Photo.txt" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"], "blocked_file_type");
    let sheet = upload(&server, &root, &documents, "Sheet.xlsx", b"sheet").await;
    let (status, body) = call(
        &server,
        &root,
        Method::POST,
        &format!("/api/nodes/{sheet}/copy"),
        Some(json!({ "parent": images })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let budget = upload(&server, &root, &templates_lib, "Budget.xlsx", b"budget").await;
    let (status, body) =
        from_template(&server, &root, &images, json!({ "template": budget })).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"], "blocked_file_type");
    assert_eq!(body["extension"], ".xlsx");
    // An image template goes in.
    let banner = upload(&server, &root, &templates_lib, "Banner.png", &png(6, 3)).await;
    let (status, body) =
        from_template(&server, &root, &images, json!({ "template": banner })).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        names(&server, &root, &images).await,
        vec!["Banner.png", "Logo.svg", "Photo.png"]
    );
}

#[tokio::test]
async fn grid_listing_marks_thumbnails() {
    let server = spawn_server().await;
    let fake = FakeIcap::start(local(), Behaviour::default()).unwrap();
    let root = server.admin("Root").await;
    let (status, body) = call(
        &server,
        &root,
        Method::PUT,
        "/api/settings/scanner",
        Some(json!({ "scanner_url": fake.url() })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (site, documents) = site_and_library(&server, &root, "Team").await;
    let images = new_library(&server, &root, &site, "Pictures", "image").await;

    let good = png(30, 20);
    upload(&server, &root, &images, "Good.png", &good).await;
    wait_scan(&server, &good, |s| s == ScanState::Clean).await;
    upload(&server, &root, &images, "Logo.svg", b"<svg/>").await;
    fake.set(Behaviour {
        fail: true,
        ..Behaviour::default()
    });
    let waiting = png(5, 5);
    upload(&server, &root, &images, "Waiting.png", &waiting).await;
    wait_scan(&server, &waiting, |s| s == ScanState::Failed).await;
    fake.set(Behaviour::default());
    let album = folder(&server, &root, &images, "Album").await;
    let chart = png(4, 4);
    upload(&server, &root, &documents, "Chart.png", &chart).await;
    wait_scan(&server, &chart, |s| s == ScanState::Clean).await;

    let grid = |path: String| {
        let server = &server;
        let root = &root;
        async move { call(server, root, Method::GET, &path, None).await }
    };
    let (status, page) = grid(format!("/api/nodes/{images}/children?view=grid")).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let marks: Vec<(String, Value)> = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| {
            (
                i["name"].as_str().unwrap().to_owned(),
                i["thumbnail"].clone(),
            )
        })
        .collect();
    assert_eq!(
        marks,
        vec![
            ("Album".to_owned(), Value::Null),
            ("Good.png".to_owned(), json!(true)),
            ("Logo.svg".to_owned(), Value::Null),
            ("Waiting.png".to_owned(), Value::Null),
        ],
        "{page}"
    );
    // A thumbnail marked can be asked for.
    let good_id = page["items"][1]["id"].as_str().unwrap().to_owned();
    let response = server
        .http(Some(&root))
        .get(server.url(&format!("/api/docs/{good_id}/thumbnail")))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    // The plain listing marks thumbnails too, in any library.
    let (_, page) = grid(format!("/api/nodes/{images}/children")).await;
    assert_eq!(page["items"][1]["thumbnail"], true, "{page}");
    let (_, page) = grid(format!("/api/nodes/{documents}/children")).await;
    assert_eq!(page["items"][0]["thumbnail"], true, "{page}");

    // The grid is for image libraries (and their folders) only, and other views are unknown.
    let (status, _) = grid(format!("/api/nodes/{documents}/children?view=grid")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _) = grid(format!("/api/nodes/{images}/children?view=tiles")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _) = grid(format!("/api/nodes/{album}/children?view=grid")).await;
    assert_eq!(status, StatusCode::OK);

    // Up to 200 a page, with the same paging as the list.
    for n in 0..200 {
        folder(&server, &root, &album, &format!("Shelf {n:03}")).await;
    }
    upload(&server, &root, &album, "Zebra.png", &good).await;
    let (status, page) = grid(format!("/api/nodes/{album}/children?view=grid")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["items"].as_array().unwrap().len(), 200);
    let next = page["next"].as_str().unwrap().to_owned();
    let (status, rest) = grid(format!(
        "/api/nodes/{album}/children?view=grid&after={next}"
    ))
    .await;
    assert_eq!(status, StatusCode::OK, "{rest}");
    assert_eq!(rest["items"].as_array().unwrap().len(), 1);
    assert_eq!(rest["items"][0]["name"], "Zebra.png");
    assert_eq!(rest["items"][0]["thumbnail"], true);
    assert!(rest["next"].is_null());
    let (_, page) = grid(format!("/api/nodes/{album}/children")).await;
    assert_eq!(page["items"].as_array().unwrap().len(), 100);
    let (status, _) = grid(format!("/api/nodes/{album}/children?view=grid&limit=201")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, page) = grid(format!("/api/nodes/{album}/children?view=grid&limit=50")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["items"].as_array().unwrap().len(), 50);
}
