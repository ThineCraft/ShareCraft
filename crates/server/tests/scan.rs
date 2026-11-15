//! Malware scanning over ICAP and quarantine: the ICAP client against the fake scanner
//! (`cargo xtask fake-icap`, started here in-process), the scan worker and its retries, which
//! content is held, every route that hands out bytes refusing held content, and the
//! administrators' quarantine.
//!
//! Content holding the EICAR test string is infected for the fake scanner. The tests never put
//! the string at the start of a file, so a scanner running on the machine itself does not take
//! the test files away; the fake scanner finds it anywhere.

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

use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use collab_core::ids::{DocId, Seq};
use common::{Site, TestClient, TestServer, TestUser, handshake_status, spawn_server, ws_request};
use fake_icap::{Behaviour, DEFAULT_PREVIEW, FakeIcap, THREAT, eicar};
use records::{ScanState, ScannerUrl};
use reqwest::header::RANGE;
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use server::icap::{Client, Verdict};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use tokio::io::{AsyncRead, ReadBuf};
use uuid::Uuid;

/// The longest a test waits for the scanner.
const SCAN_WAIT: Duration = Duration::from_secs(20);

fn local() -> SocketAddr {
    "127.0.0.1:0".parse().unwrap()
}

/// Bytes the fake scanner finds infected: `label`, then the test string, never first.
fn infected(label: &str) -> Vec<u8> {
    let mut bytes = format!("Quarterly report {label}\n").into_bytes();
    bytes.extend_from_slice(&eicar());
    bytes.extend_from_slice(b"\nend of report\n");
    bytes
}

/// A small file the fake scanner finds infected, small enough to fit a preview whole: one byte,
/// then the test string.
fn small_infected() -> Vec<u8> {
    let mut bytes = b"#".to_vec();
    bytes.extend_from_slice(&eicar());
    bytes
}

/// A file the fake scanner finds infected, exactly `size` bytes long.
fn infected_of_size(label: &str, size: usize) -> Vec<u8> {
    let mut bytes = infected(label);
    assert!(bytes.len() <= size);
    bytes.resize(size, b'.');
    bytes
}

/// A fake scanner that asks for a preview as real ones do ([`DEFAULT_PREVIEW`] bytes).
fn previewing() -> Behaviour {
    Behaviour {
        preview: Some(DEFAULT_PREVIEW),
        ..Behaviour::default()
    }
}

