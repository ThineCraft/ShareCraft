#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::net::IpAddr;

use common::user;
use records::test_support::TestDb;
use records::{
    AccessError, EndpointRefusal, IpRange, S3_BYTES_DOWN, S3_ENDPOINTS, S3_LIST_REQUESTS,
    S3_READ_REQUESTS, S3Endpoint, address_allowed, aws_endpoint, clean_prefix, prefixes_overlap,
    valid_bucket,
};
use serde_json::json;

fn endpoint(
    url: &str,
    allow: &[&str],
    plain_loopback: bool,
) -> Result<S3Endpoint, EndpointRefusal> {
    S3Endpoint::parse(
        "store",
        url,
        "auto",
        true,
        &allow
            .iter()
            .map(|value| (*value).to_string())
            .collect::<Vec<_>>(),
        plain_loopback,
    )
}

#[test]
fn parses_an_https_endpoint_and_normalises_the_host() {
    let parsed = endpoint("https://Storage.Example.COM.:8443/", &[], false).unwrap();
    assert_eq!(parsed.name, "store");
    assert_eq!(parsed.url, "https://storage.example.com:8443");
    assert_eq!(parsed.host(), "storage.example.com");
    assert_eq!(parsed.port(), Some(8443));
    assert!(parsed.https());
}

#[test]
fn refuses_http_paths_queries_credentials_and_fragments() {
    for (url, refusal) in [
        ("http://x", EndpointRefusal::NotHttps),
        ("https://x/a", EndpointRefusal::HasPath),
        ("https://x?y", EndpointRefusal::HasQuery),
        ("https://u:p@x", EndpointRefusal::HasCredentials),
        ("https://x#f", EndpointRefusal::HasQuery),
        ("ftp://x", EndpointRefusal::NotHttps),
        ("HTTPS://x", EndpointRefusal::NotHttps),
        ("x", EndpointRefusal::NotHttps),
    ] {
        assert_eq!(endpoint(url, &[], false), Err(refusal), "accepted {url:?}");
    }
}

#[test]
fn refuses_an_address_literal_unless_it_is_in_an_allowed_range() {
    for url in [
        "https://10.0.0.5",
        "https://[::1]",
        "https://169.254.169.254",
        "https://2130706433",
        "https://0x7f.1",
        "https://0177.0.0.1",
        "https://127.1",
    ] {
        assert_eq!(
            endpoint(url, &[], false),
            Err(EndpointRefusal::AddressLiteral),
            "accepted {url:?}"
        );
    }
    let parsed = endpoint("https://10.0.0.5", &["10.0.0.0/24"], false).unwrap();
    assert_eq!(parsed.host(), "10.0.0.5");
}

#[test]
fn plain_loopback_is_accepted_only_with_the_switch() {
    assert_eq!(
        endpoint("http://127.0.0.1:3456", &[], false),
        Err(EndpointRefusal::NotHttps)
    );
    let parsed = endpoint("http://127.0.0.1:3456", &[], true).unwrap();
    assert!(!parsed.https());
    assert_eq!(parsed.port(), Some(3456));
    for bad in [
        "http://127.0.0.1",
        "http://127.0.0.2:3456",
        "http://localhost:3456",
        "http://[::1]:3456",
        "http://2130706433:3456",
        "http://0177.0.0.1:3456",
        "http://127.1:3456",
    ] {
        assert_eq!(
            endpoint(bad, &[], true),
            Err(EndpointRefusal::NotHttps),
            "accepted {bad:?}"
        );
    }
}

#[test]
fn aws_names_build_the_regional_address_and_are_reserved() {
    let aws = aws_endpoint("aws:eu-west-2").unwrap();
    assert_eq!(aws.name, "aws:eu-west-2");
    assert_eq!(aws.url, "https://s3.eu-west-2.amazonaws.com");
    assert_eq!(aws.region, "eu-west-2");
    assert!(!aws.path_style);
    assert!(aws_endpoint("aws:auto").is_none());
    assert!(aws_endpoint("AWS:eu-west-2").is_none());
    assert_eq!(
        S3Endpoint::parse("aws-store", "https://x", "auto", true, &[], false),
        Err(EndpointRefusal::Reserved)
    );
}

