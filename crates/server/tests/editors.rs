//! Installed editors: their files are served from this server's origin, under their own
//! policy, to signed-in callers only.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use common::{
    TestServer, TestUser, install_stub_editor, spawn_server, spawn_server_config,
    stub_editor_fixture,
};
use reqwest::StatusCode;
use reqwest::header::{
    CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, HeaderMap, X_CONTENT_TYPE_OPTIONS,
};
use sha2::{Digest, Sha256};

/// A server with the stub editor installed as `gridcraft`.
async fn server_with_stub() -> TestServer {
    spawn_server_config(|config| {
        install_stub_editor(config, "gridcraft");
    })
    .await
}

/// The manifest of the stub fixture.
fn fixture_manifest() -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(stub_editor_fixture().join("editor.json")).unwrap())
        .unwrap()
}

/// The hashes in a fixture manifest field.
fn hashes(manifest: &serde_json::Value, field: &str) -> Vec<String> {
    manifest[field]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect()
}

/// The values of every `Content-Security-Policy` header.
fn policies(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all(CONTENT_SECURITY_POLICY)
        .iter()
        .map(|v| v.to_str().unwrap().to_owned())
        .collect()
}

/// The status and body of a `GET` sent as written, so the path reaches the server unchanged
/// (an HTTP client would tidy `..` and `%2e%2e` away first).
async fn raw_get(addr: SocketAddr, path: &str, user: &TestUser) -> (u16, String) {
    let (path, cookie) = (path.to_owned(), user.cookie.clone());
    // On a thread of its own: the server runs on this test's runtime.
    tokio::task::spawn_blocking(move || raw_get_blocking(addr, &path, &cookie))
        .await
        .unwrap()
}

fn raw_get_blocking(addr: SocketAddr, path: &str, cookie: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nCookie: {cookie}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut answer = Vec::new();
    stream.read_to_end(&mut answer).unwrap();
    let answer = String::from_utf8_lossy(&answer).into_owned();
    let status = answer.split(' ').nth(1).unwrap().parse().unwrap();
    (status, answer)
}

