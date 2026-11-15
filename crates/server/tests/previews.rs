//! Safe previews and image thumbnails over HTTP: the short list of types a browser may show,
//! with a fixed type chosen by extension and the download's sandboxing; text previews of the
//! first MiB of valid UTF-8; ranges for audio and video; thumbnails made once on the server and
//! kept beside the content store, refused quickly for hostile images and erased with their
//! content; nothing for held content; and previews through anyone links.
//!
//! Images are made in the tests; the JPEG, which nothing here can encode, is
//! `fixtures/images/gradient.jpg`.

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
use std::path::PathBuf;
use std::time::{Duration, Instant};

use common::{Site, TestServer, TestUser, spawn_server};
use fake_icap::{Behaviour, FakeIcap, eicar};
use image::{DynamicImage, ImageFormat, Rgb, RgbImage, Rgba, RgbaImage};
use records::ScanState;
use reqwest::header::{
    ACCEPT_RANGES, CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE,
    CONTENT_SECURITY_POLICY, CONTENT_TYPE, RANGE, REFERRER_POLICY, X_CONTENT_TYPE_OPTIONS,
};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use server::previews::{PreviewKind, preview_type, thumb_path};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

/// The longest a hostile image may take to be refused.
const QUICKLY: Duration = Duration::from_secs(5);
/// The longest a test waits for the scanner.
const SCAN_WAIT: Duration = Duration::from_secs(20);

const MIB: usize = 1 << 20;

fn sha(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
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

/// Uploads `bytes` as `name` into `parent`; the new document's id.
async fn upload(site: &Site, user: &TestUser, parent: &str, name: &str, bytes: &[u8]) -> String {
    let response = site
        .http(Some(user))
        .post(site.url("/api/docs/upload"))
        .query(&[("parent", parent), ("name", name)])
        .body(bytes.to_vec())
        .send()
        .await
        .unwrap();
    let (status, body) = answer(response).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["id"].as_str().unwrap().to_owned()
}

/// `GET path` as `user` (anonymous when `None`), with the `Range` header `range` if given.
async fn get(
    site: &Site,
    user: Option<&TestUser>,
    path: &str,
    range: Option<&str>,
) -> reqwest::Response {
    let mut request = site.http(user).get(site.url(path));
    if let Some(range) = range {
        request = request.header(RANGE, range);
    }
    request.send().await.unwrap()
}

async fn preview(site: &Site, user: &TestUser, doc: &str) -> reqwest::Response {
    get(site, Some(user), &format!("/api/docs/{doc}/preview"), None).await
}

async fn thumbnail(site: &Site, user: &TestUser, doc: &str) -> reqwest::Response {
    get(
        site,
        Some(user),
        &format!("/api/docs/{doc}/thumbnail"),
        None,
    )
    .await
}

fn header(response: &reqwest::Response, name: reqwest::header::HeaderName) -> String {
    response
        .headers()
        .get(&name)
        .map(|value| value.to_str().unwrap().to_owned())
        .unwrap_or_default()
}

/// The headers every preview carries whatever it shows.
fn assert_sandboxed(response: &reqwest::Response, what: &str) {
    assert_eq!(
        header(response, X_CONTENT_TYPE_OPTIONS),
        "nosniff",
        "{what}"
    );
    assert_eq!(
        header(response, CONTENT_SECURITY_POLICY),
        "sandbox",
        "{what}"
    );
    assert_eq!(header(response, CACHE_CONTROL), "no-store", "{what}");
    let disposition = header(response, CONTENT_DISPOSITION);
    assert!(
        disposition.starts_with("inline; filename=\""),
        "{what}: {disposition}"
    );
    assert!(
        disposition.contains("; filename*=UTF-8''"),
        "{what}: {disposition}"
    );
}

/// The error code of a refused request.
async fn refused(response: reqwest::Response) -> (StatusCode, String) {
    let (status, body) = answer(response).await;
    (
        status,
        body["error"].as_str().unwrap_or_default().to_owned(),
    )
}

/// An RGB image `width` × `height` encoded as `format`.
fn picture(width: u32, height: u32, format: ImageFormat) -> Vec<u8> {
    let image = RgbImage::from_fn(width, height, |x, y| {
        Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8])
    });
    let mut out = Vec::new();
    match format {
        ImageFormat::WebP => {
            image::codecs::webp::WebPEncoder::new_lossless(&mut out)
                .encode(
                    image.as_raw(),
                    width,
                    height,
                    image::ExtendedColorType::Rgb8,
                )
                .unwrap();
        }
        _ => DynamicImage::ImageRgb8(image)
            .write_to(&mut Cursor::new(&mut out), format)
            .unwrap(),
    }
    out
}

/// A two-frame GIF `width` × `height`.
fn animated(width: u32, height: u32) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut encoder = image::codecs::gif::GifEncoder::new(&mut out);
        let frames = [Rgba([255, 0, 0, 255]), Rgba([0, 0, 255, 255])]
            .map(|colour| image::Frame::new(RgbaImage::from_pixel(width, height, colour)));
        encoder.encode_frames(frames).unwrap();
    }
    out
}

