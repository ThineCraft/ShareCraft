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
use server::s3::{
    Body, COPY_MAX, Class, Cond, PartBody, PutBody, S3, S3Error, S3Target, SINGLE_PUT_MAX,
    failure_log_line,
};
use server::s3_sign::Credentials;
use server::s3_sign::sha256_hex;
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
async fn reads_send_no_write() {
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

/// A file holding `bytes` and the body that sends it.
fn file_body(bytes: &[u8]) -> (tempfile::NamedTempFile, PutBody) {
    use sha2::Digest;
    use std::io::Write;
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(bytes).unwrap();
    file.flush().unwrap();
    let body = PutBody {
        path: Some(file.path().to_path_buf()),
        size: bytes.len() as u64,
        sha256: sha2::Sha256::digest(bytes).into(),
    };
    (file, body)
}

fn empty_hash() -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(b"").into()
}

#[tokio::test]
async fn put_round_trips_and_the_store_checks_the_hash() {
    let fake = fake().await;
    fake.require_keys(KEY_ID, SECRET);
    let s3 = client();
    let t = target(&s3, &fake);
    // Several 64 KiB pieces.
    let bytes: Vec<u8> = (0..300_000_u32).map(|n| (n % 251) as u8).collect();
    let (_file, body) = file_body(&bytes);

    let head = s3
        .put(&t, "dir/new.bin", &body, Cond::IfAbsent)
        .await
        .unwrap();
    assert_eq!(head.size, 300_000);
    let stored = fake.object(BUCKET, "dir/new.bin").unwrap();
    assert_eq!(stored.bytes, bytes);
    assert_eq!(head.etag, stored.etag);
    assert_eq!(head.modified_s, stored.modified_s);
    let (_, got) = s3.get(&t, "dir/new.bin", &head.etag, None).await.unwrap();
    assert_eq!(read_all(got).await.unwrap(), bytes);

    // A hash that is not the bytes' hash: the store refuses and keeps nothing.
    let (_other, mut wrong) = file_body(b"some other bytes");
    wrong.sha256 = [7; 32];
    assert_eq!(
        s3.put(&t, "dir/bad.bin", &wrong, Cond::None)
            .await
            .unwrap_err(),
        S3Error::Other
    );
    assert!(fake.object(BUCKET, "dir/bad.bin").is_none());
}

#[tokio::test]
async fn put_if_absent_refuses_an_existing_key() {
    let fake = fake().await;
    fake.put(BUCKET, "k.txt", b"outside");
    let s3 = client();
    let t = target(&s3, &fake);
    let (_f, body) = file_body(b"ours");

    assert_eq!(
        s3.put(&t, "k.txt", &body, Cond::IfAbsent)
            .await
            .unwrap_err(),
        S3Error::Precondition
    );
    assert_eq!(fake.object(BUCKET, "k.txt").unwrap().bytes, b"outside");
    s3.put(&t, "fresh.txt", &body, Cond::IfAbsent)
        .await
        .unwrap();
    assert_eq!(fake.object(BUCKET, "fresh.txt").unwrap().bytes, b"ours");
    assert_eq!(
        fake.writes(),
        [
            ("PUT".to_owned(), "k.txt".to_owned(), "absent".to_owned()),
            (
                "PUT".to_owned(),
                "fresh.txt".to_owned(),
                "absent".to_owned()
            ),
        ]
    );
}

