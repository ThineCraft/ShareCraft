//! A connected library through its site's lifecycle and the purges that remove its items: a
//! site deleted and restored keeps its library connected as it was; a purged site leaves no
//! connection, no item records and no sealed secrets behind; a mirrored file purged from the
//! recycle bin or by the malware quarantine is purged, its item record with it, and the next
//! sync carries on.

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

use collab_core::ids::NodeId;
use common::fake_github::FakeGithub;
use common::{TestServer, TestUser, spawn_server_config, start_fake_github};
use fake_icap::{Behaviour, FakeIcap, eicar};
use records::{NewSource, SecretId, SourceChange, SourceKind, SourceMode, SourceState};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use server::AppState;
use server::source_sync::{sync_once, token_context};
use time::OffsetDateTime;
use uuid::Uuid;

const OWNER: &str = "octo-org";
const REPO: &str = "docs";
const TOKEN: &str = "ghp_lifecycle_test_token_0123456789";

/// A server with a library, in a site of its own, connected with a sealed token and a sealed
/// webhook secret to the fake's private `octo-org/docs`, synced once.
struct Env {
    server: TestServer,
    fake: FakeGithub,
    state: AppState,
    admin: TestUser,
    site: String,
    library: NodeId,
    token: SecretId,
    webhook: SecretId,
    /// Kept running for the server's scanner, when there is one.
    _scanner: Option<FakeIcap>,
}

async fn answer(response: reqwest::Response) -> (StatusCode, Value) {
    let status = response.status();
    let text = response.text().await.unwrap();
    let body = if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).unwrap_or(Value::String(text))
    };
    (status, body)
}

