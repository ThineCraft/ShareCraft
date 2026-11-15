//! Files of any type and the file policies over HTTP: a file no editor opens is a document with
//! the editor `other`, uploaded, renamed (never to an editor's extension) and given new
//! versions through upload sessions; the organisation's block list, set by administrators; and
//! each library's accepted and refused types and largest file, checked on every path into a
//! library: upload session start and finish, single-request upload, copy, move, bulk, rename
//! and new text documents. A file restored from the bin whose type is now refused is restored
//! and marked in its listings.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use common::{Site, TestUser, spawn_server};
use reqwest::header::CONTENT_RANGE;
use reqwest::{Method, StatusCode};
use revisions::BlobRef;
use serde_json::{Value, json};

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

/// A new site made by `admin`, and the id of its first library.
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

/// A new library of `kind` named `name` in the site `site`; its id.
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

/// Sets the organisation's block list as `user`; the status and answer.
async fn set_blocked(site: &Site, user: &TestUser, types: Value) -> (StatusCode, Value) {
    call(
        site,
        user,
        Method::PUT,
        "/api/settings/files",
        Some(json!({ "blocked_file_types": types })),
    )
    .await
}

/// Uploads `content` as `name` into `parent` the one-request way; the status and answer.
async fn upload(
    site: &Site,
    user: &TestUser,
    parent: &str,
    name: &str,
    content: &[u8],
) -> (StatusCode, Value) {
    let response = site
        .http(Some(user))
        .post(site.url("/api/docs/upload"))
        .query(&[("parent", parent), ("name", name)])
        .body(content.to_vec())
        .send()
        .await
        .unwrap();
    answer(response).await
}

/// Uploads `content` as `name` into `parent`; the new document's id.
async fn uploaded(
    site: &Site,
    user: &TestUser,
    parent: &str,
    name: &str,
    content: &[u8],
) -> String {
    let (status, body) = upload(site, user, parent, name, content).await;
    assert_eq!(status, StatusCode::CREATED, "{name}: {body}");
    body["id"].as_str().unwrap().to_owned()
}

/// Starts an upload session with `body`; the status and answer.
async fn start(site: &Site, user: &TestUser, body: Value) -> (StatusCode, Value) {
    call(site, user, Method::POST, "/api/uploads", Some(body)).await
}

/// The body that starts a session uploading `content` as `name` into `parent`.
fn new_file(parent: &str, name: &str, content: &[u8]) -> Value {
    json!({
        "parent": parent,
        "name": name,
        "size": content.len(),
        "sha256": BlobRef::of(content).hex(),
    })
}

