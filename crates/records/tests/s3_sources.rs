#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use collab_core::ids::{NodeId, UserId};
use common::{document_in, new_id, put, site, user};
use records::test_support::TestDb;
use records::{
    AccessError, Budget, ExternalEntry, NewSource, NodeChange, NodeKind, NodeRow, S3_BYTES_DOWN,
    S3_LIST_REQUESTS, S3Place, SealedRow, SecretId, SourceKind, SourceMode, SourceState,
    SourceStatus, Tri, Unavailable, Usage, Versioning, external_tag, external_tag_parts,
    object_tag, s3_location,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::Row;
use time::{Date, Month, OffsetDateTime};

struct Fixture {
    test: TestDb,
    user: UserId,
    site: NodeId,
    library: NodeId,
}

/// Approves the endpoint `r2` as an administrator would: a bucket is connected only on an
/// approved endpoint.
async fn approve_r2(test: &TestDb, admin: UserId) {
    test.db
        .set_setting(
            admin,
            records::S3_ENDPOINTS,
            json!([{
                "name": "r2",
                "url": "https://r2.example.com",
                "region": "auto",
                "path_style": true,
                "allow": [],
            }]),
        )
        .await
        .unwrap();
}

async fn fixture() -> Fixture {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", true).await;
    approve_r2(&test, ann.id).await;
    let site = site(&test, "Site", Some(ann.id)).await;
    let library = put(
        &test,
        new_id(),
        NodeKind::Library,
        Some(site),
        site,
        "Bucket",
    )
    .await;
    Fixture {
        test,
        user: ann.id,
        site,
        library,
    }
}

fn sealed(bytes: &[u8]) -> SealedRow {
    SealedRow {
        key_id: 1,
        nonce: [7; 12],
        ciphertext: bytes.to_vec(),
    }
}

async fn secret(f: &Fixture, bytes: &[u8]) -> SecretId {
    f.test.db.put_secret(&sealed(bytes)).await.unwrap()
}

fn place(bucket: &str, prefix: &str) -> S3Place {
    S3Place {
        endpoint: "r2".into(),
        bucket: bucket.into(),
        prefix: prefix.into(),
        access_key_id: "AKIAEXAMPLE".into(),
        versioning: Versioning::Unknown,
        conditional: Tri::Unknown,
    }
}

fn s3_source(f: &Fixture, library: NodeId, place: &S3Place, token: SecretId) -> NewSource {
    NewSource {
        library,
        kind: SourceKind::S3,
        owner: String::new(),
        repo: String::new(),
        branch: String::new(),
        folder: String::new(),
        location: Some(s3_location(&place.endpoint, &place.bucket, &place.prefix)),
        mode: SourceMode::ReadOnly,
        token: Some(token),
        poll_seconds: 300,
        connected_by: f.user,
    }
}

async fn connect(
    f: &Fixture,
    library: NodeId,
    bucket: &str,
    prefix: &str,
) -> Result<(), AccessError> {
    let place = place(bucket, prefix);
    let token = secret(f, b"sealed key").await;
    f.test
        .db
        .connect_s3_source(&s3_source(f, library, &place, token), &place)
        .await
}

async fn library(f: &Fixture, name: &str) -> NodeId {
    put(
        &f.test,
        new_id(),
        NodeKind::Library,
        Some(f.site),
        f.site,
        name,
    )
    .await
}

async fn external_document(f: &Fixture, library: NodeId, name: &str) -> NodeId {
    document_in(&f.test, f.user, library, f.site, name)
        .await
        .into()
}

fn entry(node: NodeId, path: &str, size: u64, mtime_ns: i64, tag: Option<String>) -> ExternalEntry {
    ExternalEntry {
        node,
        path: path.into(),
        repository_path: path.into(),
        size,
        mtime_ns,
        tag,
    }
}

fn status(state: SourceState, error: Option<&str>) -> SourceStatus {
    SourceStatus {
        state,
        last_commit: None,
        etag: None,
        last_checked_at: None,
        last_synced_at: None,
        last_error: error.map(str::to_owned),
        retry_at: None,
        files: 0,
        bytes: 0,
        hidden: 0,
        hidden_detail: json!({}),
        next_check_at: None,
    }
}

fn month(year: i32, month: Month, day: u8) -> Date {
    Date::from_calendar_date(year, month, day).unwrap()
}

async fn count(f: &Fixture, table: &str, library: NodeId) -> i64 {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "select count(*) from {table} where library_id = $1"
    )))
    .bind(library.0)
    .fetch_one(f.test.db.test_pool())
    .await
    .unwrap()
}

