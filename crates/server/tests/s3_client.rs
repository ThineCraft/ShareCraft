//! The S3 client against a fake S3 server on 127.0.0.1.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use collab_core::ids::NodeId;
use common::fake_s3::{FakeS3, Opts};
use records::{S3Endpoint, Versioning};
use server::s3::{Body, Class, S3, S3Error, S3Target};
use server::s3_sign::Credentials;
use server::secrets::Secret;
use uuid::Uuid;

const KEY_ID: &str = "AKIDFAKEEXAMPLE";
const SECRET: &str = "planted-S3-secret-7f3a9c";
const BUCKET: &str = "photos";

fn creds(id: &str, secret: &str) -> Credentials {
    Credentials {
        access_key_id: id.to_owned(),
        secret: Secret::new(secret),
        session_token: None,
    }
}

/// The fake's origin as an approved endpoint whose allowed range covers loopback.
fn endpoint(fake: &FakeS3, path_style: bool) -> S3Endpoint {
    S3Endpoint::parse(
        "fake",
        &fake.origin(),
        "us-east-1",
        path_style,
        &["127.0.0.0/8".to_owned()],
        true,
    )
    .unwrap()
}

/// A client allowed to talk plain HTTP to loopback, with short retry waits.
fn client() -> S3 {
    let s3 = S3::new(true);
    s3.set_retry_wait_for_tests(Duration::from_millis(5));
    s3
}

fn target(s3: &S3, fake: &FakeS3) -> S3Target {
    s3.target_from(&endpoint(fake, true), BUCKET, creds(KEY_ID, SECRET))
}

