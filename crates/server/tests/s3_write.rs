//! Safe writes to a bucket, the write probe and the shared write types, against the fake S3.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::time::Duration;

use common::fake_s3::FakeS3;
use records::{S3Endpoint, Tri, object_tag, reserved_name, windows_safe_name};
use server::s3::{PutBody, S3, S3Target};
use server::s3_sign::{Credentials, sha256_hex};
use server::s3_write::{
    make_folder, object_key, probe_writes, write_beside, write_numbered, write_object,
};
use server::secrets::Secret;
use server::source_write::{Base, Probe, Seen, WriteError, Written, numbered_name, sibling_name};
use sha2::Digest;
use time::OffsetDateTime;

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

    let (path, seen) = write_beside(&s3, &t, "docs/report.txt", &body, "Ada", at())
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
    let (path2, _) = write_beside(&s3, &t, "docs/report.txt", &other, "Ada", at())
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
        write_beside(&s3, &t, "docs/report.txt", &third, "Ada", at())
            .await
            .unwrap_err(),
        WriteError::Failed
    );
    // A file at the top of the library works as well.
    let (top, _) = write_beside(&s3, &t, "top.txt", &body, "Bo", at())
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

    let (path, seen) = write_numbered(&s3, &t, "d/new.txt", &body).await.unwrap();
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
        write_numbered(&s3, &t, "d/new.txt", &other)
            .await
            .unwrap_err(),
        WriteError::Failed
    );
}

#[tokio::test]
async fn make_folder_writes_a_zero_byte_marker_and_accepts_an_existing_one() {
    let (fake, s3, t) = setup().await;
    make_folder(&s3, &t, "photos/2024").await.unwrap();
    assert_eq!(fake.writes(), [w("PUT", "lib/photos/2024/", "absent")]);
    assert_eq!(fake.last_write_body_len(), Some(0));
    assert_eq!(fake.object(BUCKET, "lib/photos/2024/").unwrap().bytes, b"");

    // Again: a marker already there is success, and nothing is deleted.
    make_folder(&s3, &t, "photos/2024").await.unwrap();
    assert!(fake.writes().iter().all(|(method, _, _)| method == "PUT"));
    assert!(fake.object(BUCKET, "lib/photos/2024/").is_some());

    assert_eq!(
        make_folder(&s3, &t, "../x").await.unwrap_err(),
        WriteError::Failed
    );
    assert_eq!(
        make_folder(&s3, &t, "a/").await.unwrap_err(),
        WriteError::Failed
    );
}

#[tokio::test]
async fn the_probe_finds_read_write_with_conditions() {
    let (fake, s3, t) = setup().await;
    let probe = probe_writes(&s3, &t).await;
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
    let probe = probe_writes(&s3, &t).await;
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
    let probe = probe_writes(&s3, &t).await;
    assert!(matches!(probe, Probe::ReadOnly), "{probe:?}");
}

#[tokio::test]
async fn the_probe_reports_why_it_could_not_ask() {
    let fake = FakeS3::start().await;
    let s3 = client();
    // No such bucket.
    let t = target_for(&s3, &fake, "nowhere", PREFIX);
    assert!(matches!(
        probe_writes(&s3, &t).await,
        Probe::Unreachable("bucket")
    ));
    // Wrong keys.
    fake.bucket(BUCKET);
    fake.require_keys("other", "keys");
    let t = target_for(&s3, &fake, BUCKET, PREFIX);
    assert!(matches!(
        probe_writes(&s3, &t).await,
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
        let probe = probe_writes(&s3, &t).await;
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
    // Over the single-PUT limit: refused before any request.
    s3.set_single_put_max_for_tests(2);
    let before = fake.requests().len();
    assert_eq!(
        write_object(&s3, &t, "big.bin", &body, &base(None), Tri::Yes)
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
            write_beside(&s3, &t, path, &body, "Ada", at())
                .await
                .is_err()
        );
        assert!(write_numbered(&s3, &t, path, &body).await.is_err());
        assert!(make_folder(&s3, &t, path).await.is_err());
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
    let _ = write_beside(&s3, &t, "../other/secret.txt", &body, "Ada", at()).await;
    let _ = write_numbered(&s3, &t, "../other/secret.txt", &body).await;
    let _ = make_folder(&s3, &t, "../other").await;
    done(
        write_object(&s3, &t, "in.txt", &body, &base(Some(tag)), Tri::Yes)
            .await
            .unwrap(),
    );
    let _ = probe_writes(&s3, &t).await;
    write_beside(&s3, &t, "in.txt", &body, "Ada", at())
        .await
        .unwrap();
    write_numbered(&s3, &t, "in.txt", &body).await.unwrap();
    make_folder(&s3, &t, "sub").await.unwrap();

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