#[tokio::test]
async fn connect_an_s3_source_then_read_back_place_and_kind() {
    let f = fixture().await;
    let place = place("photos-bucket", "2026/raw/");
    let token = secret(&f, b"sealed key").await;
    f.test
        .db
        .connect_s3_source(&s3_source(&f, f.library, &place, token), &place)
        .await
        .unwrap();

    let (config, status) = f.test.db.source_of(f.library).await.unwrap().unwrap();
    assert_eq!(config.kind, SourceKind::S3);
    assert_eq!(
        config.location.as_deref(),
        Some("s3://r2/photos-bucket/2026/raw/")
    );
    assert_eq!(config.mode, SourceMode::ReadOnly);
    assert_eq!(config.token, Some(token));
    assert_eq!((config.owner.as_str(), config.repo.as_str()), ("", ""));
    assert_eq!((config.branch.as_str(), config.folder.as_str()), ("", ""));
    assert_eq!(status.state, SourceState::Connecting);
    assert_eq!(
        f.test.db.s3_place(f.library).await.unwrap(),
        Some(place.clone())
    );

    f.test
        .db
        .set_s3_probe(f.library, Versioning::Enabled, Tri::Yes)
        .await
        .unwrap();
    let probed = f.test.db.s3_place(f.library).await.unwrap().unwrap();
    assert_eq!(
        (probed.versioning, probed.conditional),
        (Versioning::Enabled, Tri::Yes)
    );
    assert_eq!(
        f.test.db.s3_place(new_id()).await.unwrap(),
        None,
        "a library with no connection has no place"
    );
}

#[tokio::test]
async fn connect_source_refuses_the_s3_kind_and_connect_s3_refuses_others() {
    let f = fixture().await;
    let place = place("photos-bucket", "");
    let token = secret(&f, b"sealed key").await;
    let good = s3_source(&f, f.library, &place, token);

    assert_eq!(
        f.test.db.connect_source(&good).await,
        Err(AccessError::InvalidSetting)
    );
    for kind in [SourceKind::Github, SourceKind::Share] {
        let mut other = good.clone();
        other.kind = kind;
        assert_eq!(
            f.test.db.connect_s3_source(&other, &place).await,
            Err(AccessError::InvalidSetting),
            "{kind:?}"
        );
    }
    // A location that is not the place's, a bad bucket, an unclean prefix, no key.
    let mut elsewhere = good.clone();
    elsewhere.location = Some("s3://r2/other-bucket/".into());
    assert_eq!(
        f.test.db.connect_s3_source(&elsewhere, &place).await,
        Err(AccessError::InvalidSetting)
    );
    for (bucket, prefix) in [
        ("A.b", ""),
        ("ab", ""),
        ("photos-bucket", "/x/"),
        ("photos-bucket", "x"),
    ] {
        let bad = self::place(bucket, prefix);
        assert_eq!(
            f.test
                .db
                .connect_s3_source(&s3_source(&f, f.library, &bad, token), &bad)
                .await,
            Err(AccessError::InvalidSetting),
            "{bucket:?} {prefix:?}"
        );
    }
    let mut keyless = good.clone();
    keyless.token = None;
    assert_eq!(
        f.test.db.connect_s3_source(&keyless, &place).await,
        Err(AccessError::InvalidSetting)
    );
    assert!(f.test.db.source_of(f.library).await.unwrap().is_none());
    assert_eq!(count(&f, "source_s3", f.library).await, 0);
}

#[tokio::test]
async fn overlapping_prefixes_in_one_bucket_are_refused_both_ways_and_other_buckets_are_not() {
    let f = fixture().await;
    let outer = f.library;
    let inner = library(&f, "Inner").await;
    let sibling = library(&f, "Sibling").await;
    let elsewhere = library(&f, "Elsewhere").await;
    let whole = library(&f, "Whole").await;

    connect(&f, outer, "shared-bucket", "team/").await.unwrap();
    // Inside the first, then a sibling that only shares leading letters.
    assert_eq!(
        connect(&f, inner, "shared-bucket", "team/docs/").await,
        Err(AccessError::Exists)
    );
    connect(&f, sibling, "shared-bucket", "teammates/")
        .await
        .unwrap();
    // Another bucket on the same endpoint does not matter.
    connect(&f, elsewhere, "other-bucket", "team/")
        .await
        .unwrap();
    // The whole bucket overlaps every prefix in it.
    assert_eq!(
        connect(&f, whole, "shared-bucket", "").await,
        Err(AccessError::Exists)
    );

    // And the other way round: a containing prefix after a contained one.
    let deep = library(&f, "Deep").await;
    let wide = library(&f, "Wide").await;
    connect(&f, deep, "nested-bucket", "a/b/c/").await.unwrap();
    assert_eq!(
        connect(&f, wide, "nested-bucket", "a/").await,
        Err(AccessError::Exists)
    );
    assert_eq!(count(&f, "source_s3", wide).await, 0);
    assert!(f.test.db.source_of(wide).await.unwrap().is_none());

    // A refused connection leaves no secret referrer behind it, and a freed prefix is free.
    f.test.db.disconnect_source(deep).await.unwrap();
    connect(&f, wide, "nested-bucket", "a/").await.unwrap();
}

