#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::fs;
use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

use common::fake_share::FakeShare;
use records::{SharePath, external_tag};
use server::share::{FsKind, ListError, ListLimits, OsFs, ShareFs, list_share};
use server::source::{EntryKind, TreeEntry};

fn root() -> SharePath {
    SharePath::parse(r"C:\share").unwrap()
}

fn limits() -> ListLimits {
    ListLimits {
        max_files: 100,
        max_time: Duration::from_secs(5),
        at_once: 4,
        max_depth: 64,
    }
}

async fn listing(fake: &FakeShare) -> server::share::ShareListing {
    list_share(Arc::new(fake.clone()), &root(), &limits())
        .await
        .unwrap()
}

#[tokio::test]
async fn lists_folders_before_files_with_tags_and_sizes() {
    let fake = FakeShare::new(r"C:\share");
    fake.mkdir("docs");
    fake.put_at("docs/report.txt", b"report", 27);
    fake.put_at("top.txt", b"top", 11);

    let answer = listing(&fake).await;

    assert_eq!(
        answer.entries,
        vec![
            TreeEntry {
                path: "docs".into(),
                kind: EntryKind::Dir,
                sha: String::new(),
                size: 0
            },
            TreeEntry {
                path: "docs/report.txt".into(),
                kind: EntryKind::File,
                sha: external_tag(6, 27),
                size: 6
            },
            TreeEntry {
                path: "top.txt".into(),
                kind: EntryKind::File,
                sha: external_tag(3, 11),
                size: 3
            },
        ]
    );
}

#[tokio::test]
async fn an_empty_folder_is_listed() {
    let fake = FakeShare::new(r"C:\share");
    fake.mkdir("empty");
    assert_eq!(listing(&fake).await.entries[0].path, "empty");
}

#[tokio::test]
async fn links_and_junctions_are_counted_and_never_entered() {
    let fake = FakeShare::new(r"C:\share");
    fake.mkdir(r"C:\outside");
    fake.put(r"C:\outside\secret.txt", b"secret");
    fake.link("escape", r"C:\outside");

    let answer = listing(&fake).await;

    assert!(answer.entries.is_empty());
    assert_eq!(answer.hidden.symlinks, 1);
    assert_eq!(fake.lists(), 1);
    assert!(fake.opened_paths().is_empty());
}

#[tokio::test]
async fn hidden_system_and_default_names_are_counted() {
    let fake = FakeShare::new(r"C:\share");
    fake.put("hidden.txt", b"h");
    fake.set_attributes("hidden.txt", 0x2);
    fake.mkdir("system");
    fake.set_attributes("system", 0x4);
    fake.put("Thumbs.db", b"t");
    fake.put("shown.txt", b"s");

    let answer = listing(&fake).await;

    assert_eq!(answer.hidden.system, 3);
    assert_eq!(
        answer
            .entries
            .iter()
            .map(|e| e.path.as_str())
            .collect::<Vec<_>>(),
        ["shown.txt"]
    );
}

#[tokio::test]
async fn cloud_placeholders_are_counted_as_offline() {
    let fake = FakeShare::new(r"C:\share");
    for (name, attribute) in [
        ("offline", 0x1000),
        ("recall-open", 0x40000),
        ("recall-data", 0x400000),
    ] {
        fake.put(name, b"cloud");
        fake.set_attributes(name, attribute);
    }
    assert_eq!(listing(&fake).await.hidden.offline, 3);
}

#[tokio::test]
async fn unsafe_and_non_unicode_names_are_counted() {
    let fake = FakeShare::new(r"C:\share");
    fake.put("CON", b"bad");
    fake.put_unnameable("not-unicode", b"bad");
    assert_eq!(listing(&fake).await.hidden.unnameable, 2);
}

#[tokio::test]
async fn an_over_long_path_is_counted() {
    let fake = FakeShare::new(r"C:\share");
    let first = "a".repeat(250);
    let second = "b".repeat(151);
    fake.mkdir(&first);
    fake.put(&format!("{first}/{second}"), b"long");

    let answer = listing(&fake).await;

    assert_eq!(answer.hidden.too_long, 1);
    assert_eq!(answer.entries.len(), 1);
}

#[tokio::test]
async fn an_unreadable_sub_folder_is_reported_and_the_rest_is_listed() {
    let fake = FakeShare::new(r"C:\share");
    fake.mkdir("denied");
    fake.put("denied/secret.txt", b"secret");
    fake.deny("denied");
    fake.put("shown.txt", b"shown");

    let answer = listing(&fake).await;

    assert_eq!(answer.unreadable, ["denied"]);
    assert_eq!(answer.hidden.unreadable, 1);
    assert!(answer.entries.iter().any(|e| e.path == "shown.txt"));
}