async fn read_all(mut body: Body) -> Result<Vec<u8>, S3Error> {
    let mut out = Vec::new();
    while let Some(chunk) = body.chunk().await? {
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

async fn fake() -> FakeS3 {
    let fake = FakeS3::start().await;
    fake.bucket(BUCKET);
    fake
}

#[tokio::test]
async fn lists_a_page_and_follows_the_token() {
    let fake = fake().await;
    for name in ["a/1", "a/2", "a/3", "a/4", "a/5", "b/x"] {
        fake.put(BUCKET, name, name.as_bytes());
    }
    fake.page_size(2);
    let s3 = client();
    let t = target(&s3, &fake);

    let mut keys = Vec::new();
    let mut token: Option<String> = None;
    let mut pages = 0;
    loop {
        let page = s3
            .list_page(&t, "a/", token.as_deref(), 1000)
            .await
            .unwrap();
        pages += 1;
        assert!(page.objects.len() <= 2);
        for object in &page.objects {
            assert_eq!(object.size, 3);
            assert!(object.etag.starts_with('"'));
            keys.push(object.key.clone());
        }
        match page.next {
            Some(next) => token = Some(next),
            None => break,
        }
    }
    assert_eq!(keys, ["a/1", "a/2", "a/3", "a/4", "a/5"]);
    assert_eq!(pages, 3);
    assert_eq!(fake.count("list"), 3);
    let first = &fake.requests()[0];
    assert!(first.query.contains("list-type=2"));
    assert!(first.query.contains("encoding-type=url"));
    assert!(!first.query.contains("delimiter"));
}

#[tokio::test]
async fn keys_with_spaces_unicode_and_plus_round_trip() {
    let fake = fake().await;
    fake.require_keys(KEY_ID, SECRET);
    let key = "a b/é+c.txt";
    fake.put(BUCKET, key, b"odd name");
    let s3 = client();
    let t = target(&s3, &fake);

    let page = s3.list_page(&t, "a b/", None, 1000).await.unwrap();
    assert_eq!(page.objects.len(), 1);
    assert_eq!(page.objects[0].key, key);

    let head = s3.head(&t, key).await.unwrap();
    assert_eq!(head.size, 8);
    let (_, body) = s3.get(&t, key, &head.etag, None).await.unwrap();
    assert_eq!(read_all(body).await.unwrap(), b"odd name");
}

#[tokio::test]
async fn head_reads_size_etag_time_class_and_version() {
    let fake = fake().await;
    fake.enable_versioning(BUCKET);
    fake.put_at(BUCKET, "plain.txt", b"hello", 1_700_000_000);
    fake.put_with(
        BUCKET,
        "cold.bin",
        b"frozen",
        Opts {
            etag: Some("\"abc123-2\"".to_owned()),
            class: Some("GLACIER".to_owned()),
            restored: Some(true),
        },
    );
    fake.put_with(
        BUCKET,
        "thawing.bin",
        b"warm",
        Opts {
            etag: None,
            class: Some("DEEP_ARCHIVE".to_owned()),
            restored: Some(false),
        },
    );
    let s3 = client();
    let t = target(&s3, &fake);

    let plain = s3.head(&t, "plain.txt").await.unwrap();
    assert_eq!(plain.size, 5);
    assert_eq!(plain.modified_s, 1_700_000_000);
    assert!(plain.etag.starts_with('"') && plain.etag.ends_with('"'));
    assert_eq!(plain.class, None);
    assert!(!plain.restored);
    assert!(plain.version.is_some());

    let cold = s3.head(&t, "cold.bin").await.unwrap();
    assert_eq!(cold.etag, "\"abc123-2\"");
    assert_eq!(cold.class.as_deref(), Some("GLACIER"));
    assert!(cold.restored);

    let thawing = s3.head(&t, "thawing.bin").await.unwrap();
    assert_eq!(thawing.class.as_deref(), Some("DEEP_ARCHIVE"));
    assert!(!thawing.restored);

    assert_eq!(s3.head(&t, "missing").await.unwrap_err(), S3Error::NotFound);
}

#[tokio::test]
async fn get_returns_the_bytes_and_checks_the_etag() {
    let fake = fake().await;
    fake.put(BUCKET, "doc.txt", b"the whole content");
    let s3 = client();
    let t = target(&s3, &fake);

    let head = s3.head(&t, "doc.txt").await.unwrap();
    let (got, body) = s3.get(&t, "doc.txt", &head.etag, None).await.unwrap();
    assert_eq!(got.etag, head.etag);
    assert_eq!(got.size, 17);
    assert_eq!(read_all(body).await.unwrap(), b"the whole content");

    let wrong = s3.get(&t, "doc.txt", "\"0000\"", None).await;
    assert_eq!(wrong.err(), Some(S3Error::Precondition));
    let sent = fake.requests();
    let last = sent.last().unwrap();
    assert!(last.headers.iter().any(|h| h == "if-match"));
}

#[tokio::test]
async fn a_range_get_returns_just_that_range() {
    let fake = fake().await;
    fake.put(BUCKET, "range.bin", b"0123456789");
    let s3 = client();
    let t = target(&s3, &fake);

    let head = s3.head(&t, "range.bin").await.unwrap();
    let (got, body) = s3
        .get(&t, "range.bin", &head.etag, Some((2, 5)))
        .await
        .unwrap();
    assert_eq!(got.size, 10);
    assert_eq!(read_all(body).await.unwrap(), b"2345");
}

#[tokio::test]
async fn an_object_replaced_between_head_and_get_is_a_precondition_failure() {
    let fake = fake().await;
    fake.put(BUCKET, "moving.txt", b"first");
    fake.change_before_get("moving.txt", b"second version");
    let s3 = client();
    let t = target(&s3, &fake);

    let head = s3.head(&t, "moving.txt").await.unwrap();
    assert_eq!(head.size, 5);
    let got = s3.get(&t, "moving.txt", &head.etag, None).await;
    assert_eq!(got.err(), Some(S3Error::Precondition));
}

#[tokio::test]
async fn every_request_carries_a_valid_signature_and_signs_what_it_sends() {
    let fake = fake().await;
    fake.require_keys(KEY_ID, SECRET);
    fake.put(BUCKET, "signed.txt", b"signed bytes");
    let s3 = client();
    let t = target(&s3, &fake);

    s3.list_page(&t, "", None, 10).await.unwrap();
    let head = s3.head(&t, "signed.txt").await.unwrap();
    let (_, body) = s3
        .get(&t, "signed.txt", &head.etag, Some((0, 5)))
        .await
        .unwrap();
    assert_eq!(read_all(body).await.unwrap(), b"signed");
    s3.bucket_versioning(&t).await.unwrap();

    let sent = fake.requests();
    assert_eq!(sent.len(), 4);
    for request in &sent {
        for name in [
            "authorization",
            "x-amz-date",
            "x-amz-content-sha256",
            "host",
        ] {
            assert!(
                request.headers.iter().any(|h| h == name),
                "{name} missing from {request:?}"
            );
        }
    }
    let range = &sent[2];
    assert!(range.headers.iter().any(|h| h == "range"));
    assert!(range.headers.iter().any(|h| h == "if-match"));

    let wrong_secret = s3.target_from(&endpoint(&fake, true), BUCKET, creds(KEY_ID, "nope"));
    assert_eq!(
        s3.list_page(&wrong_secret, "", None, 10).await.unwrap_err(),
        S3Error::BadKeys
    );
    let wrong_id = s3.target_from(&endpoint(&fake, true), BUCKET, creds("AKIDOTHER", SECRET));
    assert_eq!(
        s3.list_page(&wrong_id, "", None, 10).await.unwrap_err(),
        S3Error::BadKeys
    );
}

#[tokio::test]
async fn virtual_host_and_path_style_build_the_right_url_and_host() {
    let s3 = client();
    let virtual_host =
        S3Endpoint::parse("r2", "https://s3.example.com", "auto", false, &[], false).unwrap();
    let path = S3Endpoint::parse(
        "minio",
        "https://s3.example.com:9000",
        "us-east-1",
        true,
        &[],
        false,
    )
    .unwrap();
    let vt = s3.target_from(&virtual_host, BUCKET, creds(KEY_ID, SECRET));
    let pt = s3.target_from(&path, BUCKET, creds(KEY_ID, SECRET));

    assert_eq!(
        S3::request_url(&vt, Some("a b/c+d.txt")),
        (
            "https://photos.s3.example.com/a%20b/c%2Bd.txt".to_owned(),
            "photos.s3.example.com".to_owned()
        )
    );
    assert_eq!(
        S3::request_url(&vt, None),
        (
            "https://photos.s3.example.com/".to_owned(),
            "photos.s3.example.com".to_owned()
        )
    );
    assert_eq!(
        S3::request_url(&pt, Some("a b/c+d.txt")),
        (
            "https://s3.example.com:9000/photos/a%20b/c%2Bd.txt".to_owned(),
            "s3.example.com:9000".to_owned()
        )
    );
    assert_eq!(
        S3::request_url(&pt, None),
        (
            "https://s3.example.com:9000/photos".to_owned(),
            "s3.example.com:9000".to_owned()
        )
    );

    // An address-literal endpoint has no virtual-host form (`photos.127.0.0.1` is not a host
    // name a URL accepts): it is sent path-style.
    let fake = fake().await;
    fake.require_keys(KEY_ID, SECRET);
    fake.put(BUCKET, "v/key.txt", b"virtual");
    let literal = s3.target_from(&endpoint(&fake, false), BUCKET, creds(KEY_ID, SECRET));
    let (url, host) = S3::request_url(&literal, Some("v/key.txt"));
    assert_eq!(url, format!("{}/photos/v/key.txt", fake.origin()));
    assert_eq!(host, fake.addr.to_string());
    assert_eq!(s3.head(&literal, "v/key.txt").await.unwrap().size, 7);
    assert_eq!(fake.requests().last().unwrap().path, "/photos/v/key.txt");
    assert_eq!(
        fake.last_headers().get("host").unwrap().to_str().unwrap(),
        host
    );

    // One live request in virtual-host style: the name resolved is `bucket.host`, pointed at
    // a listener that counts connections. (The fake speaks plain HTTP, which only the
    // loopback literal may use, so the TLS handshake fails after the connection is made.)
    let (listener, connections) = counting_listener().await;
    let named = S3Endpoint::parse(
        "named",
        "https://s3.internal.test",
        "us-east-1",
        false,
        &["127.0.0.0/8".to_owned()],
        false,
    )
    .unwrap();
    s3.resolve_for_tests("photos.s3.internal.test", listener);
    let t = s3.target_from(&named, BUCKET, creds(KEY_ID, SECRET));
    let failed = s3.head(&t, "k").await.unwrap_err();
    assert_ne!(failed, S3Error::AddressRefused);
    wait_for(|| connections.load(Ordering::SeqCst) >= 1).await;
}

/// A listener on 127.0.0.1 that accepts and drops connections, counting them.
async fn counting_listener() -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let counted = count.clone();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            counted.fetch_add(1, Ordering::SeqCst);
            drop(stream);
        }
    });
    (addr, count)
}

