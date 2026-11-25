//! Safe writes to a bucket, the write probe and the shared write types, against the fake S3.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::collections::HashSet;
use std::time::Duration;

use collab_core::ids::NodeId;

use common::fake_s3::FakeS3;
use records::{S3Endpoint, Tri, object_tag, reserved_name, windows_safe_name};
use server::s3::{COPY_MAX, PutBody, S3, S3Target};
use server::s3_sign::{Credentials, sha256_hex};
use server::s3_write::{
    MOVE_MAX_OBJECTS, Moved, copy_item, delete_held, hold, holding_root, make_folder, move_item,
    object_key, probe_writes, sweep_holding, unhold, write_beside, write_numbered, write_object,
};
use server::secrets::Secret;
use server::source_write::{Base, Probe, Seen, WriteError, Written, numbered_name, sibling_name};
use sha2::Digest;
use time::{Date, Month, OffsetDateTime};
use uuid::Uuid;

const BUCKET: &str = "docs";
const PREFIX: &str = "lib/";

fn creds() -> Credentials {
    Credentials {
        access_key_id: "AKIDFAKEEXAMPLE".to_owned(),
        secret: Secret::new("planted-S3-secret-7f3a9c"),
        session_token: None,
    }
}

fn client() -> S3 {
    let s3 = S3::new(true);
    s3.set_retry_wait_for_tests(Duration::from_millis(5));
    s3
}

fn target_for(s3: &S3, fake: &FakeS3, bucket: &str, prefix: &str) -> S3Target {
    let endpoint = S3Endpoint::parse(
        "fake",
        &fake.origin(),
        "us-east-1",
        true,
        &["127.0.0.0/8".to_owned()],
        true,
    )
    .unwrap();
    s3.target_from(&endpoint, bucket, creds())
        .with_prefix(prefix)
}

async fn setup() -> (FakeS3, S3, S3Target) {
    let fake = FakeS3::start().await;
    fake.bucket(BUCKET);
    let s3 = client();
    let t = target_for(&s3, &fake, BUCKET, PREFIX);
    (fake, s3, t)
}

fn file_body(bytes: &[u8]) -> (tempfile::NamedTempFile, PutBody) {
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

async fn tag_of(s3: &S3, t: &S3Target, key: &str) -> String {
    let head = s3.head(t, key).await.unwrap();
    object_tag(head.size, head.modified_s, &head.etag, false)
}

fn base(tag: Option<String>) -> Base {
    Base { tag, sha256: None }
}

fn at() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
}

fn done(written: Written) -> Seen {
    match written {
        Written::Done(seen) => seen,
        Written::Conflict => panic!("expected Done, got Conflict"),
    }
}

fn w(method: &str, key: &str, cond: &str) -> (String, String, String) {
    (method.to_owned(), key.to_owned(), cond.to_owned())
}

#[tokio::test]
async fn replace_is_one_conditional_put_after_a_head() {
    let (fake, s3, t) = setup().await;
    fake.put(BUCKET, "lib/a.txt", b"old");
    let tag = tag_of(&s3, &t, "lib/a.txt").await;
    let before = fake.count("head");
    let (_f, body) = file_body(b"new bytes");

    let seen = done(
        write_object(&s3, &t, "a.txt", &body, &base(Some(tag)), Tri::Yes)
            .await
            .unwrap(),
    );
    assert_eq!(fake.count("head") - before, 1 + 1); // the check, and the one after the PUT
    assert_eq!(fake.writes(), [w("PUT", "lib/a.txt", "match")]);
    let stored = fake.object(BUCKET, "lib/a.txt").unwrap();
    assert_eq!(stored.bytes, b"new bytes");
    // The tag is what the next listing computes.
    assert_eq!(
        seen.tag,
        object_tag(
            stored.bytes.len() as u64,
            stored.modified_s,
            &stored.etag,
            false
        )
    );
    assert_eq!(seen.size, 9);
    assert_eq!(seen.modified.unix_timestamp(), stored.modified_s);
}

#[tokio::test]
async fn a_new_file_is_put_if_absent() {
    let (fake, s3, t) = setup().await;
    let (_f, body) = file_body(b"fresh");
    let seen = done(
        write_object(&s3, &t, "dir/new.txt", &body, &base(None), Tri::Yes)
            .await
            .unwrap(),
    );
    assert_eq!(fake.writes(), [w("PUT", "lib/dir/new.txt", "absent")]);
    assert_eq!(
        fake.object(BUCKET, "lib/dir/new.txt").unwrap().bytes,
        b"fresh"
    );
    assert_eq!(seen.size, 5);
}

#[tokio::test]
async fn a_target_changed_since_the_base_is_a_conflict_and_is_untouched() {
    let (fake, s3, t) = setup().await;
    fake.put(BUCKET, "lib/a.txt", b"v1");
    let tag = tag_of(&s3, &t, "lib/a.txt").await;
    fake.put_at(BUCKET, "lib/a.txt", b"changed outside", 1_800_000_000);
    let (_f, body) = file_body(b"ours");

    let result = write_object(&s3, &t, "a.txt", &body, &base(Some(tag)), Tri::Yes)
        .await
        .unwrap();
    assert!(matches!(result, Written::Conflict));
    assert!(fake.writes().is_empty());
    assert_eq!(
        fake.object(BUCKET, "lib/a.txt").unwrap().bytes,
        b"changed outside"
    );

    // A target that is gone is a conflict too.
    let result = write_object(
        &s3,
        &t,
        "gone.txt",
        &body,
        &base(Some("x1-1~00".to_owned())),
        Tri::Yes,
    )
    .await
    .unwrap();
    assert!(matches!(result, Written::Conflict));
    assert!(fake.writes().is_empty());
}

#[tokio::test]
async fn a_target_changed_between_head_and_put_is_a_conflict_not_an_overwrite() {
    let (fake, s3, t) = setup().await;
    fake.put(BUCKET, "lib/a.txt", b"v1");
    let tag = tag_of(&s3, &t, "lib/a.txt").await;
    fake.change_after_head("lib/a.txt", b"changed meanwhile");
    let (_f, body) = file_body(b"ours");

    let result = write_object(&s3, &t, "a.txt", &body, &base(Some(tag)), Tri::Yes)
        .await
        .unwrap();
    assert!(matches!(result, Written::Conflict));
    assert_eq!(
        fake.object(BUCKET, "lib/a.txt").unwrap().bytes,
        b"changed meanwhile"
    );
    // The PUT was sent and refused; nothing was stored by it.
    assert_eq!(fake.writes(), [w("PUT", "lib/a.txt", "match")]);
}

#[tokio::test]
async fn a_new_key_that_appeared_meanwhile_is_a_conflict() {
    let (fake, s3, t) = setup().await;
    // Not there at the HEAD; there by the PUT.
    fake.appear_after_miss("lib/new.txt", b"someone else's");
    let (_f, body) = file_body(b"ours");
    let result = write_object(&s3, &t, "new.txt", &body, &base(None), Tri::Yes)
        .await
        .unwrap();
    assert!(matches!(result, Written::Conflict));
    assert_eq!(
        fake.object(BUCKET, "lib/new.txt").unwrap().bytes,
        b"someone else's"
    );
    assert_eq!(fake.writes(), [w("PUT", "lib/new.txt", "absent")]);

    // An existing key with a base of "must not exist" is a conflict without a PUT.
    let before = fake.writes().len();
    let result = write_object(&s3, &t, "new.txt", &body, &base(None), Tri::Yes)
        .await
        .unwrap();
    assert!(matches!(result, Written::Conflict));
    assert_eq!(fake.writes().len(), before);
}

