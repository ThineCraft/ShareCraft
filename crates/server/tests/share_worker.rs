//! The sync worker for file shares: a share whose listings change nothing is listed less and
//! less often (at most hourly), a change, "Update now" or a read that finds a changed file
//! brings the next listing forward, a slow listing spaces out the next one, failures back off,
//! a folder no longer approved stops until the roots change, and every tick keeps the cache
//! within its size.
//!
//! Tests drive [`server::source_tick_once`] with a chosen `now` on a server without workers.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use collab_core::ids::NodeId;
use common::fake_share::FakeShare;
use common::{TIMEOUT, TestServer, TestUser, spawn_server_config_without_workers};
use records::{NewSource, SHARE_ROOTS, SourceKind, SourceMode, SourceState, SourceStatus};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use server::AppState;
use server::external::materialise;
use server::source_sync::next_share_check;
use time::OffsetDateTime;

#[cfg(windows)]
const ROOT: &str = r"C:\approved";
#[cfg(not(windows))]
const ROOT: &str = "/approved";

const OWNER: &str = "octo-org";
const REPO: &str = "docs";

/// A fixed starting time, far from the real clock.
fn t0() -> OffsetDateTime {
    OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000)
}

fn secs(seconds: i64) -> time::Duration {
    time::Duration::seconds(seconds)
}

struct Env {
    server: TestServer,
    fake: FakeShare,
    admin: TestUser,
    site: NodeId,
    library: NodeId,
    state: AppState,
}

impl Env {
    /// A share library connected (polled every 60 seconds) and not yet listed, on a server
    /// whose workers do not run.
    async fn new() -> Self {
        Self::with_server(
            spawn_server_config_without_workers(|config| {
                config.max_file_bytes = 512 * 1024 * 1024;
            })
            .await,
        )
        .await
    }

    async fn with_server(server: TestServer) -> Self {
        let admin = server.admin("Ada Admin").await;
        let library = common::Site::library(&server).await;
        let site = server
            .records_db()
            .node(library)
            .await
            .unwrap()
            .unwrap()
            .site;
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
            site,
            library,
            state,
        }
    }

    async fn tick(&self, now: OffsetDateTime) -> usize {
        server::source_tick_once(&self.state, now).await
    }

    /// Ticks at `now` and answers whether the share was looked at (a listing or an attempt).
    async fn listed_at(&self, now: OffsetDateTime) -> bool {
        let before = self.attempts();
        self.tick(now).await;
        self.attempts() > before
    }

    /// Every look at the share: the stat of its root and each folder listed.
    fn attempts(&self) -> usize {
        self.fake.stats() + self.fake.lists()
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

    async fn files(&self) -> Vec<String> {
        let mut paths: Vec<String> = self
            .server
            .records_db()
            .source_items(self.library)
            .await
            .unwrap()
            .into_iter()
            .map(|item| item.path)
            .collect();
        paths.sort();
        paths
    }

    async fn node(&self, path: &str) -> NodeId {
        self.server
            .records_db()
            .source_items(self.library)
            .await
            .unwrap()
            .into_iter()
            .find(|item| item.path == path)
            .unwrap_or_else(|| panic!("no source item at {path}"))
            .node
    }

    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut request = self
            .server
            .http(Some(&self.admin))
            .request(method, self.server.url(path));
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        let status = response.status();
        let text = response.text().await.unwrap();
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    /// Lists at `t0`, then lets `quiet` listings in a row find nothing new; answers the time of
    /// the last listing.
    async fn quiet_for(&self, quiet: u32) -> OffsetDateTime {
        let mut at = t0();
        assert!(self.listed_at(at).await);
        for _ in 0..quiet {
            at = self.status().await.next_check_at.unwrap();
            assert!(self.listed_at(at).await);
        }
        at
    }
}

