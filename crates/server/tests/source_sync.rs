//! The sync engine against the fake GitHub: a connected library mirrors its branch as folders
//! and files, a change on the branch becomes new versions, moves and recycle-bin entries, and a
//! sync that fails while reading changes nothing.

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

use std::time::Duration;

use collab_core::ids::{DocId, NodeId};
use common::fake_github::{AUTHOR_NAME, FakeGithub};
use common::{FileClient, TIMEOUT, TestServer, TestUser, spawn_server_config, start_fake_github};
use records::{NewSource, Principal, SealedRow, SourceKind, SourceState, SourceStatus};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use server::AppState;
use server::github::git_blob_id;
use server::source_sync::{SyncError, SyncReport, recover_stale_syncs, sync_once, token_context};
use sqlx::Connection;
use time::OffsetDateTime;

const OWNER: &str = "octo-org";
const REPO: &str = "docs";

/// A server whose library is connected to the fake's `octo-org/docs` branch `main`.
struct Env {
    server: TestServer,
    fake: FakeGithub,
    admin: TestUser,
    library: NodeId,
    state: AppState,
}

impl Env {
    /// A public repository, the whole of it mirrored.
    async fn new() -> Env {
        Env::connected(None, "").await
    }

    /// A repository readable only with `token` when given, its `folder` mirrored.
    async fn connected(token: Option<&str>, folder: &str) -> Env {
        let fake = start_fake_github(OWNER, REPO).await;
        let origin = fake.origin();
        let server = spawn_server_config(|c| c.github_api = origin).await;
        let admin = server.admin("Ada Admin").await;
        let library = common::Site::library(&server).await;
        let state = server.app_state().await;
        let db = server.records_db();
        let token = match token {
            Some(token) => {
                fake.require_token(token);
                let sealed = state
                    .secrets()
                    .seal(token.as_bytes(), &token_context(library))
                    .unwrap();
                let row = SealedRow {
                    key_id: sealed.key_id,
                    nonce: sealed.nonce,
                    ciphertext: sealed.ciphertext,
                };
                Some(db.put_secret(&row).await.unwrap())
            }
            None => None,
        };
        db.connect_source(&NewSource {
            library,
            kind: SourceKind::Github,
            owner: OWNER.to_owned(),
            repo: REPO.to_owned(),
            branch: "main".to_owned(),
            folder: folder.to_owned(),
            token,
            poll_seconds: 300,
            connected_by: admin.id,
        })
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

    async fn sync(&self) -> Result<Option<SyncReport>, SyncError> {
        sync_once(&self.state, self.library, OffsetDateTime::now_utc()).await
    }

    /// Syncs, failing the test unless the library changed.
    async fn synced(&self) -> SyncReport {
        self.sync().await.unwrap().expect("the library changed")
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

    /// The live items of the folder `parent`: (name, id, kind).
    async fn children(&self, parent: NodeId) -> Vec<(String, NodeId, String)> {
        let (status, page) = call(
            &self.server,
            &self.admin,
            Method::GET,
            &format!("/api/nodes/{}/children", parent.0),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{page}");
        page["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| {
                (
                    item["name"].as_str().unwrap().to_owned(),
                    NodeId(item["id"].as_str().unwrap().parse().unwrap()),
                    item["kind"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    }

    /// The node at `path` in the library, if it is live there.
    async fn find(&self, path: &str) -> Option<NodeId> {
        let mut at = self.library;
        for name in path.split('/') {
            at = self
                .children(at)
                .await
                .into_iter()
                .find(|(n, _, _)| n == name)?
                .1;
        }
        Some(at)
    }

    async fn node_at(&self, path: &str) -> NodeId {
        self.find(path)
            .await
            .unwrap_or_else(|| panic!("{path} is not in the library"))
    }

    /// Whether the node `id` is in the recycle bin (deleted, not purged).
    async fn in_bin(&self, id: NodeId) -> bool {
        self.server
            .records_db()
            .node(id)
            .await
            .unwrap()
            .is_some_and(|node| node.deleted)
    }

    /// Bob, signed in, a viewer of the library.
    async fn viewer(&self) -> TestUser {
        let bob = self.server.user("Bob Viewer").await;
        self.server
            .records_db()
            .set_node_access(
                self.admin.id,
                self.library,
                Principal::User(bob.id),
                Some("viewer"),
            )
            .await
            .unwrap();
        bob
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

/// The status and bytes of a download of `doc` as `user`.
async fn download(server: &TestServer, user: &TestUser, doc: NodeId) -> (StatusCode, Vec<u8>) {
    let response = server
        .http(Some(user))
        .get(server.url(&format!("/api/docs/{}/content", doc.0)))
        .send()
        .await
        .unwrap();
    let status = response.status();
    (status, response.bytes().await.unwrap().to_vec())
}

/// `(path, text)` pairs as the fake commits them.
fn files<'a>(list: &[(&'a str, &str)]) -> Vec<(&'a str, Vec<u8>)> {
    list.iter()
        .map(|(path, text)| (*path, text.as_bytes().to_vec()))
        .collect()
}

#[tokio::test]
async fn first_sync_creates_folders_and_files_with_the_right_bytes() {
    let env = Env::new().await;
    let tree = files(&[
        ("README.md", "# Docs\n"),
        ("guide/intro.md", "Hello from the guide.\n"),
        ("guide/deep/notes.txt", "Deep notes.\n"),
        ("empty.txt", ""),
    ]);
    let commit = env.fake.commit("main", &tree);
    let report = env.synced().await;
    assert_eq!(report.commit, commit.sha);
    assert_eq!(report.added, 4);
    assert_eq!((report.changed, report.removed, report.moved), (0, 0, 0));
    assert_eq!(
        report.bytes,
        tree.iter().map(|(_, b)| b.len() as u64).sum::<u64>()
    );

    let bob = env.viewer().await;
    for (path, bytes) in &tree {
        let id = env.node_at(path).await;
        let (status, got) = download(&env.server, &bob, id).await;
        if bytes.is_empty() {
            assert!(status.is_success(), "{path}: {status}");
        } else {
            assert_eq!(status, StatusCode::OK, "{path}");
        }
        assert_eq!(&got, bytes, "{path}");
    }
    let guide = env.children(env.node_at("guide").await).await;
    assert!(guide.iter().any(|(n, _, k)| n == "deep" && k == "folder"));

    let status = env.status().await;
    assert_eq!(status.state, SourceState::Ok);
    assert_eq!(status.last_commit.as_deref(), Some(commit.sha.as_str()));
    assert_eq!(status.files, 4);
    assert!(status.last_synced_at.is_some());
    assert!(status.etag.is_some());

    // Each revision says where it came from: the person who connected the library, the commit
    // and its GitHub author.
    let readme = env.node_at("README.md").await;
    let stored = env
        .server
        .store()
        .read(DocId(readme.0), None)
        .unwrap()
        .unwrap();
    let credited = &stored.meta.contributors[0];
    assert_eq!(credited.id, env.admin.id.0.to_string());
    assert!(
        credited.name.contains(&commit.sha[..7]),
        "{}",
        credited.name
    );
    assert!(credited.name.contains(AUTHOR_NAME), "{}", credited.name);
    assert!(credited.name.starts_with("Ada Admin"), "{}", credited.name);
}

#[tokio::test]
async fn second_sync_with_no_change_makes_one_conditional_request_and_no_blob_requests() {
    let env = Env::new().await;
    env.fake
        .commit("main", &files(&[("a.md", "A"), ("b/c.md", "C")]));
    env.synced().await;
    let before = |what| env.fake.count(what);
    let (commits, trees, blobs, tarballs) = (
        before("commits"),
        before("trees"),
        before("blobs"),
        before("tarball"),
    );
    assert_eq!(env.sync().await, Ok(None));
    assert_eq!(env.fake.count("commits"), commits + 1);
    assert_eq!(env.fake.count("commits_not_modified"), 1);
    assert_eq!(env.fake.count("trees"), trees);
    assert_eq!(env.fake.count("blobs"), blobs);
    assert_eq!(env.fake.count("tarball"), tarballs);
    let status = env.status().await;
    assert_eq!(status.state, SourceState::Ok);
    assert_eq!(status.files, 2);
}

#[tokio::test]
async fn an_edited_file_becomes_a_new_version_of_the_same_document() {
    let env = Env::new().await;
    env.fake
        .commit("main", &files(&[("notes/plan.md", "first draft")]));
    env.synced().await;
    let id = env.node_at("notes/plan.md").await;

    env.fake
        .commit("main", &files(&[("notes/plan.md", "second draft")]));
    let report = env.synced().await;
    assert_eq!((report.added, report.changed), (0, 1));
    assert_eq!(env.node_at("notes/plan.md").await, id);
    // Git holds both versions at once; the database's index follows the file session.
    assert_eq!(env.server.doc_commits(DocId(id.0)).len(), 2);
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while env.server.records_revisions(DocId(id.0)).await.len() < 2 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the new version was not indexed"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let (status, bytes) = download(&env.server, &env.admin, id).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, b"second draft");
}

#[tokio::test]
async fn a_deleted_file_goes_to_the_recycle_bin_and_its_empty_folder_is_removed() {
    let env = Env::new().await;
    env.fake.commit(
        "main",
        &files(&[("keep.md", "kept"), ("old/gone.md", "going")]),
    );
    env.synced().await;
    let gone = env.node_at("old/gone.md").await;
    let folder = env.node_at("old").await;
    let keep = env.node_at("keep.md").await;

    env.fake.commit("main", &files(&[("keep.md", "kept")]));
    let report = env.synced().await;
    assert_eq!(report.removed, 1);
    assert!(env.in_bin(gone).await, "the file is in the recycle bin");
    assert!(env.in_bin(folder).await, "its empty folder is removed");
    assert!(!env.in_bin(keep).await);
    assert_eq!(env.find("old").await, None);
    let items = env
        .server
        .records_db()
        .source_items(env.library)
        .await
        .unwrap();
    assert_eq!(
        items.iter().map(|i| i.path.as_str()).collect::<Vec<_>>(),
        ["keep.md"]
    );
    assert_eq!(env.status().await.files, 1);
}

#[tokio::test]
async fn a_moved_file_keeps_its_node_and_its_permissions() {
    let env = Env::new().await;
    env.fake
        .commit("main", &files(&[("drafts/report.md", "the report")]));
    env.synced().await;
    let id = env.node_at("drafts/report.md").await;
    let carol = env.server.user("Carol").await;
    env.server
        .records_db()
        .set_node_access(env.admin.id, id, Principal::User(carol.id), Some("viewer"))
        .await
        .unwrap();
    assert_eq!(download(&env.server, &carol, id).await.0, StatusCode::OK);

    env.fake
        .commit("main", &files(&[("final/report-final.md", "the report")]));
    let report = env.synced().await;
    assert_eq!(report.moved, 1);
    assert_eq!((report.added, report.removed), (0, 0));
    assert_eq!(env.node_at("final/report-final.md").await, id);
    assert_eq!(env.find("drafts").await, None);
    let (status, bytes) = download(&env.server, &carol, id).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, b"the report");
    // Only the one version: a move is not a new version.
    assert_eq!(env.server.records_revisions(DocId(id.0)).await.len(), 1);
}

#[tokio::test]
async fn many_changes_use_one_tarball_and_few_use_blob_requests() {
    let env = Env::new().await;
    let many: Vec<(String, String)> = (0..25)
        .map(|n| (format!("pages/page-{n:02}.md"), format!("page {n}")))
        .collect();
    let list: Vec<(&str, &str)> = many.iter().map(|(p, t)| (p.as_str(), t.as_str())).collect();
    env.fake.commit("main", &files(&list));
    let report = env.synced().await;
    assert_eq!(report.added, 25);
    assert_eq!(env.fake.count("tarball"), 1);
    assert_eq!(env.fake.count("blobs"), 0);
    let id = env.node_at("pages/page-07.md").await;
    assert_eq!(download(&env.server, &env.admin, id).await.1, b"page 7");

    let mut edited = list.clone();
    edited[3].1 = "page 3, edited";
    edited[4].1 = "page 4, edited";
    env.fake.commit("main", &files(&edited));
    let report = env.synced().await;
    assert_eq!(report.changed, 2);
    assert_eq!(env.fake.count("tarball"), 1);
    assert_eq!(env.fake.count("blobs"), 2);
    let id = env.node_at("pages/page-03.md").await;
    assert_eq!(
        download(&env.server, &env.admin, id).await.1,
        b"page 3, edited"
    );
}

#[tokio::test]
async fn a_corrupt_blob_fails_the_sync_and_changes_nothing() {
    let env = Env::new().await;
    env.fake.commit("main", &files(&[("a.md", "version one")]));
    let first = env.synced().await;
    let a = env.node_at("a.md").await;

    env.fake.commit(
        "main",
        &files(&[("a.md", "version two"), ("b.md", "a new file")]),
    );
    env.fake.corrupt_blob(&git_blob_id(b"a new file"));
    let failed = env.sync().await;
    assert!(
        matches!(&failed, Err(SyncError::Failed(m)) if m.contains("did not match")),
        "{failed:?}"
    );
    assert_eq!(download(&env.server, &env.admin, a).await.1, b"version one");
    assert_eq!(env.find("b.md").await, None);
    assert_eq!(env.server.records_revisions(DocId(a.0)).await.len(), 1);
    let status = env.status().await;
    assert_eq!(status.state, SourceState::Failed);
    assert_eq!(status.last_commit.as_deref(), Some(first.commit.as_str()));
    assert!(status.last_error.unwrap().contains("did not match"));
}

#[tokio::test]
async fn a_truncated_tree_keeps_existing_files_and_reports_it() {
    let env = Env::new().await;
    env.fake
        .commit("main", &files(&[("one.md", "1"), ("two.md", "2")]));
    let first = env.synced().await;
    let one = env.node_at("one.md").await;
    let two = env.node_at("two.md").await;

    env.fake.commit("main", &files(&[("one.md", "1")]));
    env.fake.force_truncate_trees();
    let failed = env.sync().await;
    assert!(matches!(failed, Err(SyncError::Failed(_))), "{failed:?}");
    assert!(!env.in_bin(one).await && !env.in_bin(two).await);
    let status = env.status().await;
    assert_eq!(status.state, SourceState::Failed);
    assert!(
        status
            .last_error
            .as_deref()
            .unwrap()
            .contains("too many files"),
        "{:?}",
        status.last_error
    );
    assert_eq!(status.last_commit.as_deref(), Some(first.commit.as_str()));
    assert_eq!(status.files, 2);
}

#[tokio::test]
async fn a_repository_over_the_cap_fails_before_writing() {
    let env = Env::new().await;
    let db = env.server.records_db();
    db.set_setting(env.admin.id, "source_max_files", json!(2))
        .await
        .unwrap();
    env.fake.commit(
        "main",
        &files(&[("a.md", "a"), ("b.md", "b"), ("c.md", "c")]),
    );
    let failed = env.sync().await;
    assert!(
        matches!(&failed, Err(SyncError::Failed(m)) if m.contains("3 files")),
        "{failed:?}"
    );
    assert!(env.children(env.library).await.is_empty());
    assert_eq!(env.fake.count("blobs") + env.fake.count("tarball"), 0);

    // The byte cap too.
    db.set_setting(env.admin.id, "source_max_files", json!(100))
        .await
        .unwrap();
    db.set_setting(env.admin.id, "source_max_bytes", json!(2))
        .await
        .unwrap();
    let failed = env.sync().await;
    assert!(
        matches!(&failed, Err(SyncError::Failed(m)) if m.contains("bytes")),
        "{failed:?}"
    );
    assert!(env.children(env.library).await.is_empty());
    assert_eq!(env.status().await.state, SourceState::Failed);
}

#[tokio::test]
async fn sync_ignores_the_site_quota() {
    let env = Env::new().await;
    let site = env
        .server
        .records_db()
        .node(env.library)
        .await
        .unwrap()
        .unwrap()
        .site;
    let (status, body) = call(
        &env.server,
        &env.admin,
        Method::PUT,
        &format!("/api/sites/{}/quota", site.0),
        Some(json!({ "bytes": 1 })),
    )
    .await;
    assert!(status.is_success(), "{status} {body}");
    env.fake.commit(
        "main",
        &files(&[
            ("big.md", "far more than one byte"),
            ("more.md", "and more"),
        ]),
    );
    let report = env.synced().await;
    assert_eq!(report.added, 2);
    let id = env.node_at("big.md").await;
    assert_eq!(
        download(&env.server, &env.admin, id).await.1,
        b"far more than one byte"
    );
}

#[tokio::test]
async fn blocked_file_types_are_hidden_and_counted() {
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
    env.fake.commit(
        "main",
        &files(&[("setup.exe", "MZ binary"), ("readme.md", "read me")]),
    );
    let report = env.synced().await;
    assert_eq!(report.added, 1);
    assert_eq!(report.hidden.policy, 1);
    let names: Vec<String> = env
        .children(env.library)
        .await
        .into_iter()
        .map(|(n, _, _)| n)
        .collect();
    assert_eq!(names, ["readme.md"]);
    assert_eq!(env.status().await.hidden, 1);
}

#[tokio::test]
async fn the_scan_gate_applies() {
    let env = Env::new().await;
    let scanner = fake_icap::FakeIcap::start(
        "127.0.0.1:0".parse().unwrap(),
        fake_icap::Behaviour::default(),
    )
    .unwrap();
    let (status, body) = call(
        &env.server,
        &env.admin,
        Method::PUT,
        "/api/settings/scanner",
        Some(json!({ "scanner_url": scanner.url() })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let mut infected = b"Quarterly report\n".to_vec();
    infected.extend_from_slice(&fake_icap::eicar());
    infected.extend_from_slice(b"\nend of report\n");
    env.fake.commit(
        "main",
        &[
            ("report.txt", infected.clone()),
            ("clean.md", b"nothing to see".to_vec()),
        ],
    );
    env.synced().await;
    let report = env.node_at("report.txt").await;
    let clean = env.node_at("clean.md").await;
    // Held until scanned, then the infected file stays withheld and the clean one is served.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let (status, bytes) = download(&env.server, &env.admin, clean).await;
        if status == StatusCode::OK {
            assert_eq!(bytes, b"nothing to see");
            break;
        }
        assert_eq!(status, StatusCode::LOCKED);
        assert!(tokio::time::Instant::now() < deadline, "never scanned");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let sha: [u8; 32] = <sha2::Sha256 as sha2::Digest>::digest(&infected).into();
    loop {
        let info = env.server.records_db().scan_state(&sha).await.unwrap();
        if info.is_some_and(|i| i.state == records::ScanState::Infected) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "never found infected"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        download(&env.server, &env.admin, report).await.0,
        StatusCode::LOCKED
    );
}

#[tokio::test]
async fn a_second_sync_at_the_same_time_returns_none() {
    let env = Env::new().await;
    env.fake.commit("main", &files(&[("slow.md", "slow")]));
    env.fake.hold_open_blob_bodies();
    let state = env.state.clone();
    let library = env.library;
    let first =
        tokio::spawn(async move { sync_once(&state, library, OffsetDateTime::now_utc()).await });
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while env.fake.count("blobs") == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the first sync never read"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(env.sync().await, Ok(None));
    assert_eq!(env.status().await.state, SourceState::Syncing);
    first.abort();
}

#[tokio::test]
async fn events_carry_counts_but_no_token() {
    const TOKEN: &str = "ghp_planted_token_1";
    let env = Env::connected(Some(TOKEN), "").await;
    let commit = env
        .fake
        .commit("main", &files(&[("a.md", "a"), ("b/c.md", "c")]));
    env.synced().await;
    assert!(
        env.fake
            .last_headers()
            .get("authorization")
            .is_some_and(|v| v.to_str().unwrap().ends_with(TOKEN))
    );
    let db = env.server.records_db();
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    let synced = loop {
        let events = db.events_of_kind("source.synced").await.unwrap();
        if let Some(event) = events.into_iter().next() {
            break event;
        }
        assert!(tokio::time::Instant::now() < deadline, "no source.synced");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(synced.detail["added"], 2);
    assert_eq!(synced.detail["commit"], &commit.sha[..7]);

    // A failure whose answer echoes the token.
    env.fake.commit("main", &files(&[("a.md", "a, edited")]));
    env.fake.fail_next(500);
    let failed = env.sync().await;
    assert!(matches!(failed, Err(SyncError::Failed(_))), "{failed:?}");
    let reason = loop {
        let events = db.events_of_kind("source.failed").await.unwrap();
        if let Some(event) = events.into_iter().next() {
            break event.detail["reason"].as_str().unwrap().to_owned();
        }
        assert!(tokio::time::Instant::now() < deadline, "no source.failed");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert!(!reason.contains(TOKEN));

    let mut conn = sqlx::PgConnection::connect(&env.server.db.url())
        .await
        .unwrap();
    let details: Vec<String> = sqlx::query_scalar("select detail::text from events")
        .fetch_all(&mut conn)
        .await
        .unwrap();
    assert!(!details.is_empty());
    for detail in details {
        assert!(!detail.contains(TOKEN), "{detail}");
    }
    let rows: Vec<String> =
        sqlx::query_scalar("select row_to_json(s)::text from library_sources s")
            .fetch_all(&mut conn)
            .await
            .unwrap();
    for row in rows {
        assert!(!row.contains(TOKEN), "{row}");
    }
    assert!(!format!("{failed:?}").contains(TOKEN));
}

#[tokio::test]
async fn case_collision_shows_a_suffixed_name_in_the_listing_and_counts_it() {
    let env = Env::new().await;
    env.fake
        .commit("main", &files(&[("a.md", "lower"), ("A.md", "upper")]));
    let report = env.synced().await;
    assert_eq!(report.added, 2);
    assert_eq!(report.hidden.renamed, 1);
    let mut names: Vec<String> = env
        .children(env.library)
        .await
        .into_iter()
        .map(|(n, _, _)| n)
        .collect();
    names.sort();
    assert_eq!(names, ["A.md", "a (2).md"]);
    let upper = env.node_at("A.md").await;
    let lower = env.node_at("a (2).md").await;
    assert_eq!(download(&env.server, &env.admin, upper).await.1, b"upper");
    assert_eq!(download(&env.server, &env.admin, lower).await.1, b"lower");
}

#[tokio::test]
async fn a_folder_of_the_repository_is_mirrored_alone() {
    let env = Env::connected(None, "docs").await;
    env.fake.commit(
        "main",
        &files(&[
            ("README.md", "outside"),
            ("docs/index.md", "inside"),
            ("docs/api/calls.md", "deeper"),
        ]),
    );
    let report = env.synced().await;
    assert_eq!(report.added, 2);
    assert_eq!(env.find("README.md").await, None);
    let id = env.node_at("api/calls.md").await;
    assert_eq!(download(&env.server, &env.admin, id).await.1, b"deeper");
}

#[tokio::test]
async fn a_sync_left_syncing_is_recovered_after_fifteen_minutes() {
    let env = Env::new().await;
    let db = env.server.records_db();
    let now = OffsetDateTime::now_utc();
    let mut status = env.status().await;
    status.state = SourceState::Syncing;
    status.last_checked_at = Some(now - time::Duration::minutes(5));
    db.set_source_status(env.library, &status).await.unwrap();
    // A sync only five minutes old may still be running.
    let before = now - server::source_sync::STALE_SYNC;
    assert_eq!(recover_stale_syncs(&env.state, before).await, 0);
    assert!(db.due_sources(now, 10).await.unwrap().is_empty());

    status.last_checked_at = Some(now - time::Duration::minutes(20));
    db.set_source_status(env.library, &status).await.unwrap();
    assert_eq!(recover_stale_syncs(&env.state, before).await, 1);
    assert_eq!(env.status().await.state, SourceState::Connecting);
    assert_eq!(db.due_sources(now, 10).await.unwrap(), [env.library]);

    // And a sync of a library left `syncing` runs: it holds the lock, so nothing else syncs it.
    status.last_checked_at = Some(now);
    db.set_source_status(env.library, &status).await.unwrap();
    env.fake.commit("main", &files(&[("a.md", "a")]));
    assert_eq!(env.synced().await.added, 1);
    assert_eq!(env.status().await.state, SourceState::Ok);
}

#[tokio::test]
async fn items_left_unrecorded_by_a_crash_are_adopted_not_duplicated() {
    let env = Env::new().await;
    env.fake
        .commit("main", &files(&[("folder/a.md", "a"), ("b.md", "b")]));
    env.synced().await;
    let a = env.node_at("folder/a.md").await;
    let folder = env.node_at("folder").await;
    let db = env.server.records_db();
    // As if the sync had stopped after making them but before recording them.
    db.remove_source_items(env.library, &[a, folder])
        .await
        .unwrap();
    let mut status = env.status().await;
    status.last_commit = None;
    status.etag = None;
    db.set_source_status(env.library, &status).await.unwrap();

    env.fake.commit(
        "main",
        &files(&[("folder/a.md", "a, edited"), ("b.md", "b")]),
    );
    env.synced().await;
    assert_eq!(env.node_at("folder/a.md").await, a);
    assert_eq!(env.node_at("folder").await, folder);
    assert_eq!(env.children(env.library).await.len(), 2);
    assert_eq!(download(&env.server, &env.admin, a).await.1, b"a, edited");
    let items = db.source_items(env.library).await.unwrap();
    assert_eq!(items.len(), 3);
}

/// Deletes `id` to the recycle bin as `env`'s administrator, outside the sync.
async fn delete_outside(env: &Env, id: NodeId) {
    let (status, body) = call(
        &env.server,
        &env.admin,
        Method::DELETE,
        &format!("/api/nodes/{}", id.0),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
}

#[tokio::test]
async fn a_file_moved_out_of_a_viewers_folder_ends_their_session() {
    let env = Env::new().await;
    env.fake.commit(
        "main",
        &files(&[("shared/sheet.xlsx", "cells"), ("private/keep.md", "kept")]),
    );
    env.synced().await;
    let shared = env.node_at("shared").await;
    let sheet = env.node_at("shared/sheet.xlsx").await;
    let bob = env.server.user("Bob").await;
    env.server
        .records_db()
        .set_node_access(
            env.admin.id,
            shared,
            Principal::User(bob.id),
            Some("viewer"),
        )
        .await
        .unwrap();
    let mut session = FileClient::connect_as(&env.server, DocId(sheet.0), &bob).await;

    env.fake.commit(
        "main",
        &files(&[("private/sheet.xlsx", "cells"), ("private/keep.md", "kept")]),
    );
    assert_eq!(env.synced().await.moved, 1);
    assert_eq!(env.node_at("private/sheet.xlsx").await, sheet);
    // Bob may not see where it went: his open session ends, as for a move made by hand.
    session
        .pump_until(|c| c.removed.is_some() || c.closed)
        .await;
    assert_eq!(
        download(&env.server, &bob, sheet).await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn a_folder_replaced_by_a_file_of_its_name_syncs() {
    let env = Env::new().await;
    env.fake.commit(
        "main",
        &files(&[("docs/a.md", "a"), ("docs/b.md", "b"), ("keep.md", "k")]),
    );
    env.synced().await;
    let a = env.node_at("docs/a.md").await;
    let folder = env.node_at("docs").await;

    env.fake
        .commit("main", &files(&[("docs", "now a file"), ("keep.md", "k")]));
    let report = env.synced().await;
    assert_eq!((report.added, report.removed), (1, 2));
    assert!(env.in_bin(a).await && env.in_bin(folder).await);
    let docs = env.node_at("docs").await;
    assert_ne!(docs, folder);
    assert_eq!(
        download(&env.server, &env.admin, docs).await.1,
        b"now a file"
    );
    assert_eq!(env.sync().await, Ok(None));
}

#[tokio::test]
async fn a_file_moved_onto_the_name_of_a_gone_folder_syncs() {
    let env = Env::new().await;
    env.fake.commit(
        "main",
        &files(&[("X/note.md", "the note"), ("keep.md", "k")]),
    );
    env.synced().await;
    let note = env.node_at("X/note.md").await;
    let folder = env.node_at("X").await;

    // The folder goes and its one file takes its name, in another case.
    env.fake
        .commit("main", &files(&[("x", "the note"), ("keep.md", "k")]));
    let report = env.synced().await;
    assert_eq!(report.moved, 1);
    assert!(env.in_bin(folder).await);
    assert_eq!(env.node_at("x").await, note);
    assert_eq!(download(&env.server, &env.admin, note).await.1, b"the note");
    assert_eq!(env.sync().await, Ok(None));
}

#[tokio::test]
async fn a_move_and_rename_does_not_collide_at_the_destination() {
    let env = Env::new().await;
    env.fake.commit(
        "main",
        &files(&[("a/x.md", "moving"), ("b/x.md", "staying")]),
    );
    env.synced().await;
    let moving = env.node_at("a/x.md").await;
    let staying = env.node_at("b/x.md").await;

    env.fake.commit(
        "main",
        &files(&[("b/y.md", "moving"), ("b/x.md", "staying")]),
    );
    assert_eq!(env.synced().await.moved, 1);
    assert_eq!(env.node_at("b/y.md").await, moving);
    assert_eq!(env.node_at("b/x.md").await, staying);
    assert_eq!(
        download(&env.server, &env.admin, staying).await.1,
        b"staying"
    );
    assert_eq!(env.find("a").await, None);
    assert_eq!(env.sync().await, Ok(None));
}

#[tokio::test]
async fn nodes_deleted_outside_the_sync_come_back() {
    let env = Env::new().await;
    env.fake.commit(
        "main",
        &files(&[
            ("loose.md", "loose"),
            ("dir/inner.md", "inner"),
            ("other.md", "o"),
        ]),
    );
    env.synced().await;
    delete_outside(&env, env.node_at("loose.md").await).await;
    delete_outside(&env, env.node_at("dir").await).await;

    env.fake.commit(
        "main",
        &files(&[
            ("loose.md", "loose"),
            ("dir/inner.md", "inner"),
            ("other.md", "o"),
            ("new.md", "new"),
        ]),
    );
    env.synced().await;
    let loose = env.node_at("loose.md").await;
    assert_eq!(download(&env.server, &env.admin, loose).await.1, b"loose");
    let inner = env.node_at("dir/inner.md").await;
    assert_eq!(download(&env.server, &env.admin, inner).await.1, b"inner");
    assert!(env.find("new.md").await.is_some());
    let items = env
        .server
        .records_db()
        .source_items(env.library)
        .await
        .unwrap();
    assert_eq!(items.len(), 5);
    assert_eq!(env.sync().await, Ok(None));
}

#[tokio::test]
async fn a_failed_fetch_leaves_source_items_untouched() {
    let env = Env::new().await;
    env.fake
        .commit("main", &files(&[("a.md", "a"), ("b.md", "b")]));
    env.synced().await;
    delete_outside(&env, env.node_at("a.md").await).await;
    let db = env.server.records_db();
    let before = db.source_items(env.library).await.unwrap();

    env.fake.commit(
        "main",
        &files(&[("a.md", "a, edited"), ("b.md", "b"), ("c.md", "c")]),
    );
    let c = git_blob_id(b"c");
    env.fake.corrupt_blob(&c);
    assert!(matches!(env.sync().await, Err(SyncError::Failed(_))));
    assert_eq!(db.source_items(env.library).await.unwrap(), before);

    env.fake.corrupt_blob(&c); // back as it was
    env.synced().await;
    let a = env.node_at("a.md").await;
    assert_eq!(download(&env.server, &env.admin, a).await.1, b"a, edited");
}

#[tokio::test]
async fn a_force_push_back_after_a_half_applied_sync_syncs_again() {
    let env = Env::new().await;
    let first = env.fake.commit("main", &files(&[("a.md", "one")]));
    env.synced().await;
    let a = env.node_at("a.md").await;
    // Something not from the repository holds a name the next commit needs.
    let (status, body) = call(
        &env.server,
        &env.admin,
        Method::POST,
        &format!("/api/nodes/{}/children", env.library.0),
        Some(json!({ "kind": "folder", "name": "new.md" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    env.fake
        .commit("main", &files(&[("a.md", "two"), ("new.md", "new")]));
    let failed = env.sync().await;
    assert!(matches!(failed, Err(SyncError::Failed(_))), "{failed:?}");
    // Applied as far as it went: the edit is in.
    assert_eq!(download(&env.server, &env.admin, a).await.1, b"two");

    env.fake.reset_branch("main", &first.sha);
    let report = env.synced().await;
    assert_eq!(report.changed, 1);
    assert_eq!(download(&env.server, &env.admin, a).await.1, b"one");
}

#[tokio::test]
async fn hidden_files_do_not_push_their_siblings_to_a_suffix() {
    let env = Env::new().await;
    let (status, body) = call(
        &env.server,
        &env.admin,
        Method::PATCH,
        &format!("/api/libraries/{}", env.library.0),
        Some(json!({ "max_file_bytes": 5 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    env.fake
        .commit("main", &files(&[("A.md", "far too long"), ("a.md", "ok")]));
    let report = env.synced().await;
    assert_eq!(report.hidden.too_big, 1);
    assert_eq!(report.hidden.renamed, 0);
    let names: Vec<String> = env
        .children(env.library)
        .await
        .into_iter()
        .map(|(n, _, _)| n)
        .collect();
    assert_eq!(names, ["a.md"]);
}
