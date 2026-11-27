//! S3 objects are read on first use: `HEAD`, then one `GET` with `If-Match` on the ETag just
//! seen, streamed into the content store, scanned and cached. An archived object is never asked
//! for; an object that changes under the read is never cached.

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

use std::sync::Arc;
use std::time::Duration;

use collab_core::ids::NodeId;
use common::fake_s3::{FakeS3, Opts};
use common::fake_share::FakeShare;
use common::{TIMEOUT, TestServer, TestUser, spawn_server_config_without_workers};
use fake_icap::{Behaviour, FakeIcap, eicar};
use records::{
    Budget, NewSource, S3_ENDPOINTS, S3Place, SCANNER_URL, SHARE_ROOTS, ScanState, SourceKind,
    SourceMode, Tri, Unavailable, Versioning, object_tag, s3_location,
};
use revisions::{BlobRef, Content};
use serde_json::{Value, json};
use server::AppState;
use server::external::{ExternalError, Ready, materialise};
use server::s3::S3_READS;
use server::source_sync::{sync_once, token_context};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

const BUCKET: &str = "content-bucket-3f9";
const ENDPOINT: &str = "content-endpoint-8a1";
const KEY_ID: &str = "AKIDCONTENTEXAMPLE41";
const SECRET: &str = "planted-content-secret-2b7c";

#[cfg(windows)]
const SHARE_ROOT: &str = r"C:\approved";
#[cfg(not(windows))]
const SHARE_ROOT: &str = "/approved";

struct Env {
    server: TestServer,
    fake: FakeS3,
    admin: TestUser,
    library: NodeId,
    state: AppState,
}

fn endpoint_at(origin: &str) -> Value {
    json!([{
        "name": ENDPOINT,
        "url": origin,
        "region": "us-east-1",
        "path_style": true,
        "allow": ["127.0.0.0/8"],
    }])
}

async fn set_endpoints(server: &TestServer, value: Value) {
    sqlx::query(
        "insert into settings (key, value, changed_at) values ($1, $2, now()) \
         on conflict (key) do update set value = excluded.value",
    )
    .bind(S3_ENDPOINTS)
    .bind(value)
    .execute(server.records_db().test_pool())
    .await
    .unwrap();
}

impl Env {
    /// A server with an S3 library on the fake holding `objects`, listed once.
    async fn new(objects: &[(&str, &[u8])]) -> Env {
        let fake = FakeS3::start().await;
        fake.bucket(BUCKET);
        fake.require_keys(KEY_ID, SECRET);
        for (key, bytes) in objects {
            fake.put(BUCKET, key, bytes);
        }
        Self::with_fake(fake).await
    }