#[tokio::test]
async fn put_if_match_refuses_a_changed_object() {
    let fake = fake().await;
    fake.put(BUCKET, "k.txt", b"v1");
    let s3 = client();
    let t = target(&s3, &fake);
    let seen = s3.head(&t, "k.txt").await.unwrap();
    let (_f, body) = file_body(b"ours");

    fake.put(BUCKET, "k.txt", b"changed outside");
    assert_eq!(
        s3.put(&t, "k.txt", &body, Cond::IfMatch(&seen.etag))
            .await
            .unwrap_err(),
        S3Error::Precondition
    );
    assert_eq!(
        fake.object(BUCKET, "k.txt").unwrap().bytes,
        b"changed outside"
    );
    // An object that is gone does not match either.
    assert_eq!(
        s3.put(&t, "gone.txt", &body, Cond::IfMatch(&seen.etag))
            .await
            .unwrap_err(),
        S3Error::Precondition
    );
    // The current tag matches.
    let now = s3.head(&t, "k.txt").await.unwrap();
    s3.put(&t, "k.txt", &body, Cond::IfMatch(&now.etag))
        .await
        .unwrap();
    assert_eq!(fake.object(BUCKET, "k.txt").unwrap().bytes, b"ours");
    assert_eq!(fake.writes().last().unwrap().2, "match");
}

#[tokio::test]
async fn a_conflict_answer_is_never_taken_for_success() {
    let fake = fake().await;
    let s3 = client();
    let t = target(&s3, &fake);
    let (_f, body) = file_body(b"ours");

    fake.fail_next(412, "PreconditionFailed");
    assert_eq!(
        s3.put(&t, "a.txt", &body, Cond::None).await.unwrap_err(),
        S3Error::Precondition
    );
    fake.fail_next(409, "ConditionalRequestConflict");
    assert_eq!(
        s3.put(&t, "a.txt", &body, Cond::IfAbsent)
            .await
            .unwrap_err(),
        S3Error::Precondition
    );
    // Each was sent once; nothing was stored.
    assert_eq!(fake.count("write"), 2);
    assert!(fake.object(BUCKET, "a.txt").is_none());
    // Another 409 is not a conflict.
    fake.fail_next(409, "OperationAborted");
    assert_eq!(
        s3.put(&t, "a.txt", &body, Cond::None).await.unwrap_err(),
        S3Error::Other
    );
}

#[tokio::test]
async fn put_records_the_content_hash_and_head_reads_it() {
    let fake = fake().await;
    fake.put(BUCKET, "plain.txt", b"no hash here");
    let s3 = client();
    let t = target(&s3, &fake);
    let (_f, body) = file_body(b"hashed bytes");
    let expected = sha256_hex(b"hashed bytes");

    let head = s3.put(&t, "h.txt", &body, Cond::None).await.unwrap();
    assert_eq!(head.sha256.as_deref(), Some(expected.as_str()));
    assert_eq!(
        fake.object(BUCKET, "h.txt").unwrap().sha256.as_deref(),
        Some(expected.as_str())
    );
    assert_eq!(
        s3.head(&t, "h.txt").await.unwrap().sha256.as_deref(),
        Some(expected.as_str())
    );
    assert_eq!(s3.head(&t, "plain.txt").await.unwrap().sha256, None);
}

#[tokio::test]
async fn a_write_is_sent_once_even_when_throttled() {
    let fake = fake().await;
    let s3 = client();
    let t = target(&s3, &fake);
    let (_f, body) = file_body(b"ours");

    fake.throttle_next(1);
    assert_eq!(
        s3.put(&t, "k.txt", &body, Cond::None).await.unwrap_err(),
        S3Error::Throttled
    );
    assert_eq!(fake.count("write"), 1);
    fake.fail_next(500, "InternalError");
    assert_eq!(
        s3.put(&t, "k.txt", &body, Cond::None).await.unwrap_err(),
        S3Error::Throttled
    );
    assert_eq!(fake.count("write"), 2);
    fake.throttle_next(1);
    assert_eq!(
        s3.delete(&t, "k.txt").await.unwrap_err(),
        S3Error::Throttled
    );
    assert_eq!(fake.count("write"), 3);
    s3.put(&t, "k.txt", &body, Cond::None).await.unwrap();
    assert_eq!(fake.count("write"), 4);
}

