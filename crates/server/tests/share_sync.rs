//! File-share listings become external library nodes without reading or storing their bytes.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use collab_core::ids::{DocId, NodeId};
use common::fake_github::FakeGithub;
use common::fake_share::FakeShare;
use common::{TestServer, TestUser, spawn_server_config_without_workers, start_fake_github};
use records::{
    NewSource, Principal, SHARE_CACHE_MAX_BYTES, SHARE_ROOTS, SOURCE_MAX_FILES, SourceKind,
    SourceMode, SourceStatus,
};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use server::AppState;
use server::share::{FsBudget, FsEntry, FsStat, ShareFs};
use server::source_sync::{SyncError, SyncReport, sync_once};
use sqlx::Connection;
use time::OffsetDateTime;

const ROOT: &str = r"C:\approved";

struct Env {
    server: TestServer,
    fake: FakeShare,
    admin: TestUser,
    library: NodeId,
    state: AppState,
}

impl Env {
    async fn new() -> Self {
        let server = spawn_server_config_without_workers(|config| {
            config.max_file_bytes = 512 * 1024 * 1024;
        })
        .await;
        let admin = server.admin("Ada Admin").await;
        let library = common::Site::library(&server).await;
        let state = server.app_state().await;
        let fake = FakeShare::new(ROOT);
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
        Self {
            server,
            fake,
            admin,
            library,
            state,
        }
    }

    async fn sync_at(&self, now: OffsetDateTime) -> Result<Option<SyncReport>, SyncError> {
        sync_once(&self.state, self.library, now).await
    }

    async fn synced_at(&self, now: OffsetDateTime) -> SyncReport {
        self.sync_at(now)
            .await
            .unwrap()
            .expect("the listing changed")
    }

    async fn status(&self) -> SourceStatus {
        self.server
            .records_db()
            .source_of(self.library)
            .await
            .unwrap()
            .unwrap()
            .1
    }

    async fn item(&self, path: &str) -> records::SourceItem {
        self.server
            .records_db()
            .source_items(self.library)
            .await
            .unwrap()
            .into_iter()
            .find(|item| item.path == path)
            .unwrap_or_else(|| panic!("no source item at {path}"))
    }

    async fn in_bin(&self, node: NodeId) -> bool {
        self.server
            .records_db()
            .node(node)
            .await
            .unwrap()
            .is_some_and(|node| node.deleted)
    }
}

async fn call(
    server: &TestServer,
    user: &TestUser,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = server.http(Some(user)).request(method, server.url(path));
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.unwrap();
    let status = response.status();
    let text = response.text().await.unwrap();
    let body = serde_json::from_str(&text).unwrap_or(Value::String(text));
    (status, body)
}

fn tree_head(server: &TestServer) -> gix::ObjectId {
    gix::open(server.library())
        .unwrap()
        .find_reference("refs/sharecraft/tree")
        .unwrap()
        .id()
        .detach()
}

#[tokio::test]
async fn first_listing_creates_folders_and_files_without_revisions() {
    let env = Env::new().await;
    env.fake.mkdir("docs");
    env.fake.put_at("docs/report.txt", b"not opened", 42);

    let report = env.synced_at(OffsetDateTime::UNIX_EPOCH).await;

    assert_eq!(report.added, 1);
    let folder = env.item("docs").await;
    let file = env.item("docs/report.txt").await;
    assert!(folder.blob_sha.is_none());
    assert_eq!(file.blob_sha.as_deref(), Some("x10-42"));
    let document = env
        .server
        .records_db()
        .document(DocId(file.node.0))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(document.revision, None);
    assert!(env.fake.opened_paths().is_empty());
}

#[tokio::test]
async fn a_second_listing_with_no_change_records_no_event_and_makes_no_git_commit() {
    let env = Env::new().await;
    env.fake.put("same.txt", b"same");
    env.synced_at(OffsetDateTime::UNIX_EPOCH).await;
    let before_head = tree_head(&env.server);
    let before_events = env
        .server
        .records_db()
        .events_of_kind("source.synced")
        .await
        .unwrap()
        .len();

    assert_eq!(
        env.sync_at(OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(60))
            .await,
        Ok(None)
    );
    assert_eq!(tree_head(&env.server), before_head);
    assert_eq!(
        env.server
            .records_db()
            .events_of_kind("source.synced")
            .await
            .unwrap()
            .len(),
        before_events
    );
}