async fn wait_for(done: impl Fn() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !done() {
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn a_dotted_bucket_is_sent_path_style() {
    let s3 = client();
    let virtual_host =
        S3Endpoint::parse("r2", "https://s3.example.com", "auto", false, &[], false).unwrap();
    let dotted = s3.target_from(&virtual_host, "my.photos", creds(KEY_ID, SECRET));
    assert_eq!(
        S3::request_url(&dotted, Some("x.txt")),
        (
            "https://s3.example.com/my.photos/x.txt".to_owned(),
            "s3.example.com".to_owned()
        )
    );

    let fake = FakeS3::start().await;
    fake.bucket("my.photos");
    fake.put("my.photos", "x.txt", b"dots");
    let live = s3.target_from(&endpoint(&fake, false), "my.photos", creds(KEY_ID, SECRET));
    let head = s3.head(&live, "x.txt").await.unwrap();
    assert_eq!(head.size, 4);
    assert_eq!(fake.requests().last().unwrap().path, "/my.photos/x.txt");
}

#[tokio::test]
async fn each_error_is_mapped() {
    let fake = fake().await;
    fake.put(BUCKET, "k", b"v");
    let s3 = client();
    let t = target(&s3, &fake);
    let head = s3.head(&t, "k").await.unwrap();

    let table: &[(u16, &str, S3Error)] = &[
        (404, "NoSuchKey", S3Error::NotFound),
        (404, "NoSuchBucket", S3Error::NoBucket),
        (403, "InvalidAccessKeyId", S3Error::BadKeys),
        (403, "SignatureDoesNotMatch", S3Error::BadKeys),
        (403, "RequestTimeTooSkewed", S3Error::ClockSkew),
        (403, "AccessDenied", S3Error::Denied),
        (301, "PermanentRedirect", S3Error::WrongRegion),
        (307, "TemporaryRedirect", S3Error::WrongRegion),
        (400, "AuthorizationHeaderMalformed", S3Error::WrongRegion),
        (400, "PermanentRedirect", S3Error::WrongRegion),
        (400, "InvalidRegionName", S3Error::WrongRegion),
        (412, "PreconditionFailed", S3Error::Precondition),
        (403, "InvalidObjectState", S3Error::Archived),
        (400, "InvalidObjectState", S3Error::Archived),
        (501, "NotImplemented", S3Error::Unsupported),
        (400, "NotImplemented", S3Error::Unsupported),
    ];
    for (status, code, expected) in table {
        fake.fail_next(*status, code);
        let got = s3.get(&t, "k", &head.etag, None).await.err();
        assert_eq!(got, Some(*expected), "{status} {code}");
    }

    // A bare 404 on HEAD has no body to read.
    fake.fail_next(404, "");
    assert_eq!(s3.head(&t, "k").await.unwrap_err(), S3Error::NotFound);
    // The real clock is checked by a skewed store.
    fake.skew_clock();
    assert_eq!(
        s3.list_page(&t, "", None, 10).await.unwrap_err(),
        S3Error::ClockSkew
    );

    // Nothing listening: unreachable.
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap()
    };
    let gone = S3Endpoint::parse(
        "gone",
        &format!("http://127.0.0.1:{}", closed.port()),
        "us-east-1",
        true,
        &["127.0.0.0/8".to_owned()],
        true,
    )
    .unwrap();
    let gone = s3.target_from(&gone, BUCKET, creds(KEY_ID, SECRET));
    assert_eq!(s3.head(&gone, "k").await.unwrap_err(), S3Error::Unreachable);
}

#[tokio::test]
async fn a_throttle_is_retried_and_then_reported_and_every_try_is_counted() {
    let fake = fake().await;
    fake.put(BUCKET, "k", b"v");
    let s3 = client();
    let library = NodeId(Uuid::new_v4());
    let t = target(&s3, &fake).with_library(library);

    fake.throttle_next(2);
    s3.list_page(&t, "", None, 10).await.unwrap();
    assert_eq!(fake.count("list"), 3);

    fake.throttle_next(100);
    assert_eq!(
        s3.list_page(&t, "", None, 10).await.unwrap_err(),
        S3Error::Throttled
    );
    assert_eq!(fake.count("list"), 3 + 1 + server::s3::RETRIES as usize);

    fake.throttle_next(0);
    fake.fail_next(500, "InternalError");
    let head = s3.head(&t, "k").await.unwrap();
    assert_eq!(fake.count("head"), 2);
    fake.fail_next(502, "");
    let (_, body) = s3.get(&t, "k", &head.etag, None).await.unwrap();
    assert_eq!(read_all(body).await.unwrap(), b"v");
    assert_eq!(fake.count("get"), 2);

    let usage = s3.take_usage();
    assert_eq!(usage.len(), 1);
    let (counted, usage) = usage[0];
    assert_eq!(counted, library);
    assert_eq!(usage.list, 3 + 1 + u64::from(server::s3::RETRIES));
    assert_eq!(usage.read, 4);
    assert_eq!(usage.write, 0);
    assert!(usage.bytes_down >= 1);
    assert!(s3.take_usage().is_empty());

    // The meter also takes counts from elsewhere.
    s3.count(library, Class::Read, 2, 10);
    let again = s3.take_usage();
    assert_eq!(again[0].1.read, 2);
    assert_eq!(again[0].1.bytes_down, 10);
}

#[tokio::test]
async fn a_redirect_is_never_followed() {
    let fake = fake().await;
    fake.put(BUCKET, "k", b"v");
    let elsewhere = FakeS3::start().await;
    elsewhere.bucket(BUCKET);
    elsewhere.put(BUCKET, "k", b"v");
    fake.redirect_to(&elsewhere.origin());
    let s3 = client();
    let t = target(&s3, &fake);

    assert_eq!(s3.head(&t, "k").await.unwrap_err(), S3Error::WrongRegion);
    assert_eq!(
        s3.list_page(&t, "", None, 10).await.unwrap_err(),
        S3Error::WrongRegion
    );
    assert!(elsewhere.requests().is_empty());
}

#[tokio::test]
async fn an_endpoint_that_resolves_to_a_refused_address_is_not_contacted() {
    let fake = fake().await;
    fake.put(BUCKET, "k", b"v");
    let s3 = client();

    // A name that resolves to loopback, with no range allowed, pointed at the fake.
    let unapproved = S3Endpoint::parse(
        "named",
        "https://s3.internal.test",
        "us-east-1",
        false,
        &[],
        false,
    )
    .unwrap();
    s3.resolve_for_tests("photos.s3.internal.test", fake.addr);
    let t = s3.target_from(&unapproved, BUCKET, creds(KEY_ID, SECRET));
    assert_eq!(s3.head(&t, "k").await.unwrap_err(), S3Error::AddressRefused);
    assert_eq!(
        s3.list_page(&t, "", None, 10).await.unwrap_err(),
        S3Error::AddressRefused
    );

    // The same in path style, pointed at a listener: no connection is even opened.
    let (listener, connections) = counting_listener().await;
    let path_style = S3Endpoint::parse(
        "other",
        "https://s3.other.test",
        "us-east-1",
        true,
        &[],
        false,
    )
    .unwrap();
    s3.resolve_for_tests("s3.other.test", listener);
    let t = s3.target_from(&path_style, BUCKET, creds(KEY_ID, SECRET));
    assert_eq!(s3.head(&t, "k").await.unwrap_err(), S3Error::AddressRefused);
    assert_eq!(connections.load(Ordering::SeqCst), 0);

    // The loopback literal itself, with no range allowed.
    let literal = S3Endpoint::parse("fake", &fake.origin(), "us-east-1", true, &[], true).unwrap();
    let t = s3.target_from(&literal, BUCKET, creds(KEY_ID, SECRET));
    assert_eq!(s3.head(&t, "k").await.unwrap_err(), S3Error::AddressRefused);

    // A plain-HTTP endpoint is refused by a client without the loopback switch.
    let strict = S3::new(false);
    let t = strict.target_from(&endpoint(&fake, true), BUCKET, creds(KEY_ID, SECRET));
    assert_eq!(
        strict.head(&t, "k").await.unwrap_err(),
        S3Error::AddressRefused
    );

    assert!(fake.requests().is_empty());
}

#[tokio::test]
async fn a_hung_body_times_out_and_a_short_body_is_an_error() {
    let fake = fake().await;
    fake.put(BUCKET, "hung.bin", &[7_u8; 4096]);
    fake.put(BUCKET, "short.bin", &[8_u8; 4096]);
    fake.put(BUCKET, "slow.bin", b"slow");
    fake.hang_get("hung.bin");
    fake.truncate_get("short.bin", 100);
    fake.slow_get("slow.bin", Duration::from_secs(3));
    let s3 = client();
    s3.set_timeouts_for_tests(Duration::from_secs(2), Duration::from_millis(300));
    let t = target(&s3, &fake);

    let head = s3.head(&t, "hung.bin").await.unwrap();
    let (_, body) = s3.get(&t, "hung.bin", &head.etag, None).await.unwrap();
    assert_eq!(read_all(body).await.unwrap_err(), S3Error::TimedOut);

    let head = s3.head(&t, "short.bin").await.unwrap();
    let (_, body) = s3.get(&t, "short.bin", &head.etag, None).await.unwrap();
    assert_eq!(read_all(body).await.unwrap_err(), S3Error::Malformed);

    let head = s3.head(&t, "slow.bin").await.unwrap();
    let got = s3.get(&t, "slow.bin", &head.etag, None).await;
    assert_eq!(got.err(), Some(S3Error::TimedOut));
}

#[tokio::test]
async fn the_secret_is_in_no_error_debug_or_request_log() {
    let fake = fake().await;
    fake.require_keys(KEY_ID, SECRET);
    fake.put(BUCKET, "k", b"v");
    let s3 = client();
    let t = target(&s3, &fake);

    let mut seen = vec![format!("{t:?}")];
    s3.head(&t, "k").await.unwrap();
    for (status, code) in [
        (403, "AccessDenied"),
        (500, "InternalError"),
        (404, "NoSuchKey"),
    ] {
        fake.fail_next(status, code);
        fake.throttle_next(0);
        if let Err(e) = s3.head(&t, "k").await {
            seen.push(format!("{e:?} {e}"));
        }
    }
    let bad = s3.target_from(
        &endpoint(&fake, true),
        BUCKET,
        creds(KEY_ID, "wrong-secret"),
    );
    let e = s3.list_page(&bad, "", None, 10).await.unwrap_err();
    seen.push(format!("{e:?} {e}"));
    seen.push(format!("{:?}", fake.requests()));
    seen.push(format!("{:?}", fake.last_headers()));
    for text in &seen {
        assert!(!text.contains(SECRET), "secret in {text}");
        assert!(
            !text.contains("fake S3 says"),
            "error message kept in {text}"
        );
    }
    let headers = fake.last_headers();
    assert!(headers.get("authorization").is_none());
}

#[tokio::test]
async fn versioning_is_read_and_an_unsupporting_store_is_unsupported() {
    let fake = fake().await;
    let s3 = client();
    let t = target(&s3, &fake);

    assert_eq!(s3.bucket_versioning(&t).await.unwrap(), Versioning::None);
    fake.enable_versioning(BUCKET);
    assert_eq!(s3.bucket_versioning(&t).await.unwrap(), Versioning::Enabled);
    fake.suspend_versioning(BUCKET);
    assert_eq!(
        s3.bucket_versioning(&t).await.unwrap(),
        Versioning::Suspended
    );
    fake.no_versioning_api();
    assert_eq!(
        s3.bucket_versioning(&t).await.unwrap_err(),
        S3Error::Unsupported
    );
}

#[tokio::test]
async fn the_client_sends_no_write() {
    let fake = fake().await;
    fake.require_keys(KEY_ID, SECRET);
    fake.put(BUCKET, "a/k.txt", b"content");
    let s3 = client();
    let t = target(&s3, &fake);

    s3.list_page(&t, "a/", None, 100).await.unwrap();
    let head = s3.head(&t, "a/k.txt").await.unwrap();
    let (_, body) = s3.get(&t, "a/k.txt", &head.etag, None).await.unwrap();
    read_all(body).await.unwrap();
    s3.get(&t, "a/k.txt", &head.etag, Some((1, 2)))
        .await
        .unwrap();
    s3.bucket_versioning(&t).await.unwrap();
    let _ = s3.head(&t, "missing").await;
    fake.throttle_next(1);
    s3.list_page(&t, "", None, 1).await.unwrap();

    assert_eq!(fake.count("write"), 0);
    let requests = fake.requests();
    assert!(!requests.is_empty());
    for request in requests {
        assert!(
            request.method == "GET" || request.method == "HEAD",
            "{request:?}"
        );
    }
}