#[tokio::test]
async fn installed_editor_is_served_with_its_policy() {
    let server = server_with_stub().await;
    let ann = server.user("Ann").await;
    let manifest = fixture_manifest();

    let response = server
        .http(Some(&ann))
        .get(server.url("/editors/gridcraft/index.html"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers().clone();
    let policy = policies(&headers);
    assert_eq!(policy.len(), 1, "one policy, not the site's as well");
    let policy = &policy[0];
    for hash in hashes(&manifest, "script_hashes")
        .into_iter()
        .chain(hashes(&manifest, "style_hashes"))
    {
        assert!(policy.contains(&format!("'{hash}'")), "{hash} in {policy}");
    }
    assert!(policy.contains("frame-ancestors 'self'"), "{policy}");
    assert!(!policy.contains("frame-ancestors 'none'"), "{policy}");
    for directive in ["base-uri 'none'", "form-action 'none'"] {
        assert!(policy.contains(directive), "{directive} in {policy}");
    }
    // Printing goes through a PDF in a frame, which this policy also covers.
    assert!(!policy.contains("object-src"), "{policy}");
    assert!(policy.contains("script-src 'self' 'wasm-unsafe-eval' 'sha256-"));
    assert_eq!(headers[CACHE_CONTROL], "no-cache");
    assert_eq!(headers[X_CONTENT_TYPE_OPTIONS], "nosniff");
    assert!(response.text().await.unwrap().contains("<textarea"));

    // The folder answers too, with its index page.
    let index = server
        .http(Some(&ann))
        .get(server.url("/editors/gridcraft/"))
        .send()
        .await
        .unwrap();
    assert_eq!(index.status(), StatusCode::OK);
}

#[tokio::test]
async fn editor_files_need_sign_in() {
    let server = server_with_stub().await;
    let response = server
        .http(None)
        .get(server.url("/editors/gridcraft/index.html"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let listing = server
        .http(None)
        .get(server.url("/api/editors"))
        .send()
        .await
        .unwrap();
    assert_eq!(listing.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn unknown_editor_is_not_found() {
    let server = server_with_stub().await;
    let ann = server.user("Ann").await;
    for path in [
        "/editors/wordcraft/index.html",
        "/editors/text/index.html",
        "/editors/nothing/index.html",
        "/editors/gridcraft/missing.js",
        "/editors/",
    ] {
        let response = server
            .http(Some(&ann))
            .get(server.url(path))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
    }
}

#[tokio::test]
async fn paths_cannot_leave_the_editor_folder() {
    let server = server_with_stub().await;
    let ann = server.user("Ann").await;
    let editors = server.data_dir.path().join("editors");
    std::fs::write(editors.join("beside.txt"), "marker-beside").unwrap();
    std::fs::write(server.data_dir.path().join("above.txt"), "marker-above").unwrap();
    std::fs::write(
        editors.join("gridcraft").join("inside.txt"),
        "marker-inside",
    )
    .unwrap();

    let addr = server.addr;
    let (status, answer) = raw_get(addr, "/editors/gridcraft/inside.txt", &ann).await;
    assert_eq!(status, 200);
    assert!(answer.contains("marker-inside"));

    for path in [
        "/editors/gridcraft/../beside.txt",
        "/editors/gridcraft/../../above.txt",
        "/editors/gridcraft/../../.env",
        "/editors/gridcraft/%2e%2e/beside.txt",
        "/editors/gridcraft/%2E%2E/%2e%2e/above.txt",
        "/editors/gridcraft/..%2fbeside.txt",
        "/editors/gridcraft/%2e%2e%2f%2e%2e%2fabove.txt",
        "/editors/gridcraft/..\\beside.txt",
        "/editors/gridcraft/..%5cbeside.txt",
        "/editors/gridcraft/..%5C..%5Cabove.txt",
        "/editors/gridcraft//beside.txt",
        "/editors/gridcraft/C:/Windows/win.ini",
        "/editors/gridcraft/C%3A%5CWindows%5Cwin.ini",
        "/editors/gridcraft/%2fetc%2fpasswd",
        "/editors/..%5cbeside.txt",
        "/editors/%2e%2e/beside.txt",
    ] {
        let (status, answer) = raw_get(addr, path, &ann).await;
        assert_eq!(status, 404, "{path}");
        assert!(!answer.contains("marker-"), "{path} leaked a file");
    }
}

#[tokio::test]
async fn bad_manifest_is_skipped_and_the_server_starts() {
    let server = spawn_server_config(|config| {
        install_stub_editor(config, "gridcraft");
        let other = |name: &str| {
            let folder = install_stub_editor(config, name);
            folder.join("editor.json")
        };
        // Not JSON.
        std::fs::write(other("wordcraft"), "{ not json").unwrap();
        // Names another id.
        let pdf = other("pdfcraft");
        let text = std::fs::read_to_string(&pdf).unwrap();
        std::fs::write(&pdf, text.replace("\"pdfcraft\"", "\"gridcraft\"")).unwrap();
        // No manifest at all.
        std::fs::remove_file(other("solidcraft")).unwrap();
        // Not an editor id.
        other("unknown");
        // Not a file editor.
        other("text");
    })
    .await;
    let ann = server.user("Ann").await;

    let listing: serde_json::Value = server
        .http(Some(&ann))
        .get(server.url("/api/editors"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids: Vec<&str> = listing
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["gridcraft"]);
    for id in ["wordcraft", "pdfcraft", "solidcraft", "unknown", "text"] {
        let response = server
            .http(Some(&ann))
            .get(server.url(&format!("/editors/{id}/index.html")))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{id}");
    }
}

#[tokio::test]
async fn missing_editors_directory_means_no_editors() {
    let server = spawn_server().await;
    let ann = server.user("Ann").await;
    let listing: serde_json::Value = server
        .http(Some(&ann))
        .get(server.url("/api/editors"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listing, serde_json::json!([]));
}

#[tokio::test]
async fn bad_hash_drops_the_manifest() {
    for bad in [
        "sha256-abc' 'unsafe-inline",
        "sha512-abc",
        "sha256-",
        "abc",
        "sha256-a;b",
    ] {
        let server = spawn_server_config(|config| {
            let manifest = install_stub_editor(config, "gridcraft").join("editor.json");
            let mut value: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
            value["script_hashes"] = serde_json::json!([bad]);
            std::fs::write(&manifest, serde_json::to_vec(&value).unwrap()).unwrap();
        })
        .await;
        let ann = server.user("Ann").await;
        let response = server
            .http(Some(&ann))
            .get(server.url("/editors/gridcraft/index.html"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{bad}");
    }
}

#[tokio::test]
async fn wasm_has_its_media_type() {
    let server = spawn_server_config(|config| {
        let folder = install_stub_editor(config, "gridcraft");
        std::fs::write(folder.join("app_bg.wasm"), b"\0asm\x01\0\0\0").unwrap();
    })
    .await;
    let ann = server.user("Ann").await;
    let response = server
        .http(Some(&ann))
        .get(server.url("/editors/gridcraft/app_bg.wasm"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[CONTENT_TYPE], "application/wasm");
    assert_eq!(policies(response.headers()).len(), 1);
}

#[tokio::test]
async fn editors_are_listed() {
    let server = server_with_stub().await;
    let ann = server.user("Ann").await;
    let response = server
        .http(Some(&ann))
        .get(server.url("/api/editors"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let manifest = fixture_manifest();
    assert_eq!(
        response.json::<serde_json::Value>().await.unwrap(),
        serde_json::json!([{
            "id": "gridcraft",
            "version": manifest["version"],
            "built": manifest["built"],
        }])
    );
}

#[tokio::test]
async fn site_pages_are_not_cached() {
    let server = spawn_server().await;
    let web = server.data_dir.path().join("web");
    std::fs::create_dir_all(&web).unwrap();
    std::fs::write(web.join("page.html"), "<p>hello</p>").unwrap();

    let response = server
        .http(None)
        .get(server.url("/page.html"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[CACHE_CONTROL], "no-cache");
    let policy = policies(response.headers());
    assert_eq!(policy.len(), 1);
    assert!(policy[0].contains("frame-src 'self'"), "{}", policy[0]);
    assert!(
        policy[0].contains("frame-ancestors 'none'"),
        "{}",
        policy[0]
    );
}

/// The `sha256-` hashes of the text inside each `<tag>…</tag>` of `page`, as a browser's
/// policy check computes them.
fn inline_hashes(page: &str, tag: &str) -> Vec<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut found = Vec::new();
    let mut rest = page;
    while let Some(start) = rest.find(&open) {
        let body = &rest[start + open.len()..];
        let end = body.find(&close).unwrap();
        let digest = Sha256::digest(&body.as_bytes()[..end]);
        found.push(format!("sha256-{}", STANDARD.encode(digest)));
        rest = &body[end + close.len()..];
    }
    found
}

#[test]
fn stub_fixture_hashes_match_its_page() {
    let page = std::fs::read_to_string(stub_editor_fixture().join("index.html")).unwrap();
    let manifest = fixture_manifest();
    let scripts = inline_hashes(&page, "script");
    let styles = inline_hashes(&page, "style");
    assert_eq!(scripts.len(), 1);
    assert_eq!(styles.len(), 1);
    assert_eq!(hashes(&manifest, "script_hashes"), scripts);
    assert_eq!(hashes(&manifest, "style_hashes"), styles);
    assert!(page.lines().count() < 60);
}