#[tokio::test]
async fn a_changed_file_keeps_its_node_and_gets_a_new_tag_and_loses_its_hash() {
    let env = Env::new().await;
    env.fake.put_at("draft.txt", b"one", 1);
    env.synced_at(OffsetDateTime::UNIX_EPOCH).await;
    let before = env.item("draft.txt").await;
    env.server
        .records_db()
        .set_external_content(
            before.node,
            before.blob_sha.as_deref().unwrap(),
            &[7; 32],
            OffsetDateTime::UNIX_EPOCH,
        )
        .await
        .unwrap();

    env.fake.put_at("draft.txt", b"second", 2);
    assert_eq!(
        env.synced_at(OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(60))
            .await
            .changed,
        1
    );
    let after = env.item("draft.txt").await;
    assert_eq!(after.node, before.node);
    assert_eq!(after.blob_sha.as_deref(), Some("x6-2"));
    assert_eq!(
        env.server
            .records_db()
            .external_item(after.node)
            .await
            .unwrap()
            .unwrap()
            .sha256,
        None
    );
    assert!(env.fake.opened_paths().is_empty());
}

#[tokio::test]
async fn a_renamed_file_keeps_its_node_and_permissions() {
    let env = Env::new().await;
    env.fake.put_at("old.txt", b"same", 9);
    env.synced_at(OffsetDateTime::UNIX_EPOCH).await;
    let before = env.item("old.txt").await;
    let viewer = env.server.user("Vera Viewer").await;
    env.server
        .records_db()
        .set_node_access(
            env.admin.id,
            before.node,
            Principal::User(viewer.id),
            Some("viewer"),
        )
        .await
        .unwrap();

    env.fake.rename("old.txt", "new.txt");
    let report = env
        .synced_at(OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(60))
        .await;
    assert_eq!((report.moved, report.added, report.removed), (1, 0, 0));
    assert_eq!(env.item("new.txt").await.node, before.node);
    assert!(
        env.server
            .records_db()
            .node(before.node)
            .await
            .unwrap()
            .is_some_and(|node| node.unique_scope)
    );
}

#[tokio::test]
async fn two_files_with_one_tag_are_not_mistaken_for_a_move() {
    let env = Env::new().await;
    env.fake.put_at("old-a.txt", b"a", 5);
    env.fake.put_at("old-b.txt", b"b", 5);
    env.synced_at(OffsetDateTime::UNIX_EPOCH).await;
    let old = [
        env.item("old-a.txt").await.node,
        env.item("old-b.txt").await.node,
    ];

    env.fake.remove("old-a.txt");
    env.fake.remove("old-b.txt");
    env.fake.put_at("new-a.txt", b"c", 5);
    env.fake.put_at("new-b.txt", b"d", 5);
    let report = env
        .synced_at(OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(60))
        .await;
    assert_eq!(report.moved, 0);
    assert_eq!(report.added, 2);
    let new = [
        env.item("new-a.txt").await.node,
        env.item("new-b.txt").await.node,
    ];
    assert!(new.into_iter().all(|node| !old.contains(&node)));
    for node in old {
        assert!(!env.in_bin(node).await, "the one-poll grace still applies");
    }
}

#[tokio::test]
async fn a_missing_file_stays_for_one_poll_then_goes_to_the_bin() {
    let env = Env::new().await;
    env.fake.put("gone.txt", b"gone");
    env.synced_at(OffsetDateTime::UNIX_EPOCH).await;
    let node = env.item("gone.txt").await.node;
    env.fake.remove("gone.txt");

    env.sync_at(OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(1))
        .await
        .unwrap();
    assert!(!env.in_bin(node).await);
    env.sync_at(OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(61))
        .await
        .unwrap();
    assert!(env.in_bin(node).await);
}

