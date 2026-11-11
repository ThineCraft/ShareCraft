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
    ACCEPT_ENCODING, CACHE_CONTROL, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE,
    CONTENT_SECURITY_POLICY, CONTENT_TYPE, ETAG, HeaderMap, IF_MODIFIED_SINCE, IF_NONE_MATCH,
    RANGE, VARY, X_CONTENT_TYPE_OPTIONS,
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

/// The stand-in WebAssembly file and its two compressed copies. The server hands the copies
/// over as they are, so they need not be real Brotli or gzip.
const WASM: &[u8] = b"plain wasm bytes, plain wasm bytes, plain wasm bytes";
const WASM_BR: &[u8] = b"brotli copy bytes";
const WASM_GZ: &[u8] = b"gzip copy bytes, a little longer";

/// A server with the stub editor as `gridcraft` plus `app.wasm`, with its compressed copies if
/// `copies`.
async fn server_with_wasm(copies: bool) -> TestServer {
    spawn_server_config(|config| {
        let folder = install_stub_editor(config, "gridcraft");
        std::fs::write(folder.join("app.wasm"), WASM).unwrap();
        if copies {
            std::fs::write(folder.join("app.wasm.br"), WASM_BR).unwrap();
            std::fs::write(folder.join("app.wasm.gz"), WASM_GZ).unwrap();
        }
    })
    .await
}

/// `GET /editors/gridcraft/app.wasm`, sending `Accept-Encoding: encodings` if given.
async fn get_wasm(
    server: &TestServer,
    user: &TestUser,
    encodings: Option<&str>,
) -> reqwest::Response {
    let mut request = server
        .http(Some(user))
        .get(server.url("/editors/gridcraft/app.wasm"));
    if let Some(encodings) = encodings {
        request = request.header(ACCEPT_ENCODING, encodings);
    }
    request.send().await.unwrap()
}

/// The headers every editor answer carries, compressed or not.
fn assert_editor_headers(headers: &HeaderMap) {
    assert_eq!(policies(headers).len(), 1);
    assert!(policies(headers)[0].contains("frame-ancestors 'self'"));
    assert_eq!(headers[CACHE_CONTROL], "no-cache");
    assert_eq!(headers[X_CONTENT_TYPE_OPTIONS], "nosniff");
}