#[tokio::test]
async fn a_connection_is_refused_once_its_endpoint_is_no_longer_approved() {
    let f = fixture().await;
    f.test
        .db
        .set_setting(f.user, records::S3_ENDPOINTS, json!([]))
        .await
        .unwrap();
    assert_eq!(
        connect(&f, f.library, "withdrawn-bucket", "").await,
        Err(AccessError::S3EndpointNotApproved)
    );
    assert!(f.test.db.source_of(f.library).await.unwrap().is_none());
    // The built-in AWS endpoints need no approval.
    let mut place = place("aws-bucket", "");
    place.endpoint = "aws:eu-west-2".into();
    let token = secret(&f, b"sealed key").await;
    f.test
        .db
        .connect_s3_source(&s3_source(&f, f.library, &place, token), &place)
        .await
        .unwrap();
}

#[tokio::test]
async fn two_connects_at_once_to_one_prefix_make_one_library() {
    let f = fixture().await;
    for round in 0..5 {
        let bucket = format!("race-bucket-{round}");
        let first = library(&f, &format!("First {round}")).await;
        let second = library(&f, &format!("Second {round}")).await;
        let (a, b) = tokio::join!(
            connect(&f, first, &bucket, "same/"),
            connect(&f, second, &bucket, "same/")
        );
        let results = [a, b];
        assert_eq!(
            results.iter().filter(|r| r.is_ok()).count(),
            1,
            "round {round}: {results:?}"
        );
        assert!(
            results.contains(&Err(AccessError::Exists)),
            "round {round}: {results:?}"
        );
        let rows: i64 = sqlx::query_scalar("select count(*) from source_s3 where bucket = $1")
            .bind(&bucket)
            .fetch_one(f.test.db.test_pool())
            .await
            .unwrap();
        assert_eq!(rows, 1, "round {round}");
    }
}

#[test]
fn object_tags_parse_back_and_differ_when_only_the_etag_differs() {
    let tag = object_tag(5, 1_700_000_000, "\"abc123\"", false);
    let digest = Sha256::digest(b"\"abc123\"");
    let expected: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    assert_eq!(tag, format!("x5-1700000000000000000~{expected}"));
    let parts = external_tag_parts(&tag, 5).unwrap();
    assert_eq!(parts.mtime_ns, 1_700_000_000_000_000_000);
    assert_eq!(parts.etag.as_deref(), Some(expected.as_str()));
    assert!(!parts.archived);

    let other = object_tag(5, 1_700_000_000, "\"abc124\"", false);
    assert_ne!(tag, other);
    assert_eq!(
        external_tag_parts(&other, 5).unwrap().mtime_ns,
        parts.mtime_ns
    );

    let archived = object_tag(5, 1_700_000_000, "\"abc123\"", true);
    assert_eq!(archived, format!("{tag}!a"));
    let archived_parts = external_tag_parts(&archived, 5).unwrap();
    assert!(archived_parts.archived);
    assert_eq!(archived_parts.etag, parts.etag);

    // The tag names its size; other shapes are not tags.
    assert_eq!(external_tag_parts(&tag, 6), None);
    for bad in [
        "",
        "x",
        "x5",
        "x5-",
        "x5-abc",
        "x5-1~",
        "x5-1~ABCDEF0123456789",
        "x5-1~abcdef012345678",
        "x5-1~abcdef01234567890",
        "x5-1!a",
        "5-1",
    ] {
        assert_eq!(external_tag_parts(bad, 5), None, "{bad:?}");
    }
}

#[tokio::test]
async fn share_tags_still_parse_and_record_as_before() {
    let f = fixture().await;
    assert_eq!(external_tag(5, 1700), "x5-1700");
    let parts = external_tag_parts("x5-1700", 5).unwrap();
    assert_eq!(
        (parts.mtime_ns, parts.etag, parts.archived),
        (1700, None, false)
    );
    assert_eq!(external_tag_parts("x5--20", 5).unwrap().mtime_ns, -20);

    f.test
        .db
        .connect_source(&NewSource {
            library: f.library,
            kind: SourceKind::Share,
            owner: "ignored".into(),
            repo: "ignored".into(),
            branch: "ignored".into(),
            folder: "ignored".into(),
            location: Some(r"C:\Approved\Files".into()),
            mode: SourceMode::ReadOnly,
            token: None,
            poll_seconds: 300,
            connected_by: f.user,
        })
        .await
        .unwrap();
    let node = external_document(&f, f.library, "Notes.txt").await;
    f.test
        .db
        .record_external_item(f.library, &entry(node, "Notes.txt", 17, 1000, None))
        .await
        .unwrap();
    let item = f.test.db.external_item(node).await.unwrap().unwrap();
    assert_eq!(item.kind, SourceKind::Share);
    assert_eq!(item.tag, "x17-1000");
    assert_eq!((item.size, item.mtime_ns), (17, 1000));
    assert_eq!(item.unavailable, None);
    assert_eq!(item.version, None);
    assert_eq!(item.location, r"C:\Approved\Files");

    // An entry's own tag is what is stored.
    let own = external_tag(17, 1000);
    f.test
        .db
        .record_external_item(
            f.library,
            &entry(node, "Notes.txt", 17, 1000, Some(own.clone())),
        )
        .await
        .unwrap();
    assert_eq!(
        f.test.db.external_item(node).await.unwrap().unwrap().tag,
        own
    );
}