#[tokio::test]
async fn a_file_that_comes_back_within_the_grace_keeps_its_node() {
    let env = Env::new().await;
    env.fake.put_at("back.txt", b"back", 3);
    env.synced_at(OffsetDateTime::UNIX_EPOCH).await;
    let node = env.item("back.txt").await.node;
    env.fake.remove("back.txt");
    env.sync_at(OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(1))
        .await
        .unwrap();
    env.fake.put_at("back.txt", b"back", 3);

    env.sync_at(OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(2))
        .await
        .unwrap();
    assert_eq!(env.item("back.txt").await.node, node);
}

#[tokio::test]
async fn an_offline_share_changes_nothing_and_says_so() {
    let env = Env::new().await;
    env.fake.put("kept.txt", b"kept");
    env.synced_at(OffsetDateTime::UNIX_EPOCH).await;
    let before = env
        .server
        .records_db()
        .source_items(env.library)
        .await
        .unwrap();
    env.fake.offline(true);

    let answer = env
        .sync_at(OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(60))
        .await;
    assert!(
        matches!(&answer, Err(SyncError::Failed(message)) if message == "The share could not be reached, so nothing was changed.")
    );
    assert_eq!(
        env.server
            .records_db()
            .source_items(env.library)
            .await
            .unwrap(),
        before
    );
}

#[tokio::test]
async fn an_unreadable_sub_folder_keeps_what_was_known_below_it() {
    let env = Env::new().await;
    env.fake.put("private/kept.txt", b"kept");
    env.synced_at(OffsetDateTime::UNIX_EPOCH).await;
    let node = env.item("private/kept.txt").await.node;
    env.fake.deny("private");

    env.sync_at(OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(60))
        .await
        .unwrap();
    assert_eq!(env.item("private/kept.txt").await.node, node);
    assert!(!env.in_bin(node).await);
}