#[test]
fn next_share_check_doubles_when_quiet_caps_at_an_hour_and_respects_listing_time() {
    let now = t0();
    let quick = Duration::from_millis(50);
    assert_eq!(next_share_check(now, 60, 0, quick), now + secs(60));
    assert_eq!(next_share_check(now, 60, 1, quick), now + secs(120));
    assert_eq!(next_share_check(now, 60, 5, quick), now + secs(1_920));
    // 3,840 seconds would be over the hour.
    assert_eq!(next_share_check(now, 60, 6, quick), now + secs(3_600));
    assert_eq!(next_share_check(now, 60, 63, quick), now + secs(3_600));
    assert_eq!(next_share_check(now, 60, 64, quick), now + secs(3_600));
    assert_eq!(
        next_share_check(now, 60, u32::MAX, quick),
        now + secs(3_600)
    );
    // A poll time over the hour is kept as the cap.
    assert_eq!(next_share_check(now, 7_200, 0, quick), now + secs(7_200));
    assert_eq!(next_share_check(now, 7_200, 3, quick), now + secs(7_200));
    assert_eq!(
        next_share_check(now, u32::MAX, u32::MAX, quick),
        now + secs(i64::from(u32::MAX))
    );
    // Never sooner than four times the listing, even past the cap.
    assert_eq!(
        next_share_check(now, 60, 0, Duration::from_secs(1_000)),
        now + secs(4_000)
    );
    assert_eq!(
        next_share_check(now, 60, 40, Duration::from_secs(1_000)),
        now + secs(4_000)
    );
    assert_eq!(
        next_share_check(now, 60, 0, Duration::from_secs(10)),
        now + secs(60)
    );
    // A poll time of zero still waits; an absurd listing time does not overflow.
    assert_eq!(next_share_check(now, 0, 0, Duration::ZERO), now + secs(1));
    assert!(next_share_check(now, 60, 0, Duration::MAX) >= now + secs(3_600));
}

#[tokio::test]
async fn a_quiet_share_is_listed_less_often_and_a_change_resets_the_interval() {
    let env = Env::new().await;
    env.fake.put("a.txt", b"a");

    // The first listing changes the library: listed again after the poll time.
    assert!(env.listed_at(t0()).await);
    assert_eq!(env.status().await.next_check_at, Some(t0() + secs(60)));
    assert!(!env.listed_at(t0() + secs(59)).await);

    // Each listing that changes nothing doubles the wait.
    assert!(env.listed_at(t0() + secs(60)).await);
    assert_eq!(env.status().await.next_check_at, Some(t0() + secs(180)));
    assert!(!env.listed_at(t0() + secs(179)).await);
    assert!(env.listed_at(t0() + secs(180)).await);
    assert_eq!(env.status().await.next_check_at, Some(t0() + secs(420)));
    assert!(!env.listed_at(t0() + secs(419)).await);

    // A change starts over at the poll time.
    env.fake.put_at("b.txt", b"b", 2);
    assert!(env.listed_at(t0() + secs(420)).await);
    assert_eq!(env.files().await, ["a.txt", "b.txt"]);
    assert_eq!(env.status().await.next_check_at, Some(t0() + secs(480)));
    assert!(!env.listed_at(t0() + secs(479)).await);
    assert!(env.listed_at(t0() + secs(480)).await);
}