#[tokio::test]
async fn an_archived_tag_marks_the_item_unavailable_and_a_restored_one_clears_it() {
    let f = fixture().await;
    connect(&f, f.library, "archive-bucket", "").await.unwrap();
    let node = external_document(&f, f.library, "Old.txt").await;
    let seconds = 1_700_000_000;
    let archived = object_tag(9, seconds, "\"e1\"", true);
    let restored = object_tag(9, seconds, "\"e1\"", false);
    let ns = seconds * 1_000_000_000;

    f.test
        .db
        .record_external_item(
            f.library,
            &entry(node, "Old.txt", 9, ns, Some(archived.clone())),
        )
        .await
        .unwrap();
    let item = f.test.db.external_item(node).await.unwrap().unwrap();
    assert_eq!(item.tag, archived);
    assert_eq!(item.unavailable, Some(Unavailable::Archived));

    f.test
        .db
        .record_external_item(
            f.library,
            &entry(node, "Old.txt", 9, ns, Some(restored.clone())),
        )
        .await
        .unwrap();
    let item = f.test.db.external_item(node).await.unwrap().unwrap();
    assert_eq!(item.tag, restored);
    assert_eq!(
        item.unavailable, None,
        "a restored object is readable again"
    );

    // Moving the tag with set_external_seen does the same, in both directions.
    assert!(
        f.test
            .db
            .set_external_seen(node, &restored, &archived, 9)
            .await
            .unwrap()
    );
    assert_eq!(
        f.test
            .db
            .external_item(node)
            .await
            .unwrap()
            .unwrap()
            .unavailable,
        Some(Unavailable::Archived)
    );
    assert!(
        f.test
            .db
            .set_external_seen(node, &archived, &restored, 9)
            .await
            .unwrap()
    );
    assert_eq!(
        f.test
            .db
            .external_item(node)
            .await
            .unwrap()
            .unwrap()
            .unavailable,
        None
    );

    // A batch of new nodes carries the entry's tag and the archived mark with it.
    let doc = new_id();
    let row = NodeRow {
        id: doc,
        kind: NodeKind::Document,
        parent: Some(f.library),
        site: f.site,
        name: "Batch.txt".into(),
        deleted_at: None,
        deleted_by: None,
        created_by: None,
        created_at: OffsetDateTime::now_utc(),
        deleted_with: None,
        bin_stage: None,
        purged_at: None,
    };
    let batch_tag = object_tag(3, seconds, "\"e2\"", true);
    f.test
        .db
        .apply_nodes(NodeChange {
            rows: &[row],
            documents: &[(doc, records::Editor::Text)],
            external_library: Some(f.library),
            external_items: &[entry(doc, "Batch.txt", 3, ns, Some(batch_tag.clone()))],
            bulk_create: true,
            ..NodeChange::default()
        })
        .await
        .unwrap();
    let item = f.test.db.external_item(doc).await.unwrap().unwrap();
    assert_eq!(item.tag, batch_tag);
    assert_eq!(item.unavailable, Some(Unavailable::Archived));
}