#[tokio::test]
async fn a_write_whose_answer_was_lost_is_recognised_by_its_hash() {
    let (fake, s3, t) = setup().await;
    fake.put(BUCKET, "lib/a.txt", b"v1");
    let tag = tag_of(&s3, &t, "lib/a.txt").await;
    let (_f, body) = file_body(b"our content");

    fake.lose_answer_after_write();
    let first = write_object(&s3, &t, "a.txt", &body, &base(Some(tag.clone())), Tri::Yes).await;
    assert!(first.is_err(), "the lost answer is an error");
    assert_eq!(
        fake.object(BUCKET, "lib/a.txt").unwrap().bytes,
        b"our content"
    );

    // The retry starts with a HEAD, sees our hash, and sends nothing more.
    let writes = fake.writes().len();
    let seen = done(
        write_object(&s3, &t, "a.txt", &body, &base(Some(tag)), Tri::Yes)
            .await
            .unwrap(),
    );
    assert_eq!(fake.writes().len(), writes);
    assert_eq!(seen.size, 11);

    // The same for a new file.
    let (_g, body2) = file_body(b"another");
    fake.lose_answer_after_write();
    assert!(
        write_object(&s3, &t, "b.txt", &body2, &base(None), Tri::Yes)
            .await
            .is_err()
    );
    let writes = fake.writes().len();
    done(
        write_object(&s3, &t, "b.txt", &body2, &base(None), Tri::Yes)
            .await
            .unwrap(),
    );
    assert_eq!(fake.writes().len(), writes);

    // Someone else's object with other content is not ours.
    fake.put(BUCKET, "lib/c.txt", b"theirs");
    let result = write_object(&s3, &t, "c.txt", &body, &base(None), Tri::Yes)
        .await
        .unwrap();
    assert!(matches!(result, Written::Conflict));
    // Nor is one that merely has the same bytes but no hash of ours.
    fake.put(BUCKET, "lib/d.txt", b"our content");
    let result = write_object(&s3, &t, "d.txt", &body, &base(None), Tri::Yes)
        .await
        .unwrap();
    assert!(matches!(result, Written::Conflict));
}

#[tokio::test]
async fn on_a_store_that_ignores_conditions_the_write_is_head_then_put() {
    let (fake, s3, t) = setup().await;
    fake.ignore_conditions();
    fake.put(BUCKET, "lib/a.txt", b"v1");
    let tag = tag_of(&s3, &t, "lib/a.txt").await;
    let (_f, body) = file_body(b"ours");

    let before = fake.count("head");
    done(
        write_object(&s3, &t, "a.txt", &body, &base(Some(tag)), Tri::No)
            .await
            .unwrap(),
    );
    assert_eq!(fake.writes(), [w("PUT", "lib/a.txt", "none")]);
    assert_eq!(fake.object(BUCKET, "lib/a.txt").unwrap().bytes, b"ours");
    assert!(fake.count("head") > before);

    // Unknown is treated as No. A changed target is still a conflict at the HEAD check.
    fake.put_at(BUCKET, "lib/b.txt", b"x", 1_700_000_000);
    let (_g, body2) = file_body(b"y");
    let result = write_object(
        &s3,
        &t,
        "b.txt",
        &body2,
        &base(Some("x1-1~00".to_owned())),
        Tri::Unknown,
    )
    .await
    .unwrap();
    assert!(matches!(result, Written::Conflict));
    assert_eq!(fake.object(BUCKET, "lib/b.txt").unwrap().bytes, b"x");
}

#[tokio::test]
async fn write_beside_names_the_copy_and_numbers_it_if_taken() {
    let (fake, s3, t) = setup().await;
    let (_f, body) = file_body(b"my edit");
    let name = sibling_name("report.txt", "Ada", at());
    assert_eq!(name, "report (edited by Ada 2023-11-14 22.13).txt");

    let (path, seen) = write_beside(&s3, &t, "docs/report.txt", &body, "Ada", at(), Tri::Yes)
        .await
        .unwrap();
    assert_eq!(path, format!("docs/{name}"));
    assert_eq!(seen.size, 7);
    assert_eq!(
        fake.object(BUCKET, &format!("lib/docs/{name}"))
            .unwrap()
            .bytes,
        b"my edit"
    );
    assert_eq!(fake.writes()[0].2, "absent");

    // Taken by someone else: the next number.
    let (_g, other) = file_body(b"second edit");
    let (path2, _) = write_beside(&s3, &t, "docs/report.txt", &other, "Ada", at(), Tri::Yes)
        .await
        .unwrap();
    assert_eq!(path2, format!("docs/{}", numbered_name(&name, 2)),);
    assert_eq!(
        fake.object(BUCKET, &format!("lib/{path2}")).unwrap().bytes,
        b"second edit"
    );
    // The first copy is untouched.
    assert_eq!(
        fake.object(BUCKET, &format!("lib/docs/{name}"))
            .unwrap()
            .bytes,
        b"my edit"
    );

    // All twenty taken: Failed, nothing overwritten.
    for n in 3..=20 {
        fake.put(
            BUCKET,
            &format!("lib/docs/{}", numbered_name(&name, n)),
            b"taken",
        );
    }
    let (_h, third) = file_body(b"third");
    assert_eq!(
        write_beside(&s3, &t, "docs/report.txt", &third, "Ada", at(), Tri::Yes)
            .await
            .unwrap_err(),
        WriteError::Failed
    );
    // A file at the top of the library works as well.
    let (top, _) = write_beside(&s3, &t, "top.txt", &body, "Bo", at(), Tri::Yes)
        .await
        .unwrap();
    assert!(!top.contains('/'));
}

#[test]
fn sibling_name_is_nameable_and_fits() {
    let long_person = "Someone With A Very Long Name Indeed, Truly Rather Long".to_owned();
    let names = [
        sibling_name("a.txt", "Ada", at()),
        sibling_name("noext", "Ada", at()),
        sibling_name(".hidden", "Ada", at()),
        sibling_name(&format!("{}.docx", "n".repeat(200)), &long_person, at()),
        sibling_name(
            &format!("{}.txt", "é".repeat(100)),
            "Zoë <b>/\\:*?\"|",
            at(),
        ),
        sibling_name("x.txt", "con", at()),
        sibling_name("x.txt", "", at()),
        sibling_name("x.txt", "\u{7}\u{8}  ", at()),
    ];
    for name in &names {
        assert!(name.chars().count() <= 120, "{name}");
        assert!(!name.chars().any(char::is_control), "{name}");
        assert!(!name.contains('/') && !name.contains('\\'), "{name}");
        assert!(windows_safe_name(name), "{name}");
        assert!(!reserved_name(name), "{name}");
        assert!(name.contains("(edited by "), "{name}");
        assert!(name.len() <= 255, "{name}");
    }
    assert!(names[3].ends_with(".docx"));
    assert!(names[1].ends_with("2023-11-14 22.13)"));
    // Time is UTC.
    let later = OffsetDateTime::from_unix_timestamp(1_700_000_000 + 3600 * 5 + 60 * 2).unwrap();
    assert!(sibling_name("a.txt", "Ada", later).contains("2023-11-15 03.15"));
    // The person is cut to 40 characters.
    let name = sibling_name("a.txt", &"p".repeat(100), at());
    assert!(
        name.contains(&format!("by {} 2023", "p".repeat(40))),
        "{name}"
    );
    assert_eq!(numbered_name("a.txt", 2), "a (2).txt");
    assert_eq!(numbered_name("noext", 3), "noext (3)");
    assert_eq!(numbered_name("a.b.txt", 4), "a.b (4).txt");
}