fn jpeg() -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/images/gradient.jpg");
    std::fs::read(path).unwrap()
}

/// The dimensions of an encoded image.
fn dimensions(bytes: &[u8]) -> (u32, u32) {
    let image = image::load_from_memory(bytes).unwrap();
    (image.width(), image.height())
}

// --- Hostile images, written by hand -------------------------------------------------------

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFF_u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 == 0 {
                crc >> 1
            } else {
                (crc >> 1) ^ 0xEDB8_8320
            };
        }
    }
    !crc
}

/// Bits written least significant first, as deflate packs them.
#[derive(Default)]
struct Bits {
    out: Vec<u8>,
    acc: u64,
    n: u32,
}

impl Bits {
    fn put(&mut self, value: u32, len: u32) {
        self.acc |= u64::from(value) << self.n;
        self.n += len;
        while self.n >= 8 {
            self.out.push((self.acc & 0xFF) as u8);
            self.acc >>= 8;
            self.n -= 8;
        }
    }

    /// A Huffman code, sent most significant bit first.
    fn code(&mut self, code: u32, len: u32) {
        let reversed = code.reverse_bits() >> (32 - len);
        self.put(reversed, len);
    }

    fn finish(mut self) -> Vec<u8> {
        if self.n > 0 {
            self.out.push((self.acc & 0xFF) as u8);
        }
        self.out
    }
}

/// A zlib stream of `n` zero bytes, a few bytes for every 258: one fixed-Huffman block of a
/// zero and then copies of it.
fn zlib_zeros(n: u64) -> Vec<u8> {
    let mut bits = Bits::default();
    bits.put(1, 1); // the last block
    bits.put(1, 2); // fixed Huffman codes
    let literal_zero = |bits: &mut Bits| bits.code(0x30, 8);
    literal_zero(&mut bits);
    let mut left = n - 1;
    while left >= 258 {
        bits.code(0xC5, 8); // length 258
        bits.code(0, 5); // distance 1
        left -= 258;
    }
    for _ in 0..left {
        literal_zero(&mut bits);
    }
    bits.code(0, 7); // end of block
    let mut out = vec![0x78, 0x01];
    out.extend(bits.finish());
    // Adler-32 of zeros: a stays 1, b adds 1 for each byte.
    let adler = (((n % 65_521) as u32) << 16) | 1;
    out.extend(adler.to_be_bytes());
    out
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend((data.len() as u32).to_be_bytes());
    let mut crc_input = kind.to_vec();
    crc_input.extend_from_slice(data);
    out.extend_from_slice(&crc_input);
    out.extend(crc32(&crc_input).to_be_bytes());
}