#[tokio::test]
async fn an_unreachable_root_is_an_error_not_an_empty_listing() {
    let fake = FakeShare::new(r"C:\share");
    fake.offline(true);
    let error = list_share(Arc::new(fake), &root(), &limits())
        .await
        .unwrap_err();
    assert_eq!(error, ListError::Unreachable);
}

#[tokio::test]
async fn too_many_files_stops_early() {
    let fake = FakeShare::new(r"C:\share");
    fake.mkdir("later");
    fake.put("later/unseen.txt", b"later");
    fake.put("a.txt", b"a");
    fake.put("b.txt", b"b");
    let mut small = limits();
    small.max_files = 1;

    let error = list_share(Arc::new(fake.clone()), &root(), &small)
        .await
        .unwrap_err();

    assert_eq!(error, ListError::TooManyFiles(2));
    assert_eq!(fake.lists(), 1);
}

#[tokio::test]
async fn a_slow_share_fails_too_slow() {
    let fake = FakeShare::new(r"C:\share");
    fake.slow_list(Duration::from_millis(100));
    let mut quick = limits();
    quick.max_time = Duration::from_millis(50);
    assert_eq!(
        list_share(Arc::new(fake), &root(), &quick)
            .await
            .unwrap_err(),
        ListError::TooSlow
    );
}

#[tokio::test]
async fn too_deep_fails() {
    let fake = FakeShare::new(r"C:\share");
    fake.put("one/two/file.txt", b"deep");
    let mut shallow = limits();
    shallow.max_depth = 1;
    assert_eq!(
        list_share(Arc::new(fake), &root(), &shallow)
            .await
            .unwrap_err(),
        ListError::TooDeep
    );
}

#[tokio::test]
async fn case_colliding_names_are_both_listed() {
    let fake = FakeShare::new(r"C:\share");
    fake.put("Report.pdf", b"one");
    fake.put("report.pdf", b"two");
    let answer = listing(&fake).await;
    assert_eq!(answer.entries.len(), 2);
    assert!(answer.entries.iter().any(|e| e.path == "Report.pdf"));
    assert!(answer.entries.iter().any(|e| e.path == "report.pdf"));
}

#[tokio::test]
async fn sharecrafts_own_folder_and_temporary_files_are_skipped_and_temps_reported() {
    let fake = FakeShare::new(r"C:\share");
    fake.put_at(".sharecraft-tmp-old", b"temp", 42);
    fake.mkdir(".sharecraft-bin");
    fake.put(".sharecraft-bin/held.txt", b"held");
    fake.put("shown.txt", b"shown");

    let answer = listing(&fake).await;

    assert_eq!(answer.entries.len(), 1);
    assert_eq!(answer.entries[0].path, "shown.txt");
    assert_eq!(answer.temps, [(".sharecraft-tmp-old".into(), 42)]);
    assert_eq!(answer.hidden.total(), 0);
}

#[test]
fn os_fs_lists_stats_and_opens_real_files() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("real.txt");
    fs::write(&file, b"real bytes").unwrap();
    let os = OsFs;

    let listed = os.list(dir.path()).unwrap();
    let stat = os.stat(&file).unwrap();
    let mut bytes = Vec::new();
    os.open(&file).unwrap().read_to_end(&mut bytes).unwrap();

    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name.as_deref(), Some("real.txt"));
    assert_eq!(listed[0].kind, FsKind::File);
    assert_eq!(stat.kind, FsKind::File);
    assert_eq!(stat.size, 10);
    assert_eq!(bytes, b"real bytes");
}

#[test]
fn os_fs_reports_a_missing_path_as_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let error = OsFs.stat(&dir.path().join("missing")).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
}

#[cfg(windows)]
#[test]
fn os_fs_does_not_follow_a_junction() {
    use std::process::Command;

    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret.txt"), b"secret").unwrap();
    let junction = dir.path().join("junction");
    let output = Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(&junction)
        .arg(outside.path())
        .output()
        .unwrap();
    if !output.status.success() {
        eprintln!("skipping junction assertion: cmd /c mklink /J failed");
        return;
    }

    let entries = OsFs.list(dir.path()).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].kind, FsKind::Link);
    assert_eq!(OsFs.stat(&junction).unwrap().kind, FsKind::Link);
}