#[tokio::test]
async fn content_over_the_single_put_limit_is_refused_before_any_request() {
    let fake = fake().await;
    let s3 = client();
    let t = target(&s3, &fake);
    assert_eq!(s3.single_put_max(), SINGLE_PUT_MAX);
    assert_eq!(SINGLE_PUT_MAX, 512 * 1024 * 1024);
    s3.set_single_put_max_for_tests(10);
    assert_eq!(s3.single_put_max(), 10);

    let (_f, big) = file_body(b"eleven byte");
    assert_eq!(
        s3.put(&t, "big.bin", &big, Cond::None).await.unwrap_err(),
        S3Error::Unsupported
    );
    assert!(fake.requests().is_empty());
    let (_g, ten) = file_body(b"ten bytes!");
    s3.put(&t, "ok.bin", &ten, Cond::None).await.unwrap();
    // Only the PUT and its HEAD went out.
    assert_eq!(fake.requests().len(), 2);

    // A file whose length is not the declared size is refused too, and a key with a dot
    // segment is never asked for.
    let (_h, mut lying) = file_body(b"abc");
    lying.size = 5;
    assert_eq!(
        s3.put(&t, "lie.bin", &lying, Cond::None).await.unwrap_err(),
        S3Error::Other
    );
    assert_eq!(
        s3.put(&t, "a/../b", &ten, Cond::None).await.unwrap_err(),
        S3Error::Other
    );
    assert_eq!(fake.count("write"), 1);
}

#[tokio::test]
async fn a_zero_byte_put_sends_no_body() {
    let fake = fake().await;
    fake.require_keys(KEY_ID, SECRET);
    let s3 = client();
    let t = target(&s3, &fake);
    let body = PutBody {
        path: None,
        size: 0,
        sha256: empty_hash(),
    };
    let head = s3.put(&t, "dir/", &body, Cond::IfAbsent).await.unwrap();
    assert_eq!(head.size, 0);
    assert_eq!(fake.object(BUCKET, "dir/").unwrap().bytes, b"");
    assert_eq!(fake.last_write_body_len(), Some(0));
    // No path but a size is a mistake.
    let odd = PutBody {
        path: None,
        size: 3,
        sha256: empty_hash(),
    };
    assert_eq!(
        s3.put(&t, "odd", &odd, Cond::None).await.unwrap_err(),
        S3Error::Other
    );
}

#[tokio::test]
async fn delete_removes_an_object_and_a_missing_one_is_fine() {
    let fake = fake().await;
    fake.require_keys(KEY_ID, SECRET);
    fake.put(BUCKET, "gone.txt", b"x");
    let s3 = client();
    let t = target(&s3, &fake);

    s3.delete(&t, "gone.txt").await.unwrap();
    assert!(fake.object(BUCKET, "gone.txt").is_none());
    s3.delete(&t, "never-was.txt").await.unwrap();
    assert_eq!(fake.count("write"), 2);
    assert_eq!(fake.writes()[0].0, "DELETE");
    // A missing bucket is still an error.
    let nowhere = s3.target_from(&endpoint(&fake, true), "nowhere", creds(KEY_ID, SECRET));
    assert_eq!(
        s3.delete(&nowhere, "k").await.unwrap_err(),
        S3Error::NoBucket
    );
}