#[test]
fn every_private_loopback_link_local_and_special_address_is_refused() {
    for text in [
        "0.0.0.0",
        "0.1.2.3",
        "10.0.0.1",
        "100.64.0.1",
        "127.0.0.1",
        "169.254.169.254",
        "172.16.0.1",
        "192.0.0.1",
        "192.0.2.1",
        "192.168.0.1",
        "198.18.0.1",
        "198.51.100.1",
        "203.0.113.1",
        "224.0.0.1",
        "240.0.0.1",
        "255.255.255.255",
        "::",
        "::1",
        "::ffff:10.0.0.1",
        "::ffff:169.254.169.254",
        "64:ff9b::127.0.0.1",
        "2001:2::1",
        "2001:db8::1",
        "2002:c000:0204::1",
        "2001:0:4136:e378:8000:63bf:3fff:fdd2",
        "64:ff9b:1::1",
        "fc00::1",
        "fe80::1",
        "ff02::1",
    ] {
        let ip: IpAddr = text.parse().unwrap();
        assert!(!address_allowed(ip, &[]), "allowed {text}");
    }
}

#[test]
fn an_allowed_range_admits_only_its_addresses() {
    let range = IpRange::parse("10.20.30.0/24").unwrap();
    assert_eq!(range.display(), "10.20.30.0/24");
    assert!(range.contains("10.20.30.7".parse().unwrap()));
    assert!(!range.contains("10.20.31.7".parse().unwrap()));
    assert!(address_allowed(
        "10.20.30.7".parse().unwrap(),
        std::slice::from_ref(&range)
    ));
    assert!(!address_allowed(
        "10.20.31.7".parse().unwrap(),
        std::slice::from_ref(&range)
    ));
}

#[test]
fn a_public_address_is_allowed() {
    assert!(address_allowed("8.8.8.8".parse().unwrap(), &[]));
    assert!(address_allowed(
        "2606:4700:4700::1111".parse().unwrap(),
        &[]
    ));
}

#[test]
fn over_wide_ranges_are_refused() {
    for range in ["10.0.0.0/7", "2001:db8::/15", "bad", "10.0.0.0/33"] {
        assert_eq!(
            endpoint("https://storage.example", &[range], false),
            Err(EndpointRefusal::BadRange),
            "accepted {range:?}"
        );
    }
    let too_many = (0..9).map(|n| format!("10.{n}.0.0/16")).collect::<Vec<_>>();
    assert_eq!(
        S3Endpoint::parse(
            "store",
            "https://storage.example",
            "auto",
            true,
            &too_many,
            false
        ),
        Err(EndpointRefusal::BadRange)
    );
}

#[test]
fn bucket_names_follow_the_rules() {
    assert!(valid_bucket("my-bucket.1"));
    for bad in [
        "ab",
        "A",
        "a..b",
        "-abc",
        "abc-",
        "192.168.1.1",
        "a/b",
        "a@b",
        "a:1",
    ] {
        assert!(!valid_bucket(bad), "accepted {bad:?}");
    }
}

#[test]
fn prefixes_are_cleaned_and_bad_ones_refused() {
    assert_eq!(clean_prefix(""), Some(String::new()));
    assert_eq!(clean_prefix("a/b"), Some("a/b/".to_string()));
    assert_eq!(clean_prefix("a/b/"), Some("a/b/".to_string()));
    for bad in ["/a", "a//b", "a/../b", "a/./b", "a\nb"] {
        assert_eq!(clean_prefix(bad), None, "accepted {bad:?}");
    }
    assert!(clean_prefix(&"x".repeat(511)).is_some());
    assert!(clean_prefix(&"x".repeat(512)).is_none());
}

#[test]
fn prefix_overlap_is_by_whole_segments() {
    assert!(prefixes_overlap("", "a/"));
    assert!(prefixes_overlap("a/", "a/"));
    assert!(prefixes_overlap("a/", "a/b/"));
    assert!(prefixes_overlap("a/b/", "a/"));
    assert!(!prefixes_overlap("a/", "ab/"));
    assert!(!prefixes_overlap("a/b/", "a/bc/"));
}

#[test]
fn endpoint_review_edge_cases_are_refused_or_normalised() {
    for url in [
        "https://[fe80::1%25eth0]",
        "https://x:0",
        "https://x:65536",
        "https://only_digits.123",
        "https://x\u{3002}example",
    ] {
        assert!(endpoint(url, &[], false).is_err(), "accepted {url:?}");
    }
    assert_eq!(
        endpoint("https://Example.COM./", &[], false)
            .unwrap()
            .host(),
        "example.com"
    );
}