#[tokio::test]
async fn set_external_seen_moves_the_tag_once_and_rotates_the_revision() {
    let f = fixture().await;
    connect(&f, f.library, "seen-bucket", "").await.unwrap();
    let node = external_document(&f, f.library, "Notes.txt").await;
    let seconds = 1_700_000_000;
    let ns = seconds * 1_000_000_000;
    let first = object_tag(17, seconds, "\"v1\"", false);
    let second = object_tag(23, seconds + 5, "\"v2\"", false);
    let third = object_tag(24, seconds + 9, "\"v3\"", false);
    f.test
        .db
        .record_external_item(
            f.library,
            &entry(node, "Notes.txt", 17, ns, Some(first.clone())),
        )
        .await
        .unwrap();
    f.test
        .db
        .set_external_content(node, &first, &[7; 32], OffsetDateTime::now_utc())
        .await
        .unwrap()
        .unwrap();
    assert!(
        f.test
            .db
            .set_external_version(node, &first, Some("ver-1"))
            .await
            .unwrap()
    );
    let before = f.test.db.external_item(node).await.unwrap().unwrap();
    assert_eq!(before.version.as_deref(), Some("ver-1"));
    assert!(before.sha256.is_some());

    assert!(
        f.test
            .db
            .set_external_seen(node, &first, &second, 23)
            .await
            .unwrap()
    );
    assert!(
        !f.test
            .db
            .set_external_seen(node, &first, &third, 24)
            .await
            .unwrap(),
        "the old tag is no longer current"
    );
    let after = f.test.db.external_item(node).await.unwrap().unwrap();
    assert_eq!(after.tag, second);
    assert_eq!(after.size, 23);
    assert_eq!(after.mtime_ns, (seconds + 5) * 1_000_000_000);
    assert_ne!(after.revision, before.revision);
    assert_eq!(
        after.sha256, None,
        "content learned for the old tag is forgotten"
    );
    assert_eq!(after.version, None, "so is its version");

    // A tag that is not a tag of that size is refused, not stored.
    assert!(
        f.test
            .db
            .set_external_seen(node, &second, "garbage", 23)
            .await
            .is_err()
    );
    assert_eq!(
        f.test.db.external_item(node).await.unwrap().unwrap().tag,
        second
    );

    // Two movers from one tag: one wins.
    let a = object_tag(30, seconds + 20, "\"a\"", false);
    let b = object_tag(31, seconds + 21, "\"b\"", false);
    let (x, y) = tokio::join!(
        f.test.db.set_external_seen(node, &second, &a, 30),
        f.test.db.set_external_seen(node, &second, &b, 31)
    );
    assert_eq!(
        [x.unwrap(), y.unwrap()].iter().filter(|won| **won).count(),
        1
    );
}

#[tokio::test]
async fn external_item_answers_for_s3_with_its_kind() {
    let f = fixture().await;
    connect(&f, f.library, "kinds-bucket", "pre/")
        .await
        .unwrap();
    let node = external_document(&f, f.library, "Report.txt").await;
    let seconds = 1_700_000_123;
    let tag = object_tag(40, seconds, "\"etag\"", false);
    f.test
        .db
        .record_external_item(
            f.library,
            &entry(
                node,
                "Report.txt",
                40,
                seconds * 1_000_000_000,
                Some(tag.clone()),
            ),
        )
        .await
        .unwrap();

    let item = f.test.db.external_item(node).await.unwrap().unwrap();
    assert_eq!(item.kind, SourceKind::S3);
    assert_eq!(item.library, f.library);
    assert_eq!(item.location, "s3://r2/kinds-bucket/pre/");
    assert_eq!(item.repository_path, "Report.txt");
    assert_eq!(item.tag, tag);
    assert_eq!(item.mtime_ns, seconds * 1_000_000_000);
    assert_eq!(item.version, None);

    assert!(
        f.test
            .db
            .set_external_version(node, &tag, Some("v-abc"))
            .await
            .unwrap()
    );
    assert!(
        !f.test
            .db
            .set_external_version(node, "x1-1", Some("v-old"))
            .await
            .unwrap()
    );
    assert_eq!(
        f.test
            .db
            .external_item(node)
            .await
            .unwrap()
            .unwrap()
            .version
            .as_deref(),
        Some("v-abc")
    );
    assert!(
        f.test
            .db
            .set_external_version(node, &tag, None)
            .await
            .unwrap()
    );
    assert_eq!(
        f.test
            .db
            .external_item(node)
            .await
            .unwrap()
            .unwrap()
            .version,
        None
    );
}

#[tokio::test]
async fn usage_adds_atomically_across_ten_tasks() {
    let f = fixture().await;
    connect(&f, f.library, "usage-bucket", "").await.unwrap();
    let june = month(2026, Month::June, 1);
    let mut tasks = Vec::new();
    for _ in 0..10 {
        let db = f.test.db.clone();
        let library = f.library;
        tasks.push(tokio::spawn(async move {
            db.add_source_usage(
                library,
                june,
                &Usage {
                    list: 1,
                    read: 2,
                    write: 3,
                    bytes_down: 100,
                    bytes_up: 10,
                    cache_hits: 4,
                },
            )
            .await
            .unwrap()
        }));
    }
    let mut lists = Vec::new();
    for task in tasks {
        lists.push(task.await.unwrap().list);
    }
    lists.sort_unstable();
    assert_eq!(
        lists,
        (1..=10).collect::<Vec<u64>>(),
        "every addition saw its own total, none lost"
    );
    assert_eq!(
        f.test.db.usage(f.library, june).await.unwrap(),
        Usage {
            list: 10,
            read: 20,
            write: 30,
            bytes_down: 1000,
            bytes_up: 100,
            cache_hits: 40,
        }
    );
}