#[tokio::test]
async fn every_write_is_signed_and_counted_with_its_bytes_up() {
    let fake = fake().await;
    fake.require_keys(KEY_ID, SECRET);
    let s3 = client();
    let library = NodeId(Uuid::new_v4());
    let t = target(&s3, &fake).with_library(library);
    let (_a, five) = file_body(b"12345");
    let (_b, seven) = file_body(b"1234567");

    s3.put(&t, "a", &five, Cond::IfAbsent).await.unwrap();
    s3.put(&t, "b", &seven, Cond::None).await.unwrap();
    s3.delete(&t, "a").await.unwrap();

    let usage = s3.take_usage();
    assert_eq!(usage.len(), 1);
    let (counted, usage) = usage[0];
    assert_eq!(counted, library);
    assert_eq!(usage.write, 3);
    assert_eq!(usage.bytes_up, 12);
    // The HEAD after each PUT is a read.
    assert_eq!(usage.read, 2);

    let sent = fake.requests();
    let first = &sent[0];
    assert_eq!(first.method, "PUT");
    for name in [
        "authorization",
        "x-amz-date",
        "x-amz-content-sha256",
        "x-amz-meta-sharecraft-sha256",
        "content-length",
        "if-none-match",
        "host",
    ] {
        assert!(
            first.headers.iter().any(|h| h == name),
            "{name} missing from {first:?}"
        );
    }
    assert!(!sent[2].headers.iter().any(|h| h == "if-none-match"));
    assert_eq!(sent.iter().filter(|r| r.method == "DELETE").count(), 1);
}

#[tokio::test]
async fn a_read_only_key_is_denied_on_each_write() {
    let fake = fake().await;
    fake.put(BUCKET, "k.txt", b"v");
    fake.read_only();
    let s3 = client();
    let t = target(&s3, &fake);
    let (_f, body) = file_body(b"ours");

    assert_eq!(
        s3.put(&t, "k.txt", &body, Cond::None).await.unwrap_err(),
        S3Error::Denied
    );
    assert_eq!(s3.delete(&t, "k.txt").await.unwrap_err(), S3Error::Denied);
    assert_eq!(fake.object(BUCKET, "k.txt").unwrap().bytes, b"v");
    // Reads are untouched.
    s3.head(&t, "k.txt").await.unwrap();
}

#[tokio::test]
async fn a_store_that_ignores_conditions_accepts_both() {
    let fake = fake().await;
    fake.put(BUCKET, "k.txt", b"outside");
    fake.ignore_conditions();
    let s3 = client();
    let t = target(&s3, &fake);
    let (_f, body) = file_body(b"ours");

    s3.put(&t, "k.txt", &body, Cond::IfAbsent).await.unwrap();
    s3.put(&t, "k.txt", &body, Cond::IfMatch("\"not-the-tag\""))
        .await
        .unwrap();
    assert_eq!(fake.object(BUCKET, "k.txt").unwrap().bytes, b"ours");
    assert_eq!(fake.count("write"), 2);
}

// ---- copy and multipart -------------------------------------------------------------------

fn sha_of(bytes: &[u8]) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(bytes).into()
}

/// The part of `bytes` from `offset` for `len`, read from the file that holds all of `bytes`.
fn part_of(file: &tempfile::NamedTempFile, bytes: &[u8], offset: usize, len: usize) -> PartBody {
    PartBody {
        path: file.path().to_path_buf(),
        offset: offset as u64,
        len: len as u64,
        sha256: sha_of(&bytes[offset..offset + len]),
    }
}

fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

async fn source_object(s3: &S3, t: &S3Target, key: &str, bytes: &[u8]) -> String {
    let (_file, body) = file_body(bytes);
    s3.put(t, key, &body, Cond::None).await.unwrap().etag
}