#[tokio::test]
async fn write_numbered_takes_the_next_free_name() {
    let (fake, s3, t) = setup().await;
    fake.put(BUCKET, "lib/d/new.txt", b"theirs");
    fake.put(BUCKET, "lib/d/new (2).txt", b"theirs too");
    let (_f, body) = file_body(b"ours");

    let (path, seen) = write_numbered(&s3, &t, "d/new.txt", &body, Tri::Yes)
        .await
        .unwrap();
    assert_eq!(path, "d/new (3).txt");
    assert_eq!(seen.size, 4);
    assert_eq!(
        fake.object(BUCKET, "lib/d/new.txt").unwrap().bytes,
        b"theirs"
    );
    assert_eq!(
        fake.object(BUCKET, "lib/d/new (3).txt").unwrap().bytes,
        b"ours"
    );

    for n in 4..=20 {
        fake.put(
            BUCKET,
            &format!("lib/d/{}", numbered_name("new.txt", n)),
            b"x",
        );
    }
    let (_g, other) = file_body(b"different");
    assert_eq!(
        write_numbered(&s3, &t, "d/new.txt", &other, Tri::Yes)
            .await
            .unwrap_err(),
        WriteError::Failed
    );
}

#[tokio::test]
async fn make_folder_writes_a_zero_byte_marker_and_accepts_an_existing_one() {
    let (fake, s3, t) = setup().await;
    make_folder(&s3, &t, "photos/2024", Tri::Yes).await.unwrap();
    assert_eq!(fake.writes(), [w("PUT", "lib/photos/2024/", "absent")]);
    assert_eq!(fake.last_write_body_len(), Some(0));
    assert_eq!(fake.object(BUCKET, "lib/photos/2024/").unwrap().bytes, b"");

    // Again: a marker already there is success, and nothing is deleted.
    make_folder(&s3, &t, "photos/2024", Tri::Yes).await.unwrap();
    assert!(fake.writes().iter().all(|(method, _, _)| method == "PUT"));
    assert!(fake.object(BUCKET, "lib/photos/2024/").is_some());

    assert_eq!(
        make_folder(&s3, &t, "../x", Tri::Yes).await.unwrap_err(),
        WriteError::Failed
    );
    assert_eq!(
        make_folder(&s3, &t, "a/", Tri::Yes).await.unwrap_err(),
        WriteError::Failed
    );
}

#[tokio::test]
async fn make_folder_on_a_store_that_ignores_conditions_heads_first_and_never_overwrites() {
    for conditional in [Tri::No, Tri::Unknown] {
        let (fake, s3, t) = setup().await;
        fake.ignore_conditions();
        // A marker already there (here with bytes, which a blind PUT would empty) is success
        // with no PUT.
        fake.put(BUCKET, "lib/kept/", b"marker bytes");
        make_folder(&s3, &t, "kept", conditional).await.unwrap();
        assert!(fake.writes().is_empty(), "{conditional:?}");
        assert_eq!(
            fake.object(BUCKET, "lib/kept/").unwrap().bytes,
            b"marker bytes"
        );

        // A marker not there is written, without a condition the store would ignore.
        make_folder(&s3, &t, "fresh", conditional).await.unwrap();
        assert_eq!(fake.writes(), [w("PUT", "lib/fresh/", "none")]);
        assert_eq!(fake.object(BUCKET, "lib/fresh/").unwrap().bytes, b"");
    }
}

#[tokio::test]
async fn the_probe_finds_read_write_with_conditions() {
    let (fake, s3, t) = setup().await;
    let (probe, left) = probe_writes(&s3, &t).await;
    assert_eq!(left, None);
    assert!(
        matches!(
            probe,
            Probe::ReadWrite {
                conditional: Tri::Yes
            }
        ),
        "{probe:?}"
    );
    let writes = fake.writes();
    assert_eq!(writes.len(), 4, "{writes:?}");
    assert_eq!(writes[0].0, "PUT");
    assert_eq!(writes[0].2, "absent");
    assert_eq!(writes[1].2, "absent");
    assert_eq!(writes[2].2, "match");
    assert_eq!(writes[3].0, "DELETE");
    assert!(writes[0].1.starts_with("lib/.sharecraft-tmp-probe-"));
    assert!(writes.iter().all(|(_, key, _)| *key == writes[0].1));
}

#[tokio::test]
async fn the_probe_finds_a_store_that_ignores_conditions() {
    let (fake, s3, t) = setup().await;
    fake.ignore_conditions();
    let (probe, left) = probe_writes(&s3, &t).await;
    assert_eq!(left, None);
    assert!(
        matches!(
            probe,
            Probe::ReadWrite {
                conditional: Tri::No
            }
        ),
        "{probe:?}"
    );
    assert_eq!(fake.writes().last().unwrap().0, "DELETE");
}

#[tokio::test]
async fn the_probe_finds_a_read_only_key() {
    let (fake, s3, t) = setup().await;
    fake.read_only();
    let (probe, left) = probe_writes(&s3, &t).await;
    assert_eq!(left, None);
    assert!(matches!(probe, Probe::ReadOnly), "{probe:?}");
}

#[tokio::test]
async fn the_probe_reports_why_it_could_not_ask() {
    let fake = FakeS3::start().await;
    let s3 = client();
    // No such bucket.
    let t = target_for(&s3, &fake, "nowhere", PREFIX);
    assert!(matches!(
        probe_writes(&s3, &t).await.0,
        Probe::Unreachable("bucket")
    ));
    // Wrong keys.
    fake.bucket(BUCKET);
    fake.require_keys("other", "keys");
    let t = target_for(&s3, &fake, BUCKET, PREFIX);
    assert!(matches!(
        probe_writes(&s3, &t).await.0,
        Probe::Unreachable("keys")
    ));
}

#[tokio::test]
async fn the_probe_leaves_nothing() {
    // Honouring conditions, and ignoring them.
    for ignoring in [false, true] {
        let (fake, s3, t) = setup().await;
        fake.put(BUCKET, "lib/keep.txt", b"mine");
        if ignoring {
            fake.ignore_conditions();
        }
        let (probe, left) = probe_writes(&s3, &t).await;
        assert_eq!(left, None);
        assert!(matches!(probe, Probe::ReadWrite { .. }), "{probe:?}");
        let keys: Vec<String> = fake.writes().into_iter().map(|(_, key, _)| key).collect();
        for key in keys {
            // The listing hides the probe's key by name.
            assert!(key.split('/').any(reserved_name), "{key}");
            assert!(fake.object(BUCKET, &key).is_none(), "{key} left behind");
        }
        assert_eq!(fake.object(BUCKET, "lib/keep.txt").unwrap().bytes, b"mine");
    }
}