#[tokio::test]
async fn usage_is_per_month() {
    let f = fixture().await;
    connect(&f, f.library, "month-bucket", "").await.unwrap();
    let one = Usage {
        list: 1,
        ..Usage::default()
    };
    assert_eq!(
        f.test
            .db
            .usage(f.library, month(2026, Month::March, 1))
            .await
            .unwrap(),
        Usage::default(),
        "nothing counted is zero"
    );
    f.test
        .db
        .add_source_usage(f.library, month(2026, Month::March, 3), &one)
        .await
        .unwrap();
    let second = f
        .test
        .db
        .add_source_usage(f.library, month(2026, Month::March, 29), &one)
        .await
        .unwrap();
    assert_eq!(second.list, 2, "the day of the month does not matter");
    let april = f
        .test
        .db
        .add_source_usage(f.library, month(2026, Month::April, 2), &one)
        .await
        .unwrap();
    assert_eq!(april.list, 1);
    assert_eq!(
        f.test
            .db
            .usage(f.library, month(2026, Month::March, 15))
            .await
            .unwrap()
            .list,
        2
    );
    let stored: Date = sqlx::query_scalar(
        "select month from source_usage where library_id = $1 order by month limit 1",
    )
    .bind(f.library.0)
    .fetch_one(f.test.db.test_pool())
    .await
    .unwrap();
    assert_eq!(stored, month(2026, Month::March, 1));
}

#[tokio::test]
async fn the_warning_is_marked_once() {
    let f = fixture().await;
    connect(&f, f.library, "warn-bucket", "").await.unwrap();
    let may = month(2026, Month::May, 1);
    f.test
        .db
        .add_source_usage(
            f.library,
            may,
            &Usage {
                list: 5,
                ..Usage::default()
            },
        )
        .await
        .unwrap();
    assert!(f.test.db.mark_usage_warned(f.library, may).await.unwrap());
    assert!(!f.test.db.mark_usage_warned(f.library, may).await.unwrap());
    assert!(
        f.test
            .db
            .mark_usage_warned(f.library, month(2026, Month::June, 1))
            .await
            .unwrap(),
        "the next month warns again"
    );
    // Counting more does not clear the mark.
    f.test
        .db
        .add_source_usage(
            f.library,
            may,
            &Usage {
                list: 1,
                ..Usage::default()
            },
        )
        .await
        .unwrap();
    assert!(!f.test.db.mark_usage_warned(f.library, may).await.unwrap());
    assert_eq!(f.test.db.usage(f.library, may).await.unwrap().list, 6);
}

#[tokio::test]
async fn budget_falls_back_to_the_settings_defaults() {
    let f = fixture().await;
    connect(&f, f.library, "budget-bucket", "").await.unwrap();
    let defaults = f.test.db.s3_settings().await.unwrap();
    assert_eq!(
        f.test.db.budget(f.library).await.unwrap(),
        Budget {
            list_requests: defaults.list_requests,
            read_requests: defaults.read_requests,
            bytes_down: defaults.bytes_down,
        }
    );

    f.test
        .db
        .set_settings(
            f.user,
            vec![
                (S3_LIST_REQUESTS, json!(5000)),
                (S3_BYTES_DOWN, json!(3 * 1024 * 1024 * 1024_u64)),
            ],
        )
        .await
        .unwrap();
    let from_settings = f.test.db.budget(f.library).await.unwrap();
    assert_eq!(from_settings.list_requests, 5000);
    assert_eq!(from_settings.read_requests, defaults.read_requests);
    assert_eq!(from_settings.bytes_down, Some(3 * 1024 * 1024 * 1024));

    let own = Budget {
        list_requests: 1234,
        read_requests: 5678,
        bytes_down: Some(42),
    };
    f.test.db.set_budget(f.library, &own).await.unwrap();
    assert_eq!(f.test.db.budget(f.library).await.unwrap(), own);
    assert_eq!(
        f.test.db.set_budget(new_id(), &own).await,
        Err(AccessError::NotFound)
    );
    // A library with no bucket has the defaults.
    assert_eq!(
        f.test.db.budget(new_id()).await.unwrap().list_requests,
        5000
    );
}