    async fn with_fake(fake: FakeS3) -> Env {
        let server = spawn_server_config_without_workers(|config| {
            config.max_file_bytes = 512 * 1024 * 1024;
        })
        .await;
        let admin = server.admin("Ada Admin").await;
        let library = common::Site::library(&server).await;
        let state = server.app_state().await;
        state
            .s3()
            .set_retry_wait_for_tests(Duration::from_millis(5));
        set_endpoints(&server, endpoint_at(&fake.origin())).await;
        let db = server.records_db();
        let token = state
            .secret_store()
            .put(&db, SECRET.as_bytes(), &token_context(library))
            .await
            .unwrap();
        let place = S3Place {
            endpoint: ENDPOINT.to_owned(),
            bucket: BUCKET.to_owned(),
            prefix: String::new(),
            access_key_id: KEY_ID.to_owned(),
            versioning: Versioning::Unknown,
            conditional: Tri::Unknown,
        };
        db.connect_s3_source(
            &NewSource {
                library,
                kind: SourceKind::S3,
                owner: String::new(),
                repo: String::new(),
                branch: String::new(),
                folder: String::new(),
                location: Some(s3_location(ENDPOINT, BUCKET, "")),
                mode: SourceMode::ReadOnly,
                token: Some(token),
                poll_seconds: 60,
                connected_by: admin.id,
            },
            &place,
        )
        .await
        .unwrap();
        let _ = sync_once(&state, library, OffsetDateTime::UNIX_EPOCH)
            .await
            .unwrap();
        Env {
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

    async fn item_in(&self, library: NodeId, path: &str) -> records::ExternalItem {
        let db = self.db();
        let source = db
            .source_items(library)
            .await
            .unwrap()
            .into_iter()
            .find(|item| item.repository_path == path)
            .unwrap_or_else(|| panic!("no source item at {path}"));
        db.external_item(source.node).await.unwrap().unwrap()
    }

    async fn item(&self, path: &str) -> records::ExternalItem {
        self.item_in(self.library, path).await
    }

    async fn node(&self, path: &str) -> NodeId {
        self.item(path).await.node
    }

    async fn read(&self, path: &str) -> Result<Ready, ExternalError> {
        let node = self.node(path).await;
        materialise(&self.state, node)
            .await
            .map(|ready| ready.expect("external content"))
    }

    async fn ready(&self, path: &str) -> Ready {
        match self.read(path).await {
            Ok(ready) => ready,
            Err(error) => panic!("reading {path} answered {error:?}"),
        }
    }

    fn bytes(&self, ready: &Ready) -> Vec<u8> {
        self.server
            .blobs()
            .read_all(&Content::Blob(ready.blob))
            .unwrap()
    }

    async fn cached_bytes(&self) -> u64 {
        self.db().cached_bytes().await.unwrap()
    }

    /// A file-share library on the same server holding `files`, listed once.
    async fn share_library(&self, files: &[(&str, &[u8])]) -> (NodeId, FakeShare) {
        let site = self.db().node(self.library).await.unwrap().unwrap().site;
        let response = self
            .server
            .http(Some(&self.admin))
            .post(self.server.url(&format!("/api/nodes/{}/children", site.0)))
            .json(&json!({ "kind": "library", "name": "Share files" }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::CREATED);
        let created: Value = response.json().await.unwrap();
        let library = NodeId(created["id"].as_str().unwrap().parse().unwrap());
        let fake = FakeShare::new(SHARE_ROOT);
        for (path, bytes) in files {
            fake.put_at(path, bytes, 1);
        }
        self.state.shares().set_fs(Arc::new(fake.clone()));
        let db = self.db();
        db.set_setting(self.admin.id, SHARE_ROOTS, json!([SHARE_ROOT]))
            .await
            .unwrap();
        db.connect_source(&NewSource {
            library,
            kind: SourceKind::Share,
            owner: String::new(),
            repo: String::new(),
            branch: String::new(),
            folder: String::new(),
            location: Some(SHARE_ROOT.to_owned()),
            mode: SourceMode::ReadOnly,
            token: None,
            poll_seconds: 60,
            connected_by: self.admin.id,
        })
        .await
        .unwrap();
        sync_once(&self.state, library, OffsetDateTime::UNIX_EPOCH)
            .await
            .unwrap()
            .expect("the share listing changed");
        (library, fake)
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
        tokio::time::sleep(Duration::from_millis(2)).await;
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

/// An origin on 127.0.0.1 where nothing listens.
async fn dead_origin() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{addr}")
}

#[tokio::test]
async fn first_read_stores_hashes_pins_and_records_the_hash() {
    let env = Env::new(&[("report.txt", b"quarterly")]).await;
    let before = env.item("report.txt").await;

    let ready = env.ready("report.txt").await;

    assert_eq!(env.bytes(&ready), b"quarterly");
    assert_eq!(ready.blob, BlobRef::of(b"quarterly"));
    assert_eq!(ready.revision, before.revision);
    assert_eq!(env.fake.count("head"), 1);
    assert_eq!(env.fake.count("get"), 1);
    assert_eq!(env.item("report.txt").await.sha256, Some(sha(b"quarterly")));
    assert_eq!(env.cached_bytes().await, 9);
}

#[tokio::test]
async fn a_key_with_a_space_and_a_plus_lists_with_its_exact_name_and_opens() {
    let env = Env::new(&[("dir/a b+c.txt", b"plus")]).await;
    assert_eq!(
        env.item("dir/a b+c.txt").await.repository_path,
        "dir/a b+c.txt"
    );

    let ready = env.ready("dir/a b+c.txt").await;

    assert_eq!(env.bytes(&ready), b"plus");
    assert_eq!(env.fake.count("get"), 1);
}

#[tokio::test]
async fn a_second_read_heads_and_does_not_get() {
    let env = Env::new(&[("report.txt", b"quarterly")]).await;
    drop(env.ready("report.txt").await);

    let second = env.ready("report.txt").await;

    assert_eq!(env.bytes(&second), b"quarterly");
    assert_eq!(env.fake.count("head"), 2);
    assert_eq!(env.fake.count("get"), 1);
}

#[tokio::test]
async fn two_first_reads_at_once_get_the_object_once() {
    let env = Env::new(&[("report.txt", b"quarterly")]).await;
    env.fake.slow_get("report.txt", Duration::from_millis(100));
    let node = env.node("report.txt").await;

    let (left, right) = tokio::join!(materialise(&env.state, node), materialise(&env.state, node));

    assert_eq!(left.unwrap().unwrap().blob, right.unwrap().unwrap().blob);
    assert_eq!(env.fake.count("get"), 1);
}

#[tokio::test]
async fn an_object_changed_since_the_listing_serves_the_new_bytes_and_wakes_a_listing() {
    let env = Env::new(&[("report.txt", b"old")]).await;
    let before = env.item("report.txt").await;
    env.fake.put(BUCKET, "report.txt", b"new bytes");

    let ready = env.ready("report.txt").await;

    let after = env.item("report.txt").await;
    assert_eq!(env.bytes(&ready), b"new bytes");
    assert_ne!(after.revision, before.revision);
    assert_eq!(after.size, 9);
    assert_eq!(ready.revision, after.revision);
    assert_eq!(
        server::source_tick_once(&env.state, OffsetDateTime::UNIX_EPOCH).await,
        1
    );
}

#[tokio::test]
async fn an_object_replaced_between_head_and_get_is_read_again_and_twice_is_changing() {
    let env = Env::new(&[("once.txt", b"one"), ("twice.txt", b"one")]).await;
    env.fake.change_before_get("once.txt", b"second");

    let ready = env.ready("once.txt").await;
    assert_eq!(env.bytes(&ready), b"second");
    let once = env.item("once.txt").await;
    assert_eq!(once.sha256, Some(sha(b"second")));
    assert_eq!(once.size, 6);
    drop(ready);

    env.fake.change_before_get("twice.txt", b"two");
    env.fake.change_before_get("twice.txt", b"three");
    let cached = env.cached_bytes().await;
    assert!(matches!(
        env.read("twice.txt").await,
        Err(ExternalError::Changing)
    ));
    assert_eq!(env.cached_bytes().await, cached);
    assert!(env.item("twice.txt").await.sha256.is_none());
}

#[tokio::test]
async fn a_vanished_object_is_gone_and_marked() {
    let env = Env::new(&[("gone.txt", b"there")]).await;
    env.fake.remove(BUCKET, "gone.txt");

    assert!(matches!(
        env.read("gone.txt").await,
        Err(ExternalError::Gone)
    ));
    assert_eq!(
        env.item("gone.txt").await.unavailable,
        Some(Unavailable::Gone)
    );
}

#[tokio::test]
async fn a_denied_object_is_denied_and_marked() {
    let env = Env::new(&[("denied.txt", b"there")]).await;
    env.fake.deny(BUCKET, "denied.txt");

    assert!(matches!(
        env.read("denied.txt").await,
        Err(ExternalError::Denied)
    ));
    assert_eq!(
        env.item("denied.txt").await.unavailable,
        Some(Unavailable::Denied)
    );
}

#[tokio::test]
async fn a_short_body_is_not_cached() {
    let env = Env::new(&[("short.txt", b"the whole of it")]).await;
    env.fake.truncate_get("short.txt", 4);

    assert!(matches!(
        env.read("short.txt").await,
        Err(ExternalError::Changing)
    ));
    assert_eq!(env.cached_bytes().await, 0);
    assert!(env.item("short.txt").await.sha256.is_none());
    assert!(!env.server.blobs().contains(&sha(b"the ")));
}

#[tokio::test]
async fn a_hung_body_times_out_marks_offline_and_is_not_cached() {
    let env = Env::new(&[("hung.txt", b"half and half"), ("later.txt", b"later")]).await;
    env.state
        .s3()
        .set_timeouts_for_tests(Duration::from_secs(2), Duration::from_millis(200));
    env.fake.hang_get("hung.txt");

    assert!(matches!(
        env.read("hung.txt").await,
        Err(ExternalError::Offline)
    ));
    assert_eq!(env.cached_bytes().await, 0);
    assert!(env.item("hung.txt").await.sha256.is_none());
    assert_eq!(env.state.s3().read_permits_for_tests(), S3_READS);
    assert_eq!(env.state.shares().flight_count_for_tests(), 0);
    let requests = env.fake.requests().len();
    assert!(matches!(
        env.read("later.txt").await,
        Err(ExternalError::Offline)
    ));
    assert_eq!(env.fake.requests().len(), requests);
}

#[tokio::test]
async fn an_unreachable_store_serves_what_is_cached_and_refuses_the_rest() {
    let env = Env::new(&[("cached.txt", b"cached"), ("cold.txt", b"cold")]).await;
    drop(env.ready("cached.txt").await);
    set_endpoints(&env.server, endpoint_at(&dead_origin().await)).await;

    assert!(matches!(
        env.read("cold.txt").await,
        Err(ExternalError::Offline)
    ));
    assert_eq!(env.bytes(&env.ready("cached.txt").await), b"cached");
}

#[tokio::test]
async fn offline_is_remembered_so_later_reads_send_nothing() {
    let env = Env::new(&[("one.txt", b"one"), ("two.txt", b"two")]).await;
    env.fake.throttle_next(100);

    assert!(matches!(
        env.read("one.txt").await,
        Err(ExternalError::Offline)
    ));
    let requests = env.fake.requests().len();
    assert!(matches!(
        env.read("two.txt").await,
        Err(ExternalError::Offline)
    ));
    assert_eq!(env.fake.requests().len(), requests);
}

#[tokio::test]
async fn an_archived_object_is_never_requested() {
    let fake = FakeS3::start().await;
    fake.bucket(BUCKET);
    fake.require_keys(KEY_ID, SECRET);
    fake.put_with(
        BUCKET,
        "cold.bin",
        b"frozen",
        Opts {
            class: Some("GLACIER".to_owned()),
            ..Opts::default()
        },
    );
    let env = Env::with_fake(fake).await;

    assert!(matches!(
        env.read("cold.bin").await,
        Err(ExternalError::Archived)
    ));
    assert!(matches!(
        env.read("cold.bin").await,
        Err(ExternalError::Archived)
    ));
    assert_eq!(env.fake.count("get"), 0);
    let item = env.item("cold.bin").await;
    assert_eq!(item.unavailable, Some(Unavailable::Archived));
    assert!(item.tag.ends_with("!a"));
}

#[tokio::test]
async fn a_restored_archived_object_becomes_readable() {
    let fake = FakeS3::start().await;
    fake.bucket(BUCKET);
    fake.require_keys(KEY_ID, SECRET);
    let glacier = |restored| Opts {
        class: Some("GLACIER".to_owned()),
        restored,
        ..Opts::default()
    };
    fake.put_with(BUCKET, "cold.bin", b"frozen", glacier(None));
    let env = Env::with_fake(fake).await;
    assert!(matches!(
        env.read("cold.bin").await,
        Err(ExternalError::Archived)
    ));

    env.fake
        .put_with(BUCKET, "cold.bin", b"frozen", glacier(Some(true)));
    let ready = env.ready("cold.bin").await;

    assert_eq!(env.bytes(&ready), b"frozen");
    let item = env.item("cold.bin").await;
    assert_eq!(item.unavailable, None);
    assert!(!item.tag.ends_with("!a"));
    assert_eq!(env.fake.count("get"), 1);
}

#[tokio::test]
async fn an_endpoint_no_longer_approved_refuses_even_cached_content() {
    let env = Env::new(&[("cached.txt", b"cached")]).await;
    drop(env.ready("cached.txt").await);
    set_endpoints(&env.server, json!([])).await;
    let requests = env.fake.requests().len();

    assert!(matches!(
        env.read("cached.txt").await,
        Err(ExternalError::NotApproved)
    ));
    assert_eq!(env.fake.requests().len(), requests);
}

#[tokio::test]
async fn approval_withdrawn_while_waiting_for_a_permit_is_refused() {
    let objects: Vec<(String, Vec<u8>)> = (0..5)
        .map(|n| (format!("{n}.txt"), vec![b'0' + n]))
        .collect();
    let refs: Vec<(&str, &[u8])> = objects
        .iter()
        .map(|(key, bytes)| (key.as_str(), bytes.as_slice()))
        .collect();
    let env = Env::new(&refs).await;
    env.state.s3().set_read_wait(Duration::from_secs(5));
    let mut tasks = Vec::new();
    for n in 0..4 {
        let key = format!("{n}.txt");
        env.fake.slow_get(&key, Duration::from_millis(400));
        let node = env.node(&key).await;
        let state = env.state.clone();
        tasks.push(tokio::spawn(async move { materialise(&state, node).await }));
    }
    wait_until(|| env.fake.count("get") == 4).await;
    let fifth = env.node("4.txt").await;
    let state = env.state.clone();
    let waiting = tokio::spawn(async move { materialise(&state, fifth).await });
    wait_until(|| env.fake.count("head") == 5).await;
    set_endpoints(&env.server, json!([])).await;

    assert!(matches!(
        waiting.await.unwrap(),
        Err(ExternalError::NotApproved)
    ));
    assert_eq!(env.fake.count("get"), 4);
    for task in tasks {
        let _ = task.await.unwrap();
    }
}

#[tokio::test]
async fn an_object_over_half_the_cache_is_too_large_and_not_requested() {
    let env = Env::new(&[("large.txt", b"123456")]).await;
    env.state.shares().set_cache_max_bytes_for_tests(10);

    assert!(matches!(
        env.read("large.txt").await,
        Err(ExternalError::TooLarge)
    ));
    assert_eq!(env.fake.count("get"), 0);
}

#[tokio::test]
async fn more_than_four_reads_wait_and_then_answer_busy() {
    let objects: Vec<(String, Vec<u8>)> = (0..5)
        .map(|n| (format!("{n}.txt"), vec![b'0' + n]))
        .collect();
    let refs: Vec<(&str, &[u8])> = objects
        .iter()
        .map(|(key, bytes)| (key.as_str(), bytes.as_slice()))
        .collect();
    let env = Env::new(&refs).await;
    env.state.s3().set_read_wait(Duration::from_millis(20));
    let mut nodes = Vec::new();
    for n in 0..5 {
        let key = format!("{n}.txt");
        env.fake.slow_get(&key, Duration::from_millis(500));
        nodes.push(env.node(&key).await);
    }
    let first_four: Vec<_> = nodes
        .drain(..4)
        .map(|node| {
            let state = env.state.clone();
            tokio::spawn(async move { materialise(&state, node).await })
        })
        .collect();
    wait_until(|| env.fake.count("get") == 4).await;

    let fifth = materialise(&env.state, nodes[0]).await;
    assert!(
        matches!(fifth, Err(ExternalError::Busy)),
        "fifth read answered {:?}",
        fifth.as_ref().err()
    );
    for task in first_four {
        assert!(task.await.unwrap().unwrap().is_some());
    }
    assert_eq!(env.fake.count("get"), 4);
}

#[tokio::test]
async fn share_reads_and_s3_reads_do_not_share_permits() {
    let objects: Vec<(String, Vec<u8>)> = (0..5)
        .map(|n| (format!("s3-{n}.txt"), vec![b'a' + n]))
        .collect();
    let refs: Vec<(&str, &[u8])> = objects
        .iter()
        .map(|(key, bytes)| (key.as_str(), bytes.as_slice()))
        .collect();
    let env = Env::new(&refs).await;
    let files: Vec<(String, Vec<u8>)> = (0..5)
        .map(|n| (format!("share-{n}.txt"), vec![b'k' + n]))
        .collect();
    let file_refs: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(path, bytes)| (path.as_str(), bytes.as_slice()))
        .collect();
    let (share, share_fake) = env.share_library(&file_refs).await;
    env.state.shares().set_read_wait(Duration::from_millis(20));
    env.state.s3().set_read_wait(Duration::from_millis(20));

    // Every file-share permit taken: S3 still reads.
    let mut tasks = Vec::new();
    for n in 0..4 {
        let path = format!("share-{n}.txt");
        share_fake.slow_open(&path, Duration::from_millis(400));
        let node = env.item_in(share, &path).await.node;
        let state = env.state.clone();
        tasks.push(tokio::spawn(async move { materialise(&state, node).await }));
    }
    wait_until(|| share_fake.open_started() == 4).await;
    assert_eq!(env.bytes(&env.ready("s3-4.txt").await), b"e");
    for task in tasks.drain(..) {
        assert!(task.await.unwrap().unwrap().is_some());
    }

    // Every S3 permit taken: the file share still reads.
    for n in 0..4 {
        let key = format!("s3-{n}.txt");
        env.fake.slow_get(&key, Duration::from_millis(400));
        let node = env.node(&key).await;
        let state = env.state.clone();
        tasks.push(tokio::spawn(async move { materialise(&state, node).await }));
    }
    wait_until(|| env.fake.count("get") == 5).await;
    let node = env.item_in(share, "share-4.txt").await.node;
    let ready = materialise(&env.state, node).await.unwrap().unwrap();
    assert_eq!(env.bytes(&ready), b"o");
    for task in tasks {
        assert!(task.await.unwrap().unwrap().is_some());
    }
}

#[tokio::test]
async fn the_scan_gate_holds_infected_content_and_serves_clean_content() {
    let mut infected = b"report prefix ".to_vec();
    infected.extend_from_slice(&eicar());
    let clean = b"clean report".to_vec();
    let env = Env::new(&[
        ("infected.txt", infected.as_slice()),
        ("clean.txt", clean.as_slice()),
    ])
    .await;
    let scanner = FakeIcap::start("127.0.0.1:0".parse().unwrap(), Behaviour::default()).unwrap();
    env.db()
        .set_setting(env.admin.id, SCANNER_URL, json!(scanner.url()))
        .await
        .unwrap();

    let bad_node = env.node("infected.txt").await;
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

    let clean_node = env.node("clean.txt").await;
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
async fn the_cache_evicts_least_recently_used_across_shares_and_s3() {
    let env = Env::new(&[("cold.txt", b"bbb"), ("new.txt", b"ccc")]).await;
    let (share, _share_fake) = env.share_library(&[("held.txt", b"aaa")]).await;
    env.state.shares().set_cache_max_bytes_for_tests(6);
    let held_node = env.item_in(share, "held.txt").await.node;
    let held = materialise(&env.state, held_node).await.unwrap().unwrap();
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
async fn a_read_that_finishes_after_the_listing_moved_the_tag_records_nothing_and_serves_no_stale_bytes()
 {
    let env = Env::new(&[("race.txt", b"first")]).await;
    let item = env.item("race.txt").await;
    env.fake.slow_get("race.txt", Duration::from_millis(200));
    let state = env.state.clone();
    let read = tokio::spawn(async move { materialise(&state, item.node).await });
    wait_until(|| env.fake.count("get") == 1).await;
    // The object changes and a listing records it while the first answer is on its way.
    env.fake
        .put_at(BUCKET, "race.txt", b"second", 1_800_000_000);
    let new_tag = object_tag(6, 1_800_000_000, &etag_of_fake(b"second"), false);
    assert!(
        env.db()
            .set_external_seen(item.node, &item.tag, &new_tag, 6)
            .await
            .unwrap()
    );

    let ready = read.await.unwrap().unwrap().unwrap();
    let current = env.item("race.txt").await;

    assert_eq!(env.bytes(&ready), b"second");
    assert_eq!(current.sha256, Some(sha(b"second")));
    assert_eq!(ready.revision, current.revision);
    assert_ne!(ready.revision, item.revision);
}

/// The fake's default ETag of `bytes`: the first 32 hex digits of their SHA-256, quoted.
fn etag_of_fake(bytes: &[u8]) -> String {
    let hex: String = sha(bytes).iter().map(|b| format!("{b:02x}")).collect();
    format!("\"{}\"", &hex[..32])
}

#[tokio::test]
async fn a_multipart_style_etag_is_not_taken_for_a_hash() {
    let fake = FakeS3::start().await;
    fake.bucket(BUCKET);
    fake.require_keys(KEY_ID, SECRET);
    fake.put_with(
        BUCKET,
        "parts.bin",
        b"assembled from parts",
        Opts {
            etag: Some("\"abc-3\"".to_owned()),
            ..Opts::default()
        },
    );
    let env = Env::with_fake(fake).await;

    let ready = env.ready("parts.bin").await;

    assert_eq!(ready.blob.sha256, sha(b"assembled from parts"));
    assert_eq!(
        env.item("parts.bin").await.sha256,
        Some(sha(b"assembled from parts"))
    );
}

#[tokio::test]
async fn over_the_read_budget_cached_files_open_and_others_are_refused() {
    let env = Env::new(&[("cached.txt", b"cached"), ("cold.txt", b"cold")]).await;
    env.db()
        .set_budget(
            env.library,
            &Budget {
                list_requests: 1_000,
                read_requests: 2,
                bytes_down: None,
            },
        )
        .await
        .unwrap();
    drop(env.ready("cached.txt").await);
    let requests = env.fake.requests().len();

    assert_eq!(env.bytes(&env.ready("cached.txt").await), b"cached");
    assert!(matches!(
        env.read("cold.txt").await,
        Err(ExternalError::OverBudget)
    ));
    assert_eq!(env.fake.requests().len(), requests);
}

#[tokio::test]
async fn reads_are_counted_in_the_meter_with_their_bytes() {
    let env = Env::new(&[("counted.txt", b"twelve bytes")]).await;
    let _ = env.state.s3().take_usage();

    drop(env.ready("counted.txt").await);
    drop(env.ready("counted.txt").await);

    let usage = env.state.s3().take_usage();
    let (_, counted) = usage
        .iter()
        .find(|(library, _)| *library == env.library)
        .expect("the library's usage");
    assert_eq!(counted.read, 3);
    assert_eq!(counted.bytes_down, 12);
    assert_eq!(counted.cache_hits, 1);
    assert_eq!(counted.list, 0);
    assert_eq!(env.fake.count("head") + env.fake.count("get"), 3);
}

#[tokio::test]
async fn site_usage_is_unchanged_by_reads() {
    let env = Env::new(&[("quota.txt", &[b'q'; 50_000])]).await;
    let site = env.db().node(env.library).await.unwrap().unwrap().site;
    let before = env.db().site_usage(site).await.unwrap();

    let _ready = env.ready("quota.txt").await;

    assert_eq!(env.db().site_usage(site).await.unwrap(), before);
}

#[tokio::test]
async fn a_read_sends_no_write() {
    let env = Env::new(&[("one.txt", b"one"), ("two.txt", b"two")]).await;
    env.fake.change_before_get("two.txt", b"changed");
    drop(env.ready("one.txt").await);
    drop(env.ready("one.txt").await);
    drop(env.ready("two.txt").await);

    assert_eq!(env.fake.count("write"), 0);
    assert!(
        env.fake
            .requests()
            .iter()
            .all(|request| request.method == "GET" || request.method == "HEAD")
    );
}