#[tokio::test]
async fn editor_files_are_served_brotli_when_accepted() {
    let server = server_with_wasm(true).await;
    let ann = server.user("Ann").await;
    let response = get_wasm(&server, &ann, Some("br, gzip")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers().clone();
    assert_eq!(headers[CONTENT_ENCODING], "br");
    assert_eq!(headers[CONTENT_TYPE], "application/wasm");
    assert!(
        headers.get_all(VARY).iter().any(|v| v
            .to_str()
            .unwrap()
            .to_ascii_lowercase()
            .contains("accept-encoding")),
        "{headers:?}"
    );
    assert_editor_headers(&headers);
    assert_eq!(response.bytes().await.unwrap().as_ref(), WASM_BR);
}

#[tokio::test]
async fn editor_files_fall_back_to_gzip() {
    let server = server_with_wasm(true).await;
    let ann = server.user("Ann").await;
    let response = get_wasm(&server, &ann, Some("gzip")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers().clone();
    assert_eq!(headers[CONTENT_ENCODING], "gzip");
    assert_eq!(headers[CONTENT_TYPE], "application/wasm");
    assert_editor_headers(&headers);
    assert_eq!(response.bytes().await.unwrap().as_ref(), WASM_GZ);
}

#[tokio::test]
async fn editor_files_are_plain_without_accept_encoding() {
    let server = server_with_wasm(true).await;
    let ann = server.user("Ann").await;
    for encodings in [None, Some("identity")] {
        let response = get_wasm(&server, &ann, encodings).await;
        assert_eq!(response.status(), StatusCode::OK);
        let headers = response.headers().clone();
        assert!(headers.get(CONTENT_ENCODING).is_none(), "{encodings:?}");
        assert_eq!(headers[CONTENT_TYPE], "application/wasm");
        assert_editor_headers(&headers);
        assert_eq!(response.bytes().await.unwrap().as_ref(), WASM);
    }
}

#[tokio::test]
async fn editor_without_copies_is_served_plain() {
    let server = server_with_wasm(false).await;
    let ann = server.user("Ann").await;
    let response = get_wasm(&server, &ann, Some("br, gzip")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers().clone();
    assert!(headers.get(CONTENT_ENCODING).is_none());
    assert_eq!(headers[CONTENT_TYPE], "application/wasm");
    assert_editor_headers(&headers);
    assert_eq!(response.bytes().await.unwrap().as_ref(), WASM);
}

/// A copy asked for by name is a file of the editor like any other: it stays behind the guard
/// and cannot reach outside the editor's folder.
#[tokio::test]
async fn compressed_copies_asked_for_by_name_stay_inside_the_editor() {
    let server = server_with_wasm(true).await;
    let ann = server.user("Ann").await;
    let named = server
        .http(Some(&ann))
        .get(server.url("/editors/gridcraft/app.wasm.br"))
        .send()
        .await
        .unwrap();
    assert_eq!(named.status(), StatusCode::OK);
    assert_editor_headers(named.headers());
    assert_eq!(named.bytes().await.unwrap().as_ref(), WASM_BR);

    let signed_out = server
        .http(None)
        .get(server.url("/editors/gridcraft/app.wasm.br"))
        .header(ACCEPT_ENCODING, "br")
        .send()
        .await
        .unwrap();
    assert_eq!(signed_out.status(), StatusCode::NOT_FOUND);
    for path in [
        "/editors/gridcraft/../app.wasm.br",
        "/editors/gridcraft/%2e%2e/app.wasm.br",
        "/editors/gridcraft/..%2fapp.wasm.gz",
    ] {
        let (status, _) = raw_get(server.addr, path, &ann).await;
        assert_eq!(status, 404, "{path}");
    }
}

/// Range and revalidation answer for the copy actually sent.
#[tokio::test]
async fn compressed_answers_support_ranges_and_revalidation() {
    let server = server_with_wasm(true).await;
    let ann = server.user("Ann").await;

    let first = get_wasm(&server, &ann, Some("br")).await;
    let headers = first.headers().clone();

    let ranged = server
        .http(Some(&ann))
        .get(server.url("/editors/gridcraft/app.wasm"))
        .header(ACCEPT_ENCODING, "br")
        .header(RANGE, "bytes=0-4")
        .send()
        .await
        .unwrap();
    assert_eq!(ranged.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(ranged.headers()[CONTENT_ENCODING], "br");
    assert_eq!(ranged.headers()[CONTENT_LENGTH], "5");
    let range = ranged.headers()[CONTENT_RANGE].to_str().unwrap().to_owned();
    assert_eq!(range, format!("bytes 0-4/{}", WASM_BR.len()));
    assert_eq!(ranged.bytes().await.unwrap().as_ref(), &WASM_BR[..5]);

    let etag = headers.get(ETAG).expect("an ETag");
    let answer = server
        .http(Some(&ann))
        .get(server.url("/editors/gridcraft/app.wasm"))
        .header(ACCEPT_ENCODING, "br")
        .header(IF_NONE_MATCH, etag)
        .send()
        .await
        .unwrap();
    assert_eq!(answer.status(), StatusCode::NOT_MODIFIED);
    // The plain file has its own validator: the br one does not match it.
    let plain = get_wasm(&server, &ann, None).await;
    assert_ne!(plain.headers()[ETAG], *etag);
    let modified = headers.get("last-modified").expect("Last-Modified");
    let answer = server
        .http(Some(&ann))
        .get(server.url("/editors/gridcraft/app.wasm"))
        .header(ACCEPT_ENCODING, "br")
        .header(IF_MODIFIED_SINCE, modified)
        .send()
        .await
        .unwrap();
    assert_eq!(answer.status(), StatusCode::NOT_MODIFIED);
    assert_editor_headers(answer.headers());
}

/// The page itself, not only the wasm, is served compressed and keeps its policy.
#[tokio::test]
async fn the_editor_page_is_served_compressed() {
    let server = spawn_server_config(|config| {
        let folder = install_stub_editor(config, "gridcraft");
        std::fs::write(folder.join("index.html.br"), b"page brotli copy").unwrap();
        std::fs::write(folder.join("index.html.gz"), b"page gzip copy").unwrap();
    })
    .await;
    let ann = server.user("Ann").await;
    let response = server
        .http(Some(&ann))
        .get(server.url("/editors/gridcraft/index.html"))
        .header(ACCEPT_ENCODING, "br, gzip")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers().clone();
    assert_eq!(headers[CONTENT_ENCODING], "br");
    assert!(
        headers[CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    assert_editor_headers(&headers);
    assert!(policies(&headers)[0].contains("'sha256-"));
    assert_eq!(
        response.bytes().await.unwrap().as_ref(),
        b"page brotli copy"
    );
}

/// HEAD answers with the headers of the copy a GET would send, and no body.
#[tokio::test]
async fn head_requests_describe_the_compressed_copy() {
    let server = server_with_wasm(true).await;
    let ann = server.user("Ann").await;
    let response = server
        .http(Some(&ann))
        .head(server.url("/editors/gridcraft/app.wasm"))
        .header(ACCEPT_ENCODING, "br")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers().clone();
    assert_eq!(headers[CONTENT_ENCODING], "br");
    assert_eq!(headers[CONTENT_TYPE], "application/wasm");
    assert_eq!(headers[CONTENT_LENGTH], WASM_BR.len().to_string().as_str());
    assert_editor_headers(&headers);
    assert!(response.bytes().await.unwrap().is_empty());
}