#[tokio::test]
async fn a_copy_sends_the_source_and_its_etag_and_answers_the_new_head() {
    let fake = fake().await;
    fake.require_keys(KEY_ID, SECRET);
    let s3 = client();
    let t = target(&s3, &fake);
    let from = "a b/é+c.txt";
    let to = "dest dir/ü+d e.txt";
    let etag = source_object(&s3, &t, from, b"copy me").await;

    let head = s3
        .copy(&t, from, &etag, 7, to, Cond::IfAbsent)
        .await
        .unwrap();
    assert_eq!(head.size, 7);
    let stored = fake.object(BUCKET, to).unwrap();
    assert_eq!(stored.bytes, b"copy me");
    assert_eq!(head.etag, stored.etag);
    assert_eq!(head.modified_s, stored.modified_s);
    // The source's content hash travels with the copy.
    assert_eq!(stored.sha256, fake.object(BUCKET, from).unwrap().sha256);
    assert!(stored.sha256.is_some());
    assert_eq!(head.sha256, stored.sha256);
    assert!(fake.object(BUCKET, from).is_some());

    let copies: Vec<_> = fake
        .writes()
        .into_iter()
        .filter(|w| w.0 == "COPY")
        .collect();
    assert_eq!(copies, [("COPY".into(), to.into(), "absent".into())]);
    let sent = fake
        .requests()
        .into_iter()
        .find(|r| r.method == "PUT" && r.headers.iter().any(|h| h == "x-amz-copy-source"))
        .unwrap();
    for name in [
        "x-amz-copy-source-if-match",
        "if-none-match",
        "authorization",
    ] {
        assert!(sent.headers.iter().any(|h| h == name), "{name}: {sent:?}");
    }

    // With If-Match the condition word is "match"; the destination does not exist, so the
    // store refuses.
    let again = s3
        .copy(&t, from, &etag, 7, "second.txt", Cond::IfMatch("\"x\""))
        .await;
    assert_eq!(again.unwrap_err(), S3Error::Precondition);
    assert!(
        fake.writes()
            .iter()
            .any(|w| w.0 == "COPY" && w.2 == "match")
    );
    // A copy is never stored as an ordinary empty PUT.
    assert!(fake.object(BUCKET, "second.txt").is_none());
}

#[tokio::test]
async fn a_copy_from_a_changed_source_is_a_precondition_failure() {
    let fake = fake().await;
    let s3 = client();
    let t = target(&s3, &fake);
    let etag = source_object(&s3, &t, "src.txt", b"v1").await;
    // Someone else rewrites the source after we recorded its tag.
    fake.put(BUCKET, "src.txt", b"v2 from outside");

    assert_eq!(
        s3.copy(&t, "src.txt", &etag, 2, "dst.txt", Cond::IfAbsent)
            .await
            .unwrap_err(),
        S3Error::Precondition
    );
    assert!(fake.object(BUCKET, "dst.txt").is_none());
}

#[tokio::test]
async fn a_copy_onto_a_taken_key_with_if_absent_is_a_precondition_failure() {
    let fake = fake().await;
    let s3 = client();
    let t = target(&s3, &fake);
    let etag = source_object(&s3, &t, "src.txt", b"ours").await;
    fake.put(BUCKET, "dst.txt", b"theirs");

    assert_eq!(
        s3.copy(&t, "src.txt", &etag, 4, "dst.txt", Cond::IfAbsent)
            .await
            .unwrap_err(),
        S3Error::Precondition
    );
    assert_eq!(fake.object(BUCKET, "dst.txt").unwrap().bytes, b"theirs");
}

