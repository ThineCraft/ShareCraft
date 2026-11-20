//! Share-backed content is fetched once, scanned, cached, and evicted safely.

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

use std::io::ErrorKind;
use std::sync::Arc;
use std::time::{Duration, Instant};

use collab_core::ids::{DocId, NodeId};
use common::fake_share::{FakeShare, native};
use common::{
    TIMEOUT, TestServer, TestUser, spawn_server_config, spawn_server_config_without_workers,
    start_fake_github,
};
use fake_icap::{Behaviour, FakeIcap, eicar};
use records::{
    NewSource, SCANNER_URL, SHARE_ROOTS, ScanState, SourceKind, SourceMode, Unavailable,
};
use revisions::{BlobRef, Content};
use serde_json::json;
use server::AppState;
use server::external::{ExternalError, Ready, evict, materialise};
use server::source_sync::sync_once;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use uuid::Uuid;

#[cfg(windows)]
const ROOT: &str = r"C:\approved";
#[cfg(not(windows))]
const ROOT: &str = "/approved";

struct Env {
    server: TestServer,
    fake: FakeShare,
    admin: TestUser,
    library: NodeId,
    state: AppState,
}

impl Env {
    async fn new(files: &[(&str, &[u8], i64)]) -> Self {
        let server = spawn_server_config_without_workers(|config| {
            config.max_file_bytes = 512 * 1024 * 1024;
        })
        .await;
        let admin = server.admin("External content admin").await;
        let library = common::Site::library(&server).await;
        let state = server.app_state().await;
        let fake = FakeShare::new(ROOT);
        for (path, bytes, modified) in files {
            fake.put_at(path, bytes, *modified);
        }
        state.shares().set_fs(Arc::new(fake.clone()));
        let db = server.records_db();
        db.set_setting(admin.id, SHARE_ROOTS, json!([ROOT]))
            .await
            .unwrap();
        db.connect_source(&NewSource {
            library,
            kind: SourceKind::Share,
            owner: String::new(),
            repo: String::new(),
            branch: String::new(),
            folder: String::new(),
            location: Some(ROOT.to_owned()),
            mode: SourceMode::ReadOnly,
            token: None,
            poll_seconds: 60,
            connected_by: admin.id,
        })
        .await
        .unwrap();
        let listed = sync_once(&state, library, OffsetDateTime::UNIX_EPOCH)
            .await
            .unwrap();
        if files.is_empty() {
            assert!(listed.is_none());
        } else {
            listed.expect("the initial listing changed");
        }
        Self {
            server,
            fake,
            admin,
            library,
            state,
        }
    }

    fn db(&self) -> records::Db {
        self.server.records_db()
    }

    async fn item(&self, path: &str) -> records::ExternalItem {
        let db = self.db();
        let source = db
            .source_items(self.library)
            .await
            .unwrap()
            .into_iter()
            .find(|item| item.repository_path == path)
            .unwrap_or_else(|| panic!("no source item at {path}"));
        db.external_item(source.node).await.unwrap().unwrap()
    }

    async fn ready(&self, path: &str) -> Ready {
        let item = self.item(path).await;
        materialise(&self.state, item.node)
            .await
            .unwrap()
            .expect("external content")
    }

    fn bytes(&self, ready: &Ready) -> Vec<u8> {
        self.server
            .blobs()
            .read_all(&Content::Blob(ready.blob))
            .unwrap()
    }

    async fn cache_contains(&self, blob: BlobRef) -> bool {
        let count: i64 = sqlx::query_scalar("select count(*) from source_cache where sha256 = $1")
            .bind(blob.hex())
            .fetch_one(self.db().test_pool())
            .await
            .unwrap();
        count == 1
    }
}

fn sha(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

async fn wait_until(mut done: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while !done() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "condition timed out"
        );
        tokio::task::yield_now().await;
    }
}