#[tokio::test]
async fn update_now_lists_at_once_whatever_the_next_check_time() {
    let env = Env::new().await;
    env.fake.put("a.txt", b"a");
    let last = env.quiet_for(3).await;
    let next = env.status().await.next_check_at.unwrap();
    assert_eq!(next, last + secs(480));
    let soon = last + secs(1);
    assert!(!env.listed_at(soon).await);

    let (status, answer) = env
        .call(
            Method::POST,
            &format!("/api/libraries/{}/source/sync", env.library.0),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{answer}");

    assert!(env.listed_at(soon).await);
    let after = env.status().await;
    assert_eq!(after.state, SourceState::Ok);
    // The count started over: this quiet listing is the first in a row.
    assert_eq!(after.next_check_at, Some(soon + secs(120)));
}

#[tokio::test]
async fn an_offline_share_backs_off_and_recovers_without_losing_files() {
    let env = Env::new().await;
    env.fake.put("a.txt", b"a");
    env.fake.put("docs/b.txt", b"b");
    let last = env.quiet_for(2).await;
    let next = env.status().await.next_check_at.unwrap();
    assert_eq!(next, last + secs(240));
    env.fake.offline(true);

    // Offline: failed, retried after a minute, then two, files kept throughout.
    assert!(env.listed_at(next).await);
    let failed = env.status().await;
    assert_eq!(failed.state, SourceState::Failed, "{failed:?}");
    assert_eq!(failed.retry_at, Some(next + secs(60)));
    assert_eq!(failed.next_check_at, None);
    assert_eq!(env.files().await, ["a.txt", "docs", "docs/b.txt"]);
    assert!(!env.listed_at(next + secs(59)).await);
    assert!(env.listed_at(next + secs(60)).await);
    assert_eq!(env.status().await.retry_at, Some(next + secs(180)));
    assert!(!env.listed_at(next + secs(179)).await);

    // Back online: listed at the retry time, nothing lost, the count started over.
    env.fake.offline(false);
    let back = next + secs(180);
    assert!(env.listed_at(back).await);
    let recovered = env.status().await;
    assert_eq!(recovered.state, SourceState::Ok, "{recovered:?}");
    assert_eq!(recovered.retry_at, None);
    assert_eq!(recovered.next_check_at, Some(back + secs(120)));
    assert_eq!(env.files().await, ["a.txt", "docs", "docs/b.txt"]);
    for path in ["a.txt", "docs/b.txt"] {
        let node = env.node(path).await;
        let live = env.server.records_db().node(node).await.unwrap().unwrap();
        assert!(!live.deleted, "{path} went to the bin");
    }
}

#[tokio::test]
async fn a_root_removed_stops_retries_and_approving_it_again_resumes() {
    let env = Env::new().await;
    env.fake.put("a.txt", b"a");
    assert!(env.listed_at(t0()).await);

    let (status, answer) = env
        .call(
            Method::PUT,
            "/api/settings/shares",
            Some(json!({ "roots": [] })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    env.tick(t0() + secs(60)).await;
    let stopped = env.status().await;
    assert_eq!(stopped.state, SourceState::NotConnected, "{stopped:?}");
    assert_eq!(env.files().await, ["a.txt"]);

    // Not retried, however long it waits.
    for later in [secs(120), secs(3_600), secs(86_400)] {
        assert!(!env.listed_at(t0() + later).await);
        assert_eq!(env.status().await.state, SourceState::NotConnected);
    }

    // Approving the folder again wakes it.
    let (status, answer) = env
        .call(
            Method::PUT,
            "/api/settings/shares",
            Some(json!({ "roots": [ROOT] })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let resumed_at = t0() + secs(86_401);
    assert!(env.listed_at(resumed_at).await);
    let resumed = env.status().await;
    assert_eq!(resumed.state, SourceState::Ok, "{resumed:?}");
    assert_eq!(resumed.last_error, None);
    assert_eq!(env.files().await, ["a.txt"]);
}

#[tokio::test]
async fn a_read_that_finds_a_changed_file_brings_the_listing_forward() {
    let env = Env::new().await;
    env.fake.put_at("report.txt", b"old", 1);
    let last = env.quiet_for(3).await;
    assert_eq!(env.status().await.next_check_at, Some(last + secs(480)));

    env.fake.put_at("report.txt", b"new bytes", 2);
    let node = env.node("report.txt").await;
    let ready = materialise(&env.state, node).await.unwrap().unwrap();
    drop(ready);

    let soon = last + secs(1);
    assert!(env.listed_at(soon).await);
    let after = env.status().await;
    assert_eq!(after.state, SourceState::Ok);
    assert!(
        after.next_check_at.unwrap() <= soon + secs(120),
        "{after:?}"
    );
}

#[tokio::test]
async fn a_slow_listing_spaces_out_the_next_one() {
    let env = Env::new().await;
    env.fake.put("a.txt", b"a");
    env.state
        .shares()
        .set_listing_took_for_tests(Duration::from_secs(600));

    // Four times the listing, counted from when the listing finished.
    assert!(env.listed_at(t0()).await);
    let next = t0() + secs(600 + 2_400);
    assert_eq!(env.status().await.next_check_at, Some(next));
    assert!(!env.listed_at(next - secs(1)).await);
    assert!(env.listed_at(next).await);
    // Quiet now, and still four times the listing at least, even past the hour.
    assert_eq!(
        env.status().await.next_check_at,
        Some(next + secs(600 + 2_400))
    );
}

#[tokio::test]
async fn a_sync_that_waited_for_a_turn_counts_its_next_check_from_when_it_listed() {
    let env = Env::new().await;
    env.fake.put("a.txt", b"a");
    // The sync takes its turn and is held before it starts listing.
    let held = env.state.sources().hold_starts().await;
    let state = env.state.clone();
    let tick = tokio::spawn(async move { server::source_tick_once(&state, t0()).await });
    tokio::time::timeout(TIMEOUT, env.state.sources().wait_start_held())
        .await
        .expect("the sync never reached its start");
    // A measured wait: the sync now starts listing at least a whole second after `t0`.
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    drop(held);
    assert_eq!(tick.await.unwrap(), 1);

    let next = env.status().await.next_check_at.unwrap();
    assert!(next >= t0() + secs(61), "{next} is counted from the tick");
    assert!(next <= t0() + secs(60) + TIMEOUT, "{next}");
}

#[tokio::test]
async fn a_listing_longer_than_the_stale_time_is_not_reset_while_it_holds_the_lock() {
    let env = Env::new().await;
    env.fake.put("a.txt", b"a");
    let lists = env.fake.lists();
    env.fake.pause_list_after_read();
    let state = env.state.clone();
    let first = tokio::spawn(async move { server::source_tick_once(&state, t0()).await });
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while env.fake.lists() == lists {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the listing never began"
        );
        tokio::task::yield_now().await;
    }
    assert_eq!(env.status().await.state, SourceState::Syncing);

    // Far past the stale time: the listing still holds its lock, so it is left alone.
    let later = t0() + server::source_sync::STALE_SYNC + secs(60);
    assert_eq!(env.tick(later).await, 0);
    let held = env.status().await;
    assert_eq!(held.state, SourceState::Syncing, "{held:?}");
    assert_eq!(held.last_checked_at, Some(t0()));
    assert_eq!(env.fake.lists(), lists + 1);

    env.fake.resume_list_after_read();
    assert_eq!(first.await.unwrap(), 1);
    let done = env.status().await;
    assert_eq!(done.state, SourceState::Ok, "{done:?}");
    assert_eq!(env.files().await, ["a.txt"]);
    assert_eq!(env.fake.lists(), lists + 1);
}

#[tokio::test]
async fn the_tick_evicts_after_the_cache_size_is_lowered() {
    let env = Env::new().await;
    env.fake.put("report.txt", b"quarterly");
    assert!(env.listed_at(t0()).await);
    let node = env.node("report.txt").await;
    drop(materialise(&env.state, node).await.unwrap().unwrap());
    let db = env.server.records_db();
    assert_eq!(db.cached_bytes().await.unwrap(), 9);

    env.state.shares().set_cache_max_bytes_for_tests(0);
    // Nothing is due: the tick only keeps the cache within its size.
    assert!(!env.listed_at(t0() + secs(1)).await);

    assert_eq!(db.cached_bytes().await.unwrap(), 0);
}

#[tokio::test]
async fn a_github_library_and_a_share_library_are_both_synced_by_one_tick() {
    let fake_github = common::start_fake_github(OWNER, REPO).await;
    fake_github.commit("main", &[("a.md", b"first".to_vec())]);
    let origin = fake_github.origin();
    let server = spawn_server_config_without_workers(|config| {
        config.max_file_bytes = 512 * 1024 * 1024;
        config.github_api = origin;
    })
    .await;
    let env = Env::with_server(server).await;
    env.fake.put("share.txt", b"share");
    let (status, answer) = env
        .call(
            Method::POST,
            &format!("/api/sites/{}/libraries/github", env.site.0),
            Some(json!({ "name": "Mirror", "repository": format!("{OWNER}/{REPO}") })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{answer}");
    let github = NodeId(answer["id"].as_str().unwrap().parse().unwrap());

    // Whole seconds, as the database keeps only microseconds.
    let now = OffsetDateTime::now_utc().replace_nanosecond(0).unwrap();
    assert_eq!(env.tick(now).await, 2);

    let db = env.server.records_db();
    let (_, mirrored) = db.source_of(github).await.unwrap().unwrap();
    assert_eq!(mirrored.state, SourceState::Ok, "{mirrored:?}");
    assert_eq!(mirrored.files, 1);
    // A GitHub library keeps its poll time; only shares get a next check time.
    assert_eq!(mirrored.next_check_at, None);
    let shared = env.status().await;
    assert_eq!(shared.state, SourceState::Ok, "{shared:?}");
    assert_eq!(shared.files, 1);
    assert_eq!(shared.next_check_at, Some(now + secs(60)));
}

#[tokio::test]
async fn a_disconnect_stops_a_long_listing_and_no_new_listing_starts() {
    let env = Env::new().await;
    env.fake.put("a.txt", b"a");
    assert!(env.listed_at(t0()).await);
    // A walk of over 10 seconds: 100 folders, four at a time, half a second each.
    for n in 0..100 {
        env.fake.put(&format!("folder-{n:03}/file.txt"), b"x");
    }
    env.fake.slow_list(Duration::from_millis(500));
    let lists = env.fake.lists();
    let state = env.state.clone();
    let listing =
        tokio::spawn(async move { server::source_tick_once(&state, t0() + secs(60)).await });
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while env.fake.lists() < lists + 2 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the listing never began"
        );
        tokio::task::yield_now().await;
    }
    assert_eq!(env.status().await.state, SourceState::Syncing);

    let started = std::time::Instant::now();
    let (status, answer) = env
        .call(
            Method::DELETE,
            &format!("/api/libraries/{}/source", env.library.0),
            None,
        )
        .await;

    assert_eq!(status, StatusCode::NO_CONTENT, "{answer}");
    assert!(
        started.elapsed() < TIMEOUT,
        "the disconnect waited {:?}",
        started.elapsed()
    );
    assert_eq!(listing.await.unwrap(), 1);
    assert!(
        env.server
            .records_db()
            .source_of(env.library)
            .await
            .unwrap()
            .is_none()
    );
    // The walk gave up: far fewer folders were read than a full listing reads, and no new
    // listing starts.
    let after = env.fake.lists();
    assert!(after < lists + 50, "{} folders read", after - lists);
    assert_eq!(env.tick(t0() + secs(120)).await, 0);
    assert_eq!(env.tick(t0() + secs(86_400)).await, 0);
    assert_eq!(env.fake.lists(), after);
}