#[tokio::test]
async fn replacing_the_key_answers_the_old_secret_and_moves_the_generation() {
    let f = fixture().await;
    let place = place("key-bucket", "");
    let old = secret(&f, b"old sealed key").await;
    f.test
        .db
        .connect_s3_source(&s3_source(&f, f.library, &place, old), &place)
        .await
        .unwrap();
    f.test
        .db
        .set_source_status(
            f.library,
            &status(SourceState::NotConnected, Some("The keys were refused.")),
        )
        .await
        .unwrap();
    let generation = f
        .test
        .db
        .source_generation(f.library)
        .await
        .unwrap()
        .unwrap();

    let new = secret(&f, b"new sealed key").await;
    assert_eq!(
        f.test
            .db
            .set_s3_key(f.library, "AKIANEWKEY", new)
            .await
            .unwrap(),
        Some(old)
    );
    let (config, status) = f.test.db.source_of(f.library).await.unwrap().unwrap();
    assert_eq!(config.token, Some(new));
    assert_eq!(
        f.test
            .db
            .s3_place(f.library)
            .await
            .unwrap()
            .unwrap()
            .access_key_id,
        "AKIANEWKEY"
    );
    assert_eq!(
        f.test.db.source_generation(f.library).await.unwrap(),
        Some(generation + 1)
    );
    assert_eq!(status.state, SourceState::Connecting, "tried again");
    assert_eq!(status.last_error, None);
    // The displaced secret can now be deleted; the current one cannot.
    f.test.db.delete_secret(old).await.unwrap();
    assert!(f.test.db.get_secret(old).await.unwrap().is_none());
    assert!(f.test.db.delete_secret(new).await.is_err());

    // The same secret again displaces nothing.
    assert_eq!(
        f.test
            .db
            .set_s3_key(f.library, "AKIANEWKEY", new)
            .await
            .unwrap(),
        None
    );
    // A key id the record refuses leaves both parts as they were.
    let other = secret(&f, b"other sealed key").await;
    assert_eq!(
        f.test.db.set_s3_key(f.library, "", other).await,
        Err(AccessError::InvalidSetting)
    );
    assert_eq!(
        f.test
            .db
            .source_of(f.library)
            .await
            .unwrap()
            .unwrap()
            .0
            .token,
        Some(new)
    );
    // Not an S3 library.
    let plain = library(&f, "Plain").await;
    assert_eq!(
        f.test.db.set_s3_key(plain, "AKIA", other).await,
        Err(AccessError::NotFound)
    );
}