#[tokio::test]
async fn the_endpoints_setting_refuses_bad_entries_and_non_administrators() {
    let test = TestDb::create().await;
    let admin = user(&test, "Admin", true).await;
    let member = user(&test, "Member", false).await;
    let valid = json!([{
        "name": "r2",
        "url": "https://account.r2.cloudflarestorage.com",
        "region": "auto",
        "path_style": true,
        "allow": []
    }]);
    assert_eq!(
        test.db
            .set_setting(member.id, S3_ENDPOINTS, valid.clone())
            .await,
        Err(AccessError::Forbidden)
    );
    for bad in [
        json!({}),
        json!([{"name": "r2"}]),
        json!([{
            "name": "r2", "url": "http://x", "region": "auto",
            "path_style": true, "allow": []
        }]),
        json!([
            {"name": "same", "url": "https://one.example", "region": "auto", "path_style": true, "allow": []},
            {"name": "same", "url": "https://two.example", "region": "auto", "path_style": true, "allow": []}
        ]),
        json!(
            (0..21)
                .map(|n| json!({
                    "name": format!("store-{n}"), "url": format!("https://store-{n}.example"),
                    "region": "auto", "path_style": true, "allow": []
                }))
                .collect::<Vec<_>>()
        ),
    ] {
        assert_eq!(
            test.db.set_setting(admin.id, S3_ENDPOINTS, bad).await,
            Err(AccessError::InvalidSetting)
        );
    }
    test.db
        .set_setting(admin.id, S3_ENDPOINTS, valid)
        .await
        .unwrap();
}

#[tokio::test]
async fn s3_settings_have_defaults_and_read_back() {
    let test = TestDb::create().await;
    let admin = user(&test, "Admin", true).await;
    let defaults = test.db.s3_settings().await.unwrap();
    assert!(defaults.endpoints.is_empty());
    assert_eq!(defaults.list_requests, 100_000);
    assert_eq!(defaults.read_requests, 1_000_000);
    assert_eq!(defaults.bytes_down, None);

    test.db
        .set_setting(
            admin.id,
            S3_ENDPOINTS,
            json!([{
                "name": "r2", "url": "https://Account.R2.CloudflareStorage.com/", "region": "auto",
                "path_style": true, "allow": ["10.20.30.0/24"]
            }]),
        )
        .await
        .unwrap();
    test.db
        .set_setting(admin.id, S3_LIST_REQUESTS, json!(2_000))
        .await
        .unwrap();
    test.db
        .set_setting(admin.id, S3_READ_REQUESTS, json!(3_000))
        .await
        .unwrap();
    test.db
        .set_setting(admin.id, S3_BYTES_DOWN, json!(1_073_741_824_u64))
        .await
        .unwrap();

    let settings = test.db.s3_settings().await.unwrap();
    assert_eq!(settings.endpoints.len(), 1);
    assert_eq!(
        settings.endpoints[0].url,
        "https://account.r2.cloudflarestorage.com"
    );
    assert_eq!(settings.list_requests, 2_000);
    assert_eq!(settings.read_requests, 3_000);
    assert_eq!(settings.bytes_down, Some(1_073_741_824));

    for (key, bad) in [
        (S3_LIST_REQUESTS, json!(999)),
        (S3_READ_REQUESTS, json!("1000")),
        (S3_BYTES_DOWN, json!(1_073_741_823_u64)),
    ] {
        assert_eq!(
            test.db.set_setting(admin.id, key, bad).await,
            Err(AccessError::InvalidSetting)
        );
    }
    test.db
        .set_setting(admin.id, S3_BYTES_DOWN, json!(null))
        .await
        .unwrap();
    assert_eq!(test.db.s3_settings().await.unwrap().bytes_down, None);

    sqlx::query("update settings set value = $1 where key = $2")
        .bind(json!([
            {"name": "good", "url": "https://good.example", "region": "auto", "path_style": true, "allow": []},
            {"name": "bad", "url": "http://bad.example", "region": "auto", "path_style": true, "allow": []}
        ]))
        .bind(S3_ENDPOINTS)
        .execute(test.db.test_pool())
        .await
        .unwrap();
    let settings = test.db.s3_settings().await.unwrap();
    assert_eq!(settings.endpoints.len(), 1);
    assert_eq!(settings.endpoints[0].name, "good");
}