async fn wait_scan_row(db: &records::Db, bytes: &[u8]) {
    let digest = sha(bytes);
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        if db.scan_state(&digest).await.unwrap().is_some() {
            return;
        }
        assert!(tokio::time::Instant::now() < deadline, "scan row timed out");
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn first_read_stores_hashes_pins_and_records_the_hash() {
    let env = Env::new(&[("report.txt", b"quarterly", 1)]).await;
    let before = env.item("report.txt").await;

    let ready = env.ready("report.txt").await;

    assert_eq!(env.bytes(&ready), b"quarterly");
    assert_eq!(ready.blob, BlobRef::of(b"quarterly"));
    assert_eq!(ready.revision, before.revision);
    assert_eq!(env.fake.opens("report.txt"), 1);
    assert_eq!(env.item("report.txt").await.sha256, Some(sha(b"quarterly")));
    assert!(env.cache_contains(ready.blob).await);
}

#[tokio::test]
async fn a_second_read_does_not_open_the_file_again() {
    let env = Env::new(&[("report.txt", b"quarterly", 1)]).await;
    let first = env.ready("report.txt").await;
    drop(first);
    let stats = env.fake.stats();

    let second = env.ready("report.txt").await;

    assert_eq!(env.bytes(&second), b"quarterly");
    assert_eq!(env.fake.stats(), stats + 1);
    assert_eq!(env.fake.opens("report.txt"), 1);
}

#[tokio::test]
async fn two_first_reads_at_once_open_the_file_once() {
    let env = Env::new(&[("report.txt", b"quarterly", 1)]).await;
    env.fake.slow_open("report.txt", Duration::from_millis(100));
    let node = env.item("report.txt").await.node;
    let left_state = env.state.clone();
    let right_state = env.state.clone();

    let (left, right) = tokio::join!(
        materialise(&left_state, node),
        materialise(&right_state, node)
    );

    let left = left.unwrap().unwrap();
    let right = right.unwrap().unwrap();
    assert_eq!(left.blob, right.blob);
    assert_eq!(env.fake.opens("report.txt"), 1);
}

#[tokio::test]
async fn a_file_changed_since_the_listing_serves_the_new_bytes_and_wakes_a_listing() {
    let env = Env::new(&[("report.txt", b"old", 1)]).await;
    let before = env.item("report.txt").await;
    env.fake.put_at("report.txt", b"new bytes", 2);

    let ready = env.ready("report.txt").await;

    let after = env.item("report.txt").await;
    assert_eq!(env.bytes(&ready), b"new bytes");
    assert_ne!(after.revision, before.revision);
    assert_eq!(after.size, 9);
    assert_eq!(
        server::source_tick_once(&env.state, OffsetDateTime::UNIX_EPOCH).await,
        1
    );
}

#[tokio::test]
async fn a_file_changed_without_a_new_time_is_noticed_when_it_is_read_again_after_eviction() {
    let env = Env::new(&[("same.txt", b"first", 7)]).await;
    let first = env.ready("same.txt").await;
    let old = first.blob;
    let old_revision = first.revision.clone();
    drop(first);
    env.state.shares().set_cache_max_bytes_for_tests(0);
    assert_eq!(evict(&env.state).await, 1);
    env.state.shares().set_cache_max_bytes_for_tests(10);
    env.fake.put_at("same.txt", b"other", 7);

    let second = env.ready("same.txt").await;

    assert_eq!(env.bytes(&second), b"other");
    assert_ne!(second.blob, old);
    let current = env.item("same.txt").await;
    assert_eq!(current.sha256, Some(sha(b"other")));
    assert_ne!(second.revision, old_revision);
    assert_eq!(second.revision, current.revision);
}

#[tokio::test]
async fn a_vanished_file_is_gone_and_marked() {
    let env = Env::new(&[("gone.txt", b"there", 1)]).await;
    let node = env.item("gone.txt").await.node;
    env.fake.vanish_on_open("gone.txt");

    assert!(matches!(
        materialise(&env.state, node).await,
        Err(ExternalError::Gone)
    ));
    assert_eq!(
        env.db()
            .external_item(node)
            .await
            .unwrap()
            .unwrap()
            .unavailable,
        Some(Unavailable::Gone)
    );
}

#[tokio::test]
async fn a_denied_file_is_denied_and_marked() {
    let env = Env::new(&[("denied.txt", b"there", 1)]).await;
    let node = env.item("denied.txt").await.node;
    env.fake.deny("denied.txt");

    assert!(matches!(
        materialise(&env.state, node).await,
        Err(ExternalError::Denied)
    ));
    assert_eq!(
        env.db()
            .external_item(node)
            .await
            .unwrap()
            .unwrap()
            .unavailable,
        Some(Unavailable::Denied)
    );
}

#[tokio::test]
async fn a_file_that_changes_while_being_read_is_not_cached() {
    let env = Env::new(&[("moving.txt", b"one", 1)]).await;
    let node = env.item("moving.txt").await.node;
    env.fake.change_during_read("moving.txt", b"two");
    env.fake.change_during_read("moving.txt", b"six");

    assert!(matches!(
        materialise(&env.state, node).await,
        Err(ExternalError::Changing)
    ));
    assert_eq!(env.db().cached_bytes().await.unwrap(), 0);
    assert!(
        env.db()
            .external_item(node)
            .await
            .unwrap()
            .unwrap()
            .sha256
            .is_none()
    );
}

#[tokio::test]
async fn one_change_followed_by_a_stable_read_records_matching_metadata() {
    let env = Env::new(&[("settles.txt", b"one", 1)]).await;
    let before = env.item("settles.txt").await;
    env.fake.change_during_read("settles.txt", b"longer");

    let ready = env.ready("settles.txt").await;
    let after = env.item("settles.txt").await;

    assert_eq!(env.bytes(&ready), b"longer");
    assert_eq!(after.size, 6);
    assert_eq!(after.mtime_ns, 2);
    assert_eq!(after.sha256, Some(ready.blob.sha256));
    assert_ne!(after.revision, before.revision);
    assert_eq!(ready.revision, after.revision);
}

#[tokio::test]
async fn growth_between_stats_is_checked_against_the_cache_limit() {
    let env = Env::new(&[("grows.txt", b"four", 1)]).await;
    env.state.shares().set_cache_max_bytes_for_tests(10);
    env.fake.change_after_stat("grows.txt", b"123456");
    let node = env.item("grows.txt").await.node;

    assert!(matches!(
        materialise(&env.state, node).await,
        Err(ExternalError::TooLarge)
    ));
    assert_eq!(env.db().cached_bytes().await.unwrap(), 0);
}

#[tokio::test]
async fn a_short_read_is_not_cached() {
    let env = Env::new(&[("short.txt", b"whole", 1)]).await;
    let node = env.item("short.txt").await.node;
    env.fake.truncate_read("short.txt", 2);

    assert!(matches!(
        materialise(&env.state, node).await,
        Err(ExternalError::Changing)
    ));
    assert_eq!(env.db().cached_bytes().await.unwrap(), 0);
}

#[tokio::test]
async fn an_offline_share_serves_what_is_cached_and_refuses_the_rest() {
    let env = Env::new(&[("cached.txt", b"cached", 1), ("cold.txt", b"cold", 1)]).await;
    let cached = env.ready("cached.txt").await;
    drop(cached);
    let cold = env.item("cold.txt").await.node;
    env.fake.offline(true);

    assert_eq!(env.bytes(&env.ready("cached.txt").await), b"cached");
    assert!(matches!(
        materialise(&env.state, cold).await,
        Err(ExternalError::Offline)
    ));
}

#[tokio::test]
async fn offline_is_remembered_so_later_reads_do_not_wait_again() {
    let env = Env::new(&[("one.txt", b"one", 1), ("two.txt", b"two", 1)]).await;
    env.fake.offline(true);
    let one = env.item("one.txt").await.node;
    let two = env.item("two.txt").await.node;

    assert!(matches!(
        materialise(&env.state, one).await,
        Err(ExternalError::Offline)
    ));
    let stats = env.fake.stats();
    assert!(matches!(
        materialise(&env.state, two).await,
        Err(ExternalError::Offline)
    ));
    assert_eq!(env.fake.stats(), stats);
}

#[tokio::test]
async fn hung_opens_time_out_mark_the_library_offline_and_keep_reads_bounded() {
    let env = Env::new(&[
        ("hung-0.txt", b"zero", 1),
        ("hung-1.txt", b"one!", 1),
        ("hung-2.txt", b"two!", 1),
        ("hung-3.txt", b"three", 1),
        ("later.txt", b"later", 1),
    ])
    .await;
    env.state
        .shares()
        .set_operation_timeout(Duration::from_millis(150));
    let mut tasks = Vec::new();
    for n in 0..4 {
        let path = format!("hung-{n}.txt");
        env.fake.slow_open(&path, Duration::from_millis(500));
        let node = env.item(&path).await.node;
        let state = env.state.clone();
        tasks.push(tokio::spawn(async move { materialise(&state, node).await }));
    }
    wait_until(|| env.fake.open_started() == 4).await;

    let started = Instant::now();
    for task in tasks {
        assert!(matches!(task.await.unwrap(), Err(ExternalError::Offline)));
    }
    assert!(started.elapsed() < Duration::from_millis(300));
    let opens = env.fake.open_started();
    let later = env.item("later.txt").await.node;
    let second_started = Instant::now();

    assert!(matches!(
        materialise(&env.state, later).await,
        Err(ExternalError::Offline)
    ));
    assert!(second_started.elapsed() < Duration::from_millis(20));
    assert_eq!(env.fake.open_started(), opens);
    assert!(env.fake.maximum_active_opens() <= 4);
}

#[tokio::test]
async fn a_hung_read_chunk_times_out_and_marks_the_library_offline() {
    let env = Env::new(&[
        ("slow-read.txt", b"content", 1),
        ("later.txt", b"later!", 1),
    ])
    .await;
    env.state
        .shares()
        .set_operation_timeout(Duration::from_millis(30));
    env.fake
        .slow_read("slow-read.txt", Duration::from_millis(300));
    let node = env.item("slow-read.txt").await.node;
    let started = Instant::now();

    assert!(matches!(
        materialise(&env.state, node).await,
        Err(ExternalError::Offline)
    ));
    assert!(started.elapsed() < Duration::from_millis(200));
    let opens = env.fake.open_started();
    let later = env.item("later.txt").await.node;

    assert!(matches!(
        materialise(&env.state, later).await,
        Err(ExternalError::Offline)
    ));
    assert_eq!(env.fake.open_started(), opens);
}

#[tokio::test]
async fn a_withdrawn_root_refuses_even_cached_content() {
    let env = Env::new(&[("cached.txt", b"cached", 1)]).await;
    let ready = env.ready("cached.txt").await;
    drop(ready);
    env.db()
        .set_setting(env.admin.id, SHARE_ROOTS, json!([]))
        .await
        .unwrap();
    let node = env.item("cached.txt").await.node;

    assert!(matches!(
        materialise(&env.state, node).await,
        Err(ExternalError::NotApproved)
    ));
}

#[tokio::test]
async fn a_waiter_rechecks_root_approval_after_the_flight() {
    let env = Env::new(&[("held.txt", b"held", 1)]).await;
    env.fake.slow_open("held.txt", Duration::from_millis(500));
    let node = env.item("held.txt").await.node;
    let holder_state = env.state.clone();
    let holder = tokio::spawn(async move { materialise(&holder_state, node).await });
    wait_until(|| env.fake.open_started() == 1).await;

    let waiter_state = env.state.clone();
    let waiter = tokio::spawn(async move { materialise(&waiter_state, node).await });
    wait_until(|| env.state.shares().flight_references_for_tests(node) >= 5).await;
    env.db()
        .set_setting(env.admin.id, SHARE_ROOTS, json!([]))
        .await
        .unwrap();

    let held = holder.await.unwrap().unwrap().unwrap();
    drop(held);
    assert!(matches!(
        waiter.await.unwrap(),
        Err(ExternalError::NotApproved)
    ));
    assert_eq!(env.fake.opens("held.txt"), 1);
}

async fn assert_mid_stream_failure(
    kind: ErrorKind,
    expected: ExternalError,
    unavailable: Option<Unavailable>,
) {
    let env = Env::new(&[("fault.txt", b"fault bytes", 1)]).await;
    let node = env.item("fault.txt").await.node;
    env.fake.fail_during_read("fault.txt", kind);

    assert!(matches!(
        materialise(&env.state, node).await,
        Err(error) if error == expected
    ));
    assert_eq!(env.item("fault.txt").await.unavailable, unavailable);
    let stats = env.fake.stats();
    let opens = env.fake.opens("fault.txt");

    assert!(matches!(
        materialise(&env.state, node).await,
        Err(error) if error == expected
    ));
    assert_eq!(env.fake.stats(), stats);
    assert_eq!(env.fake.opens("fault.txt"), opens);
}

#[tokio::test]
async fn mid_stream_not_found_marks_the_item_gone_and_is_remembered() {
    assert_mid_stream_failure(
        ErrorKind::NotFound,
        ExternalError::Gone,
        Some(Unavailable::Gone),
    )
    .await;
}

#[tokio::test]
async fn mid_stream_permission_denied_marks_the_item_denied_and_is_remembered() {
    assert_mid_stream_failure(
        ErrorKind::PermissionDenied,
        ExternalError::Denied,
        Some(Unavailable::Denied),
    )
    .await;
}

#[tokio::test]
async fn mid_stream_connection_reset_marks_the_library_offline_and_is_remembered() {
    assert_mid_stream_failure(ErrorKind::ConnectionReset, ExternalError::Offline, None).await;
}

#[tokio::test]
async fn a_link_swapped_in_after_the_listing_is_refused() {
    let env = Env::new(&[("inside.txt", b"inside", 1)]).await;
    let node = env.item("inside.txt").await.node;
    env.fake.put(&native(r"C:\outside\secret.txt"), b"secret");
    env.fake
        .link("inside.txt", &native(r"C:\outside\secret.txt"));

    assert!(matches!(
        materialise(&env.state, node).await,
        Err(ExternalError::Gone)
    ));
    assert!(env.fake.opened_paths().is_empty());
}

#[tokio::test]
async fn a_file_over_half_the_cache_is_too_large() {
    let env = Env::new(&[("large.txt", b"123456", 1)]).await;
    env.state.shares().set_cache_max_bytes_for_tests(10);
    let node = env.item("large.txt").await.node;

    assert!(matches!(
        materialise(&env.state, node).await,
        Err(ExternalError::TooLarge)
    ));
    assert_eq!(env.fake.opens("large.txt"), 0);
}

#[tokio::test]
async fn more_than_four_reads_wait_and_then_answer_busy() {
    let files: Vec<(String, Vec<u8>, i64)> = (0..5)
        .map(|n| (format!("{n}.txt"), vec![b'0' + n], 1))
        .collect();
    let refs: Vec<(&str, &[u8], i64)> = files
        .iter()
        .map(|(path, bytes, at)| (path.as_str(), bytes.as_slice(), *at))
        .collect();
    let env = Env::new(&refs).await;
    env.state.shares().set_read_wait(Duration::from_millis(20));
    let mut nodes = Vec::new();
    for n in 0..5 {
        let path = format!("{n}.txt");
        env.fake.slow_open(&path, Duration::from_millis(500));
        nodes.push(env.item(&path).await.node);
    }

    let first_four: Vec<_> = nodes
        .drain(..4)
        .map(|node| {
            let state = env.state.clone();
            tokio::spawn(async move { materialise(&state, node).await })
        })
        .collect();
    wait_until(|| env.fake.open_started() == 4).await;
    let fifth = materialise(&env.state, nodes[0]).await;
    assert!(
        matches!(fifth, Err(ExternalError::Busy)),
        "fifth read answered {:?}",
        fifth.as_ref().err()
    );
    let mut answers = Vec::new();
    answers.push(fifth);
    for task in first_four {
        answers.push(task.await.unwrap());
    }

    assert_eq!(
        answers
            .iter()
            .filter(|answer| matches!(answer, Err(ExternalError::Busy)))
            .count(),
        1
    );
    assert_eq!(
        answers
            .iter()
            .filter(|answer| matches!(answer, Ok(Some(_))))
            .count(),
        4
    );
}

#[tokio::test]
async fn cancelling_requests_does_not_release_permits_before_open_finishes() {
    let files: Vec<(String, Vec<u8>, i64)> = (0..5)
        .map(|n| (format!("cancel-{n}.txt"), vec![b'0' + n], 1))
        .collect();
    let refs: Vec<(&str, &[u8], i64)> = files
        .iter()
        .map(|(path, bytes, at)| (path.as_str(), bytes.as_slice(), *at))
        .collect();
    let env = Env::new(&refs).await;
    env.state.shares().set_read_wait(Duration::from_millis(20));
    let mut tasks = Vec::new();
    for n in 0..4 {
        let path = format!("cancel-{n}.txt");
        env.fake.slow_open(&path, Duration::from_millis(400));
        let node = env.item(&path).await.node;
        let state = env.state.clone();
        tasks.push(tokio::spawn(async move { materialise(&state, node).await }));
    }
    wait_until(|| env.fake.open_started() == 4).await;
    for task in tasks {
        task.abort();
    }

    let fifth = env.item("cancel-4.txt").await.node;
    assert!(matches!(
        materialise(&env.state, fifth).await,
        Err(ExternalError::Busy)
    ));
}

#[tokio::test]
async fn with_no_scanner_content_is_served_unscanned_as_uploads_are() {
    let env = Env::new(&[("plain.txt", b"plain", 1)]).await;

    let ready = env.ready("plain.txt").await;

    assert_eq!(env.bytes(&ready), b"plain");
    assert!(
        env.db()
            .availability(&ready.blob.sha256)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn the_scan_gate_holds_infected_content_and_serves_clean_content() {
    let mut infected = b"report prefix ".to_vec();
    infected.extend_from_slice(&eicar());
    let clean = b"clean report".to_vec();
    let env = Env::new(&[
        ("infected.txt", infected.as_slice(), 1),
        ("clean.txt", clean.as_slice(), 1),
    ])
    .await;
    let fake = FakeIcap::start("127.0.0.1:0".parse().unwrap(), Behaviour::default()).unwrap();
    env.db()
        .set_setting(env.admin.id, SCANNER_URL, json!(fake.url()))
        .await
        .unwrap();

    let bad_node = env.item("infected.txt").await.node;
    let bad_state = env.state.clone();
    let bad = tokio::spawn(async move { materialise(&bad_state, bad_node).await });
    wait_scan_row(&env.db(), &infected).await;
    assert_eq!(
        server::scan_once(&env.state, OffsetDateTime::now_utc()).await,
        1
    );
    assert!(matches!(
        bad.await.unwrap(),
        Err(ExternalError::Held(ScanState::Infected))
    ));

    let clean_node = env.item("clean.txt").await.node;
    let clean_state = env.state.clone();
    let clean_read = tokio::spawn(async move { materialise(&clean_state, clean_node).await });
    wait_scan_row(&env.db(), &clean).await;
    assert_eq!(
        server::scan_once(&env.state, OffsetDateTime::now_utc()).await,
        1
    );
    let ready = clean_read.await.unwrap().unwrap().unwrap();
    assert_eq!(env.bytes(&ready), clean);
}

#[tokio::test]
async fn scan_refusals_still_keep_the_cache_within_its_bound() {
    let mut one = b"1 ".to_vec();
    one.extend_from_slice(&eicar());
    let mut two = b"22 ".to_vec();
    two.extend_from_slice(&eicar());
    let mut three = b"333 ".to_vec();
    three.extend_from_slice(&eicar());
    let env = Env::new(&[
        ("bad-one.txt", one.as_slice(), 1),
        ("bad-two.txt", two.as_slice(), 1),
        ("bad-three.txt", three.as_slice(), 1),
    ])
    .await;
    env.state.shares().set_cache_max_bytes_for_tests(150);
    let fake = FakeIcap::start("127.0.0.1:0".parse().unwrap(), Behaviour::default()).unwrap();
    env.db()
        .set_setting(env.admin.id, SCANNER_URL, json!(fake.url()))
        .await
        .unwrap();

    for (path, bytes) in [
        ("bad-one.txt", one.as_slice()),
        ("bad-two.txt", two.as_slice()),
        ("bad-three.txt", three.as_slice()),
    ] {
        let node = env.item(path).await.node;
        let state = env.state.clone();
        let read = tokio::spawn(async move { materialise(&state, node).await });
        wait_scan_row(&env.db(), bytes).await;
        assert_eq!(
            server::scan_once(&env.state, OffsetDateTime::now_utc()).await,
            1
        );
        assert!(matches!(
            read.await.unwrap(),
            Err(ExternalError::Held(ScanState::Infected))
        ));
    }

    assert!(env.db().cached_bytes().await.unwrap() <= 150);
}

#[tokio::test]
async fn the_cache_evicts_least_recently_used_and_keeps_what_is_being_read() {
    let env = Env::new(&[
        ("held.txt", b"aaa", 1),
        ("cold.txt", b"bbb", 1),
        ("new.txt", b"ccc", 1),
    ])
    .await;
    env.state.shares().set_cache_max_bytes_for_tests(6);
    let held = env.ready("held.txt").await;
    let cold = env.ready("cold.txt").await;
    let old = OffsetDateTime::UNIX_EPOCH;
    env.db().touch_cached(&held.blob.sha256, old).await.unwrap();
    env.db()
        .touch_cached(&cold.blob.sha256, old + time::Duration::seconds(1))
        .await
        .unwrap();
    drop(cold);

    let newest = env.ready("new.txt").await;

    assert!(env.server.blobs().contains(&held.blob.sha256));
    assert!(!env.server.blobs().contains(&BlobRef::of(b"bbb").sha256));
    assert!(env.server.blobs().contains(&newest.blob.sha256));
}

#[tokio::test]
async fn evicted_content_is_read_again_on_the_next_open() {
    let env = Env::new(&[("again.txt", b"again", 1)]).await;
    let ready = env.ready("again.txt").await;
    drop(ready);
    env.state.shares().set_cache_max_bytes_for_tests(0);
    assert_eq!(evict(&env.state).await, 1);
    env.state.shares().set_cache_max_bytes_for_tests(10);

    let again = env.ready("again.txt").await;

    assert_eq!(env.bytes(&again), b"again");
    assert_eq!(env.fake.opens("again.txt"), 2);
}

#[tokio::test]
async fn content_also_named_by_a_stored_version_is_not_erased_by_eviction() {
    let env = Env::new(&[("kept.txt", b"kept", 1)]).await;
    let ready = env.ready("kept.txt").await;
    let owner = env.server.user("Stored version owner").await;
    let doc = DocId(Uuid::new_v4());
    env.server
        .create_document(doc, "ordinary.txt", records::Editor::Text, &owner)
        .await;
    env.db()
        .record_blob_ref(doc, ready.blob, "stored-version")
        .await
        .unwrap();
    drop(ready);
    env.state.shares().set_cache_max_bytes_for_tests(0);

    assert_eq!(evict(&env.state).await, 1);
    assert!(env.server.blobs().contains(&BlobRef::of(b"kept").sha256));
}

#[tokio::test]
async fn a_read_that_finishes_after_the_listing_moved_the_tag_retries_current_content() {
    let env = Env::new(&[("race.txt", b"first", 1)]).await;
    let item = env.item("race.txt").await;
    env.fake.slow_read("race.txt", Duration::from_millis(150));
    let state = env.state.clone();
    let read = tokio::spawn(async move { materialise(&state, item.node).await });
    wait_until(|| env.fake.read_started() == 1).await;
    env.fake.slow_stat("race.txt", Duration::from_millis(150));
    wait_until(|| env.fake.stat_delayed() == 1).await;
    env.fake.put_at("race.txt", b"second", 2);
    env.db()
        .set_external_stat(item.node, &item.tag, 6, 2)
        .await
        .unwrap();

    let ready = read.await.unwrap().unwrap().unwrap();
    let current = env.item("race.txt").await;

    assert_eq!(env.bytes(&ready), b"second");
    assert_eq!(ready.revision, current.revision);
    assert_ne!(ready.revision, item.revision);
    assert_eq!(current.sha256, Some(sha(b"second")));
}

#[tokio::test]
async fn site_usage_is_unchanged_by_reads() {
    let env = Env::new(&[("quota.txt", b"not quota", 1)]).await;
    let site = env.db().node(env.library).await.unwrap().unwrap().site;
    let before = env.db().site_usage(site).await.unwrap();

    let _ready = env.ready("quota.txt").await;

    assert_eq!(env.db().site_usage(site).await.unwrap(), before);
}

#[tokio::test]
async fn materialise_is_none_for_an_ordinary_file_and_for_a_github_file() {
    let env = Env::new(&[]).await;
    let owner = env.server.user("Ordinary owner").await;
    let ordinary = DocId(Uuid::new_v4());
    env.server
        .create_document(ordinary, "ordinary.txt", records::Editor::Text, &owner)
        .await;

    assert!(
        materialise(&env.state, NodeId(ordinary.0))
            .await
            .unwrap()
            .is_none()
    );
    let github = start_fake_github("octo", "papers").await;
    github.commit("main", &[("github.txt", b"mirrored".to_vec())]);
    let server = spawn_server_config(|config| config.github_api = github.origin()).await;
    let admin = server.admin("GitHub content admin").await;
    let library = common::Site::library(&server).await;
    let state = server.app_state().await;
    server
        .records_db()
        .connect_source(&NewSource {
            library,
            kind: SourceKind::Github,
            owner: "octo".to_owned(),
            repo: "papers".to_owned(),
            branch: "main".to_owned(),
            folder: String::new(),
            location: None,
            mode: SourceMode::ReadOnly,
            token: None,
            poll_seconds: 60,
            connected_by: admin.id,
        })
        .await
        .unwrap();
    sync_once(&state, library, OffsetDateTime::UNIX_EPOCH)
        .await
        .unwrap()
        .expect("GitHub file was mirrored");
    let github_node = server
        .records_db()
        .source_items(library)
        .await
        .unwrap()
        .into_iter()
        .find(|item| item.repository_path == "github.txt")
        .unwrap()
        .node;

    assert!(materialise(&state, github_node).await.unwrap().is_none());
}