#[tokio::test]
async fn errors_map_to_plain_write_errors() {
    let (fake, s3, t) = setup().await;
    let (_f, body) = file_body(b"data");

    fake.read_only();
    assert_eq!(
        write_object(&s3, &t, "a.txt", &body, &base(None), Tri::Yes)
            .await
            .unwrap_err(),
        WriteError::Denied
    );
    let (fake, s3, t) = setup().await;
    fake.throttle_next(50);
    assert_eq!(
        write_object(&s3, &t, "a.txt", &body, &base(None), Tri::Yes)
            .await
            .unwrap_err(),
        WriteError::Offline
    );
    let (fake, s3, t) = setup().await;
    fake.fail_next(400, "Strange");
    assert_eq!(
        write_object(&s3, &t, "a.txt", &body, &base(None), Tri::No)
            .await
            .unwrap_err(),
        WriteError::Failed
    );
    // Over 5 GiB (what ShareCraft can move afterwards): refused before any request.
    let huge = PutBody {
        size: COPY_MAX + 1,
        ..body.clone()
    };
    let before = fake.requests().len();
    assert_eq!(
        write_object(&s3, &t, "big.bin", &huge, &base(None), Tri::Yes)
            .await
            .unwrap_err(),
        WriteError::TooLarge
    );
    // More parts than a multipart upload takes: refused before any request too.
    s3.set_single_put_max_for_tests(2);
    s3.set_part_bytes_for_tests(1);
    let many = PutBody {
        size: 10_001,
        ..body.clone()
    };
    assert_eq!(
        write_object(&s3, &t, "big.bin", &many, &base(None), Tri::Yes)
            .await
            .unwrap_err(),
        WriteError::TooLarge
    );
    assert_eq!(fake.requests().len(), before);
    s3.set_single_put_max_for_tests(1 << 20);

    // No such bucket.
    let t2 = target_for(&s3, &fake, "nowhere", PREFIX);
    assert_eq!(
        write_object(&s3, &t2, "a.txt", &body, &base(None), Tri::Yes)
            .await
            .unwrap_err(),
        WriteError::Gone
    );
    // Keys the store does not know.
    fake.require_keys("someone", "else");
    assert_eq!(
        write_object(&s3, &t, "a.txt", &body, &base(None), Tri::Yes)
            .await
            .unwrap_err(),
        WriteError::Denied
    );
}

#[tokio::test]
async fn a_bad_path_is_refused_before_any_request() {
    let (fake, s3, t) = setup().await;
    let (_f, body) = file_body(b"data");
    let long = format!("{}/x", "d".repeat(300));
    let bad = [
        "",
        "/abs.txt",
        "a//b.txt",
        "a/./b.txt",
        "a/../b.txt",
        "..",
        "a/",
        "tab\tname.txt",
        "bell\u{7}.txt",
        "a/.sharecraft-bin/x.txt",
        ".sharecraft-tmp-probe-1",
        "A/.SHARECRAFT-BIN",
        "con.txt/aux",
        "name?.txt",
        "trailing.",
        long.as_str(),
    ];
    for path in bad {
        assert_eq!(object_key(&t, path), None, "{path:?}");
        for conditional in [Tri::Yes, Tri::No] {
            assert!(
                write_object(&s3, &t, path, &body, &base(None), conditional)
                    .await
                    .is_err(),
                "{path:?}"
            );
        }
        assert!(
            write_beside(&s3, &t, path, &body, "Ada", at(), Tri::Yes)
                .await
                .is_err()
        );
        assert!(
            write_numbered(&s3, &t, path, &body, Tri::Yes)
                .await
                .is_err()
        );
        assert!(make_folder(&s3, &t, path, Tri::Yes).await.is_err());
    }
    // A key over 1,024 bytes.
    let deep = vec!["d".repeat(200); 6].join("/");
    assert_eq!(object_key(&t, &deep), None);
    assert_eq!(fake.requests().len(), 0);

    assert_eq!(object_key(&t, "a/b.txt").as_deref(), Some("lib/a/b.txt"));
    assert_eq!(
        object_key(&target_for(&s3, &fake, BUCKET, ""), "a.txt").as_deref(),
        Some("a.txt")
    );
}

#[tokio::test]
async fn no_key_outside_the_prefix_is_ever_written() {
    let (fake, s3, t) = setup().await;
    fake.put(BUCKET, "other/secret.txt", b"not ours");
    fake.put(BUCKET, "lib/in.txt", b"v1");
    let tag = tag_of(&s3, &t, "lib/in.txt").await;
    let (_f, body) = file_body(b"ours");

    let _ = write_object(&s3, &t, "../other/secret.txt", &body, &base(None), Tri::No).await;
    let _ = write_beside(&s3, &t, "../other/secret.txt", &body, "Ada", at(), Tri::Yes).await;
    let _ = write_numbered(&s3, &t, "../other/secret.txt", &body, Tri::Yes).await;
    let _ = make_folder(&s3, &t, "../other", Tri::No).await;
    done(
        write_object(&s3, &t, "in.txt", &body, &base(Some(tag)), Tri::Yes)
            .await
            .unwrap(),
    );
    let _ = probe_writes(&s3, &t).await;
    write_beside(&s3, &t, "in.txt", &body, "Ada", at(), Tri::Yes)
        .await
        .unwrap();
    write_numbered(&s3, &t, "in.txt", &body, Tri::Yes)
        .await
        .unwrap();
    make_folder(&s3, &t, "sub", Tri::Yes).await.unwrap();

    for (_, key, _) in fake.writes() {
        assert!(key.starts_with("lib/"), "{key}");
    }
    assert_eq!(
        fake.object(BUCKET, "other/secret.txt").unwrap().bytes,
        b"not ours"
    );
    // The hash of the body is recorded on what was written.
    assert_eq!(
        fake.object(BUCKET, "lib/in.txt").unwrap().sha256.as_deref(),
        Some(sha256_hex(b"ours").as_str())
    );
}

#[tokio::test]
async fn a_probe_object_that_cannot_be_removed_is_named() {
    let (fake, s3, t) = setup().await;
    // Three writes pass, the DELETE is refused.
    fake.deny_writes_after(3);
    let (probe, left) = probe_writes(&s3, &t).await;
    assert!(
        matches!(
            probe,
            Probe::ReadWrite {
                conditional: Tri::Yes
            }
        ),
        "{probe:?}"
    );
    let name = left.expect("the name of what was left");
    assert!(name.starts_with(".sharecraft-tmp-probe-"), "{name}");
    assert!(!name.contains('/'));
    assert!(reserved_name(&name));
    assert!(fake.object(BUCKET, &format!("lib/{name}")).is_some());
}

#[tokio::test]
async fn a_failure_after_the_probe_object_landed_still_sends_the_delete() {
    let (fake, s3, t) = setup().await;
    fake.lose_answer_after_write();
    let (probe, left) = probe_writes(&s3, &t).await;
    assert!(matches!(probe, Probe::Unreachable("offline")), "{probe:?}");
    assert_eq!(left, None);
    let writes = fake.writes();
    assert_eq!(writes.len(), 2, "{writes:?}");
    assert_eq!(writes[1].0, "DELETE");
    assert_eq!(writes[0].1, writes[1].1);
    assert!(fake.object(BUCKET, &writes[0].1).is_none());
}

#[tokio::test]
async fn a_refusal_at_the_second_step_is_read_only() {
    let (fake, s3, t) = setup().await;
    fake.deny_writes_after(1);
    let (probe, left) = probe_writes(&s3, &t).await;
    assert!(matches!(probe, Probe::ReadOnly), "{probe:?}");
    // The DELETE was refused as well, so the object is reported.
    assert!(left.is_some());
}