/// Sends all of `content` to session `id` in one chunk, then finishes it; the status and
/// answer of the finish.
async fn send_and_finish(
    site: &Site,
    user: &TestUser,
    id: &str,
    content: &[u8],
) -> (StatusCode, Value) {
    let end = content.len() - 1;
    let response = site
        .http(Some(user))
        .put(site.url(&format!("/api/uploads/{id}")))
        .header(CONTENT_RANGE, format!("bytes 0-{end}/{}", content.len()))
        .body(content.to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    call(
        site,
        user,
        Method::POST,
        &format!("/api/uploads/{id}/finish"),
        None,
    )
    .await
}

/// The bytes of `doc`'s current version.
async fn download(site: &Site, user: &TestUser, doc: &str) -> Vec<u8> {
    let response = site
        .http(Some(user))
        .get(site.url(&format!("/api/docs/{doc}/content")))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    response.bytes().await.unwrap().to_vec()
}

/// Renames the node; the status and answer.
async fn rename(site: &Site, user: &TestUser, id: &str, name: &str) -> (StatusCode, Value) {
    call(
        site,
        user,
        Method::PATCH,
        &format!("/api/nodes/{id}"),
        Some(json!({ "name": name })),
    )
    .await
}

/// Moves the node into `parent`; the status and answer.
async fn move_to(site: &Site, user: &TestUser, id: &str, parent: &str) -> (StatusCode, Value) {
    call(
        site,
        user,
        Method::PATCH,
        &format!("/api/nodes/{id}"),
        Some(json!({ "parent": parent })),
    )
    .await
}

/// Copies the node into `parent`; the status and answer.
async fn copy_to(site: &Site, user: &TestUser, id: &str, parent: &str) -> (StatusCode, Value) {
    call(
        site,
        user,
        Method::POST,
        &format!("/api/nodes/{id}/copy"),
        Some(json!({ "parent": parent })),
    )
    .await
}

/// The children of `parent` as listed for `user`.
async fn listed(site: &Site, user: &TestUser, parent: &str) -> Vec<Value> {
    let (status, page) = call(
        site,
        user,
        Method::GET,
        &format!("/api/nodes/{parent}/children"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    page["items"].as_array().unwrap().clone()
}

/// The listed child named `name` of `parent`.
async fn listed_named(site: &Site, user: &TestUser, parent: &str, name: &str) -> Value {
    listed(site, user, parent)
        .await
        .into_iter()
        .find(|item| item["name"] == name)
        .unwrap_or_else(|| panic!("{name} is not listed"))
}

/// Asserts that a refusal is `422 blocked_file_type` naming `extension`.
fn assert_blocked(status: StatusCode, body: &Value, extension: &str) {
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"], "blocked_file_type", "{body}");
    assert_eq!(body["extension"], extension, "{body}");
}

/// Asserts that a refusal is `413 file_too_large` naming `limit`.
fn assert_too_large(status: StatusCode, body: &Value, limit: u64) {
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    assert_eq!(body["error"], "file_too_large", "{body}");
    assert_eq!(body["limit"], limit, "{body}");
}

/// Files no editor opens upload, one request or in a session, as documents with the editor
/// `other`, keeping their names and bytes, and open as such.
#[tokio::test]
async fn any_file_type_uploads_as_other() {
    let server = spawn_server().await;
    let admin = server.admin("Root").await;
    let (_, library) = site_and_library(&server, &admin, "Files").await;

    for (name, content) in [
        ("Archive.zip", &b"PK\x03\x04 zip bytes"[..]),
        ("Photo.png", &b"\x89PNG\r\n\x1a\n png bytes"[..]),
        ("README", &b"no extension at all"[..]),
    ] {
        let (status, body) = upload(&server, &admin, &library, name, content).await;
        assert_eq!(status, StatusCode::CREATED, "{name}: {body}");
        assert_eq!(body["editor"], "other", "{name}");
        assert_eq!(body["name"], name);
        let doc = body["id"].as_str().unwrap();
        let (status, opened) = call(
            &server,
            &admin,
            Method::GET,
            &format!("/api/docs/{doc}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(opened["editor"], "other");
        assert_eq!(download(&server, &admin, doc).await, content);
        assert_eq!(
            listed_named(&server, &admin, &library, name).await["editor"],
            "other"
        );
    }

    // Through an upload session too.
    let content = b"seven bytes of tar";
    let (status, session) = start(
        &server,
        &admin,
        new_file(&library, "Backup.tar.gz", content),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{session}");
    let (status, body) =
        send_and_finish(&server, &admin, session["id"].as_str().unwrap(), content).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["editor"], "other");
    assert_eq!(body["name"], "Backup.tar.gz");

    // A name that is only an editor's extension is still no name.
    let (status, _) = upload(&server, &admin, &library, ".xlsx", b"data").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // An empty other file cannot be made without its bytes.
    let (status, body) = call(
        &server,
        &admin,
        Method::POST,
        "/api/docs",
        Some(json!({ "parent": library, "name": "Empty.zip", "editor": "other" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "upload_required");
}

/// Files with an editor's native extension still open in that editor; an `other` file has no
/// editor, so no file session opens on it.
#[tokio::test]
async fn editor_types_still_open_in_their_editor() {
    let server = spawn_server().await;
    let admin = server.admin("Root").await;
    let (_, library) = site_and_library(&server, &admin, "Editors").await;
    for (name, editor) in [
        ("Budget.XLSX", "gridcraft"),
        ("Letter.docx", "wordcraft"),
        ("Scan.pdf", "pdfcraft"),
        ("Part.solidcraft.json", "solidcraft"),
    ] {
        let (status, body) = upload(&server, &admin, &library, name, b"bytes").await;
        assert_eq!(status, StatusCode::CREATED, "{name}: {body}");
        assert_eq!(body["editor"], editor, "{name}");
    }
    let other = uploaded(&server, &admin, &library, "Data.json", b"{}").await;
    let status = common::handshake_status(common::ws_request(
        server.addr,
        &format!("/ws/docs/{other}"),
        Some(&admin.cookie),
    ))
    .await;
    assert_eq!(status, 400);
    // Nor is it saved as an editor saves.
    let response = server
        .http(Some(&admin))
        .put(server.url(&format!("/api/docs/{other}/content")))
        .header("x-sharecraft-seat", "no-seat")
        .body(b"new".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

/// An `other` file takes any new name, with or without an extension, but never one an editor
/// opens; an editor's file keeps its extension.
#[tokio::test]
async fn other_files_rename_but_never_to_an_editor_extension() {
    let server = spawn_server().await;
    let admin = server.admin("Root").await;
    let (_, library) = site_and_library(&server, &admin, "Renames").await;
    let doc = uploaded(&server, &admin, &library, "Notes.zip", b"zip").await;

    for name in ["Notes.tar.gz", "Notes", "Notes.TXT"] {
        let (status, body) = rename(&server, &admin, &doc, name).await;
        assert_eq!(status, StatusCode::OK, "{name}: {body}");
        assert_eq!(body["name"], name);
    }
    for name in [
        "Notes.xlsx",
        "Notes.DOCX",
        "Notes.pdf",
        "Notes.solidcraft.json",
    ] {
        let (status, body) = rename(&server, &admin, &doc, name).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{name}: {body}");
        assert_eq!(body["error"], "wrong_extension", "{name}");
    }
    let (_, opened) = call(
        &server,
        &admin,
        Method::GET,
        &format!("/api/docs/{doc}"),
        None,
    )
    .await;
    assert_eq!(opened["editor"], "other");
    assert_eq!(opened["name"], "Notes.TXT");

    // An editor's file keeps its own extension.
    let sheet = uploaded(&server, &admin, &library, "Plan.xlsx", b"sheet").await;
    let (status, body) = rename(&server, &admin, &sheet, "Plan.zip").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"], "wrong_extension");
    let (status, _) = rename(&server, &admin, &sheet, "Budget.xlsx").await;
    assert_eq!(status, StatusCode::OK);
}

/// A new version of an `other` file comes through an upload session, as for an editor's file.
#[tokio::test]
async fn new_versions_of_other_files_through_sessions() {
    let server = spawn_server().await;
    let admin = server.admin("Root").await;
    let (_, library) = site_and_library(&server, &admin, "Versions").await;
    let doc = uploaded(&server, &admin, &library, "Model.stl", b"first version").await;
    let (_, opened) = call(
        &server,
        &admin,
        Method::GET,
        &format!("/api/docs/{doc}"),
        None,
    )
    .await;
    let base = opened["revision"].as_str().unwrap().to_owned();

    let content = b"second version, longer";
    let (status, session) = start(
        &server,
        &admin,
        json!({
            "doc": doc,
            "size": content.len(),
            "sha256": BlobRef::of(content).hex(),
            "base": base,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{session}");
    let (status, body) =
        send_and_finish(&server, &admin, session["id"].as_str().unwrap(), content).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["changed"], true);
    assert_eq!(download(&server, &admin, &doc).await, content);
    let revisions = server
        .records_db()
        .revisions(collab_core::ids::DocId(doc.parse().unwrap()))
        .await
        .unwrap();
    assert_eq!(revisions.len(), 2);
}

/// The organisation's and the libraries' file policies hold on every path into a library.
#[tokio::test]
async fn policies_hold_on_every_path() {
    let server = spawn_server().await;
    let admin = server.admin("Root").await;
    let (site, open) = site_and_library(&server, &admin, "Policies").await;
    let strict = new_library(&server, &admin, &site, "Strict", "document").await;
    set_library(
        &server,
        &admin,
        &strict,
        json!({ "blocked_types": [".exe"], "max_file_bytes": 1000 }),
    )
    .await;
    let images = new_library(&server, &admin, &site, "Pictures", "image").await;
    let (status, body) = set_blocked(&server, &admin, json!([".bat"])).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Upload session start: the library's list, the organisation's, the largest file, and
    // the image list.
    let (status, body) = start(&server, &admin, new_file(&strict, "Setup.exe", b"MZ")).await;
    assert_blocked(status, &body, ".exe");
    let (status, body) = start(&server, &admin, new_file(&open, "Run.BAT", b"echo")).await;
    assert_blocked(status, &body, ".bat");
    let big = vec![7u8; 2000];
    let (status, body) = start(&server, &admin, new_file(&strict, "Big.zip", &big)).await;
    assert_too_large(status, &body, 1000);
    let (status, body) = start(&server, &admin, new_file(&images, "Report.zip", b"zip")).await;
    assert_blocked(status, &body, ".zip");
    let (status, body) = start(&server, &admin, new_file(&images, "Photo.JPG", b"jpg")).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // Upload session finish: the settings changed while the bytes arrived.
    let late = b"late zip";
    let (status, session) = start(&server, &admin, new_file(&strict, "Late.zip", late)).await;
    assert_eq!(status, StatusCode::CREATED, "{session}");
    set_library(
        &server,
        &admin,
        &strict,
        json!({ "blocked_types": [".exe", ".zip"] }),
    )
    .await;
    let (status, body) =
        send_and_finish(&server, &admin, session["id"].as_str().unwrap(), late).await;
    assert_blocked(status, &body, ".zip");
    set_library(
        &server,
        &admin,
        &strict,
        json!({ "blocked_types": [".exe"] }),
    )
    .await;

    // Single-request upload.
    let (status, body) = upload(&server, &admin, &strict, "Setup.exe", b"MZ").await;
    assert_blocked(status, &body, ".exe");
    let (status, body) = upload(&server, &admin, &strict, "Big.zip", &big).await;
    assert_too_large(status, &body, 1000);
    let (status, body) = upload(&server, &admin, &images, "Notes", b"text").await;
    assert_blocked(status, &body, "");
    let (status, body) = upload(&server, &admin, &open, "Run.bat", b"echo").await;
    assert_blocked(status, &body, ".bat");

    // A new text document is a new name too.
    let (status, body) = call(
        &server,
        &admin,
        Method::POST,
        "/api/docs",
        Some(json!({ "parent": images, "name": "Plan.txt" })),
    )
    .await;
    assert_blocked(status, &body, ".txt");

    // Copy and move into the library, one item or a folder holding it, and in bulk.
    let exe = uploaded(&server, &admin, &open, "Tool.exe", b"MZ tool").await;
    let large = uploaded(&server, &admin, &open, "Large.zip", &big).await;
    let (status, folder) = call(
        &server,
        &admin,
        Method::POST,
        &format!("/api/nodes/{open}/children"),
        Some(json!({ "kind": "folder", "name": "Kit" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{folder}");
    let folder = folder["id"].as_str().unwrap().to_owned();
    uploaded(&server, &admin, &folder, "Inner.exe", b"MZ inner").await;

    let (status, body) = copy_to(&server, &admin, &exe, &strict).await;
    assert_blocked(status, &body, ".exe");
    let (status, body) = move_to(&server, &admin, &exe, &strict).await;
    assert_blocked(status, &body, ".exe");
    let (status, body) = copy_to(&server, &admin, &large, &strict).await;
    assert_too_large(status, &body, 1000);
    let (status, body) = move_to(&server, &admin, &large, &strict).await;
    assert_too_large(status, &body, 1000);
    let (status, body) = copy_to(&server, &admin, &folder, &strict).await;
    assert_blocked(status, &body, ".exe");
    let (status, body) = move_to(&server, &admin, &folder, &strict).await;
    assert_blocked(status, &body, ".exe");
    let (status, body) = move_to(&server, &admin, &exe, &images).await;
    assert_blocked(status, &body, ".exe");
    for op in ["move", "copy"] {
        let (status, results) = call(
            &server,
            &admin,
            Method::POST,
            "/api/bulk",
            Some(json!({ "op": op, "items": [exe, large, folder], "parent": strict })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{results}");
        let errors: Vec<&Value> = results
            .as_array()
            .unwrap()
            .iter()
            .map(|r| &r["error"])
            .collect();
        assert_eq!(
            errors,
            [
                &json!("blocked_file_type"),
                &json!("file_too_large"),
                &json!("blocked_file_type")
            ],
            "{op}: {results}"
        );
    }
    assert!(listed(&server, &admin, &strict).await.is_empty());
    // Within the library they came from, nothing changes for them.
    let (status, body) = move_to(&server, &admin, &exe, &folder).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Rename.
    let fine = uploaded(&server, &admin, &strict, "Fine.zip", b"zip").await;
    let (status, body) = rename(&server, &admin, &fine, "Fine.exe").await;
    assert_blocked(status, &body, ".exe");
    let (status, body) = rename(&server, &admin, &fine, "Fine.bat").await;
    assert_blocked(status, &body, ".bat");
    let (status, body) = rename(&server, &admin, &fine, "Better.zip").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // An upload session for a new version is held to the size limit, at start and at finish.
    let (status, body) = start(
        &server,
        &admin,
        json!({ "doc": fine, "size": big.len(), "sha256": BlobRef::of(&big).hex() }),
    )
    .await;
    assert_too_large(status, &body, 1000);
}

/// A file's extension is the longest suffix in the list being checked, whatever its case.
#[tokio::test]
async fn longest_extension_matches_first() {
    let server = spawn_server().await;
    let admin = server.admin("Root").await;
    let (site, archives) = site_and_library(&server, &admin, "Archives").await;
    set_library(
        &server,
        &admin,
        &archives,
        json!({ "allowed_types": [".tar.gz", ".zip"] }),
    )
    .await;
    uploaded(&server, &admin, &archives, "Backup.TAR.GZ", b"tar").await;
    let (status, body) = upload(&server, &admin, &archives, "Single.gz", b"gz").await;
    assert_blocked(status, &body, ".gz");

    let other = new_library(&server, &admin, &site, "Other", "document").await;
    set_library(
        &server,
        &admin,
        &other,
        json!({ "blocked_types": [".gz", ".tar.gz"] }),
    )
    .await;
    let (status, body) = upload(&server, &admin, &other, "Backup.Tar.Gz", b"tar").await;
    assert_blocked(status, &body, ".tar.gz");
    let (status, body) = upload(&server, &admin, &other, "Single.gz", b"gz").await;
    assert_blocked(status, &body, ".gz");
    uploaded(&server, &admin, &other, "Backup.tgz", b"tgz").await;
}

/// Only administrators read and set the organisation's block list; it is cleaned and checked,
/// and applies at once.
#[tokio::test]
async fn org_block_list_is_for_administrators() {
    let server = spawn_server().await;
    let admin = server.admin("Root").await;
    let ann = server.user("Ann").await;
    let home = server.home_of(&ann).await.0.to_string();

    let (status, _) = call(&server, &ann, Method::GET, "/api/settings/files", None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = set_blocked(&server, &ann, json!([".exe"])).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, body) = call(&server, &admin, Method::GET, "/api/settings/files", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "blocked_file_types": [] }));

    uploaded(&server, &ann, &home, "Before.exe", b"MZ").await;
    let (status, body) = set_blocked(&server, &admin, json!([" .EXE ", ".bat"])).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({ "blocked_file_types": [".exe", ".bat"] }));
    let (_, body) = call(&server, &admin, Method::GET, "/api/settings/files", None).await;
    assert_eq!(body, json!({ "blocked_file_types": [".exe", ".bat"] }));
    let (status, body) = upload(&server, &ann, &home, "After.exe", b"MZ").await;
    assert_blocked(status, &body, ".exe");

    for bad in [
        json!(["exe"]),
        json!([".exe", ".exe"]),
        json!("exe"),
        json!([".a b"]),
    ] {
        let (status, body) = set_blocked(&server, &admin, bad.clone()).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}: {body}");
    }
    let many: Vec<String> = (0..101).map(|n| format!(".x{n}")).collect();
    let (status, _) = set_blocked(&server, &admin, json!(many)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (_, body) = call(&server, &admin, Method::GET, "/api/settings/files", None).await;
    assert_eq!(body, json!({ "blocked_file_types": [".exe", ".bat"] }));

    let changed: i64 = sqlx::query_scalar(
        "select count(*) from events where kind = 'setting.changed' \
         and detail->>'key' = 'blocked_file_types'",
    )
    .fetch_one(server.records_db().test_pool())
    .await
    .unwrap();
    assert_eq!(changed, 1);
    let (status, _) = set_blocked(&server, &admin, json!([])).await;
    assert_eq!(status, StatusCode::OK);
    uploaded(&server, &ann, &home, "After.exe", b"MZ").await;
}

/// A file restored from the bin whose type is now refused is restored, and marked
/// `"policy": "blocked"` in its listings, as is a file whose library's settings now refuse it.
#[tokio::test]
async fn restored_blocked_file_is_marked() {
    let server = spawn_server().await;
    let admin = server.admin("Root").await;
    let (_, library) = site_and_library(&server, &admin, "Bin").await;
    let exe = uploaded(&server, &admin, &library, "Tool.exe", b"MZ").await;
    uploaded(&server, &admin, &library, "Fine.zip", b"zip").await;
    let (status, _) = call(
        &server,
        &admin,
        Method::DELETE,
        &format!("/api/nodes/{exe}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = set_blocked(&server, &admin, json!([".exe"])).await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = call(
        &server,
        &admin,
        Method::POST,
        &format!("/api/bin/{exe}/restore"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let restored = listed_named(&server, &admin, &library, "Tool.exe").await;
    assert_eq!(restored["policy"], "blocked", "{restored}");
    let fine = listed_named(&server, &admin, &library, "Fine.zip").await;
    assert!(fine.get("policy").is_none(), "{fine}");

    // Settings that now refuse a file mark it too; it stays.
    set_library(
        &server,
        &admin,
        &library,
        json!({ "allowed_types": [".exe"] }),
    )
    .await;
    let fine = listed_named(&server, &admin, &library, "Fine.zip").await;
    assert_eq!(fine["policy"], "blocked", "{fine}");
}