#[tokio::test]
async fn an_error_inside_a_200_copy_answer_is_a_failure() {
    let fake = fake().await;
    let s3 = client();
    let t = target(&s3, &fake);
    let etag = source_object(&s3, &t, "src.txt", b"ours").await;
    fake.error_in_200_next();

    let result = s3
        .copy(&t, "src.txt", &etag, 4, "dst.txt", Cond::IfAbsent)
        .await;
    assert_eq!(result.unwrap_err(), S3Error::Throttled);
    assert!(fake.object(BUCKET, "dst.txt").is_none());
    // It was sent once, not retried; the next copy is fine.
    assert_eq!(fake.writes().iter().filter(|w| w.0 == "COPY").count(), 1);
    s3.copy(&t, "src.txt", &etag, 4, "dst.txt", Cond::IfAbsent)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_copy_over_five_gib_is_refused_before_sending() {
    let fake = fake().await;
    let s3 = client();
    let t = target(&s3, &fake);
    let etag = source_object(&s3, &t, "big.bin", b"stand-in").await;

    assert_eq!(
        s3.copy(
            &t,
            "big.bin",
            &etag,
            COPY_MAX + 1,
            "big2.bin",
            Cond::IfAbsent
        )
        .await
        .unwrap_err(),
        S3Error::Unsupported
    );
    assert!(fake.writes().iter().all(|w| w.0 != "COPY"));
    // Exactly 5 GiB is allowed (the stand-in object is small; the size is the caller's).
    let heads = fake.count("head");
    s3.copy(&t, "big.bin", &etag, COPY_MAX, "big2.bin", Cond::IfAbsent)
        .await
        .unwrap();
    // One HEAD only (of the new object): the source is not asked for.
    assert_eq!(fake.count("head"), heads + 1);
}

#[tokio::test]
async fn a_multipart_upload_sends_hashed_parts_and_completes_with_the_condition() {
    let fake = fake().await;
    fake.require_keys(KEY_ID, SECRET);
    fake.min_part_bytes(10);
    let s3 = client();
    let t = target(&s3, &fake);
    let bytes: Vec<u8> = (0..25_u8).collect();
    let (file, _) = file_body(&bytes);
    let key = "multi/big file+é.bin";

    let id = s3.start_multipart(&t, key, &sha_of(&bytes)).await.unwrap();
    assert!(!id.is_empty());
    assert_eq!(fake.open_multiparts(), [(key.to_owned(), id.clone())]);
    let mut parts = Vec::new();
    for (number, (offset, len)) in [(0, 10), (10, 10), (20, 5)].into_iter().enumerate() {
        let number = number as u16 + 1;
        let body = part_of(&file, &bytes, offset, len);
        let etag = s3.put_part(&t, key, &id, number, &body).await.unwrap();
        parts.push((number, etag));
    }
    // A part whose bytes are not the hash it signed is refused by the store.
    let mut wrong = part_of(&file, &bytes, 0, 10);
    wrong.sha256 = [9; 32];
    assert_eq!(
        s3.put_part(&t, key, &id, 1, &wrong).await.unwrap_err(),
        S3Error::Other
    );

    let head = s3
        .complete_multipart(&t, key, &id, &parts, Cond::IfAbsent)
        .await
        .unwrap();
    assert_eq!(head.size, 25);
    let stored = fake.object(BUCKET, key).unwrap();
    assert_eq!(stored.bytes, bytes);
    assert_eq!(
        stored.sha256.as_deref(),
        Some(hex(&sha_of(&bytes)).as_str())
    );
    assert_eq!(head.sha256, stored.sha256);
    assert_eq!(head.etag, stored.etag);
    assert!(fake.open_multiparts().is_empty());
    let kinds: Vec<_> = fake.writes().into_iter().map(|w| (w.0, w.2)).collect();
    let expected: Vec<(String, String)> = [
        ("PART", "1"),
        ("PART", "2"),
        ("PART", "3"),
        ("PART", "1"),
        ("COMPLETE", "absent"),
    ]
    .into_iter()
    .map(|(a, b)| (a.to_owned(), b.to_owned()))
    .collect();
    assert_eq!(kinds, expected);

    // The condition is honoured at completion: the key is taken now.
    let id = s3.start_multipart(&t, key, &sha_of(&bytes)).await.unwrap();
    let last = part_of(&file, &bytes, 20, 5);
    let etag = s3.put_part(&t, key, &id, 1, &last).await.unwrap();
    assert_eq!(
        s3.complete_multipart(&t, key, &id, &[(1, etag.clone())], Cond::IfAbsent)
            .await
            .unwrap_err(),
        S3Error::Precondition
    );
    assert_eq!(fake.object(BUCKET, key).unwrap().bytes, bytes);

    // A failing part is an error, sent once.
    fake.fail_part(2);
    let body = part_of(&file, &bytes, 0, 10);
    assert!(s3.put_part(&t, key, &id, 2, &body).await.is_err());
    // A part too small for the middle of an upload is refused at completion.
    let small = part_of(&file, &bytes, 0, 3);
    let tiny = s3.put_part(&t, key, &id, 3, &small).await.unwrap();
    assert_eq!(
        s3.complete_multipart(&t, key, &id, &[(1, etag), (3, tiny)], Cond::None)
            .await
            .unwrap_err(),
        S3Error::Other
    );
    assert_eq!(fake.object(BUCKET, key).unwrap().bytes, bytes);
}

#[tokio::test]
async fn a_short_file_part_is_refused() {
    let fake = fake().await;
    let s3 = client();
    let t = target(&s3, &fake);
    let bytes = b"only twelve!".to_vec();
    let (file, _) = file_body(&bytes);
    let id = s3.start_multipart(&t, "k", &sha_of(&bytes)).await.unwrap();

    // Asks for bytes past the end of the file.
    let mut body = part_of(&file, &bytes, 4, 8);
    body.len = 20;
    assert_eq!(
        s3.put_part(&t, "k", &id, 1, &body).await.unwrap_err(),
        S3Error::Other
    );
    body.offset = 100;
    body.len = 1;
    assert_eq!(
        s3.put_part(&t, "k", &id, 1, &body).await.unwrap_err(),
        S3Error::Other
    );
    // Numbers run from 1 to 10,000.
    let ok = part_of(&file, &bytes, 0, 4);
    for number in [0_u16, 10_001] {
        assert_eq!(
            s3.put_part(&t, "k", &id, number, &ok).await.unwrap_err(),
            S3Error::Other
        );
    }
    // Nothing was sent for any of them.
    assert!(fake.writes().iter().all(|w| w.0 != "PART"));
    // The last part, ending exactly at the end of the file, is fine.
    let last = part_of(&file, &bytes, 4, 8);
    s3.put_part(&t, "k", &id, 1, &last).await.unwrap();
    assert_eq!(fake.writes().iter().filter(|w| w.0 == "PART").count(), 1);
}

#[tokio::test]
async fn an_error_inside_a_200_completion_is_a_failure() {
    let fake = fake().await;
    let s3 = client();
    let t = target(&s3, &fake);
    let bytes = b"abc".to_vec();
    let (file, _) = file_body(&bytes);
    let id = s3.start_multipart(&t, "k", &sha_of(&bytes)).await.unwrap();
    let etag = s3
        .put_part(&t, "k", &id, 1, &part_of(&file, &bytes, 0, 3))
        .await
        .unwrap();
    fake.error_in_200_next();

    let result = s3
        .complete_multipart(&t, "k", &id, &[(1, etag.clone())], Cond::None)
        .await;
    assert_eq!(result.unwrap_err(), S3Error::Throttled);
    assert!(fake.object(BUCKET, "k").is_none());
    // The upload is still there, so the caller can abort it.
    assert_eq!(fake.open_multiparts().len(), 1);
    s3.complete_multipart(&t, "k", &id, &[(1, etag)], Cond::None)
        .await
        .unwrap();
    assert_eq!(fake.object(BUCKET, "k").unwrap().bytes, bytes);
}

#[tokio::test]
async fn abort_forgets_the_upload_and_a_missing_upload_is_fine() {
    let fake = fake().await;
    fake.require_keys(KEY_ID, SECRET);
    let s3 = client();
    let t = target(&s3, &fake);
    let id = s3.start_multipart(&t, "k", &[1; 32]).await.unwrap();
    assert_eq!(fake.open_multiparts().len(), 1);

    s3.abort_multipart(&t, "k", &id).await.unwrap();
    assert!(fake.open_multiparts().is_empty());
    // Gone already: NoSuchUpload counts as done.
    s3.abort_multipart(&t, "k", &id).await.unwrap();
    let aborts = fake.writes().iter().filter(|w| w.0 == "ABORT").count();
    assert_eq!(aborts, 2);
    // A part for an upload that is gone is NotFound.
    let (file, _) = file_body(b"xyz");
    let body = part_of(&file, b"xyz", 0, 3);
    assert_eq!(
        s3.put_part(&t, "k", &id, 1, &body).await.unwrap_err(),
        S3Error::NotFound
    );
    // Other failures are still failures.
    fake.fail_next(403, "AccessDenied");
    assert_eq!(
        s3.abort_multipart(&t, "k", &id).await.unwrap_err(),
        S3Error::Denied
    );
}

#[tokio::test]
async fn a_head_404_is_not_logged_and_a_put_403_is() {
    // The log line is decided by one function: a 404 to a HEAD or a DELETE is quiet.
    assert_eq!(failure_log_line("HEAD", 404, None), None);
    assert_eq!(failure_log_line("DELETE", 404, Some("NoSuchUpload")), None);
    assert_eq!(
        failure_log_line("GET", 404, Some("NoSuchKey")).as_deref(),
        Some("sharecraft-server: S3 answered 404 NoSuchKey")
    );
    assert_eq!(
        failure_log_line("PUT", 403, Some("AccessDenied")).as_deref(),
        Some("sharecraft-server: S3 answered 403 AccessDenied")
    );
    assert_eq!(
        failure_log_line("HEAD", 403, None).as_deref(),
        Some("sharecraft-server: S3 answered 403 (no code)")
    );
    assert!(failure_log_line("PUT", 404, Some("NoSuchBucket")).is_some());

    // And the calls still answer as before.
    let fake = fake().await;
    let s3 = client();
    let t = target(&s3, &fake);
    assert_eq!(s3.head(&t, "missing").await.unwrap_err(), S3Error::NotFound);
    s3.delete(&t, "missing").await.unwrap();
    fake.read_only();
    let (_f, body) = file_body(b"x");
    assert_eq!(
        s3.put(&t, "k", &body, Cond::None).await.unwrap_err(),
        S3Error::Denied
    );
}

#[tokio::test]
async fn copies_and_parts_are_counted_as_writes() {
    let fake = fake().await;
    fake.min_part_bytes(1);
    let s3 = client();
    let library = NodeId(Uuid::new_v4());
    let t = target(&s3, &fake).with_library(library);
    let etag = source_object(&s3, &t, "src.txt", b"12345").await;
    let _ = s3.take_usage();

    s3.copy(&t, "src.txt", &etag, 5, "dst.txt", Cond::IfAbsent)
        .await
        .unwrap();
    let bytes = b"abcdefg".to_vec();
    let (file, _) = file_body(&bytes);
    let id = s3.start_multipart(&t, "m", &sha_of(&bytes)).await.unwrap();
    let e1 = s3
        .put_part(&t, "m", &id, 1, &part_of(&file, &bytes, 0, 4))
        .await
        .unwrap();
    let e2 = s3
        .put_part(&t, "m", &id, 2, &part_of(&file, &bytes, 4, 3))
        .await
        .unwrap();
    s3.complete_multipart(&t, "m", &id, &[(1, e1), (2, e2)], Cond::None)
        .await
        .unwrap();
    let other = s3.start_multipart(&t, "n", &sha_of(&bytes)).await.unwrap();
    s3.abort_multipart(&t, "n", &other).await.unwrap();

    let usage = s3.take_usage();
    assert_eq!(usage.len(), 1);
    let (counted, usage) = usage[0];
    assert_eq!(counted, library);
    // copy 1, start 1, parts 2, complete 1, start 1, abort 1.
    assert_eq!(usage.write, 7);
    // Parts count their bytes up; the copy, the documents and the HEADs count none.
    assert_eq!(usage.bytes_up, 7);
    // The HEAD of the new object after the copy, and the HEAD after completion.
    assert_eq!(usage.read, 2);
}