#[tokio::test]
async fn unreadable_files_ignore_new_policy_and_cache_limits() {
    let env = Env::new().await;
    env.fake.put("private/kept.exe", b"MZ");
    env.fake.put("private/large.txt", b"large");
    env.state
        .shares()
        .set_fs(Arc::new(LargeListingShare(env.fake.clone())));
    env.synced_at(OffsetDateTime::UNIX_EPOCH).await;
    let before = [
        env.item("private/kept.exe").await,
        env.item("private/large.txt").await,
    ];
    let before_names = [
        env.server
            .records_db()
            .node(before[0].node)
            .await
            .unwrap()
            .unwrap()
            .name,
        env.server
            .records_db()
            .node(before[1].node)
            .await
            .unwrap()
            .unwrap()
            .name,
    ];

    env.fake.deny("private");
    let (status, body) = call(
        &env.server,
        &env.admin,
        Method::PATCH,
        &format!("/api/libraries/{}", env.library.0),
        Some(json!({ "blocked_types": [".exe"] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    env.server
        .records_db()
        .set_setting(
            env.admin.id,
            SHARE_CACHE_MAX_BYTES,
            json!(256 * 1024 * 1024_u64),
        )
        .await
        .unwrap();

    env.sync_at(OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(60))
        .await
        .unwrap();
    env.sync_at(OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(120))
        .await
        .unwrap();

    for (item, name) in before.into_iter().zip(before_names) {
        assert_eq!(env.item(&item.path).await, item);
        let node = env
            .server
            .records_db()
            .node(item.node)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(node.name, name);
        assert!(!node.deleted);
    }
}

#[tokio::test]
async fn over_the_file_cap_changes_nothing() {
    let env = Env::new().await;
    env.server
        .records_db()
        .set_setting(env.admin.id, SOURCE_MAX_FILES, json!(1))
        .await
        .unwrap();
    env.fake.put("a.txt", b"a");
    env.fake.put("b.txt", b"b");

    let answer = env.sync_at(OffsetDateTime::UNIX_EPOCH).await;
    assert!(
        matches!(&answer, Err(SyncError::Failed(message)) if message == "The folder holds more than 1 files, the most this server shows for one library, so nothing was changed.")
    );
    assert!(
        env.server
            .records_db()
            .source_items(env.library)
            .await
            .unwrap()
            .is_empty()
    );
}

struct TimeoutShare(FakeShare);

impl ShareFs for TimeoutShare {
    fn list(
        &self,
        _approved_root: &Path,
        _dir: &Path,
        _budget: &FsBudget,
    ) -> io::Result<Vec<FsEntry>> {
        Err(io::Error::new(io::ErrorKind::TimedOut, "test timeout"))
    }

    fn stat(&self, approved_root: &Path, path: &Path) -> io::Result<FsStat> {
        self.0.stat(approved_root, path)
    }

    fn open(&self, approved_root: &Path, path: &Path) -> io::Result<Box<dyn io::Read + Send>> {
        self.0.open(approved_root, path)
    }

    fn canonical(&self, approved_root: &Path, path: &Path) -> io::Result<PathBuf> {
        self.0.canonical(approved_root, path)
    }
}

struct LargeListingShare(FakeShare);

impl ShareFs for LargeListingShare {
    fn list(
        &self,
        approved_root: &Path,
        dir: &Path,
        budget: &FsBudget,
    ) -> io::Result<Vec<FsEntry>> {
        let mut entries = self.0.list(approved_root, dir, budget)?;
        for entry in &mut entries {
            if entry.name.as_deref() == Some("large.txt") {
                entry.size = 128 * 1024 * 1024 + 1;
            }
        }
        Ok(entries)
    }

    fn stat(&self, approved_root: &Path, path: &Path) -> io::Result<FsStat> {
        self.0.stat(approved_root, path)
    }

    fn open(&self, approved_root: &Path, path: &Path) -> io::Result<Box<dyn io::Read + Send>> {
        self.0.open(approved_root, path)
    }

    fn canonical(&self, approved_root: &Path, path: &Path) -> io::Result<PathBuf> {
        self.0.canonical(approved_root, path)
    }
}

#[tokio::test]
async fn a_listing_over_time_changes_nothing() {
    let env = Env::new().await;
    env.state
        .shares()
        .set_fs(Arc::new(TimeoutShare(env.fake.clone())));
    let answer = env.sync_at(OffsetDateTime::UNIX_EPOCH).await;
    assert!(
        matches!(&answer, Err(SyncError::Failed(message)) if message == "Listing the folder took longer than 600 seconds, so nothing was changed.")
    );
    assert!(
        env.server
            .records_db()
            .source_items(env.library)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn a_root_no_longer_approved_is_not_connected_and_files_stay_listed() {
    let env = Env::new().await;
    env.fake.put("kept.txt", b"kept");
    env.synced_at(OffsetDateTime::UNIX_EPOCH).await;
    let before = env
        .server
        .records_db()
        .source_items(env.library)
        .await
        .unwrap();
    env.server
        .records_db()
        .set_setting(env.admin.id, SHARE_ROOTS, json!([]))
        .await
        .unwrap();

    let answer = env
        .sync_at(OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(60))
        .await;
    assert!(
        matches!(&answer, Err(SyncError::NotConnected(message)) if message == "This folder is no longer approved for ShareCraft. An administrator can approve it again.")
    );
    assert_eq!(
        env.server
            .records_db()
            .source_items(env.library)
            .await
            .unwrap(),
        before
    );
}

#[tokio::test]
async fn case_colliding_files_are_both_numbered() {
    let env = Env::new().await;
    env.fake.put_at("Report.pdf", b"upper", 1);
    env.fake.put_at("report.pdf", b"lower", 2);
    let report = env.synced_at(OffsetDateTime::UNIX_EPOCH).await;
    let mut paths: Vec<_> = env
        .server
        .records_db()
        .source_items(env.library)
        .await
        .unwrap()
        .into_iter()
        .map(|item| item.path)
        .collect();
    paths.sort();
    assert_eq!(paths, ["Report (1).pdf", "report (2).pdf"]);
    assert_eq!(report.hidden.renamed, 2);
}

#[tokio::test]
async fn blocked_types_and_over_size_files_are_hidden_and_counted() {
    let env = Env::new().await;
    let (status, body) = call(
        &env.server,
        &env.admin,
        Method::PATCH,
        &format!("/api/libraries/{}", env.library.0),
        Some(json!({ "blocked_types": [".exe"] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    env.server
        .records_db()
        .set_setting(
            env.admin.id,
            SHARE_CACHE_MAX_BYTES,
            json!(256 * 1024 * 1024_u64),
        )
        .await
        .unwrap();
    env.fake.put("bad.exe", b"MZ");
    env.fake.put("large.txt", b"12345");
    env.fake.put("ok.txt", b"1234");
    env.state
        .shares()
        .set_fs(Arc::new(LargeListingShare(env.fake.clone())));

    let report = env.synced_at(OffsetDateTime::UNIX_EPOCH).await;
    assert_eq!(report.hidden.policy, 1);
    assert_eq!(report.hidden.too_big, 1);
    assert_eq!(
        env.server
            .records_db()
            .source_items(env.library)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn links_system_files_and_placeholders_are_hidden_and_counted_in_hidden_detail() {
    let env = Env::new().await;
    env.fake.put("real.txt", b"real");
    env.fake.link("shortcut.txt", "real.txt");
    env.fake.put("desktop.ini", b"system");
    env.fake.put("offline.txt", b"placeholder");
    env.fake.set_attributes("offline.txt", 0x1000);

    let report = env.synced_at(OffsetDateTime::UNIX_EPOCH).await;
    assert_eq!(
        (
            report.hidden.symlinks,
            report.hidden.system,
            report.hidden.offline
        ),
        (1, 1, 1)
    );
    let mut conn = sqlx::PgConnection::connect(&env.server.db.url())
        .await
        .unwrap();
    let detail: Value =
        sqlx::query_scalar("select hidden_detail from library_sources where library_id = $1")
            .bind(env.library.0)
            .fetch_one(&mut conn)
            .await
            .unwrap();
    assert_eq!(detail["links"], 1);
    assert_eq!(detail["system"], 1);
    assert_eq!(detail["offline"], 1);
}

#[tokio::test]
async fn site_usage_is_unchanged_by_a_listing() {
    let env = Env::new().await;
    let db = env.server.records_db();
    let site = db.node(env.library).await.unwrap().unwrap().site;
    let before = db.site_usage(site).await.unwrap();
    env.fake.put("large.txt", vec![b'x'; 50_000]);

    env.synced_at(OffsetDateTime::UNIX_EPOCH).await;
    assert_eq!(db.site_usage(site).await.unwrap(), before);
}

#[tokio::test]
async fn a_generation_change_stops_a_long_apply() {
    let env = Env::new().await;
    for number in 0..120 {
        env.fake.put(&format!("file-{number:03}.txt"), b"x");
    }
    env.fake.slow_list(Duration::from_millis(200));
    let state = env.state.clone();
    let library = env.library;
    let task =
        tokio::spawn(async move { sync_once(&state, library, OffsetDateTime::UNIX_EPOCH).await });
    let deadline = tokio::time::Instant::now() + common::TIMEOUT;
    while env.fake.lists() == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "listing did not start"
        );
        tokio::task::yield_now().await;
    }
    env.server
        .records_db()
        .bump_source_generation(env.library)
        .await
        .unwrap();

    assert!(matches!(task.await.unwrap(), Err(SyncError::Failed(_))));
    let status = env.status().await;
    assert_eq!(status.files, 0);
    assert_eq!(status.last_synced_at, None);
    assert!(
        env.server
            .records_db()
            .try_lock_source(env.library)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn a_generation_change_stops_folder_heavy_apply_and_next_sync_completes() {
    let env = Env::new().await;
    for number in 0..200 {
        env.fake.put(&format!("folder-{number:03}/file.txt"), b"x");
    }
    let state = env.state.clone();
    let library = env.library;
    let task =
        tokio::spawn(async move { sync_once(&state, library, OffsetDateTime::UNIX_EPOCH).await });
    let deadline = tokio::time::Instant::now() + common::TIMEOUT;
    loop {
        let folders = env
            .server
            .records_db()
            .source_items(env.library)
            .await
            .unwrap()
            .into_iter()
            .filter(|item| item.blob_sha.is_none())
            .count();
        if folders >= 5 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "folder apply did not start"
        );
        tokio::task::yield_now().await;
    }
    env.server
        .records_db()
        .bump_source_generation(env.library)
        .await
        .unwrap();

    assert!(matches!(task.await.unwrap(), Err(SyncError::Failed(_))));
    let after_stop = env
        .server
        .records_db()
        .source_items(env.library)
        .await
        .unwrap();
    let folders_after_stop = after_stop
        .iter()
        .filter(|item| item.blob_sha.is_none())
        .count();
    assert!(
        folders_after_stop <= 105,
        "created {folders_after_stop} of 200 folders after the generation changed"
    );

    env.synced_at(OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(60))
        .await;
    let completed = env
        .server
        .records_db()
        .source_items(env.library)
        .await
        .unwrap();
    assert_eq!(
        completed
            .iter()
            .filter(|item| item.blob_sha.is_none())
            .count(),
        200
    );
    assert_eq!(completed.len(), 400);
}

#[tokio::test]
async fn events_carry_counts_and_no_path() {
    let env = Env::new().await;
    env.fake.put("folder/a.txt", b"a");
    env.fake.put("b.txt", b"b");
    env.synced_at(OffsetDateTime::UNIX_EPOCH).await;
    let events = env
        .server
        .records_db()
        .events_of_kind("source.synced")
        .await
        .unwrap();
    let detail = &events.first().expect("source.synced event").detail;
    assert_eq!(detail["added"], 2);
    assert_eq!(detail["files"], 2);
    assert!(detail.get("seconds").is_some());
    assert!(detail.get("path").is_none());
    assert!(detail.get("commit").is_none());
}

#[tokio::test]
async fn github_sync_is_unaffected() {
    const OWNER: &str = "octo-org";
    const REPO: &str = "docs";
    let fake: FakeGithub = start_fake_github(OWNER, REPO).await;
    let origin = fake.origin();
    let server = spawn_server_config_without_workers(|config| config.github_api = origin).await;
    let admin = server.admin("Ada Admin").await;
    let library = common::Site::library(&server).await;
    server
        .records_db()
        .connect_source(&NewSource {
            library,
            kind: SourceKind::Github,
            owner: OWNER.to_owned(),
            repo: REPO.to_owned(),
            branch: "main".to_owned(),
            folder: String::new(),
            location: None,
            mode: SourceMode::ReadOnly,
            token: None,
            poll_seconds: 300,
            connected_by: admin.id,
        })
        .await
        .unwrap();
    fake.commit("main", &[("hello.txt", b"hello".to_vec())]);

    let report = sync_once(
        &server.app_state().await,
        library,
        OffsetDateTime::UNIX_EPOCH,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(report.added, 1);
    let item = server
        .records_db()
        .source_items(library)
        .await
        .unwrap()
        .remove(0);
    assert!(
        server
            .records_db()
            .document(DocId(item.node.0))
            .await
            .unwrap()
            .unwrap()
            .revision
            .is_some()
    );
}

#[tokio::test]
#[ignore = "task-card performance measurement"]
async fn measure_first_listing_of_2000_files() {
    let env = Env::new().await;
    env.server
        .records_db()
        .set_setting(env.admin.id, SOURCE_MAX_FILES, json!(2_100))
        .await
        .unwrap();
    for number in 0..2_000 {
        env.fake.put(&format!("file-{number:04}.txt"), b"x");
    }
    let started = Instant::now();
    let report = env.synced_at(OffsetDateTime::UNIX_EPOCH).await;
    let took = started.elapsed();
    assert_eq!(report.added, 2_000);
    eprintln!(
        "TASK6_MEASUREMENT files=2000 seconds={:.3} files_per_second={:.2}",
        took.as_secs_f64(),
        2_000.0 / took.as_secs_f64()
    );
}