#[tokio::test]
async fn disconnect_removes_s3_and_usage_rows_and_answers_the_secret() {
    let f = fixture().await;
    let place = place("gone-bucket", "p/");
    let token = secret(&f, b"sealed key").await;
    f.test
        .db
        .connect_s3_source(&s3_source(&f, f.library, &place, token), &place)
        .await
        .unwrap();
    f.test
        .db
        .add_source_usage(
            f.library,
            month(2026, Month::July, 1),
            &Usage {
                read: 3,
                ..Usage::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(count(&f, "source_s3", f.library).await, 1);
    assert_eq!(count(&f, "source_usage", f.library).await, 1);

    assert_eq!(
        f.test.db.disconnect_source(f.library).await.unwrap(),
        vec![token]
    );
    assert_eq!(count(&f, "source_s3", f.library).await, 0);
    assert_eq!(count(&f, "source_usage", f.library).await, 0);
    assert!(f.test.db.source_of(f.library).await.unwrap().is_none());
    f.test.db.delete_secret(token).await.unwrap();
    assert!(f.test.db.get_secret(token).await.unwrap().is_none());
    // The prefix is free for another library.
    let again = library(&f, "Again").await;
    connect(&f, again, "gone-bucket", "p/").await.unwrap();
}

#[tokio::test]
async fn purging_a_site_with_an_s3_library_succeeds() {
    let f = fixture().await;
    connect(&f, f.library, "purge-bucket", "").await.unwrap();
    let node = external_document(&f, f.library, "A.txt").await;
    let tag = object_tag(4, 1_700_000_000, "\"e\"", false);
    f.test
        .db
        .record_external_item(
            f.library,
            &entry(node, "A.txt", 4, 1_700_000_000_000_000_000, Some(tag)),
        )
        .await
        .unwrap();
    f.test
        .db
        .add_source_usage(
            f.library,
            month(2026, Month::July, 1),
            &Usage {
                list: 1,
                ..Usage::default()
            },
        )
        .await
        .unwrap();
    let token = f
        .test
        .db
        .source_of(f.library)
        .await
        .unwrap()
        .unwrap()
        .0
        .token
        .unwrap();

    let now = OffsetDateTime::now_utc();
    let purged = |id: NodeId, kind: NodeKind, parent: Option<NodeId>, name: &str| NodeRow {
        id,
        kind,
        parent,
        site: f.site,
        name: name.into(),
        deleted_at: Some(now),
        deleted_by: None,
        created_by: None,
        created_at: now,
        deleted_with: (kind != NodeKind::Site).then_some(f.site),
        bin_stage: None,
        purged_at: Some(now),
    };
    let rows = [
        purged(f.site, NodeKind::Site, None, "Site"),
        purged(f.library, NodeKind::Library, Some(f.site), "Bucket"),
        purged(node, NodeKind::Document, Some(f.library), "A.txt"),
    ];
    f.test
        .db
        .apply_nodes(NodeChange {
            rows: &rows,
            purge_site: Some(f.site),
            ..NodeChange::default()
        })
        .await
        .unwrap();

    for table in [
        "library_sources",
        "source_s3",
        "source_usage",
        "source_items",
    ] {
        assert_eq!(count(&f, table, f.library).await, 0, "{table}");
    }
    let secrets: i64 = sqlx::query("select count(*) as n from secrets where id = $1")
        .bind(token.0)
        .fetch_one(f.test.db.test_pool())
        .await
        .unwrap()
        .get("n");
    assert_eq!(secrets, 0, "the sealed key goes with the site");
}

#[tokio::test]
async fn migrating_a_database_with_github_and_share_rows_keeps_them() {
    let test = TestDb::create_at(Some(23)).await;
    let ann = user(&test, "MigrationAnn", true).await;
    let site = site(&test, "Migration Site", Some(ann.id)).await;
    let mut libraries = Vec::new();
    for name in ["Github", "Share"] {
        libraries.push(put(&test, new_id(), NodeKind::Library, Some(site), site, name).await);
    }
    let (github, share) = (libraries[0], libraries[1]);
    let github_doc: NodeId = document_in(&test, ann.id, github, site, "A.md")
        .await
        .into();
    let share_doc: NodeId = document_in(&test, ann.id, share, site, "B.txt")
        .await
        .into();
    let pool = test.db.test_pool();
    sqlx::query(
        "insert into library_sources \
         (library_id, kind, owner, repo, branch, folder, poll_seconds, connected_by, generation) \
         values ($1, 'github', 'owner', 'repo', 'main', '', 300, $2, 4)",
    )
    .bind(github.0)
    .bind(ann.id.0)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "insert into library_sources \
         (library_id, kind, owner, repo, branch, folder, location, mode, poll_seconds, \
          connected_by, generation) \
         values ($1, 'share', '', '', '', '', 'C:\\Approved\\Files', 'read_write', 300, $2, 9)",
    )
    .bind(share.0)
    .bind(ann.id.0)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "insert into source_items \
         (library_id, path, repository_path, node_id, blob_sha, size) \
         values ($1, 'A.md', 'docs/A.md', $2, 'blob-one', 7)",
    )
    .bind(github.0)
    .bind(github_doc.0)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "insert into source_items \
         (library_id, path, repository_path, node_id, blob_sha, size, revision, modified_at, \
          unavailable) \
         values ($1, 'B.txt', 'B.txt', $2, 'x11-5000', 11, $3, now(), 'denied')",
    )
    .bind(share.0)
    .bind(share_doc.0)
    .bind("a".repeat(40))
    .execute(pool)
    .await
    .unwrap();

    test.db.migrate().await.unwrap();

    let (config, _) = test.db.source_of(github).await.unwrap().unwrap();
    assert_eq!(config.kind, SourceKind::Github);
    assert_eq!(config.location, None);
    assert_eq!(config.mode, SourceMode::ReadOnly);
    assert_eq!(test.db.source_generation(github).await.unwrap(), Some(4));
    let items = test.db.source_items(github).await.unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].repository_path, "docs/A.md");
    assert_eq!(items[0].blob_sha.as_deref(), Some("blob-one"));

    let (config, _) = test.db.source_of(share).await.unwrap().unwrap();
    assert_eq!(config.kind, SourceKind::Share);
    assert_eq!(config.location.as_deref(), Some(r"C:\Approved\Files"));
    assert_eq!(config.mode, SourceMode::ReadWrite);
    assert_eq!(test.db.source_generation(share).await.unwrap(), Some(9));
    let item = test.db.external_item(share_doc).await.unwrap().unwrap();
    assert_eq!(item.kind, SourceKind::Share);
    assert_eq!(item.tag, "x11-5000");
    assert_eq!((item.size, item.mtime_ns), (11, 5000));
    assert_eq!(item.unavailable, Some(Unavailable::Denied));
    assert_eq!(item.version, None);
    assert_eq!(test.db.external_item(github_doc).await.unwrap(), None);

    // The new constraints are in force on the migrated database: the old rules still hold,
    // and an S3 library and an archived item are now allowed.
    let fresh = put(
        &test,
        new_id(),
        NodeKind::Library,
        Some(site),
        site,
        "Fresh",
    )
    .await;
    let place = S3Place {
        endpoint: "r2".into(),
        bucket: "migrated-bucket".into(),
        prefix: String::new(),
        access_key_id: "AKIA".into(),
        versioning: Versioning::None,
        conditional: Tri::No,
    };
    let token = test.db.put_secret(&sealed(b"sealed key")).await.unwrap();
    approve_r2(&test, ann.id).await;
    test.db
        .connect_s3_source(
            &NewSource {
                library: fresh,
                kind: SourceKind::S3,
                owner: String::new(),
                repo: String::new(),
                branch: String::new(),
                folder: String::new(),
                location: Some(s3_location("r2", "migrated-bucket", "")),
                mode: SourceMode::ReadOnly,
                token: Some(token),
                poll_seconds: 300,
                connected_by: ann.id,
            },
            &place,
        )
        .await
        .unwrap();
    assert_eq!(test.db.s3_place(fresh).await.unwrap(), Some(place));
    assert!(
        test.db
            .set_external_unavailable(share_doc, "x11-5000", Some(Unavailable::Archived))
            .await
            .unwrap()
    );
    let github_write =
        sqlx::query("update library_sources set mode = 'read_write' where library_id = $1")
            .bind(github.0)
            .execute(pool)
            .await;
    assert!(github_write.is_err(), "a GitHub library is still read-only");
}