#[tokio::test]
async fn on_a_store_that_ignores_conditions_a_taken_copy_name_is_not_overwritten() {
    let (fake, s3, t) = setup().await;
    fake.ignore_conditions();
    let (_f, body) = file_body(b"my edit");
    let name = sibling_name("report.txt", "Ada", at());
    fake.put(BUCKET, &format!("lib/{name}"), b"theirs");
    for conditional in [Tri::No, Tri::Unknown] {
        let (path, _) = write_beside(&s3, &t, "report.txt", &body, "Ada", at(), conditional)
            .await
            .unwrap();
        assert_eq!(path, numbered_name(&name, 2));
        fake.remove(BUCKET, &format!("lib/{path}"));
    }
    assert_eq!(
        fake.object(BUCKET, &format!("lib/{name}")).unwrap().bytes,
        b"theirs"
    );
    assert!(fake.writes().iter().all(|(_, _, cond)| cond == "none"));

    fake.put(BUCKET, "lib/new.txt", b"theirs");
    let (path, _) = write_numbered(&s3, &t, "new.txt", &body, Tri::No)
        .await
        .unwrap();
    assert_eq!(path, "new (2).txt");
    assert_eq!(fake.object(BUCKET, "lib/new.txt").unwrap().bytes, b"theirs");
}

// ---- Bucket moves (slice 2) ----

/// What the tree recorded for each library-relative path: a file's tag from its `HEAD`, and an
/// empty tag for a folder (a path ending `/`).
async fn known(s3: &S3, t: &S3Target, paths: &[&str]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for path in paths {
        let tag = if path.ends_with('/') {
            String::new()
        } else {
            tag_of(s3, t, &format!("{PREFIX}{path}")).await
        };
        out.push(((*path).to_owned(), tag));
    }
    out
}

fn kinds(fake: &FakeS3) -> Vec<String> {
    fake.writes().into_iter().map(|(kind, _, _)| kind).collect()
}

fn seen_paths(moved: &Moved) -> Vec<String> {
    let mut paths: Vec<String> = moved.seen.iter().map(|(p, _)| p.clone()).collect();
    paths.sort();
    paths
}

fn day(year: i32, month: Month, d: u8) -> Date {
    Date::from_calendar_date(year, month, d).unwrap()
}

fn new_root() -> String {
    holding_root(day(2026, Month::November, 24), NodeId(Uuid::new_v4()))
}

#[tokio::test]
async fn moving_a_file_copies_then_deletes_and_answers_the_new_head() {
    let (fake, s3, t) = setup().await;
    fake.put(BUCKET, "lib/a.txt", b"hello");
    let k = known(&s3, &t, &["a.txt"]).await;

    let moved = move_item(&s3, &t, "a.txt", "dir/b.txt", false, &k)
        .await
        .unwrap();
    assert_eq!(
        fake.writes(),
        [
            w("COPY", "lib/dir/b.txt", "absent"),
            w("DELETE", "lib/a.txt", "none")
        ]
    );
    // The copy is conditional on the source's tag.
    let copy = fake
        .requests()
        .into_iter()
        .find(|r| r.method == "PUT")
        .unwrap();
    assert!(
        copy.headers
            .contains(&"x-amz-copy-source-if-match".to_owned())
    );
    assert!(fake.object(BUCKET, "lib/a.txt").is_none());
    let stored = fake.object(BUCKET, "lib/dir/b.txt").unwrap();
    assert_eq!(stored.bytes, b"hello");
    assert_eq!(moved.objects, 1);
    assert_eq!(moved.left_behind, 0);
    assert_eq!(moved.seen.len(), 1);
    let (path, seen) = &moved.seen[0];
    assert_eq!(path, "dir/b.txt");
    assert_eq!(
        seen.tag,
        object_tag(5, stored.modified_s, &stored.etag, false)
    );
    assert_eq!(seen.size, 5);

    // A path that would leave the prefix is refused before anything is asked.
    let writes = fake.writes().len();
    for (from, to) in [
        ("../x.txt", "y.txt"),
        ("dir/b.txt", "../y.txt"),
        ("dir/b.txt", ""),
    ] {
        assert_eq!(
            move_item(&s3, &t, from, to, false, &[]).await.unwrap_err(),
            WriteError::Failed,
            "{from} -> {to}"
        );
    }
    // A folder cannot move into itself.
    assert_eq!(
        move_item(&s3, &t, "dir/", "dir/inner/", true, &[])
            .await
            .unwrap_err(),
        WriteError::Failed
    );
    assert_eq!(fake.writes().len(), writes);
}

#[tokio::test]
async fn moving_a_folder_moves_every_object_and_its_marker() {
    let (fake, s3, t) = setup().await;
    fake.put(BUCKET, "lib/docs/", b"");
    fake.put(BUCKET, "lib/docs/a.txt", b"a");
    fake.put(BUCKET, "lib/docs/sub/b.txt", b"bb");
    fake.put(BUCKET, "lib/docs2/x.txt", b"not inside");
    fake.put(BUCKET, "lib/docs.txt", b"not inside");
    let k = known(
        &s3,
        &t,
        &["docs/", "docs/a.txt", "docs/sub/", "docs/sub/b.txt"],
    )
    .await;

    let moved = move_item(&s3, &t, "docs/", "archive/docs/", true, &k)
        .await
        .unwrap();
    assert_eq!(moved.objects, 3);
    assert_eq!(moved.left_behind, 0);
    assert_eq!(
        seen_paths(&moved),
        [
            "archive/docs/",
            "archive/docs/a.txt",
            "archive/docs/sub/b.txt"
        ]
    );
    assert_eq!(
        kinds(&fake),
        ["COPY", "COPY", "COPY", "DELETE", "DELETE", "DELETE"]
    );
    assert!(
        fake.writes()
            .iter()
            .all(|(kind, _, cond)| kind == "DELETE" || cond == "absent")
    );
    for key in ["lib/docs/", "lib/docs/a.txt", "lib/docs/sub/b.txt"] {
        assert!(fake.object(BUCKET, key).is_none(), "{key}");
    }
    assert_eq!(
        fake.object(BUCKET, "lib/archive/docs/sub/b.txt")
            .unwrap()
            .bytes,
        b"bb"
    );
    assert!(fake.object(BUCKET, "lib/archive/docs/").is_some());
    assert!(fake.object(BUCKET, "lib/docs2/x.txt").is_some());
    assert!(fake.object(BUCKET, "lib/docs.txt").is_some());
}

#[tokio::test]
async fn moving_a_folder_over_the_cap_is_refused_before_any_write() {
    let (fake, s3, t) = setup().await;
    fake.page_size(400);
    for i in 0..=MOVE_MAX_OBJECTS {
        fake.put(BUCKET, &format!("lib/big/{i:04}.txt"), b"x");
    }
    let result = move_item(&s3, &t, "big/", "other/", true, &[]).await;
    assert_eq!(
        result.unwrap_err(),
        WriteError::TooMany(MOVE_MAX_OBJECTS + 1)
    );
    assert!(fake.writes().is_empty());
    let result = copy_item(&s3, &t, "big/", "other/", true, &[], &[]).await;
    assert_eq!(result.unwrap_err(), WriteError::TooMany(1001));
    assert_eq!(server::source_write::thousands(1001), "1,001");
    assert_eq!(server::source_write::thousands(1_234_567), "1,234,567");
    assert_eq!(server::source_write::thousands(999), "999");
    let result = hold(&s3, &t, "big/", true, &new_root(), &[]).await;
    assert_eq!(result.unwrap_err(), WriteError::TooMany(1001));
    assert!(fake.writes().is_empty());
}