fn sha(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn hex(sha: &[u8; 32]) -> String {
    sha.iter().map(|b| format!("{b:02x}")).collect()
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

/// Changes the scanner settings as `admin`; the answer.
async fn scanner(site: &Site, admin: &TestUser, settings: Value) -> Value {
    let (status, body) = call(
        site,
        admin,
        Method::PUT,
        "/api/settings/scanner",
        Some(settings),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
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

/// Uploads `bytes` as a new version of the file `name` in `parent`.
async fn replace(site: &Site, user: &TestUser, parent: &str, name: &str, bytes: &[u8]) {
    let response = site
        .http(Some(user))
        .post(site.url("/api/docs/upload"))
        .query(&[
            ("parent", parent),
            ("name", name),
            ("on_conflict", "replace"),
        ])
        .body(bytes.to_vec())
        .send()
        .await
        .unwrap();
    let (status, body) = answer(response).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

async fn download(site: &Site, user: &TestUser, doc: &str) -> (StatusCode, Value) {
    call(
        site,
        user,
        Method::GET,
        &format!("/api/docs/{doc}/content"),
        None,
    )
    .await
}

/// The scan state of `bytes`, once `done` holds for it.
async fn wait_scan(site: &Site, bytes: &[u8], done: impl Fn(ScanState) -> bool) -> ScanState {
    let sha = sha(bytes);
    let deadline = tokio::time::Instant::now() + SCAN_WAIT;
    loop {
        let info = site.records_db().scan_state(&sha).await.unwrap();
        if let Some(info) = &info
            && done(info.state)
        {
            return info.state;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the content's scan never finished: {info:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn state_of(site: &Site, bytes: &[u8]) -> Option<records::ScanInfo> {
    site.records_db().scan_state(&sha(bytes)).await.unwrap()
}

/// The listing entry of `doc` in `parent`.
async fn listed(site: &Site, user: &TestUser, parent: &str, doc: &str) -> Value {
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
        .find(|item| item["id"] == doc)
        .cloned()
        .unwrap()
}

/// A server with a fake scanner it is set to use, an administrator and a person with a folder.
struct Scanned {
    server: TestServer,
    fake: FakeIcap,
    admin: TestUser,
    ann: TestUser,
    home: String,
}

async fn scanned(behaviour: Behaviour) -> Scanned {
    let server = spawn_server().await;
    let fake = FakeIcap::start(local(), behaviour).unwrap();
    let admin = server.admin("Scan Admin").await;
    let answer = scanner(&server, &admin, json!({ "scanner_url": fake.url() })).await;
    assert_eq!(answer["ok"], true, "{answer}");
    let ann = server.user("Ann").await;
    let home = server.home_of(&ann).await.0.to_string();
    Scanned {
        server,
        fake,
        admin,
        ann,
        home,
    }
}

/// A port nothing listens on.
fn free_port() -> u16 {
    std::net::TcpListener::bind(local())
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Starts a fake scanner on `port`, trying again while the port is still being released.
fn fake_on(port: u16, behaviour: &Behaviour) -> FakeIcap {
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    for _ in 0..200 {
        if let Ok(fake) = FakeIcap::start(addr, behaviour.clone()) {
            return fake;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("port {port} never came free");
}

fn client(fake: &FakeIcap) -> Client {
    Client::new(
        ScannerUrl::parse(&fake.url()).unwrap(),
        Duration::from_secs(10),
    )
}

#[tokio::test]
async fn icap_clean_and_infected_verdicts() {
    let fake = FakeIcap::start(local(), Behaviour::default()).unwrap();
    let client = client(&fake);
    let options = client.options().await.unwrap();
    assert_eq!(options.istag.as_deref(), Some("\"fake-1\""));
    assert_eq!(options.max_connections, Some(4));
    assert_eq!(options.preview, None);

    let clean = b"nothing to see here".as_slice();
    let verdict = client
        .scan(clean, clean.len() as u64, "notes.txt")
        .await
        .unwrap();
    assert_eq!(verdict, Verdict::Clean);

    let bad = infected("one");
    let verdict = client
        .scan(bad.as_slice(), bad.len() as u64, "Report \"Q3\"\r\n.xlsx")
        .await
        .unwrap();
    assert_eq!(verdict, Verdict::Infected(THREAT.to_owned()));

    let verdict = client.scan(&b""[..], 0, "empty.bin").await.unwrap();
    assert_eq!(verdict, Verdict::Clean);
    assert_eq!(fake.seen().scans, 3);
    assert_eq!(fake.seen().infected, 1);

    // With a preview, as real scanners ask: a small infected file, and one exactly the
    // preview's size, fit the preview whole (`ieof`) and are answered without a `100 Continue`.
    let with_preview = FakeIcap::start(local(), previewing()).unwrap();
    let previewed = crate::client(&with_preview);
    let size = DEFAULT_PREVIEW;
    for (bad, name) in [
        (small_infected(), "small.txt"),
        (infected_of_size("exact", size), "exact.bin"),
    ] {
        let verdict = previewed
            .scan(bad.as_slice(), bad.len() as u64, name)
            .await
            .unwrap();
        assert_eq!(verdict, Verdict::Infected(THREAT.to_owned()), "{name}");
    }
    let clean = vec![b'c'; size];
    let verdict = previewed
        .scan(clean.as_slice(), clean.len() as u64, "clean.bin")
        .await
        .unwrap();
    assert_eq!(verdict, Verdict::Clean);
    let seen = with_preview.seen();
    assert_eq!(
        (seen.previews, seen.ieofs, seen.continues, seen.infected),
        (3, 3, 0, 2)
    );

    // Not an ICAP address.
    for text in [
        "http://127.0.0.1/scan",
        "icap://user:secret@127.0.0.1/scan",
        "icap:///scan",
        "icap://127.0.0.1:0/scan",
        "icap://127.0.0.1:99999/scan",
        "icap://127.0.0.1",
    ] {
        assert!(ScannerUrl::parse(text).is_none(), "{text}");
    }
    let parsed = ScannerUrl::parse("icap://scanner.example/avscan").unwrap();
    assert_eq!(parsed.to_string(), "icap://scanner.example:1344/avscan");
    // The fake scanner refuses bare LF line ends; none were sent.
    assert_eq!(fake.seen().malformed, 0);
}

/// Listens for one connection, keeps every byte the client sends until it has been quiet for
/// half a second, then answers `answer` and hands the bytes back.
fn capture(answer: &'static [u8]) -> (SocketAddr, std::sync::mpsc::Receiver<Vec<u8>>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind(local()).unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(n) => bytes.extend_from_slice(&buffer[..n]),
            }
        }
        let _ = stream.write_all(answer);
        let _ = tx.send(bytes);
    });
    (addr, rx)
}

/// Whether every line feed in `bytes` follows a carriage return.
fn crlf_only(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .enumerate()
        .all(|(at, b)| *b != b'\n' || (at > 0 && bytes[at - 1] == b'\r'))
}

#[tokio::test]
async fn icap_requests_use_crlf_only() {
    let client_at = |addr: SocketAddr| {
        Client::new(
            ScannerUrl::parse(&format!("icap://{addr}/scan")).unwrap(),
            Duration::from_secs(10),
        )
    };
    let options_answer: &[u8] =
        b"ICAP/1.0 200 OK\r\nISTag: \"x\"\r\nPreview: 4\r\nEncapsulated: null-body=0\r\n\r\n";
    let clean: &[u8] = b"ICAP/1.0 204 No Content\r\nEncapsulated: null-body=0\r\n\r\n";

    let (addr, sent) = capture(options_answer);
    let options = client_at(addr).options().await.unwrap();
    assert_eq!(options.preview, Some(4));
    let bytes = sent.recv().unwrap();
    assert!(bytes.starts_with(b"OPTIONS icap://"), "{bytes:?}");
    assert!(bytes.ends_with(b"\r\n\r\n"), "{bytes:?}");
    assert!(crlf_only(&bytes), "{}", String::from_utf8_lossy(&bytes));

    // Without a preview, with one that the file overflows, and with one that holds it whole.
    for (preview, body, ending) in [
        (None, b"abcdefghij".as_slice(), b"\r\n0\r\n\r\n".as_slice()),
        (
            Some(4),
            b"abcdefghij".as_slice(),
            b"\r\n0\r\n\r\n".as_slice(),
        ),
        (
            Some(4),
            b"abc".as_slice(),
            b"\r\n0; ieof\r\n\r\n".as_slice(),
        ),
    ] {
        let (addr, sent) = capture(clean);
        let options = server::icap::Options {
            preview,
            ..server::icap::Options::default()
        };
        let verdict = client_at(addr)
            .scan_with(&options, body, body.len() as u64, "a b.txt")
            .await
            .unwrap();
        assert_eq!(verdict, Verdict::Clean);
        let bytes = sent.recv().unwrap();
        let text = String::from_utf8_lossy(&bytes).into_owned();
        assert!(bytes.starts_with(b"RESPMOD icap://"), "{text}");
        assert!(crlf_only(&bytes), "{text}");
        assert!(bytes.ends_with(ending), "{text}");
        assert!(
            text.contains("\r\n\r\nGET /a%20b.txt HTTP/1.1\r\n"),
            "{text}"
        );
    }
}

/// The infected answers real scanners give (c-icap with ClamAV: `200` with `X-Infection-Found`
/// and an encapsulated HTTP response, with or without a body; or `403` with `X-Virus-ID`),
/// right after a preview that held the whole file, right after a preview without a
/// `100 Continue`, and after the whole file.
#[tokio::test]
async fn icap_infected_answers_as_real_scanners_send_them() {
    const WITH_BODY: &[u8] = b"ICAP/1.0 200 OK\r\nISTag: \"x\"\r\n\
        X-Infection-Found: Type=0; Resolution=2; Threat=Eicar-Test-Signature;\r\n\
        Encapsulated: res-hdr=0, res-body=71\r\n\r\n\
        HTTP/1.1 403 Forbidden\r\nContent-Type: text/html\r\nContent-Length: 11\r\n\r\n\
        b\r\nVirus found\r\n0\r\n\r\n";
    const NULL_BODY: &[u8] = b"ICAP/1.0 200 OK\r\nISTag: \"x\"\r\n\
        X-Infection-Found: Type=0; Resolution=2; Threat=Eicar-Test-Signature;\r\n\
        Encapsulated: res-hdr=0, null-body=46\r\n\r\n\
        HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n";
    const VIRUS_ID: &[u8] = b"ICAP/1.0 403 Forbidden\r\nX-Virus-ID: Eicar-Test-Signature\r\n\
        Encapsulated: null-body=0\r\n\r\n";
    let small = small_infected();
    let large = infected_of_size("large", 4096);
    for answer in [WITH_BODY, NULL_BODY, VIRUS_ID] {
        for (preview, body) in [
            (Some(1024), small.as_slice()),
            (Some(small.len() as u64), small.as_slice()),
            (Some(1024), large.as_slice()),
            (None, small.as_slice()),
        ] {
            let (addr, sent) = capture(answer);
            let client = Client::new(
                ScannerUrl::parse(&format!("icap://{addr}/scan")).unwrap(),
                Duration::from_secs(10),
            );
            let options = server::icap::Options {
                preview,
                ..server::icap::Options::default()
            };
            let verdict = client
                .scan_with(&options, body, body.len() as u64, "file.bin")
                .await;
            let label = format!(
                "{preview:?}, {} bytes, {}",
                body.len(),
                String::from_utf8_lossy(&answer[..20])
            );
            assert_eq!(
                verdict.unwrap(),
                Verdict::Infected(THREAT.to_owned()),
                "{label}"
            );
            let request = sent.recv().unwrap();
            let ieof = preview.is_some_and(|n| body.len() as u64 <= n);
            assert_eq!(request.ends_with(b"\r\n0; ieof\r\n\r\n"), ieof, "{label}");
        }
    }
}

/// A scanner that blocks by replacing the file with an HTTP `403` page and no infection header
/// blocks it: the file is infected, "blocked by the scanner"; a `200` giving back an HTTP `200`
/// is clean. With and without a preview, through the client and through an upload.
#[tokio::test]
async fn icap_block_pages_without_an_infection_header_are_infected() {
    for preview in [None, Some(DEFAULT_PREVIEW)] {
        let fake = FakeIcap::start(
            local(),
            Behaviour {
                preview,
                block_page: true,
                replace_clean: true,
                ..Behaviour::default()
            },
        )
        .unwrap();
        let client = client(&fake);
        for bad in [small_infected(), infected_of_size("large", 4096)] {
            let verdict = client
                .scan(bad.as_slice(), bad.len() as u64, "invoice.pdf")
                .await
                .unwrap();
            assert_eq!(
                verdict,
                Verdict::Infected(server::icap::BLOCKED.to_owned()),
                "{preview:?}, {} bytes",
                bad.len()
            );
        }
        let clean = vec![b'c'; 3000];
        let verdict = client
            .scan(clean.as_slice(), clean.len() as u64, "clean.bin")
            .await
            .unwrap();
        assert_eq!(verdict, Verdict::Clean, "{preview:?}");
    }

    let Scanned {
        server,
        fake: _fake,
        ann,
        home,
        ..
    } = scanned(Behaviour {
        block_page: true,
        replace_clean: true,
        ..previewing()
    })
    .await;
    let bad = infected("block page");
    let doc = upload(&server, &ann, &home, "Blocked.bin", &bad).await;
    let state = wait_scan(&server, &bad, |s| {
        matches!(
            s,
            ScanState::Infected | ScanState::Failed | ScanState::Clean
        )
    })
    .await;
    assert_eq!(state, ScanState::Infected);
    let info = state_of(&server, &bad).await.unwrap();
    assert_eq!(info.detail.as_deref(), Some(server::icap::BLOCKED));
    let (status, _) = download(&server, &ann, &doc).await;
    assert_eq!(status, StatusCode::LOCKED);
}

/// A small infected file, whose whole content fits the scanner's preview, and one exactly the
/// preview's size, uploaded as anyone would: both found infected and quarantined, with the
/// threat named.
#[tokio::test]
async fn infected_files_within_the_preview_are_quarantined() {
    let Scanned {
        server,
        fake,
        ann,
        home,
        ..
    } = scanned(previewing()).await;
    let small = small_infected();
    let exact = infected_of_size("exact", DEFAULT_PREVIEW);
    let mut docs = Vec::new();
    for (name, bytes) in [("Small.txt", &small), ("Exact.bin", &exact)] {
        let doc = upload(&server, &ann, &home, name, bytes).await;
        let state = wait_scan(&server, bytes, |s| {
            matches!(s, ScanState::Infected | ScanState::Failed)
        })
        .await;
        let info = state_of(&server, bytes).await.unwrap();
        assert_eq!(state, ScanState::Infected, "{name}: {info:?}");
        assert_eq!(info.detail.as_deref(), Some(THREAT), "{name}");
        docs.push(doc);
    }
    for doc in &docs {
        let entry = listed(&server, &ann, &home, doc).await;
        assert_eq!(entry["scan"], "quarantined", "{entry}");
        assert_eq!(entry["threat"], THREAT, "{entry}");
    }
    let seen = fake.seen();
    assert_eq!((seen.ieofs, seen.continues, seen.infected), (2, 0, 2));
}

#[tokio::test]
async fn icap_deadline_covers_the_whole_exchange() {
    // An answer dribbled a byte every half second would take most of a minute.
    let fake = FakeIcap::start(
        local(),
        Behaviour {
            dribble: Some(Duration::from_millis(500)),
            ..Behaviour::default()
        },
    )
    .unwrap();
    let client = Client::new(
        ScannerUrl::parse(&fake.url()).unwrap(),
        Duration::from_secs(2),
    );
    let started = std::time::Instant::now();
    let failed = client.scan(&b"slowly"[..], 6, "slow.bin").await;
    assert!(
        matches!(failed, Err(server::icap::IcapError::TimedOut)),
        "{failed:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );

    // A head that never ends is refused once it passes its limit.
    fake.set(Behaviour {
        endless_head: true,
        ..Behaviour::default()
    });
    let client = Client::new(
        ScannerUrl::parse(&fake.url()).unwrap(),
        Duration::from_secs(30),
    );
    let started = std::time::Instant::now();
    let failed = client.scan(&b"endless"[..], 7, "endless.bin").await;
    assert!(
        matches!(failed, Err(server::icap::IcapError::Malformed(_))),
        "{failed:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
}

/// Reads `total` bytes of a pattern without ever holding them, noting the most read beyond
/// what the fake scanner has received.
struct Generator {
    left: u64,
    produced: u64,
    fake: Arc<FakeIcap>,
    most_ahead: Arc<AtomicU64>,
}

impl AsyncRead for Generator {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let ahead = self.produced.saturating_sub(self.fake.seen().received);
        self.most_ahead.fetch_max(ahead, Ordering::SeqCst);
        let n = usize::try_from(self.left)
            .unwrap_or(usize::MAX)
            .min(buf.remaining())
            .min(256 * 1024);
        buf.put_slice(&vec![b'a'; n]);
        self.left -= n as u64;
        self.produced += n as u64;
        Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn icap_streams_without_holding_the_file() {
    const SIZE: u64 = 200 << 20;
    let fake = Arc::new(FakeIcap::start(local(), Behaviour::default()).unwrap());
    let client = client(&fake);
    let most_ahead = Arc::new(AtomicU64::new(0));
    let reader = Generator {
        left: SIZE,
        produced: 0,
        fake: Arc::clone(&fake),
        most_ahead: Arc::clone(&most_ahead),
    };
    let verdict = client.scan(reader, SIZE, "big.bin").await.unwrap();
    assert_eq!(verdict, Verdict::Clean);
    assert_eq!(fake.seen().received, SIZE);
    let ahead = most_ahead.load(Ordering::SeqCst);
    assert!(ahead < 8 << 20, "{ahead} bytes were held at once");
}

#[tokio::test]
async fn icap_preview_and_continue() {
    let fake = FakeIcap::start(
        local(),
        Behaviour {
            preview: Some(1024),
            ..Behaviour::default()
        },
    )
    .unwrap();
    let client = client(&fake);
    assert_eq!(client.options().await.unwrap().preview, Some(1024));

    // Longer than the preview: the scanner asks for the rest.
    let long = vec![b'z'; 10 * 1024];
    let verdict = client
        .scan(long.as_slice(), long.len() as u64, "long.bin")
        .await
        .unwrap();
    assert_eq!(verdict, Verdict::Clean);
    let seen = fake.seen();
    assert_eq!((seen.previews, seen.continues, seen.ieofs), (1, 1, 0));
    assert_eq!(seen.received, long.len() as u64);

    // The whole file fits in the preview: `ieof`, no continue.
    let short = vec![b'y'; 500];
    let verdict = client
        .scan(short.as_slice(), short.len() as u64, "short.bin")
        .await
        .unwrap();
    assert_eq!(verdict, Verdict::Clean);
    let seen = fake.seen();
    assert_eq!((seen.previews, seen.continues, seen.ieofs), (2, 1, 1));

    // Found in the preview: answered at once.
    let mut early = infected("early");
    early.resize(10 * 1024, b'.');
    let verdict = client
        .scan(early.as_slice(), early.len() as u64, "early.bin")
        .await
        .unwrap();
    assert_eq!(verdict, Verdict::Infected(THREAT.to_owned()));
    assert_eq!(fake.seen().continues, 1);

    // Found after the preview.
    let mut late = vec![b'.'; 5000];
    late.extend_from_slice(&infected("late"));
    let verdict = client
        .scan(late.as_slice(), late.len() as u64, "late.bin")
        .await
        .unwrap();
    assert_eq!(verdict, Verdict::Infected(THREAT.to_owned()));
    assert_eq!(fake.seen().continues, 2);
}

/// A request's status and answer; `423 held` expected of every one.
async fn expect_held(name: &str, request: reqwest::RequestBuilder, scan: &str) {
    let (status, body) = answer(request.send().await.unwrap()).await;
    assert_eq!(status, StatusCode::LOCKED, "{name}: {body}");
    assert_eq!(body["error"], "held", "{name}: {body}");
    assert_eq!(body["scan"], scan, "{name}: {body}");
}

#[tokio::test]
async fn held_content_is_never_served() {
    let Scanned {
        server,
        fake,
        admin,
        ann,
        home,
    } = scanned(Behaviour::default()).await;
    let (status, _) = call(
        &server,
        &admin,
        Method::PUT,
        "/api/settings/sharing",
        Some(json!({ "anyone_links": true })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Infected, opened in an editor.
    let bad = infected("table");
    let quarantined = upload(&server, &ann, &home, "Infected.xlsx", &bad).await;
    wait_scan(&server, &bad, |s| s == ScanState::Infected).await;
    // The same content under a name that has a preview and a thumbnail.
    let quarantined_png = upload(&server, &ann, &home, "Infected.png", &bad).await;
    // The same content as a template of a team site Ann owns.
    let (status, body) = call(
        &server,
        &admin,
        Method::POST,
        "/api/sites",
        Some(json!({ "name": "Team" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let team = body["id"].as_str().unwrap().to_owned();
    let (status, body) = call(
        &server,
        &admin,
        Method::PUT,
        &format!("/api/nodes/{team}/access"),
        Some(json!({ "email": ann.email, "role": "owner" })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, body) = call(
        &server,
        &ann,
        Method::POST,
        &format!("/api/sites/{team}/libraries"),
        Some(json!({ "name": "Templates", "kind": "template" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let templates = body["id"].as_str().unwrap().to_owned();
    let quarantined_template = upload(&server, &ann, &templates, "Infected.xlsx", &bad).await;

    // Held: the scanner fails, so it waits.
    fake.set(Behaviour {
        fail: true,
        ..Behaviour::default()
    });
    let waiting = b"held until the scanner answers".to_vec();
    let held = upload(&server, &ann, &home, "Held.xlsx", &waiting).await;
    wait_scan(&server, &waiting, |s| s == ScanState::Failed).await;
    let held_png = upload(&server, &ann, &home, "Held.png", &waiting).await;
    let held_template = upload(&server, &ann, &templates, "Held.xlsx", &waiting).await;
    fake.set(Behaviour::default());

    // An older version infected, the current one clean.
    let versions = upload(&server, &ann, &home, "Versions.bin", &bad).await;
    let (_, opened) = call(
        &server,
        &ann,
        Method::GET,
        &format!("/api/docs/{versions}"),
        None,
    )
    .await;
    let old = opened["revision"].as_str().unwrap().to_owned();
    let fixed = b"the fixed version".to_vec();
    replace(&server, &ann, &home, "Versions.bin", &fixed).await;
    wait_scan(&server, &fixed, |s| s == ScanState::Clean).await;
    let (status, _) = download(&server, &ann, &versions).await;
    assert_eq!(status, StatusCode::OK);

    // A text document saved with the test string in it, quarantined afterwards.
    let (status, body) = call(
        &server,
        &ann,
        Method::POST,
        "/api/docs",
        Some(json!({ "parent": home, "name": "Note.txt", "editor": "text" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let note = body["id"].as_str().unwrap().to_owned();
    let text = String::from_utf8(infected("note")).unwrap();
    let mut writer =
        TestClient::connect_as(&server, DocId(Uuid::parse_str(&note).unwrap()), &ann).await;
    writer.type_at(0, &text).await;
    writer
        .pump_until(|c| c.pending() == 0 && c.seq() == Seq(1))
        .await;
    writer.wait_saved(Seq(1)).await;
    writer.close().await;
    wait_scan(&server, text.as_bytes(), |s| s == ScanState::Infected).await;

    let (_, made) = call(
        &server,
        &ann,
        Method::POST,
        &format!("/api/nodes/{home}/links"),
        Some(json!({ "kind": "anyone", "role": "viewer", "expires_at": (OffsetDateTime::now_utc() + time::Duration::days(3)).format(&time::format_description::well_known::Rfc3339).unwrap() })),
    )
    .await;
    let token = made["url"]
        .as_str()
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap()
        .to_owned();

    let http = server.http(Some(&ann));
    let anon = server.http(None);
    let url = |path: String| server.url(&path);
    let rows: Vec<(&str, reqwest::RequestBuilder, &str)> = vec![
        (
            "download",
            http.get(url(format!("/api/docs/{quarantined}/content"))),
            "infected",
        ),
        (
            "ranged download",
            http.get(url(format!("/api/docs/{quarantined}/content")))
                .header(RANGE, "bytes=0-9"),
            "infected",
        ),
        (
            "held download",
            http.get(url(format!("/api/docs/{held}/content"))),
            "failed",
        ),
        (
            "held ranged download",
            http.get(url(format!("/api/docs/{held}/content")))
                .header(RANGE, "bytes=-5"),
            "failed",
        ),
        (
            "older version",
            http.get(url(format!("/api/docs/{versions}/content")))
                .query(&[("revision", old.as_str())]),
            "infected",
        ),
        (
            "anyone link download",
            anon.get(url(format!("/api/l/{token}/content/{quarantined}"))),
            "infected",
        ),
        (
            "anyone link held download",
            anon.get(url(format!("/api/l/{token}/content/{held}")))
                .header(RANGE, "bytes=0-1"),
            "failed",
        ),
        (
            "anyone link text",
            anon.get(url(format!("/api/l/{token}/text/{note}"))),
            "infected",
        ),
        (
            "text document download",
            http.get(url(format!("/api/docs/{note}/content"))),
            "infected",
        ),
        (
            "preview",
            http.get(url(format!("/api/docs/{quarantined_png}/preview"))),
            "infected",
        ),
        (
            "held preview",
            http.get(url(format!("/api/docs/{held_png}/preview"))),
            "failed",
        ),
        (
            "text document preview",
            http.get(url(format!("/api/docs/{note}/preview"))),
            "infected",
        ),
        (
            "thumbnail",
            http.get(url(format!("/api/docs/{quarantined_png}/thumbnail"))),
            "infected",
        ),
        (
            "held thumbnail",
            http.get(url(format!("/api/docs/{held_png}/thumbnail"))),
            "failed",
        ),
        (
            "anyone link preview",
            anon.get(url(format!("/api/l/{token}/preview/{quarantined_png}"))),
            "infected",
        ),
        (
            "anyone link held ranged preview",
            anon.get(url(format!("/api/l/{token}/preview/{held_png}")))
                .header(RANGE, "bytes=0-1"),
            "failed",
        ),
        (
            "copy",
            http.post(url(format!("/api/nodes/{quarantined}/copy")))
                .json(&json!({ "parent": home })),
            "infected",
        ),
        (
            "held copy",
            http.post(url(format!("/api/nodes/{held}/copy")))
                .json(&json!({ "parent": home })),
            "failed",
        ),
        (
            "text document copy",
            http.post(url(format!("/api/nodes/{note}/copy")))
                .json(&json!({ "parent": home })),
            "infected",
        ),
        (
            "new from template",
            http.post(url(format!("/api/libraries/{templates}/from-template")))
                .json(&json!({ "template": quarantined_template })),
            "infected",
        ),
        (
            "held new from template",
            http.post(url(format!("/api/libraries/{templates}/from-template")))
                .json(&json!({ "template": held_template })),
            "failed",
        ),
    ];
    for (name, request, scan) in rows {
        expect_held(name, request, scan).await;
    }

    // Opening in an editor, and the text room loading the revision.
    for (name, doc) in [
        ("editor", &quarantined),
        ("held editor", &held),
        ("text room", &note),
    ] {
        let status = handshake_status(ws_request(
            server.addr,
            &format!("/ws/docs/{doc}"),
            Some(&ann.cookie),
        ))
        .await;
        assert_eq!(status, 423, "{name}");
    }

    // A bulk copy refuses each held item.
    let (status, results) = call(
        &server,
        &ann,
        Method::POST,
        "/api/bulk",
        Some(json!({ "op": "copy", "items": [quarantined, held, versions], "parent": home })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{results}");
    assert_eq!(results[0]["error"], "held", "{results}");
    assert_eq!(results[0]["scan"], "infected", "{results}");
    assert_eq!(results[1]["error"], "held", "{results}");
    assert_eq!(results[2]["ok"], true, "{results}");

    // Listing, opening the details, renaming and sharing still work, and show the mark.
    let entry = listed(&server, &ann, &home, &quarantined).await;
    assert_eq!(entry["scan"], "quarantined", "{entry}");
    assert_eq!(entry["threat"], THREAT, "{entry}");
    let entry = listed(&server, &ann, &home, &held).await;
    assert_eq!(entry["scan"], "held", "{entry}");
    assert!(entry.get("threat").is_none(), "{entry}");
    let entry = listed(&server, &ann, &home, &versions).await;
    assert!(entry.get("scan").is_none(), "{entry}");
    let (status, detail) = call(
        &server,
        &ann,
        Method::GET,
        &format!("/api/docs/{quarantined}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["scan"], "quarantined");
    assert_eq!(detail["threat"], THREAT);
    let (status, body) = call(
        &server,
        &ann,
        Method::PATCH,
        &format!("/api/nodes/{held}"),
        Some(json!({ "name": "Held renamed.xlsx" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = call(
        &server,
        &ann,
        Method::DELETE,
        &format!("/api/nodes/{held}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn editor_saves_are_not_held_but_quarantined_when_infected() {
    let Scanned {
        server,
        fake,
        ann,
        home,
        ..
    } = scanned(Behaviour {
        slow: Some(Duration::from_secs(3)),
        ..Behaviour::default()
    })
    .await;
    let (status, body) = call(
        &server,
        &ann,
        Method::POST,
        "/api/docs",
        Some(json!({ "parent": home, "name": "Draft.txt", "editor": "text" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let doc = body["id"].as_str().unwrap().to_owned();
    let text = String::from_utf8(infected("draft")).unwrap();
    let mut writer =
        TestClient::connect_as(&server, DocId(Uuid::parse_str(&doc).unwrap()), &ann).await;
    writer.type_at(0, &text).await;
    writer
        .pump_until(|c| c.pending() == 0 && c.seq() == Seq(1))
        .await;
    writer.wait_saved(Seq(1)).await;

    // Saved from an editor: waiting for the scanner, and still available.
    let info = state_of(&server, text.as_bytes()).await.unwrap();
    assert_eq!(info.state, ScanState::Pending);
    assert!(!info.hold);
    let (status, _) = download(&server, &ann, &doc).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        listed(&server, &ann, &home, &doc)
            .await
            .get("scan")
            .is_none()
    );

    // An upload of other bytes meanwhile is held until the scanner answers.
    let uploaded = b"an upload waits".to_vec();
    let file = upload(&server, &ann, &home, "Upload.bin", &uploaded).await;
    let (status, body) = download(&server, &ann, &file).await;
    assert_eq!(status, StatusCode::LOCKED, "{body}");
    assert_eq!(body["scan"], "pending");
    assert!(state_of(&server, &uploaded).await.unwrap().hold);

    // The scanner finds it: quarantined.
    wait_scan(&server, text.as_bytes(), |s| s == ScanState::Infected).await;
    let (status, body) = download(&server, &ann, &doc).await;
    assert_eq!(status, StatusCode::LOCKED, "{body}");
    assert_eq!(body["scan"], "infected");
    let entry = listed(&server, &ann, &home, &doc).await;
    assert_eq!(entry["scan"], "quarantined");
    assert_eq!(entry["threat"], THREAT);
    wait_scan(&server, &uploaded, |s| s == ScanState::Clean).await;
    let (status, _) = download(&server, &ann, &file).await;
    assert_eq!(status, StatusCode::OK);
    assert!(fake.seen().scans >= 2);
}

/// The quarantine as an administrator sees it: each content's hash and state.
async fn quarantined(site: &Site, admin: &TestUser) -> Vec<(String, String)> {
    let (status, body) = call(site, admin, Method::GET, "/api/admin/quarantine", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body.as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            (
                entry["sha256"].as_str().unwrap().to_owned(),
                entry["scan"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

/// While the scanner is down, a failed scan holds only what an upload named: a document saved
/// from an editor and old content queued by "Scan existing files" stay available (and out of
/// the quarantine); an upload is held and listed. Uploads are scanned before the backlog.
#[tokio::test]
async fn scanner_outage_holds_only_uploads() {
    let server = spawn_server().await;
    let admin = server.admin("Scan Admin").await;
    let ann = server.user("Ann").await;
    let home = server.home_of(&ann).await.0.to_string();
    let old = b"stored before the scanner was set".to_vec();
    let old_doc = upload(&server, &ann, &home, "Old.bin", &old).await;

    // A scanner that is not there.
    let url = format!("icap://127.0.0.1:{}/scan", free_port());
    scanner(&server, &admin, json!({ "scanner_url": url })).await;

    // Saved from an editor: the scan fails, the document stays available.
    let (status, body) = call(
        &server,
        &ann,
        Method::POST,
        "/api/docs",
        Some(json!({ "parent": home, "name": "Draft.txt", "editor": "text" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let draft = body["id"].as_str().unwrap().to_owned();
    let text = "written while the scanner is down";
    let mut writer =
        TestClient::connect_as(&server, DocId(Uuid::parse_str(&draft).unwrap()), &ann).await;
    writer.type_at(0, text).await;
    writer
        .pump_until(|c| c.pending() == 0 && c.seq() == Seq(1))
        .await;
    writer.wait_saved(Seq(1)).await;
    wait_scan(&server, text.as_bytes(), |s| s == ScanState::Failed).await;
    assert!(!state_of(&server, text.as_bytes()).await.unwrap().hold);
    let (status, body) = download(&server, &ann, &draft).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        listed(&server, &ann, &home, &draft)
            .await
            .get("scan")
            .is_none()
    );
    let status = handshake_status(ws_request(
        server.addr,
        &format!("/ws/docs/{draft}"),
        Some(&ann.cookie),
    ))
    .await;
    assert_eq!(status, 101, "the editor opens it again");

    // Queued by "Scan existing files": the scan fails, the old content stays available.
    let (status, body) = call(
        &server,
        &admin,
        Method::POST,
        "/api/admin/scan-existing",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    wait_scan(&server, &old, |s| s == ScanState::Failed).await;
    let (status, body) = download(&server, &ann, &old_doc).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // An upload is held when its scan fails, and listed in the quarantine.
    let uploaded = b"uploaded while the scanner is down".to_vec();
    let file = upload(&server, &ann, &home, "Upload.bin", &uploaded).await;
    wait_scan(&server, &uploaded, |s| s == ScanState::Failed).await;
    let (status, body) = download(&server, &ann, &file).await;
    assert_eq!(status, StatusCode::LOCKED, "{body}");
    assert_eq!(body["scan"], "failed");
    let listed_now = quarantined(&server, &admin).await;
    assert_eq!(
        listed_now,
        vec![(hex(&sha(&uploaded)), "failed".to_owned())],
        "only the upload is held"
    );

    // When all three are due again, the upload comes first although it failed last.
    let later = OffsetDateTime::now_utc() + time::Duration::hours(2);
    let due = server.records_db().due_scans(later, 3).await.unwrap();
    assert!(due.len() >= 3, "{due:?}");
    assert_eq!(due[0].blob.sha256, sha(&uploaded), "{due:?}");
    let first = server.records_db().due_scans(later, 1).await.unwrap();
    assert_eq!(first[0].blob.sha256, sha(&uploaded), "{first:?}");
}

/// Empties the content rows but those of `kept`, flags them for a rebuild, and rebuilds them
/// from Git as the next start would.
async fn rebuild_from_git(site: &Site, kept: &[u8; 32]) {
    let db = site.records_db();
    sqlx::query("delete from blob_refs")
        .execute(db.test_pool())
        .await
        .unwrap();
    sqlx::query("delete from blobs where sha256 <> $1")
        .bind(hex(kept))
        .execute(db.test_pool())
        .await
        .unwrap();
    db.set_blob_refs_flag(records::BLOB_REFS_DIRTY)
        .await
        .unwrap();
    let state = site.app_state().await;
    let report = server::reconcile_blobs(&state, OffsetDateTime::now_utc())
        .await
        .unwrap();
    assert!(report.rebuilt, "{report:?}");
}

/// Content rows rebuilt from Git (a lost database, or one restored from an older backup) come
/// back held until scanned when a scanner is configured, and unscanned when none is; a row
/// that still exists keeps its state.
#[tokio::test]
async fn content_rebuilt_from_git_is_held_until_scanned() {
    let Scanned {
        server,
        fake,
        admin,
        ann,
        home,
    } = scanned(Behaviour::default()).await;
    let state = server.app_state().await;
    let good = b"content whose row is lost".to_vec();
    let bad = infected("restored");
    let good_doc = upload(&server, &ann, &home, "Good.bin", &good).await;
    let bad_doc = upload(&server, &ann, &home, "Bad.bin", &bad).await;
    wait_scan(&server, &good, |s| s == ScanState::Clean).await;
    wait_scan(&server, &bad, |s| s == ScanState::Infected).await;

    // The scanner is down while the rows are rebuilt.
    let down = format!("icap://127.0.0.1:{}/scan", free_port());
    scanner(&server, &admin, json!({ "scanner_url": down })).await;
    rebuild_from_git(&server, &sha(&bad)).await;
    let info = state_of(&server, &good).await.unwrap();
    assert!(info.hold, "{info:?}");
    assert!(
        matches!(info.state, ScanState::Pending | ScanState::Failed),
        "{info:?}"
    );
    let (status, body) = download(&server, &ann, &good_doc).await;
    assert_eq!(status, StatusCode::LOCKED, "{body}");
    // The row that was kept is neither downgraded nor upgraded.
    assert_eq!(
        state_of(&server, &bad).await.unwrap().state,
        ScanState::Infected
    );
    let (status, _) = download(&server, &ann, &bad_doc).await;
    assert_eq!(status, StatusCode::LOCKED);

    // Once scanned, it is available again.
    scanner(&server, &admin, json!({ "scanner_url": fake.url() })).await;
    // The worker, woken by the setting, may be scanning it already; a failed attempt with the
    // scanner that was down is retried here at once.
    let deadline = tokio::time::Instant::now() + SCAN_WAIT;
    while state_of(&server, &good).await.unwrap().state != ScanState::Clean {
        assert!(tokio::time::Instant::now() < deadline, "never scanned");
        server::scan_once(&state, OffsetDateTime::now_utc() + time::Duration::hours(2)).await;
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let (status, _) = download(&server, &ann, &good_doc).await;
    assert_eq!(status, StatusCode::OK);

    // With no scanner, rebuilt rows are unscanned, and available.
    scanner(&server, &admin, json!({ "scanner_url": "" })).await;
    rebuild_from_git(&server, &sha(&bad)).await;
    let info = state_of(&server, &good).await.unwrap();
    assert_eq!(info.state, ScanState::Unscanned, "{info:?}");
    assert!(!info.hold);
    let (status, _) = download(&server, &ann, &good_doc).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        state_of(&server, &bad).await.unwrap().state,
        ScanState::Infected
    );
}

/// An upload held while it waits for the scanner is listed in the quarantine with its state,
/// so that one stuck after the scanner setting is cleared can be found and released.
#[tokio::test]
async fn held_pending_uploads_are_listed_in_the_quarantine() {
    let Scanned {
        server,
        fake,
        admin,
        ann,
        home,
    } = scanned(Behaviour {
        slow: Some(Duration::from_secs(30)),
        ..Behaviour::default()
    })
    .await;
    let bytes = b"waiting for a scanner that goes away".to_vec();
    let doc = upload(&server, &ann, &home, "Waiting.bin", &bytes).await;
    // The scanner setting is cleared: nothing is released, and the upload stays held.
    scanner(&server, &admin, json!({ "scanner_url": "" })).await;
    let info = state_of(&server, &bytes).await.unwrap();
    assert_eq!(info.state, ScanState::Pending);
    assert!(info.hold);
    let (status, _) = download(&server, &ann, &doc).await;
    assert_eq!(status, StatusCode::LOCKED);
    let (status, body) = call(&server, &admin, Method::GET, "/api/admin/quarantine", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let entry = body
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["sha256"] == hex(&sha(&bytes)))
        .cloned()
        .unwrap_or_else(|| panic!("not listed: {body}"));
    assert_eq!(entry["scan"], "pending", "{entry}");
    assert_eq!(entry["documents"][0]["id"], doc, "{entry}");
    // An administrator can release it.
    let (status, body) = call(
        &server,
        &admin,
        Method::POST,
        &format!("/api/admin/quarantine/{}/release", hex(&sha(&bytes))),
        Some(json!({ "reason": "the scanner was retired" })),
    )
    .await;
    assert!(status.is_success(), "{status}: {body}");
    let (status, _) = download(&server, &ann, &doc).await;
    assert_eq!(status, StatusCode::OK);
    assert!(quarantined(&server, &admin).await.is_empty());

    // Or delete it.
    let other = b"another upload left waiting".to_vec();
    scanner(&server, &admin, json!({ "scanner_url": fake.url() })).await;
    let gone = upload(&server, &ann, &home, "Gone.bin", &other).await;
    scanner(&server, &admin, json!({ "scanner_url": "" })).await;
    assert_eq!(
        state_of(&server, &other).await.unwrap().state,
        ScanState::Pending
    );
    let (status, body) = call(
        &server,
        &admin,
        Method::POST,
        &format!("/api/admin/quarantine/{}/delete", hex(&sha(&other))),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = download(&server, &ann, &gone).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn scanner_failures_hold_and_retry() {
    let server = spawn_server().await;
    let state = server.app_state().await;
    let admin = server.admin("Scan Admin").await;
    let ann = server.user("Ann").await;
    let home = server.home_of(&ann).await.0.to_string();
    let port = free_port();
    let url = format!("icap://127.0.0.1:{port}/scan");
    let answer = scanner(
        &server,
        &admin,
        json!({ "scanner_url": url, "scanner_timeout_seconds": 5 }),
    )
    .await;
    assert_eq!(answer["ok"], false, "{answer}");
    assert!(answer["error"].is_string(), "{answer}");

    // Refused: held, retried after a minute.
    let refused = b"the scanner is not there".to_vec();
    let doc = upload(&server, &ann, &home, "Refused.bin", &refused).await;
    wait_scan(&server, &refused, |s| s == ScanState::Failed).await;
    let info = state_of(&server, &refused).await.unwrap();
    assert_eq!(info.attempts, 1);
    let wait = info.next_scan_at.unwrap() - OffsetDateTime::now_utc();
    assert!(
        wait > time::Duration::seconds(50) && wait <= time::Duration::seconds(61),
        "{wait}"
    );
    let (status, body) = download(&server, &ann, &doc).await;
    assert_eq!(status, StatusCode::LOCKED, "{body}");
    assert_eq!(body["scan"], "failed");
    assert_eq!(listed(&server, &ann, &home, &doc).await["scan"], "held");

    let mut fake = fake_on(port, &Behaviour::default());
    // Not yet due.
    server::scan_once(&state, OffsetDateTime::now_utc()).await;
    assert_eq!(
        state_of(&server, &refused).await.unwrap().state,
        ScanState::Failed
    );
    server::scan_once(
        &state,
        OffsetDateTime::now_utc() + time::Duration::minutes(2),
    )
    .await;
    assert_eq!(
        state_of(&server, &refused).await.unwrap().state,
        ScanState::Clean
    );
    let response = server
        .http(Some(&ann))
        .get(server.url(&format!("/api/docs/{doc}/content")))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.bytes().await.unwrap().as_ref(), refused.as_slice());

    // Times out halfway through the body.
    let big = vec![b'h'; 1 << 20];
    fake.set(Behaviour {
        stall_after: Some(big.len() as u64 / 2),
        ..Behaviour::default()
    });
    let doc = upload(&server, &ann, &home, "Stalled.bin", &big).await;
    wait_scan(&server, &big, |s| s == ScanState::Failed).await;
    let (status, _) = download(&server, &ann, &doc).await;
    assert_eq!(status, StatusCode::LOCKED);
    fake.set(Behaviour::default());
    server::scan_once(
        &state,
        OffsetDateTime::now_utc() + time::Duration::minutes(2),
    )
    .await;
    assert_eq!(
        state_of(&server, &big).await.unwrap().state,
        ScanState::Clean
    );

    // Answers garbage.
    fake.set(Behaviour {
        garbage: true,
        ..Behaviour::default()
    });
    let garbled = b"the scanner talks nonsense".to_vec();
    let doc = upload(&server, &ann, &home, "Garbled.bin", &garbled).await;
    wait_scan(&server, &garbled, |s| s == ScanState::Failed).await;
    let (status, _) = download(&server, &ann, &doc).await;
    assert_eq!(status, StatusCode::LOCKED);
    fake.set(Behaviour::default());
    server::scan_once(
        &state,
        OffsetDateTime::now_utc() + time::Duration::minutes(2),
    )
    .await;
    assert_eq!(
        state_of(&server, &garbled).await.unwrap().state,
        ScanState::Clean
    );

    // Dribbles its answer a byte at a time: the whole exchange has a deadline.
    fake.set(Behaviour {
        dribble: Some(Duration::from_millis(500)),
        ..Behaviour::default()
    });
    let dribbled = b"the scanner answers very slowly".to_vec();
    let started = std::time::Instant::now();
    let doc = upload(&server, &ann, &home, "Dribbled.bin", &dribbled).await;
    wait_scan(&server, &dribbled, |s| s == ScanState::Failed).await;
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "{:?}",
        started.elapsed()
    );
    let info = state_of(&server, &dribbled).await.unwrap();
    assert!(
        info.detail
            .as_deref()
            .unwrap_or_default()
            .contains("in time"),
        "{info:?}"
    );
    let (status, _) = download(&server, &ann, &doc).await;
    assert_eq!(status, StatusCode::LOCKED);
    fake.set(Behaviour::default());
    server::scan_once(
        &state,
        OffsetDateTime::now_utc() + time::Duration::minutes(2),
    )
    .await;
    assert_eq!(
        state_of(&server, &dribbled).await.unwrap().state,
        ScanState::Clean
    );

    // Restarts during a scan.
    fake.set(Behaviour {
        slow: Some(Duration::from_secs(10)),
        ..Behaviour::default()
    });
    let before = fake.seen().scans;
    let restarted = b"the scanner restarts".to_vec();
    let doc = upload(&server, &ann, &home, "Restarted.bin", &restarted).await;
    let deadline = tokio::time::Instant::now() + SCAN_WAIT;
    while fake.seen().scans == before {
        assert!(tokio::time::Instant::now() < deadline, "never scanned");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    fake.stop();
    wait_scan(&server, &restarted, |s| s == ScanState::Failed).await;
    let (status, _) = download(&server, &ann, &doc).await;
    assert_eq!(status, StatusCode::LOCKED);
    let fake = fake_on(port, &Behaviour::default());
    server::scan_once(
        &state,
        OffsetDateTime::now_utc() + time::Duration::minutes(2),
    )
    .await;
    assert_eq!(
        state_of(&server, &restarted).await.unwrap().state,
        ScanState::Clean
    );
    let (status, _) = download(&server, &ann, &doc).await;
    assert_eq!(status, StatusCode::OK);

    // The retries come after 1, 5, 15 and then every 60 minutes.
    fake.set(Behaviour {
        fail: true,
        ..Behaviour::default()
    });
    let failing = b"the scanner keeps failing".to_vec();
    let doc = upload(&server, &ann, &home, "Failing.bin", &failing).await;
    wait_scan(&server, &failing, |s| s == ScanState::Failed).await;
    let mut now = OffsetDateTime::now_utc();
    for (attempts, minutes) in [(2, 5), (3, 15), (4, 60), (5, 60)] {
        now = state_of(&server, &failing)
            .await
            .unwrap()
            .next_scan_at
            .unwrap()
            + time::Duration::seconds(1);
        server::scan_once(&state, now).await;
        let info = state_of(&server, &failing).await.unwrap();
        assert_eq!(info.state, ScanState::Failed);
        assert_eq!(info.attempts, attempts);
        let wait = info.next_scan_at.unwrap() - now;
        assert!(
            (wait - time::Duration::minutes(minutes)).abs() < time::Duration::seconds(2),
            "attempt {attempts}: {wait}"
        );
    }
    let (status, _) = download(&server, &ann, &doc).await;
    assert_eq!(status, StatusCode::LOCKED);
    // The content was never lost.
    assert!(server.blobs().contains(&sha(&failing)));
    fake.set(Behaviour::default());
    server::scan_once(&state, now + time::Duration::minutes(61)).await;
    assert_eq!(
        state_of(&server, &failing).await.unwrap().state,
        ScanState::Clean
    );
    let (status, _) = download(&server, &ann, &doc).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn unavailable_allow_lets_files_through() {
    let server = spawn_server().await;
    let admin = server.admin("Scan Admin").await;
    let ann = server.user("Ann").await;
    let home = server.home_of(&ann).await.0.to_string();
    let url = format!("icap://127.0.0.1:{}/scan", free_port());
    scanner(&server, &admin, json!({ "scanner_url": url })).await;
    let bytes = b"waiting for a scanner that is down".to_vec();
    let doc = upload(&server, &ann, &home, "Down.bin", &bytes).await;
    wait_scan(&server, &bytes, |s| s == ScanState::Failed).await;
    let (status, _) = download(&server, &ann, &doc).await;
    assert_eq!(status, StatusCode::LOCKED);

    // Only an administrator changes it.
    let (status, _) = call(
        &server,
        &ann,
        Method::PUT,
        "/api/settings/scanner",
        Some(json!({ "scanner_unavailable": "allow" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(&server, &ann, Method::GET, "/api/settings/scanner", None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, body) = call(
        &server,
        &admin,
        Method::PUT,
        "/api/settings/scanner",
        Some(json!({ "scanner_unavailable": "sometimes" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    scanner(&server, &admin, json!({ "scanner_unavailable": "allow" })).await;
    let (status, _) = download(&server, &ann, &doc).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        listed(&server, &ann, &home, &doc)
            .await
            .get("scan")
            .is_none()
    );
    // Still retried, and held again once the setting goes back.
    assert_eq!(
        state_of(&server, &bytes).await.unwrap().state,
        ScanState::Failed
    );
    scanner(&server, &admin, json!({ "scanner_unavailable": "hold" })).await;
    let (status, _) = download(&server, &ann, &doc).await;
    assert_eq!(status, StatusCode::LOCKED);

    // Clearing the scanner releases nothing: held content stays held.
    let answer = scanner(&server, &admin, json!({ "scanner_url": "" })).await;
    assert_eq!(answer["ok"], true, "{answer}");
    let (status, body) = download(&server, &ann, &doc).await;
    assert_eq!(status, StatusCode::LOCKED, "{body}");
    assert_eq!(body["scan"], "failed");
    assert_eq!(listed(&server, &ann, &home, &doc).await["scan"], "held");
    // New content stored with no scanner is unscanned, and available.
    let fresh = b"stored with no scanner".to_vec();
    let fresh_doc = upload(&server, &ann, &home, "Fresh.bin", &fresh).await;
    assert_eq!(
        state_of(&server, &fresh).await.unwrap().state,
        ScanState::Unscanned
    );
    let (status, _) = download(&server, &ann, &fresh_doc).await;
    assert_eq!(status, StatusCode::OK);
    scanner(&server, &admin, json!({ "scanner_url": url })).await;

    let (status, settings) =
        call(&server, &admin, Method::GET, "/api/settings/scanner", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(settings["scanner_url"], url);
    assert_eq!(settings["scanner_unavailable"], "hold");
    assert_eq!(settings["scanner_timeout_seconds"], 120);
    assert_eq!(settings["scanner_too_large"], "hold");
    assert_eq!(settings["scanner_max_bytes"], 2_u64 << 30);

    // A scanner address that is not one, or carries a password, is refused.
    for bad in [
        "http://scanner/x",
        "icap://who:pw@scanner/x",
        "icap://scanner",
    ] {
        let (status, body) = call(
            &server,
            &admin,
            Method::PUT,
            "/api/settings/scanner",
            Some(json!({ "scanner_url": bad })),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
        assert_eq!(body["error"], "invalid_scanner_url", "{bad}");
    }
}

#[tokio::test]
async fn too_large_held_unless_allowed() {
    let Scanned {
        server,
        fake,
        admin,
        ann,
        home,
    } = scanned(Behaviour::default()).await;
    scanner(&server, &admin, json!({ "scanner_max_bytes": 1000 })).await;
    let large = vec![b'L'; 2000];
    let doc = upload(&server, &ann, &home, "Large.bin", &large).await;
    wait_scan(&server, &large, |s| s == ScanState::TooLarge).await;
    let (status, body) = download(&server, &ann, &doc).await;
    assert_eq!(status, StatusCode::LOCKED, "{body}");
    assert_eq!(body["scan"], "too_large");
    assert_eq!(listed(&server, &ann, &home, &doc).await["scan"], "held");
    assert_eq!(fake.seen().scans, 0);

    let small = vec![b's'; 500];
    upload(&server, &ann, &home, "Small.bin", &small).await;
    wait_scan(&server, &small, |s| s == ScanState::Clean).await;
    assert_eq!(fake.seen().scans, 1);

    scanner(&server, &admin, json!({ "scanner_too_large": "allow" })).await;
    let (status, _) = download(&server, &ann, &doc).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn same_content_scanned_once() {
    let Scanned {
        server,
        fake,
        ann,
        home,
        ..
    } = scanned(Behaviour::default()).await;
    let state = server.app_state().await;
    let bytes = b"the same bytes, twice".to_vec();
    let first = upload(&server, &ann, &home, "First.bin", &bytes).await;
    let second = upload(&server, &ann, &home, "Second.bin", &bytes).await;
    wait_scan(&server, &bytes, |s| s == ScanState::Clean).await;
    let third = upload(&server, &ann, &home, "Third.bin", &bytes).await;
    server::scan_once(&state, OffsetDateTime::now_utc()).await;
    for doc in [&first, &second, &third] {
        let (status, _) = download(&server, &ann, doc).await;
        assert_eq!(status, StatusCode::OK);
    }
    let bad = infected("twice");
    upload(&server, &ann, &home, "Bad one.bin", &bad).await;
    upload(&server, &ann, &home, "Bad two.bin", &bad).await;
    wait_scan(&server, &bad, |s| s == ScanState::Infected).await;
    server::scan_once(&state, OffsetDateTime::now_utc()).await;
    assert_eq!(fake.seen().scans, 2);
}

#[tokio::test]
async fn no_scanner_means_unscanned_and_available() {
    let server = spawn_server().await;
    let state = server.app_state().await;
    let ann = server.user("Ann").await;
    let home = server.home_of(&ann).await.0.to_string();
    let bad = infected("unscanned");
    let doc = upload(&server, &ann, &home, "Anything.bin", &bad).await;
    let info = state_of(&server, &bad).await.unwrap();
    assert_eq!(info.state, ScanState::Unscanned);
    assert_eq!(
        server::scan_once(&state, OffsetDateTime::now_utc()).await,
        0
    );
    let (status, _) = download(&server, &ann, &doc).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        listed(&server, &ann, &home, &doc)
            .await
            .get("scan")
            .is_none()
    );
}

#[tokio::test]
async fn existing_content_is_unscanned_until_asked() {
    let server = spawn_server().await;
    let state = server.app_state().await;
    let admin = server.admin("Scan Admin").await;
    let ann = server.user("Ann").await;
    let home = server.home_of(&ann).await.0.to_string();
    let good = b"stored before the scanner".to_vec();
    let bad = infected("before");
    let kept = upload(&server, &ann, &home, "Good.bin", &good).await;
    let doomed = upload(&server, &ann, &home, "Bad.bin", &bad).await;

    // Slow, so that the queued content can be seen waiting.
    let fake = FakeIcap::start(
        local(),
        Behaviour {
            slow: Some(Duration::from_secs(3)),
            ..Behaviour::default()
        },
    )
    .unwrap();
    scanner(&server, &admin, json!({ "scanner_url": fake.url() })).await;
    server::scan_once(&state, OffsetDateTime::now_utc()).await;
    assert_eq!(fake.seen().scans, 0);
    assert_eq!(
        state_of(&server, &good).await.unwrap().state,
        ScanState::Unscanned
    );
    for doc in [&kept, &doomed] {
        let (status, _) = download(&server, &ann, doc).await;
        assert_eq!(status, StatusCode::OK);
    }

    let (status, _) = call(
        &server,
        &ann,
        Method::POST,
        "/api/admin/scan-existing",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, body) = call(
        &server,
        &admin,
        Method::POST,
        "/api/admin/scan-existing",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["queued"], 2);
    // Queued content stays available while it waits, uploads included.
    for (bytes, doc) in [(&good, &kept), (&bad, &doomed)] {
        assert_eq!(
            state_of(&server, bytes).await.unwrap().state,
            ScanState::Pending
        );
        let (status, body) = download(&server, &ann, doc).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            listed(&server, &ann, &home, doc)
                .await
                .get("scan")
                .is_none()
        );
    }
    wait_scan(&server, &good, |s| s == ScanState::Clean).await;
    wait_scan(&server, &bad, |s| s == ScanState::Infected).await;
    let (status, _) = download(&server, &ann, &kept).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = download(&server, &ann, &doomed).await;
    assert_eq!(status, StatusCode::LOCKED);
}

#[tokio::test]
async fn release_and_delete_are_for_administrators() {
    let Scanned {
        server,
        fake: _fake,
        admin,
        ann,
        home,
    } = scanned(Behaviour::default()).await;
    let bad = infected("release");
    let doc = upload(&server, &ann, &home, "Flagged.bin", &bad).await;
    wait_scan(&server, &bad, |s| s == ScanState::Infected).await;
    let hash = hex(&sha(&bad));

    for (method, path, body) in [
        (Method::GET, "/api/admin/quarantine".to_owned(), None),
        (
            Method::POST,
            format!("/api/admin/quarantine/{hash}/release"),
            Some(json!({ "reason": "I am sure it is fine" })),
        ),
        (
            Method::POST,
            format!("/api/admin/quarantine/{hash}/delete"),
            None,
        ),
    ] {
        let (status, answer) = call(&server, &ann, method, &path, body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}: {answer}");
    }
    assert_eq!(
        state_of(&server, &bad).await.unwrap().state,
        ScanState::Infected
    );

    let (status, list) = call(&server, &admin, Method::GET, "/api/admin/quarantine", None).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let entry = list
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["sha256"] == hash)
        .cloned()
        .unwrap();
    assert_eq!(entry["threat"], THREAT, "{entry}");
    assert_eq!(entry["scan"], "infected", "{entry}");
    assert_eq!(entry["documents"][0]["id"], doc, "{entry}");
    assert!(
        entry["documents"][0]["path"]
            .as_str()
            .unwrap()
            .ends_with("Flagged.bin"),
        "{entry}"
    );

    let (status, body) = call(
        &server,
        &admin,
        Method::POST,
        &format!("/api/admin/quarantine/{hash}/release"),
        Some(json!({ "reason": "ok" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (status, body) = call(
        &server,
        &admin,
        Method::POST,
        &format!("/api/admin/quarantine/{hash}/release"),
        Some(json!({ "reason": "A test file, checked by hand" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        state_of(&server, &bad).await.unwrap().state,
        ScanState::Released
    );
    let (status, _) = download(&server, &ann, &doc).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        listed(&server, &ann, &home, &doc)
            .await
            .get("scan")
            .is_none()
    );
    let events = server
        .records_db()
        .events_of_kind("content.released")
        .await
        .unwrap();
    let event = events.last().unwrap();
    assert_eq!(event.actor, Some(admin.id));
    assert_eq!(event.detail["sha256"], hash);
    assert_eq!(event.detail["reason"], "A test file, checked by hand");
    // Released content is not held, so there is nothing to release again.
    let (status, _) = call(
        &server,
        &admin,
        Method::POST,
        &format!("/api/admin/quarantine/{hash}/release"),
        Some(json!({ "reason": "again, please" })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn quarantine_delete_purges_and_erases() {
    let Scanned {
        server,
        fake: _fake,
        admin,
        ann,
        home,
    } = scanned(Behaviour::default()).await;
    let bad = infected("delete");
    let one = upload(&server, &ann, &home, "One.bin", &bad).await;
    let two = upload(&server, &ann, &home, "Two.bin", &bad).await;
    wait_scan(&server, &bad, |s| s == ScanState::Infected).await;
    let hash = hex(&sha(&bad));
    let (status, body) = call(
        &server,
        &admin,
        Method::POST,
        &format!("/api/admin/quarantine/{hash}/delete"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["documents"], 2);
    for doc in [&one, &two] {
        let (status, _) = call(
            &server,
            &ann,
            Method::GET,
            &format!("/api/docs/{doc}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
    // Not in the recycle bin either, and the content is erased.
    let (status, page) = call(
        &server,
        &ann,
        Method::GET,
        &format!("/api/nodes/{home}/children"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(page["items"].as_array().unwrap().is_empty(), "{page}");
    assert!(!server.blobs().contains(&sha(&bad)));
    assert!(state_of(&server, &bad).await.is_none());
    let events = server
        .records_db()
        .events_of_kind("content.deleted")
        .await
        .unwrap();
    let event = events.last().unwrap();
    assert_eq!(event.actor, Some(admin.id));
    assert_eq!(event.detail["documents"], 2);
    let (status, list) = call(&server, &admin, Method::GET, "/api/admin/quarantine", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!list.as_array().unwrap().iter().any(|e| e["sha256"] == hash));

    // Content an older version still names stays stored.
    let worse = infected("older");
    let old = upload(&server, &ann, &home, "Old.bin", &worse).await;
    replace(&server, &ann, &home, "Old.bin", b"cleaned up").await;
    let current = upload(&server, &ann, &home, "Current.bin", &worse).await;
    wait_scan(&server, &worse, |s| s == ScanState::Infected).await;
    let (status, body) = call(
        &server,
        &admin,
        Method::POST,
        &format!("/api/admin/quarantine/{}/delete", hex(&sha(&worse))),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["documents"], 1);
    let (status, _) = call(
        &server,
        &ann,
        Method::GET,
        &format!("/api/docs/{current}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(
        &server,
        &ann,
        Method::GET,
        &format!("/api/docs/{old}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(server.blobs().contains(&sha(&worse)));
}

#[tokio::test]
async fn scan_events_recorded() {
    let Scanned {
        server,
        fake,
        admin,
        ann,
        home,
    } = scanned(Behaviour::default()).await;
    let good = b"a clean file".to_vec();
    let bad = infected("events");
    upload(&server, &ann, &home, "Good.bin", &good).await;
    upload(&server, &ann, &home, "Bad.bin", &bad).await;
    wait_scan(&server, &good, |s| s == ScanState::Clean).await;
    wait_scan(&server, &bad, |s| s == ScanState::Infected).await;
    fake.set(Behaviour {
        fail: true,
        ..Behaviour::default()
    });
    let failing = b"the scanner fails on this".to_vec();
    upload(&server, &ann, &home, "Failing.bin", &failing).await;
    wait_scan(&server, &failing, |s| s == ScanState::Failed).await;

    let db = server.records_db();
    let scanned = db.events_of_kind("content.scanned").await.unwrap();
    let of = |bytes: &[u8]| {
        let hash = hex(&sha(bytes));
        scanned
            .iter()
            .find(|e| e.detail["sha256"] == hash)
            .unwrap_or_else(|| panic!("no event for {hash}"))
            .clone()
    };
    assert_eq!(of(&good).detail["result"], "clean");
    assert!(of(&good).detail["detail"].is_null());
    assert_eq!(of(&bad).detail["result"], "infected");
    assert_eq!(of(&bad).detail["detail"], THREAT);
    assert_eq!(of(&failing).detail["result"], "failed");
    assert!(of(&good).actor.is_none());

    let changed = db.events_of_kind("setting.changed").await.unwrap();
    let url = changed
        .iter()
        .find(|e| e.detail["key"] == "scanner_url")
        .unwrap();
    assert_eq!(url.actor, Some(admin.id));
    assert_eq!(url.detail["to"], fake.url());
}