impl Env {
    async fn new(files: &[(&str, Vec<u8>)], scanning: bool) -> Env {
        let fake = start_fake_github(OWNER, REPO).await;
        fake.require_token(TOKEN);
        let origin = fake.origin();
        let server = spawn_server_config(|c| c.github_api = origin).await;
        let admin = server.admin("Ada Admin").await;
        let state = server.app_state().await;
        let mut env = Env {
            server,
            fake,
            state,
            admin,
            site: String::new(),
            library: NodeId(Uuid::nil()),
            token: SecretId(Uuid::nil()),
            webhook: SecretId(Uuid::nil()),
            _scanner: None,
        };
        if scanning {
            let scanner =
                FakeIcap::start("127.0.0.1:0".parse().unwrap(), Behaviour::default()).unwrap();
            let (status, body) = env
                .call(
                    Method::PUT,
                    "/api/settings/scanner",
                    Some(json!({ "scanner_url": scanner.url() })),
                )
                .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            env._scanner = Some(scanner);
        }
        let (status, body) = env
            .call(
                Method::POST,
                "/api/sites",
                Some(json!({ "name": format!("Mirror {}", Uuid::new_v4().simple()) })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        env.site = body["id"].as_str().unwrap().to_owned();
        let (status, body) = env
            .call(
                Method::POST,
                &format!("/api/sites/{}/libraries", env.site),
                Some(json!({ "name": "Repository", "kind": "document" })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        env.library = NodeId(body["id"].as_str().unwrap().parse().unwrap());

        let db = env.server.records_db();
        env.token = env
            .state
            .secret_store()
            .put(&db, TOKEN.as_bytes(), &token_context(env.library))
            .await
            .unwrap();
        env.webhook = env
            .state
            .secret_store()
            .put(&db, b"webhook secret", b"webhook")
            .await
            .unwrap();
        db.connect_source(&NewSource {
            library: env.library,
            kind: SourceKind::Github,
            owner: OWNER.to_owned(),
            repo: REPO.to_owned(),
            branch: "main".to_owned(),
            folder: String::new(),
            location: None,
            mode: SourceMode::ReadOnly,
            token: Some(env.token),
            poll_seconds: 300,
            connected_by: env.admin.id,
        })
        .await
        .unwrap();
        db.change_source(
            env.library,
            &SourceChange {
                branch: None,
                folder: None,
                poll_seconds: None,
                token: None,
                webhook_secret: Some(Some(env.webhook)),
            },
        )
        .await
        .unwrap();
        env.fake.commit("main", files);
        env.sync().await;
        env
    }

    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut request = self
            .server
            .http(Some(&self.admin))
            .request(method, self.server.url(path));
        if let Some(body) = body {
            request = request.json(&body);
        }
        answer(request.send().await.unwrap()).await
    }

    async fn sync(&self) {
        sync_once(&self.state, self.library, OffsetDateTime::now_utc())
            .await
            .unwrap()
            .expect("the library changed");
    }

    /// The node recorded for the repository path `path`.
    async fn item(&self, path: &str) -> NodeId {
        self.server
            .records_db()
            .source_items(self.library)
            .await
            .unwrap()
            .into_iter()
            .find(|item| item.path == path)
            .unwrap_or_else(|| panic!("{path} is not recorded"))
            .node
    }
}

fn files(list: &[(&str, &str)]) -> Vec<(String, Vec<u8>)> {
    list.iter()
        .map(|(p, t)| ((*p).to_owned(), t.as_bytes().to_vec()))
        .collect()
}

fn borrowed(files: &[(String, Vec<u8>)]) -> Vec<(&str, Vec<u8>)> {
    files.iter().map(|(p, b)| (p.as_str(), b.clone())).collect()
}

/// A site deleted and restored keeps its library connected as it was; a purged site leaves
/// nothing of the connection: no `library_sources` row, no `source_items` rows and neither
/// sealed secret.
#[tokio::test]
async fn a_site_with_a_connected_library_is_deleted_restored_and_purged() {
    let tree = files(&[("README.md", "# Docs\n"), ("guide/intro.md", "Hello.\n")]);
    let env = Env::new(&borrowed(&tree), false).await;
    let db = env.server.records_db();
    let (config, status) = db.source_of(env.library).await.unwrap().unwrap();
    assert_eq!(status.state, SourceState::Ok);
    let items = db.source_items(env.library).await.unwrap();
    assert_eq!(items.len(), 3, "{items:?}");

    let (code, body) = env
        .call(Method::DELETE, &format!("/api/sites/{}", env.site), None)
        .await;
    assert_eq!(code, StatusCode::NO_CONTENT, "{body}");
    let (code, body) = env
        .call(
            Method::POST,
            &format!("/api/admin/sites/{}/restore", env.site),
            None,
        )
        .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let (config_after, status_after) = db.source_of(env.library).await.unwrap().unwrap();
    assert_eq!(config_after, config);
    assert_eq!(status_after.state, status.state);
    assert_eq!(status_after.last_commit, status.last_commit);
    assert_eq!(db.source_items(env.library).await.unwrap(), items);

    let (code, body) = env
        .call(Method::DELETE, &format!("/api/sites/{}", env.site), None)
        .await;
    assert_eq!(code, StatusCode::NO_CONTENT, "{body}");
    let (code, body) = env
        .call(
            Method::DELETE,
            &format!("/api/admin/sites/{}", env.site),
            None,
        )
        .await;
    assert_eq!(code, StatusCode::NO_CONTENT, "{body}");
    assert!(db.source_of(env.library).await.unwrap().is_none());
    assert!(db.source_items(env.library).await.unwrap().is_empty());
    assert!(db.source_library_of(env.library).await.unwrap().is_none());
    for secret in [env.token, env.webhook] {
        assert!(
            db.get_secret(secret).await.unwrap().is_none(),
            "a sealed secret outlived its purged library"
        );
    }
}

/// A mirrored file the branch dropped goes to the recycle bin; purging it there works. A
/// mirrored file whose content is infected is purged by the quarantine, its item record with
/// it, and the library keeps syncing afterwards.
#[tokio::test]
async fn mirrored_files_are_purged_from_the_bin_and_by_the_quarantine() {
    let mut bad = b"Quarterly report\n".to_vec();
    bad.extend_from_slice(&eicar());
    bad.extend_from_slice(b"\nend\n");
    let tree = vec![
        ("keep.md".to_owned(), b"kept\n".to_vec()),
        ("dropped.md".to_owned(), b"dropped\n".to_vec()),
        ("infected.bin".to_owned(), bad.clone()),
    ];
    let env = Env::new(&borrowed(&tree), true).await;
    let db = env.server.records_db();

    // The branch drops a file: it goes to the bin, and is purged from there in two steps.
    let dropped = env.item("dropped.md").await;
    let kept: Vec<(String, Vec<u8>)> = tree
        .iter()
        .filter(|(p, _)| p != "dropped.md")
        .cloned()
        .collect();
    env.fake.commit("main", &borrowed(&kept));
    env.sync().await;
    for _ in 0..2 {
        let (code, body) = env
            .call(Method::DELETE, &format!("/api/bin/{}", dropped.0), None)
            .await;
        assert_eq!(code, StatusCode::NO_CONTENT, "{body}");
    }
    let node = db.node(dropped).await.unwrap().unwrap();
    assert!(node.deleted);

    // The quarantine purges the infected mirrored file.
    let infected = env.item("infected.bin").await;
    let sha: [u8; 32] = revisions::BlobRef::of(&bad).sha256;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let info = db.scan_state(&sha).await.unwrap();
        if info.is_some_and(|i| i.state == records::ScanState::Infected) {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "never scanned");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let hash: String = sha.iter().map(|b| format!("{b:02x}")).collect();
    let (code, body) = env
        .call(
            Method::POST,
            &format!("/api/admin/quarantine/{hash}/delete"),
            None,
        )
        .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body["documents"], 1, "{body}");
    assert!(db.node(infected).await.unwrap().unwrap().deleted);
    let items = db.source_items(env.library).await.unwrap();
    assert!(
        items.iter().all(|item| item.node != infected),
        "the purged file is still recorded: {items:?}"
    );

    // The next change on the branch syncs, and the infected path comes back as a new file.
    let mut changed = kept.clone();
    changed.push(("new.md".to_owned(), b"new\n".to_vec()));
    env.fake.commit("main", &borrowed(&changed));
    env.sync().await;
    let again = env.item("infected.bin").await;
    assert_ne!(again, infected);
    assert!(!db.node(again).await.unwrap().unwrap().deleted);
    env.item("new.md").await;
}