#[tokio::test]
async fn moving_an_object_over_five_gib_is_refused_before_any_write() {
    let (fake, s3, t) = setup().await;
    let huge = server::s3::COPY_MAX + 1;
    fake.put(BUCKET, "lib/huge.bin", b"pretend");
    fake.claim_size("lib/huge.bin", huge);
    fake.put(BUCKET, "lib/f/small.txt", b"s");
    fake.put(BUCKET, "lib/f/huge.bin", b"pretend");
    fake.claim_size("lib/f/huge.bin", huge);
    let k = known(&s3, &t, &["huge.bin", "f/", "f/small.txt", "f/huge.bin"]).await;

    assert_eq!(
        move_item(&s3, &t, "huge.bin", "moved.bin", false, &k)
            .await
            .unwrap_err(),
        WriteError::TooLargeToMove
    );
    assert_eq!(
        move_item(&s3, &t, "f/", "g/", true, &k).await.unwrap_err(),
        WriteError::TooLargeToMove
    );
    assert_eq!(
        copy_item(&s3, &t, "f/", "g/", true, &k, &[])
            .await
            .unwrap_err(),
        WriteError::TooLargeToMove
    );
    assert_eq!(
        hold(&s3, &t, "huge.bin", false, &new_root(), &k)
            .await
            .unwrap_err(),
        WriteError::TooLargeToMove
    );
    assert!(fake.writes().is_empty());
}

#[tokio::test]
async fn moving_a_file_changed_outside_is_refused_and_nothing_is_deleted() {
    let (fake, s3, t) = setup().await;
    fake.put_at(BUCKET, "lib/a.txt", b"v1", 1_700_000_000);
    let k = known(&s3, &t, &["a.txt"]).await;
    fake.put_at(BUCKET, "lib/a.txt", b"changed outside", 1_800_000_000);
    assert_eq!(
        move_item(&s3, &t, "a.txt", "b.txt", false, &k)
            .await
            .unwrap_err(),
        WriteError::Changed
    );
    assert!(fake.writes().is_empty());

    // Changed between the check and the copy: the store refuses the copy.
    let k = known(&s3, &t, &["a.txt"]).await;
    fake.change_after_head("lib/a.txt", b"changed meanwhile");
    assert_eq!(
        move_item(&s3, &t, "a.txt", "b.txt", false, &k)
            .await
            .unwrap_err(),
        WriteError::Changed
    );
    assert_eq!(fake.writes(), [w("COPY", "lib/b.txt", "absent")]);
    assert_eq!(
        fake.object(BUCKET, "lib/a.txt").unwrap().bytes,
        b"changed meanwhile"
    );
    assert!(fake.object(BUCKET, "lib/b.txt").is_none());

    // A file that is gone is a change too.
    fake.remove(BUCKET, "lib/a.txt");
    assert_eq!(
        move_item(&s3, &t, "a.txt", "b.txt", false, &k)
            .await
            .unwrap_err(),
        WriteError::Changed
    );
    assert_eq!(fake.writes().len(), 1);
}

#[tokio::test]
async fn a_key_that_appeared_inside_the_folder_is_a_change() {
    let (fake, s3, t) = setup().await;
    fake.put(BUCKET, "lib/docs/a.txt", b"a");
    let k = known(&s3, &t, &["docs/", "docs/a.txt"]).await;
    fake.put(BUCKET, "lib/docs/new.txt", b"from outside");
    assert_eq!(
        move_item(&s3, &t, "docs/", "other/", true, &k)
            .await
            .unwrap_err(),
        WriteError::Changed
    );
    assert!(fake.writes().is_empty());

    // A known file changed inside the folder is a change as well.
    fake.remove(BUCKET, "lib/docs/new.txt");
    fake.put_at(BUCKET, "lib/docs/a.txt", b"edited", 1_800_000_000);
    assert_eq!(
        hold(&s3, &t, "docs/", true, &new_root(), &k)
            .await
            .unwrap_err(),
        WriteError::Changed
    );
    assert!(fake.writes().is_empty());
    assert!(fake.object(BUCKET, "lib/docs/a.txt").is_some());
}

#[tokio::test]
async fn moving_onto_a_taken_name_is_refused_before_any_write() {
    let (fake, s3, t) = setup().await;
    fake.put(BUCKET, "lib/a.txt", b"mine");
    fake.put(BUCKET, "lib/b.txt", b"theirs");
    fake.put(BUCKET, "lib/docs/a.txt", b"a");
    fake.put(BUCKET, "lib/dest/docs/a.txt", b"theirs");
    let k = known(&s3, &t, &["a.txt", "b.txt", "docs/", "docs/a.txt"]).await;
    assert_eq!(
        move_item(&s3, &t, "a.txt", "b.txt", false, &k)
            .await
            .unwrap_err(),
        WriteError::NameTaken
    );
    assert_eq!(
        move_item(&s3, &t, "docs/", "dest/docs/", true, &k)
            .await
            .unwrap_err(),
        WriteError::NameTaken
    );
    assert!(fake.writes().is_empty());

    // A name taken between the check and the copy: the copy is refused, and nothing of the
    // other writer's is touched.
    fake.appear_after_miss("lib/c.txt", b"someone else's");
    assert_eq!(
        move_item(&s3, &t, "a.txt", "c.txt", false, &k)
            .await
            .unwrap_err(),
        WriteError::NameTaken
    );
    assert_eq!(fake.writes(), [w("COPY", "lib/c.txt", "absent")]);
    assert_eq!(
        fake.object(BUCKET, "lib/c.txt").unwrap().bytes,
        b"someone else's"
    );
    assert_eq!(fake.object(BUCKET, "lib/a.txt").unwrap().bytes, b"mine");
}

#[tokio::test]
async fn a_failed_copy_mid_folder_removes_the_copies_and_keeps_every_original() {
    let (fake, s3, t) = setup().await;
    fake.put(BUCKET, "lib/docs/", b"");
    fake.put(BUCKET, "lib/docs/a.txt", b"a");
    fake.put(BUCKET, "lib/docs/b.txt", b"b");
    fake.put(BUCKET, "lib/docs/c.txt", b"c");
    fake.put(BUCKET, "lib/new/other.txt", b"not ours");
    let k = known(
        &s3,
        &t,
        &["docs/", "docs/a.txt", "docs/b.txt", "docs/c.txt"],
    )
    .await;
    fake.fail_copy_to("lib/new/docs/b.txt", 403, "AccessDenied");

    let result = move_item(&s3, &t, "docs/", "new/docs/", true, &k).await;
    assert_eq!(result.unwrap_err(), WriteError::Denied);
    assert_eq!(
        fake.writes(),
        [
            w("COPY", "lib/new/docs/", "absent"),
            w("COPY", "lib/new/docs/a.txt", "absent"),
            w("COPY", "lib/new/docs/b.txt", "absent"),
            w("DELETE", "lib/new/docs/", "none"),
            w("DELETE", "lib/new/docs/a.txt", "none"),
        ]
    );
    for key in [
        "lib/docs/",
        "lib/docs/a.txt",
        "lib/docs/b.txt",
        "lib/docs/c.txt",
        "lib/new/other.txt",
    ] {
        assert!(fake.object(BUCKET, key).is_some(), "{key}");
    }
    for key in ["lib/new/docs/", "lib/new/docs/a.txt", "lib/new/docs/b.txt"] {
        assert!(fake.object(BUCKET, key).is_none(), "{key}");
    }
}