/// A black RGBA PNG `width` × `height`, whose pixels compress to almost nothing.
fn zeros_png(width: u32, height: u32) -> Vec<u8> {
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend(width.to_be_bytes());
    ihdr.extend(height.to_be_bytes());
    ihdr.extend([8, 6, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    let raw = u64::from(height) * (1 + u64::from(width) * 4);
    chunk(&mut out, b"IDAT", &zlib_zeros(raw));
    chunk(&mut out, b"IEND", &[]);
    out
}

/// A 1 × 1 GIF of `frames` frames.
fn many_frames_gif(frames: usize) -> Vec<u8> {
    let mut out = b"GIF89a".to_vec();
    out.extend([1, 0, 1, 0, 0x80, 0, 0]);
    out.extend([0, 0, 0, 255, 255, 255]);
    for _ in 0..frames {
        out.extend([
            0x2C, 0, 0, 0, 0, 1, 0, 1, 0, 0, 0x02, 0x02, 0x44, 0x01, 0x00,
        ]);
    }
    out.push(0x3B);
    out
}

/// Bytes the fake scanner finds infected, a valid PNG first.
fn infected_png() -> Vec<u8> {
    let mut bytes = picture(20, 20, ImageFormat::Png);
    bytes.extend_from_slice(&eicar());
    bytes
}

struct World {
    server: TestServer,
    ann: TestUser,
    home: String,
}

async fn world() -> World {
    let server = spawn_server().await;
    let ann = server.user("Ann").await;
    let home = server.home_of(&ann).await.0.to_string();
    World { server, ann, home }
}

/// Switches anyone links on and makes one on `node` as `user`; its token and id.
async fn anyone_link(
    site: &Site,
    admin: &TestUser,
    user: &TestUser,
    node: &str,
) -> (String, String) {
    let (status, _) = call(
        site,
        admin,
        Method::PUT,
        "/api/settings/sharing",
        Some(json!({ "anyone_links": true })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let expires = (OffsetDateTime::now_utc() + time::Duration::days(3))
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    let (status, made) = call(
        site,
        user,
        Method::POST,
        &format!("/api/nodes/{node}/links"),
        Some(json!({ "kind": "anyone", "role": "viewer", "expires_at": expires })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    let token = made["url"]
        .as_str()
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap()
        .to_owned();
    (token, made["id"].as_str().unwrap().to_owned())
}

#[tokio::test]
async fn preview_types_and_headers() {
    // The fixed types, by extension only.
    for (name, kind, content_type) in [
        ("a.png", PreviewKind::Image, "image/png"),
        ("A.PNG", PreviewKind::Image, "image/png"),
        ("a.jpg", PreviewKind::Image, "image/jpeg"),
        ("a.jpeg", PreviewKind::Image, "image/jpeg"),
        ("a.gif", PreviewKind::Image, "image/gif"),
        ("a.webp", PreviewKind::Image, "image/webp"),
        ("a.avif", PreviewKind::Image, "image/avif"),
        ("a.bmp", PreviewKind::Image, "image/bmp"),
        ("a.svg", PreviewKind::Image, "image/svg+xml"),
        ("a.mp3", PreviewKind::Audio, "audio/mpeg"),
        ("a.ogg", PreviewKind::Audio, "audio/ogg"),
        ("a.wav", PreviewKind::Audio, "audio/wav"),
        ("a.m4a", PreviewKind::Audio, "audio/mp4"),
        ("a.flac", PreviewKind::Audio, "audio/flac"),
        ("a.mp4", PreviewKind::Video, "video/mp4"),
        ("a.webm", PreviewKind::Video, "video/webm"),
        ("a.mov", PreviewKind::Video, "video/mp4"),
        ("a.txt", PreviewKind::Text, "text/plain; charset=utf-8"),
        ("a.html", PreviewKind::Text, "text/plain; charset=utf-8"),
        ("a.htm", PreviewKind::Text, "text/plain; charset=utf-8"),
        ("a.ps1", PreviewKind::Text, "text/plain; charset=utf-8"),
        ("a.yml", PreviewKind::Text, "text/plain; charset=utf-8"),
        (
            "archive.tar.json",
            PreviewKind::Text,
            "text/plain; charset=utf-8",
        ),
    ] {
        let found = preview_type(name).unwrap_or_else(|| panic!("{name} has a preview"));
        assert_eq!(found.kind, kind, "{name}");
        assert_eq!(found.content_type, content_type, "{name}");
    }
    for name in [
        "a.pdf", "a.xlsx", "a.docx", "a.exe", "a.tif", "a.heic", "a.xhtml", "a.js.exe", "png",
        "a.", "a",
    ] {
        assert!(preview_type(name).is_none(), "{name} has no preview");
    }

    let World { server, ann, home } = world().await;
    let png = picture(30, 20, ImageFormat::Png);
    let doc = upload(&server, &ann, &home, "Logo.png", &png).await;
    let response = preview(&server, &ann, &doc).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_sandboxed(&response, "png");
    assert_eq!(header(&response, CONTENT_TYPE), "image/png");
    assert_eq!(
        header(&response, CONTENT_DISPOSITION),
        "inline; filename=\"Logo.png\"; filename*=UTF-8''Logo.png"
    );
    assert!(!header(&response, "x-sharecraft-revision".parse().unwrap()).is_empty());
    assert_eq!(header(&response, CONTENT_LENGTH), png.len().to_string());
    assert_eq!(response.bytes().await.unwrap(), png);

    // The download is still an attachment of no type.
    let response = get(
        &server,
        Some(&ann),
        &format!("/api/docs/{doc}/content"),
        None,
    )
    .await;
    assert_eq!(header(&response, CONTENT_TYPE), "application/octet-stream");
    assert!(header(&response, CONTENT_DISPOSITION).starts_with("attachment;"));

    // Types with no preview.
    for (name, bytes) in [
        ("Report.pdf", b"%PDF-1.7 not really".to_vec()),
        ("Budget.xlsx", b"PK not really".to_vec()),
        ("Setup.exe", b"MZ not really".to_vec()),
        ("Scan.tif", b"II* not really".to_vec()),
    ] {
        let doc = upload(&server, &ann, &home, name, &bytes).await;
        let (status, error) = refused(preview(&server, &ann, &doc).await).await;
        assert_eq!(
            (status, error.as_str()),
            (StatusCode::UNSUPPORTED_MEDIA_TYPE, "no_preview"),
            "{name}"
        );
    }

    // View is needed: anyone else sees nothing there.
    let bob = server.user("Bob").await;
    let (status, error) = refused(preview(&server, &bob, &doc).await).await;
    assert_eq!(
        (status, error.as_str()),
        (StatusCode::NOT_FOUND, "not_found")
    );
    let (status, _) =
        refused(get(&server, None, &format!("/api/docs/{doc}/preview"), None).await).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn text_preview_first_mib_and_utf8_only() {
    let World { server, ann, home } = world().await;

    // A small file is shown whole.
    let csv = "name,amount\nAnn,3\n".as_bytes();
    let doc = upload(&server, &ann, &home, "Sums.csv", csv).await;
    let response = preview(&server, &ann, &doc).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_sandboxed(&response, "csv");
    assert_eq!(header(&response, CONTENT_TYPE), "text/plain; charset=utf-8");
    assert_eq!(response.bytes().await.unwrap(), csv);

    // A large one is cut at 1 MiB, never inside a character.
    let mut big = vec![b'a'; MIB - 1];
    big.extend("\u{e9}".as_bytes()); // two bytes, across the 1 MiB mark
    big.extend(vec![b'b'; MIB / 2]);
    let doc = upload(&server, &ann, &home, "Big.log", &big).await;
    let response = preview(&server, &ann, &doc).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.bytes().await.unwrap();
    assert_eq!(body.len(), MIB - 1);
    assert!(std::str::from_utf8(&body).is_ok());
    assert!(body.iter().all(|b| *b == b'a'));

    // Exactly 1 MiB of text, cut cleanly, is shown whole.
    let mut exact = vec![b'x'; MIB];
    let doc = upload(&server, &ann, &home, "Exact.md", &exact).await;
    let response = preview(&server, &ann, &doc).await;
    assert_eq!(response.bytes().await.unwrap().len(), MIB);
    exact.extend(b"more");
    let doc = upload(&server, &ann, &home, "Longer.md", &exact).await;
    let response = preview(&server, &ann, &doc).await;
    assert_eq!(response.bytes().await.unwrap().len(), MIB);

    // Not UTF-8: no preview, whether at the start or deep in the first MiB.
    for (name, bytes) in [
        ("Latin1.txt", b"caf\xe9 au lait".to_vec()),
        ("Binary.json", {
            let mut bytes = vec![b'{'; 1000];
            bytes.push(0xFF);
            bytes
        }),
        ("Utf16.txt", vec![0xFF, 0xFE, b'h', 0, b'i', 0]),
    ] {
        let doc = upload(&server, &ann, &home, name, &bytes).await;
        let (status, error) = refused(preview(&server, &ann, &doc).await).await;
        assert_eq!(
            (status, error.as_str()),
            (StatusCode::UNSUPPORTED_MEDIA_TYPE, "no_preview"),
            "{name}"
        );
    }
    // Bytes past the first MiB are never looked at.
    let mut tail = vec![b'z'; MIB];
    tail.extend([0xFF, 0xFE]);
    let doc = upload(&server, &ann, &home, "Tail.txt", &tail).await;
    let response = preview(&server, &ann, &doc).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.bytes().await.unwrap().len(), MIB);
}

#[tokio::test]
async fn previews_never_run_content() {
    let World { server, ann, home } = world().await;

    // A web page is shown as text.
    let page = b"<html><body><script>alert(document.cookie)</script></body></html>";
    for name in ["Page.html", "Page.htm"] {
        let doc = upload(&server, &ann, &home, name, page).await;
        let response = preview(&server, &ann, &doc).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_sandboxed(&response, name);
        assert_eq!(header(&response, CONTENT_TYPE), "text/plain; charset=utf-8");
        assert_eq!(response.bytes().await.unwrap(), page.as_slice());
    }

    // An SVG with a script is an image, behind the sandbox, and has no thumbnail.
    let svg = br#"<svg xmlns="http://www.w3.org/2000/svg"><script>alert(1)</script><rect width="5" height="5"/></svg>"#;
    let doc = upload(&server, &ann, &home, "Drawing.svg", svg).await;
    let response = preview(&server, &ann, &doc).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_sandboxed(&response, "svg");
    assert_eq!(header(&response, CONTENT_TYPE), "image/svg+xml");
    let (status, error) = refused(thumbnail(&server, &ann, &doc).await).await;
    assert_eq!(
        (status, error.as_str()),
        (StatusCode::UNSUPPORTED_MEDIA_TYPE, "no_thumbnail")
    );

    // A "PNG" that is really a web page keeps the PNG type, is never sniffed, and has no
    // thumbnail.
    let doc = upload(&server, &ann, &home, "Photo.png", page).await;
    let response = preview(&server, &ann, &doc).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_sandboxed(&response, "fake png");
    assert_eq!(header(&response, CONTENT_TYPE), "image/png");
    let (status, error) = refused(thumbnail(&server, &ann, &doc).await).await;
    assert_eq!(
        (status, error.as_str()),
        (StatusCode::UNSUPPORTED_MEDIA_TYPE, "no_thumbnail")
    );
    let thumbs = server.data_dir.path().join("thumbs");
    let made = walk(&thumbs);
    assert!(made.is_empty(), "{made:?}");

    // Names with quotes, line breaks and other scripts cannot break out of the header.
    for asked in [
        "Re\"port\".txt",
        "Two\r\nLines.txt",
        "Back\\slash.txt",
        "\u{dc}bersicht \u{65e5}\u{672c}.txt",
        "100%.txt",
    ] {
        let doc = upload(&server, &ann, &home, asked, b"plain words").await;
        let response = preview(&server, &ann, &doc).await;
        assert_eq!(response.status(), StatusCode::OK, "{asked}");
        let raw = response.headers()[CONTENT_DISPOSITION].as_bytes().to_vec();
        assert!(raw.is_ascii(), "{asked}");
        assert!(!raw.contains(&b'\r') && !raw.contains(&b'\n'), "{asked}");
        let text = String::from_utf8(raw).unwrap();
        let (plain, extended) = text.split_once("; filename*=UTF-8''").unwrap();
        let quoted = plain
            .strip_prefix("inline; filename=\"")
            .unwrap()
            .strip_suffix('"')
            .unwrap();
        assert!(!quoted.contains(['"', '\\', '%']), "{asked}: {text}");
        assert!(
            extended
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$&+-.^_`|~%".contains(&b)),
            "{asked}: {text}"
        );
    }
    let doc = upload(&server, &ann, &home, "\u{dc}bersicht.txt", b"x").await;
    let response = preview(&server, &ann, &doc).await;
    assert_eq!(
        header(&response, CONTENT_DISPOSITION),
        "inline; filename=\"_bersicht.txt\"; filename*=UTF-8''%C3%9Cbersicht.txt"
    );
}

/// Every file under `dir`, recursively.
fn walk(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    for entry in entries {
        let path = entry.unwrap().path();
        if path.is_dir() {
            found.extend(walk(&path));
        } else {
            found.push(path);
        }
    }
    found
}

#[tokio::test]
async fn media_previews_support_ranges() {
    let World { server, ann, home } = world().await;
    let bytes: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
    for (name, content_type) in [
        ("Song.mp3", "audio/mpeg"),
        ("Voice.m4a", "audio/mp4"),
        ("Clip.mov", "video/mp4"),
        ("Clip.webm", "video/webm"),
    ] {
        let doc = upload(&server, &ann, &home, name, &bytes).await;
        let path = format!("/api/docs/{doc}/preview");

        let whole = get(&server, Some(&ann), &path, None).await;
        assert_eq!(whole.status(), StatusCode::OK, "{name}");
        assert_sandboxed(&whole, name);
        assert_eq!(header(&whole, CONTENT_TYPE), content_type, "{name}");
        assert_eq!(header(&whole, ACCEPT_RANGES), "bytes", "{name}");
        assert_eq!(whole.bytes().await.unwrap(), bytes);

        for (range, start, end) in [
            ("bytes=0-99", 0usize, 99usize),
            ("bytes=900-", 900, 999),
            ("bytes=-10", 990, 999),
            ("bytes=500-5000", 500, 999),
        ] {
            let part = get(&server, Some(&ann), &path, Some(range)).await;
            assert_eq!(part.status(), StatusCode::PARTIAL_CONTENT, "{name} {range}");
            assert_sandboxed(&part, name);
            assert_eq!(header(&part, CONTENT_TYPE), content_type);
            assert_eq!(
                header(&part, CONTENT_RANGE),
                format!("bytes {start}-{end}/1000")
            );
            assert_eq!(part.bytes().await.unwrap(), bytes[start..=end]);
        }
        let past = get(&server, Some(&ann), &path, Some("bytes=1000-")).await;
        assert_eq!(past.status(), StatusCode::RANGE_NOT_SATISFIABLE, "{name}");
        assert_eq!(header(&past, CONTENT_RANGE), "bytes */1000");
        // Several ranges: the whole file.
        let several = get(&server, Some(&ann), &path, Some("bytes=0-1,5-6")).await;
        assert_eq!(several.status(), StatusCode::OK);
    }

    // Images and text are always sent whole.
    let png = picture(10, 10, ImageFormat::Png);
    for (name, body) in [
        ("Dot.png", png.as_slice()),
        ("Notes.txt", b"some notes".as_slice()),
    ] {
        let doc = upload(&server, &ann, &home, name, body).await;
        let response = get(
            &server,
            Some(&ann),
            &format!("/api/docs/{doc}/preview"),
            Some("bytes=0-1"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK, "{name}");
        assert!(response.headers().get(CONTENT_RANGE).is_none(), "{name}");
        assert_eq!(response.bytes().await.unwrap(), body);
    }
}

#[tokio::test]
async fn thumbnails_made_once_and_cached() {
    let World { server, ann, home } = world().await;
    let store = server.blobs();

    let wide = picture(600, 400, ImageFormat::Png);
    let doc = upload(&server, &ann, &home, "Wide.png", &wide).await;
    let path = thumb_path(&store, &sha(&wide));
    assert!(!path.exists());
    let response = thumbnail(&server, &ann, &doc).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header(&response, CONTENT_TYPE), "image/png");
    assert_eq!(header(&response, CACHE_CONTROL), "private, max-age=300");
    assert_eq!(header(&response, X_CONTENT_TYPE_OPTIONS), "nosniff");
    assert_eq!(header(&response, CONTENT_SECURITY_POLICY), "sandbox");
    let first = response.bytes().await.unwrap();
    assert_eq!(dimensions(&first), (256, 171));

    // Kept at <data>/thumbs/<xx>/<sha256>.png.
    let hex: String = sha(&wide).iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(
        path,
        server
            .data_dir
            .path()
            .join("thumbs")
            .join(&hex[..2])
            .join(format!("{hex}.png"))
    );
    assert_eq!(std::fs::read(&path).unwrap(), first);

    // Asked again, it is the stored one: not made again.
    let marker = picture(3, 2, ImageFormat::Png);
    std::fs::write(&path, &marker).unwrap();
    let again = thumbnail(&server, &ann, &doc).await;
    assert_eq!(again.status(), StatusCode::OK);
    assert_eq!(again.bytes().await.unwrap(), marker);

    // The same content under another name shares it.
    let twin = upload(&server, &ann, &home, "Twin.png", &wide).await;
    let response = thumbnail(&server, &ann, &twin).await;
    assert_eq!(response.bytes().await.unwrap(), marker);

    // Each kind of image, tall ones and small ones (never enlarged).
    for (name, bytes, expected) in [
        ("Tall.png", picture(100, 1000, ImageFormat::Png), (26, 256)),
        ("Small.png", picture(40, 30, ImageFormat::Png), (40, 30)),
        ("Photo.jpg", jpeg(), (256, 192)),
        ("Photo.jpeg", picture(1, 1, ImageFormat::Png), (1, 1)),
        ("Moving.gif", animated(40, 20), (40, 20)),
        (
            "Lossless.webp",
            picture(512, 256, ImageFormat::WebP),
            (256, 128),
        ),
    ] {
        let doc = upload(&server, &ann, &home, name, &bytes).await;
        let response = thumbnail(&server, &ann, &doc).await;
        assert_eq!(response.status(), StatusCode::OK, "{name}");
        let body = response.bytes().await.unwrap();
        assert_eq!(&body[..8], b"\x89PNG\r\n\x1a\n", "{name}");
        assert_eq!(dimensions(&body), expected, "{name}");
    }

    // Many at once for one new image: each gets it, and one file is kept.
    let fresh = picture(700, 300, ImageFormat::Png);
    let doc = upload(&server, &ann, &home, "Fresh.png", &fresh).await;
    let asks = (0..6).map(|_| thumbnail(&server, &ann, &doc));
    let answers = futures_util::future::join_all(asks).await;
    let mut bodies = Vec::new();
    for response in answers {
        assert_eq!(response.status(), StatusCode::OK);
        bodies.push(response.bytes().await.unwrap());
    }
    assert!(bodies.windows(2).all(|pair| pair[0] == pair[1]));
    assert_eq!(
        std::fs::read(thumb_path(&store, &sha(&fresh))).unwrap(),
        bodies[0]
    );

    // Types with no thumbnail.
    for (name, bytes) in [
        ("Notes.txt", b"words".to_vec()),
        ("Picture.bmp", b"BM not really".to_vec()),
        ("Picture.avif", b"not really".to_vec()),
        ("Song.mp3", b"ID3 not really".to_vec()),
    ] {
        let doc = upload(&server, &ann, &home, name, &bytes).await;
        let (status, error) = refused(thumbnail(&server, &ann, &doc).await).await;
        assert_eq!(
            (status, error.as_str()),
            (StatusCode::UNSUPPORTED_MEDIA_TYPE, "no_thumbnail"),
            "{name}"
        );
    }
    // View is needed.
    let bob = server.user("Bob").await;
    let (status, _) = refused(thumbnail(&server, &bob, &doc).await).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn hostile_images_refused_quickly() {
    let World { server, ann, home } = world().await;
    let mut truncated = jpeg();
    truncated.truncate(truncated.len() * 3 / 5);
    let hostile = [
        // 12,000 × 12,000 RGBA: inside the size limit, 576 MB decoded, about 3.5 MB stored.
        ("Bomb.png", zeros_png(12_000, 12_000)),
        ("Wide.png", zeros_png(50_000, 1)),
        ("Cut.jpg", truncated),
        ("Frames.gif", many_frames_gif(10_000)),
        (
            "Garbage.webp",
            b"RIFF\xff\xff\xff\xffWEBPVP8 garbage".to_vec(),
        ),
    ];
    let mut docs = Vec::new();
    for (name, bytes) in &hostile {
        let doc = upload(&server, &ann, &home, name, bytes).await;
        let started = Instant::now();
        let (status, error) = refused(thumbnail(&server, &ann, &doc).await).await;
        assert_eq!(
            (status, error.as_str()),
            (StatusCode::UNSUPPORTED_MEDIA_TYPE, "no_thumbnail"),
            "{name}"
        );
        assert!(
            started.elapsed() < QUICKLY,
            "{name} took {:?}",
            started.elapsed()
        );
        docs.push(doc);
    }
    // Each for the reason expected, decided before the pixels are decoded.
    let store = server.blobs();
    for ((name, bytes), expected) in hostile.iter().zip([
        "TooLarge",
        "TooLarge",
        "Truncated",
        "TooManyFrames",
        "Decode",
    ]) {
        let error = server::previews::thumbnail(&store, &sha(bytes)).unwrap_err();
        assert!(
            format!("{error:?}").starts_with(expected),
            "{name}: {error:?}"
        );
    }
    // Nothing was kept, not even part of a file.
    let thumbs = server.data_dir.path().join("thumbs");
    let made = walk(&thumbs);
    assert!(made.is_empty(), "{made:?}");
    let tmp = server.data_dir.path().join("blobs").join("tmp");
    let left = walk(&tmp);
    assert!(left.is_empty(), "{left:?}");

    // All at once, twice over, while a good image is asked for: the server keeps serving.
    let good = picture(300, 300, ImageFormat::Png);
    let good_doc = upload(&server, &ann, &home, "Good.png", &good).await;
    let started = Instant::now();
    let hostile_asks = docs
        .iter()
        .chain(docs.iter())
        .map(|doc| thumbnail(&server, &ann, doc));
    let (refusals, good_answer, me) = tokio::join!(
        futures_util::future::join_all(hostile_asks),
        thumbnail(&server, &ann, &good_doc),
        get(&server, Some(&ann), "/api/me", None),
    );
    for response in refusals {
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }
    assert_eq!(good_answer.status(), StatusCode::OK);
    assert_eq!(me.status(), StatusCode::OK);
    assert!(
        started.elapsed() < QUICKLY * 2,
        "took {:?}",
        started.elapsed()
    );
    assert_eq!(walk(&thumbs).len(), 1);
}

#[tokio::test]
async fn thumbnail_erased_with_its_blob() {
    let server = spawn_server().await;
    let admin = server.admin("Purger").await;
    let home = server.home_of(&admin).await.0.to_string();
    let store = server.blobs();

    // Purged from the bin: the content and its thumbnail go.
    let png = picture(300, 200, ImageFormat::Png);
    let doc = upload(&server, &admin, &home, "Gone.png", &png).await;
    assert_eq!(
        thumbnail(&server, &admin, &doc).await.status(),
        StatusCode::OK
    );
    let path = thumb_path(&store, &sha(&png));
    assert!(path.is_file());
    let (status, body) = call(
        &server,
        &admin,
        Method::DELETE,
        &format!("/api/nodes/{doc}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    // In the bin the content stays, and so does its thumbnail.
    assert!(path.is_file());
    for _ in 0..2 {
        let (status, body) = call(
            &server,
            &admin,
            Method::DELETE,
            &format!("/api/bin/{doc}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    }
    assert!(!store.contains(&sha(&png)));
    assert!(!path.exists());

    // Content another file still names keeps its thumbnail.
    let shared = picture(260, 260, ImageFormat::Png);
    let one = upload(&server, &admin, &home, "One.png", &shared).await;
    let _two = upload(&server, &admin, &home, "Two.png", &shared).await;
    assert_eq!(
        thumbnail(&server, &admin, &one).await.status(),
        StatusCode::OK
    );
    call(
        &server,
        &admin,
        Method::DELETE,
        &format!("/api/nodes/{one}"),
        None,
    )
    .await;
    for _ in 0..2 {
        call(
            &server,
            &admin,
            Method::DELETE,
            &format!("/api/bin/{one}"),
            None,
        )
        .await;
    }
    assert!(thumb_path(&store, &sha(&shared)).is_file());

    // Deleted from the quarantine: its content and any thumbnail of it go.
    let fake = FakeIcap::start(local(), Behaviour::default()).unwrap();
    let (status, body) = call(
        &server,
        &admin,
        Method::PUT,
        "/api/settings/scanner",
        Some(json!({ "scanner_url": fake.url() })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let bad = infected_png();
    upload(&server, &admin, &home, "Bad.png", &bad).await;
    wait_scan(&server, &bad, ScanState::Infected).await;
    // A thumbnail left from before (made by hand here: held content never gets one).
    let bad_thumb = thumb_path(&store, &sha(&bad));
    std::fs::create_dir_all(bad_thumb.parent().unwrap()).unwrap();
    std::fs::write(&bad_thumb, picture(2, 2, ImageFormat::Png)).unwrap();
    let hash: String = sha(&bad).iter().map(|b| format!("{b:02x}")).collect();
    let (status, body) = call(
        &server,
        &admin,
        Method::POST,
        &format!("/api/admin/quarantine/{hash}/delete"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!store.contains(&sha(&bad)));
    assert!(!bad_thumb.exists());
}

fn local() -> SocketAddr {
    "127.0.0.1:0".parse().unwrap()
}

/// Waits until the scan state of `bytes` is `state`.
async fn wait_scan(site: &Site, bytes: &[u8], state: ScanState) {
    let deadline = tokio::time::Instant::now() + SCAN_WAIT;
    loop {
        let info = site.records_db().scan_state(&sha(bytes)).await.unwrap();
        if info.as_ref().is_some_and(|info| info.state == state) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the content never became {state:?}: {info:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn held_content_has_no_preview_or_thumbnail() {
    let server = spawn_server().await;
    let admin = server.admin("Scan Admin").await;
    let ann = server.user("Ann").await;
    let home = server.home_of(&ann).await.0.to_string();
    let fake = FakeIcap::start(
        local(),
        Behaviour {
            fail: true,
            ..Behaviour::default()
        },
    )
    .unwrap();
    let (status, body) = call(
        &server,
        &admin,
        Method::PUT,
        "/api/settings/scanner",
        Some(json!({ "scanner_url": fake.url() })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (token, _) = anyone_link(&server, &admin, &ann, &home).await;

    // Held while the scanner fails.
    let waiting = picture(300, 300, ImageFormat::Png);
    let held = upload(&server, &ann, &home, "Waiting.png", &waiting).await;
    wait_scan(&server, &waiting, ScanState::Failed).await;
    fake.set(Behaviour::default());
    // Infected.
    let bad = infected_png();
    let infected = upload(&server, &ann, &home, "Infected.png", &bad).await;
    wait_scan(&server, &bad, ScanState::Infected).await;

    for (doc, scan) in [(&held, "failed"), (&infected, "infected")] {
        for path in [
            format!("/api/docs/{doc}/preview"),
            format!("/api/docs/{doc}/thumbnail"),
        ] {
            let (status, body) = answer(get(&server, Some(&ann), &path, None).await).await;
            assert_eq!(status, StatusCode::LOCKED, "{path}: {body}");
            assert_eq!(body["error"], "held", "{path}");
            assert_eq!(body["scan"], scan, "{path}");
        }
        let path = format!("/api/l/{token}/preview/{doc}");
        let (status, body) = answer(get(&server, None, &path, Some("bytes=0-1")).await).await;
        assert_eq!(status, StatusCode::LOCKED, "link: {body}");
        assert_eq!(body["scan"], scan);
    }
    assert!(walk(&server.data_dir.path().join("thumbs")).is_empty());

    // Released, it has both.
    let hash: String = sha(&waiting).iter().map(|b| format!("{b:02x}")).collect();
    let (status, body) = call(
        &server,
        &admin,
        Method::POST,
        &format!("/api/admin/quarantine/{hash}/release"),
        Some(json!({ "reason": "checked by hand" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(preview(&server, &ann, &held).await.status(), StatusCode::OK);
    let response = thumbnail(&server, &ann, &held).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(dimensions(&response.bytes().await.unwrap()), (256, 256));
}

#[tokio::test]
async fn anyone_link_preview() {
    let server = spawn_server().await;
    let admin = server.admin("Link Admin").await;
    let ann = server.user("Ann").await;
    let home = server.home_of(&ann).await.0.to_string();
    let png = picture(20, 10, ImageFormat::Png);
    let image = upload(&server, &ann, &home, "Shared.png", &png).await;
    let text = upload(
        &server,
        &ann,
        &home,
        "Page.html",
        b"<script>alert(1)</script>",
    )
    .await;
    let audio: Vec<u8> = (0..500u32).map(|i| (i % 7) as u8).collect();
    let song = upload(&server, &ann, &home, "Song.ogg", &audio).await;
    let sheet = upload(&server, &ann, &home, "Sheet.xlsx", b"PK not really").await;
    let (token, link) = anyone_link(&server, &admin, &ann, &home).await;

    let response = get(
        &server,
        None,
        &format!("/api/l/{token}/preview/{image}"),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_sandboxed(&response, "link image");
    assert_eq!(header(&response, CONTENT_TYPE), "image/png");
    assert_eq!(header(&response, REFERRER_POLICY), "no-referrer");
    assert!(
        response
            .headers()
            .get(reqwest::header::SET_COOKIE)
            .is_none()
    );
    assert_eq!(response.bytes().await.unwrap(), png);

    let response = get(
        &server,
        None,
        &format!("/api/l/{token}/preview/{text}"),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_sandboxed(&response, "link text");
    assert_eq!(header(&response, CONTENT_TYPE), "text/plain; charset=utf-8");

    let response = get(
        &server,
        None,
        &format!("/api/l/{token}/preview/{song}"),
        Some("bytes=10-19"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(header(&response, CONTENT_TYPE), "audio/ogg");
    assert_eq!(header(&response, CONTENT_RANGE), "bytes 10-19/500");
    assert_eq!(response.bytes().await.unwrap(), audio[10..20]);

    let (status, error) = refused(
        get(
            &server,
            None,
            &format!("/api/l/{token}/preview/{sheet}"),
            None,
        )
        .await,
    )
    .await;
    assert_eq!(
        (status, error.as_str()),
        (StatusCode::UNSUPPORTED_MEDIA_TYPE, "no_preview")
    );

    // Not below the link, a folder, or a broken token: nothing.
    let other_home = server.home_of(&admin).await.0.to_string();
    let elsewhere = upload(&server, &admin, &other_home, "Else.png", &png).await;
    for path in [
        format!("/api/l/{token}/preview/{elsewhere}"),
        format!("/api/l/{token}/preview/{home}"),
        format!("/api/l/not-a-token/preview/{image}"),
    ] {
        let (status, error) = refused(get(&server, None, &path, None).await).await;
        assert_eq!(
            (status, error.as_str()),
            (StatusCode::NOT_FOUND, "link_not_found"),
            "{path}"
        );
    }

    // Revoked, it shows nothing.
    let (status, _) = call(
        &server,
        &ann,
        Method::DELETE,
        &format!("/api/links/{link}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, error) = refused(
        get(
            &server,
            None,
            &format!("/api/l/{token}/preview/{image}"),
            None,
        )
        .await,
    )
    .await;
    assert_eq!(
        (status, error.as_str()),
        (StatusCode::NOT_FOUND, "link_not_found")
    );
}