#[tokio::test]
async fn a_failed_delete_is_counted_as_left_behind() {
    let (fake, s3, t) = setup().await;
    fake.put(BUCKET, "lib/docs/a.txt", b"a");
    fake.put(BUCKET, "lib/docs/b.txt", b"b");
    let k = known(&s3, &t, &["docs/", "docs/a.txt", "docs/b.txt"]).await;
    // Two copies and the first delete are accepted; the second delete is refused.
    fake.deny_writes_after(3);
    let moved = move_item(&s3, &t, "docs/", "new/", true, &k).await.unwrap();
    assert_eq!(moved.objects, 2);
    assert_eq!(moved.left_behind, 1);
    assert_eq!(seen_paths(&moved), ["new/a.txt", "new/b.txt"]);
    assert_eq!(kinds(&fake), ["COPY", "COPY", "DELETE", "DELETE"]);
    assert!(fake.object(BUCKET, "lib/docs/a.txt").is_none());
    assert!(fake.object(BUCKET, "lib/docs/b.txt").is_some());
    assert!(fake.object(BUCKET, "lib/new/a.txt").is_some());
    assert!(fake.object(BUCKET, "lib/new/b.txt").is_some());
}

#[tokio::test]
async fn copying_keeps_the_original() {
    let (fake, s3, t) = setup().await;
    fake.put(BUCKET, "lib/a.txt", b"one");
    fake.put(BUCKET, "lib/docs/", b"");
    fake.put(BUCKET, "lib/docs/b.txt", b"two");
    let k = known(&s3, &t, &["a.txt", "docs/", "docs/b.txt"]).await;

    let copied = copy_item(&s3, &t, "a.txt", "a copy.txt", false, &k, &[])
        .await
        .unwrap();
    assert_eq!(copied.objects, 1);
    assert_eq!(seen_paths(&copied), ["a copy.txt"]);
    let copied = copy_item(&s3, &t, "docs/", "docs copy/", true, &k, &[])
        .await
        .unwrap();
    assert_eq!(copied.objects, 2);
    assert_eq!(seen_paths(&copied), ["docs copy/", "docs copy/b.txt"]);
    assert_eq!(kinds(&fake), ["COPY", "COPY", "COPY"]);
    assert_eq!(fake.object(BUCKET, "lib/a.txt").unwrap().bytes, b"one");
    assert_eq!(fake.object(BUCKET, "lib/a copy.txt").unwrap().bytes, b"one");
    assert_eq!(fake.object(BUCKET, "lib/docs/b.txt").unwrap().bytes, b"two");
    assert_eq!(
        fake.object(BUCKET, "lib/docs copy/b.txt").unwrap().bytes,
        b"two"
    );
}

#[tokio::test]
async fn a_copy_leaves_out_what_it_is_told_to_skip_and_counts_and_checks_none_of_it() {
    let (fake, s3, t) = setup().await;
    fake.put(BUCKET, "lib/docs/", b"");
    fake.put(BUCKET, "lib/docs/a.txt", b"one");
    fake.put(BUCKET, "lib/docs/hidden.txt", b"two");
    fake.put(BUCKET, "lib/docs/hid/", b"");
    fake.put(BUCKET, "lib/docs/hid/b.txt", b"three");
    fake.put(BUCKET, "lib/docs/hid/deep/c.txt", b"four");
    fake.put(BUCKET, "lib/docs/hidden.txt.bak", b"five");
    // The tree's record of the hidden file is stale: left out, it is not checked.
    let mut k = known(
        &s3,
        &t,
        &[
            "docs/",
            "docs/a.txt",
            "docs/hidden.txt",
            "docs/hid/",
            "docs/hid/b.txt",
            "docs/hid/deep/c.txt",
            "docs/hidden.txt.bak",
        ],
    )
    .await;
    for entry in &mut k {
        if entry.0 == "docs/hid/b.txt" {
            entry.1 = "x0-0~stale".to_owned();
        }
    }
    let skip = vec!["docs/hid/".to_owned(), "docs/hidden.txt".to_owned()];

    let copied = copy_item(&s3, &t, "docs/", "docs copy/", true, &k, &skip)
        .await
        .unwrap();
    assert_eq!(copied.objects, 3);
    assert_eq!(
        seen_paths(&copied),
        ["docs copy/", "docs copy/a.txt", "docs copy/hidden.txt.bak"]
    );
    assert_eq!(kinds(&fake), ["COPY", "COPY", "COPY"]);
    for gone in ["hidden.txt", "hid/", "hid/b.txt", "hid/deep/c.txt"] {
        assert!(
            fake.object(BUCKET, &format!("lib/docs copy/{gone}"))
                .is_none(),
            "{gone}"
        );
    }
    assert!(fake.object(BUCKET, "lib/docs/hid/b.txt").is_some());
}

#[tokio::test]
async fn hold_and_unhold_round_trip_the_bytes_and_the_sha_metadata() {
    let (fake, s3, t) = setup().await;
    let (_f, body) = file_body(b"precious");
    write_object(&s3, &t, "docs/a.txt", &body, &base(None), Tri::Yes)
        .await
        .unwrap();
    make_folder(&s3, &t, "docs", Tri::Yes).await.unwrap();
    let sha = fake.object(BUCKET, "lib/docs/a.txt").unwrap().sha256;
    assert!(sha.is_some());
    let entry = NodeId(Uuid::new_v4());
    let root = holding_root(day(2026, Month::November, 24), entry);
    assert_eq!(root, format!(".sharecraft-bin/2026-11-24/{}/", entry.0));
    let k = known(&s3, &t, &["docs/", "docs/a.txt"]).await;
    let before = fake.writes().len();

    let held = hold(&s3, &t, "docs/", true, &root, &k).await.unwrap();
    assert_eq!(held.objects, 2);
    assert_eq!(held.left_behind, 0);
    assert_eq!(
        seen_paths(&held),
        [format!("{root}docs/"), format!("{root}docs/a.txt")]
    );
    let held_key = format!("lib/{root}docs/a.txt");
    let stored = fake.object(BUCKET, &held_key).unwrap();
    assert_eq!(stored.bytes, b"precious");
    assert_eq!(stored.sha256, sha);
    assert!(fake.object(BUCKET, "lib/docs/a.txt").is_none());
    assert!(fake.object(BUCKET, "lib/docs/").is_none());
    assert!(
        fake.writes()[before..]
            .iter()
            .all(|(_, key, _)| key.starts_with("lib/"))
    );

    let back = unhold(&s3, &t, &root, "docs/", "docs/", true)
        .await
        .unwrap();
    assert_eq!(back.objects, 2);
    assert_eq!(seen_paths(&back), ["docs/", "docs/a.txt"]);
    let stored = fake.object(BUCKET, "lib/docs/a.txt").unwrap();
    assert_eq!(stored.bytes, b"precious");
    assert_eq!(stored.sha256, sha);
    assert!(fake.object(BUCKET, &held_key).is_none());

    // A root that is not a holding root, or a holding key over 1,024 bytes, is refused first.
    let writes = fake.writes().len();
    let k = known(&s3, &t, &["docs/a.txt"]).await;
    for bad in [
        "docs/",
        ".sharecraft-bin/../",
        ".sharecraft-bin/2026-11-24/x/",
        ".sharecraft-bin/2026-11-24/",
        "",
    ] {
        assert_eq!(
            hold(&s3, &t, "docs/a.txt", false, bad, &k)
                .await
                .unwrap_err(),
            WriteError::Failed,
            "{bad}"
        );
    }
    // 967 bytes: a key in the library, but too long once under the holding root.
    let segment = "日".repeat(80);
    let long = format!("{segment}/{segment}/{segment}/{segment}.txt");
    assert_eq!(long.len(), 967);
    assert!(object_key(&t, &long).is_some());
    assert_eq!(
        hold(&s3, &t, &long, false, &root, &[]).await.unwrap_err(),
        WriteError::Failed
    );
    assert_eq!(fake.writes().len(), writes);
}

#[tokio::test]
async fn unhold_onto_a_taken_name_answers_name_taken() {
    let (fake, s3, t) = setup().await;
    fake.put(BUCKET, "lib/a.txt", b"deleted one");
    let k = known(&s3, &t, &["a.txt"]).await;
    let root = new_root();
    hold(&s3, &t, "a.txt", false, &root, &k).await.unwrap();
    fake.put(BUCKET, "lib/a.txt", b"a new one");
    let writes = fake.writes().len();

    assert_eq!(
        unhold(&s3, &t, &root, "a.txt", "a.txt", false)
            .await
            .unwrap_err(),
        WriteError::NameTaken
    );
    assert_eq!(fake.writes().len(), writes);
    let back = unhold(&s3, &t, &root, "a.txt", "a (2).txt", false)
        .await
        .unwrap();
    assert_eq!(seen_paths(&back), ["a (2).txt"]);
    assert_eq!(
        fake.object(BUCKET, "lib/a (2).txt").unwrap().bytes,
        b"deleted one"
    );
    assert_eq!(
        fake.object(BUCKET, "lib/a.txt").unwrap().bytes,
        b"a new one"
    );
}

#[tokio::test]
async fn delete_held_removes_only_that_root() {
    let (fake, s3, t) = setup().await;
    let one = new_root();
    let two = new_root();
    fake.put(BUCKET, &format!("lib/{one}docs/"), b"");
    fake.put(BUCKET, &format!("lib/{one}docs/a.txt"), b"a");
    fake.put(BUCKET, &format!("lib/{one}b.txt"), b"b");
    fake.put(BUCKET, &format!("lib/{two}a.txt"), b"kept");
    fake.put(BUCKET, "lib/a.txt", b"kept");
    // The same root under another prefix is another library's.
    fake.put(BUCKET, &format!("other/{one}a.txt"), b"kept");

    assert_eq!(delete_held(&s3, &t, &one).await.unwrap(), 3);
    assert_eq!(kinds(&fake), ["DELETE", "DELETE", "DELETE"]);
    assert!(
        fake.object(BUCKET, &format!("lib/{one}docs/a.txt"))
            .is_none()
    );
    assert!(fake.object(BUCKET, &format!("lib/{two}a.txt")).is_some());
    assert!(fake.object(BUCKET, "lib/a.txt").is_some());
    assert!(fake.object(BUCKET, &format!("other/{one}a.txt")).is_some());

    // A failed delete is reported after every key was tried.
    fake.put(BUCKET, &format!("lib/{two}b.txt"), b"b");
    fake.deny_writes_after(3);
    assert_eq!(
        delete_held(&s3, &t, &two).await.unwrap_err(),
        WriteError::Denied
    );
    assert_eq!(fake.writes().len(), 5);
    // Not a holding root: refused without a request.
    assert_eq!(
        delete_held(&s3, &t, "").await.unwrap_err(),
        WriteError::Failed
    );
    assert_eq!(
        delete_held(&s3, &t, "docs/").await.unwrap_err(),
        WriteError::Failed
    );
    assert_eq!(fake.writes().len(), 5);
}

#[tokio::test]
async fn the_sweep_removes_only_old_unnamed_objects_inside_the_holding_prefix() {
    let (fake, s3, t) = setup().await;
    let old = day(2026, Month::January, 1);
    let unnamed = NodeId(Uuid::new_v4());
    let kept = NodeId(Uuid::new_v4());
    let gone_one = format!("lib/{}a.txt", holding_root(old, unnamed));
    let gone_two = format!("lib/{}docs/b.txt", holding_root(old, unnamed));
    let left = [
        format!("lib/{}a.txt", holding_root(old, kept)),
        format!(
            "lib/{}a.txt",
            holding_root(day(2026, Month::November, 20), unnamed)
        ),
        "lib/.sharecraft-bin/notes.txt".to_owned(),
        "lib/.sharecraft-bin/2026-01-01/notes.txt".to_owned(),
        format!("lib/.sharecraft-bin/2026-01-01/{}", unnamed.0),
        format!("lib/.sharecraft-bin/not-a-date/{}/a.txt", unnamed.0),
        format!("lib/.sharecraft-bin/2026-13-01/{}/a.txt", unnamed.0),
        "lib/.sharecraft-bin/2026-01-01/not-an-id/a.txt".to_owned(),
        "lib/.sharecraft-binder/2026-01-01/x/a.txt".to_owned(),
        "lib/old.txt".to_owned(),
        format!("lib/docs/{}a.txt", holding_root(old, unnamed)),
        format!("other/{}a.txt", holding_root(old, unnamed)),
    ];
    fake.put(BUCKET, &gone_one, b"x");
    fake.put(BUCKET, &gone_two, b"x");
    for key in &left {
        fake.put(BUCKET, key, b"x");
    }
    let keep: HashSet<NodeId> = [kept].into_iter().collect();

    let count = sweep_holding(&s3, &t, day(2026, Month::August, 1), &keep)
        .await
        .unwrap();
    assert_eq!(count, 2);
    let mut deleted: Vec<String> = fake.writes().into_iter().map(|(_, key, _)| key).collect();
    deleted.sort();
    let mut expected = vec![gone_one, gone_two];
    expected.sort();
    assert_eq!(deleted, expected);
    assert!(kinds(&fake).iter().all(|kind| kind == "DELETE"));
    for key in &left {
        assert!(fake.object(BUCKET, key).is_some(), "{key}");
    }
}

#[tokio::test]
async fn moving_a_file_with_no_recorded_tag_is_a_change() {
    let (fake, s3, t) = setup().await;
    fake.put(BUCKET, "lib/a.txt", b"never recorded");
    let other = known(&s3, &t, &["a.txt"]).await;
    let other = [("b.txt".to_owned(), other[0].1.clone())];
    for k in [&[][..], &other[..]] {
        assert_eq!(
            move_item(&s3, &t, "a.txt", "c.txt", false, k)
                .await
                .unwrap_err(),
            WriteError::Changed
        );
        assert_eq!(
            copy_item(&s3, &t, "a.txt", "c.txt", false, k, &[])
                .await
                .unwrap_err(),
            WriteError::Changed
        );
        assert_eq!(
            hold(&s3, &t, "a.txt", false, &new_root(), k)
                .await
                .unwrap_err(),
            WriteError::Changed
        );
    }
    assert!(fake.writes().is_empty());
    assert!(fake.object(BUCKET, "lib/a.txt").is_some());
}
