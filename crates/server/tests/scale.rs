//! A library of 100,000 files: breaking and restoring inheritance, listing, the explorer, a
//! rename and the start-up reconcile, each timed against a threshold for a debug build.
//!
//! These tests are ignored by `cargo test`: they take minutes. Run them with `cargo xtask scale`,
//! which prints the times. The library is written by a bulk loader that exists only here: one
//! Git commit holding every record, and one database transaction inserting the rows.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use collab_core::ids::{DocId, NodeId};
use common::{TestClient, TestServer, TestUser, config_at, spawn_server};
use gix::object::tree::EntryKind;
use gix::refs::Target;
use gix::refs::transaction::PreviousValue;
use protocol::RemovedReason;
use records::Cursor;
use reqwest::header::{COOKIE, HeaderMap, HeaderValue, ORIGIN};
use reqwest::{Method, StatusCode};
use revisions::{Author, Stamp, TreeRecord};
use serde_json::{Value, json};
use uuid::Uuid;

/// Files in the large library's folder.
const FILES: usize = 100_000;
/// Files in the small library's folder, for comparison.
const SMALL_FILES: usize = 100;
/// When the loaded files were made, as their records and rows say.
const CREATED_AT: &str = "2026-11-12T09:00:00Z";

/// Thresholds, for a debug build.
const BREAK_OR_RESTORE: Duration = Duration::from_secs(1);
const LISTING_PAGE: Duration = Duration::from_millis(500);
const EXPLORER: Duration = Duration::from_secs(1);
const RENAME: Duration = Duration::from_secs(2);
const START: Duration = Duration::from_secs(60);

/// The times measured, printed as they are taken and checked against their thresholds at the
/// end, so that one miss does not hide the others.
#[derive(Default)]
struct Times {
    rows: Vec<(String, Duration, Option<Duration>)>,
}

impl Times {
    fn record(&mut self, what: impl Into<String>, took: Duration, limit: Option<Duration>) {
        let what = what.into();
        match limit {
            Some(limit) => println!(
                "scale: {what}: {} ms (threshold {} ms)",
                took.as_millis(),
                limit.as_millis()
            ),
            None => println!("scale: {what}: {} ms (no threshold)", took.as_millis()),
        }
        self.rows.push((what, took, limit));
    }

    /// Prints every time, then fails naming each threshold missed.
    fn check(&self) {
        println!("scale: --- measured ---");
        for (what, took, limit) in &self.rows {
            let verdict = match limit {
                Some(limit) if took > limit => {
                    format!("MISSED (threshold {} ms)", limit.as_millis())
                }
                Some(limit) => format!("ok (threshold {} ms)", limit.as_millis()),
                None => "no threshold".to_owned(),
            };
            println!("scale: {what:<55} {:>8} ms  {verdict}", took.as_millis());
        }
        let missed: Vec<String> = self
            .rows
            .iter()
            .filter(|(_, took, limit)| limit.is_some_and(|limit| *took > limit))
            .map(|(what, took, _)| format!("{what}: {} ms", took.as_millis()))
            .collect();
        assert!(missed.is_empty(), "thresholds missed: {missed:?}");
    }
}

/// A site with its library, a folder in it, and the files the loader put in the folder, named
/// `File 000001.txt` and on, in that order.
struct Library {
    site: NodeId,
    library: NodeId,
    folder: NodeId,
    members: String,
    files: Vec<NodeId>,
}

/// A server with the large library (and the small one, if asked for), the administrator Root
/// and Bob, a member of each site's Members group.
struct World {
    server: TestServer,
    root: TestUser,
    bob: TestUser,
    as_root: reqwest::Client,
    as_bob: reqwest::Client,
    big: Library,
    small: Option<Library>,
}

/// A client sending `user`'s cookie and the server's origin, which waits as long as a slow
/// answer takes, so that a time over its threshold is measured rather than cut off.
fn client(server: &TestServer, user: &TestUser) -> reqwest::Client {
    let mut headers = HeaderMap::new();
    headers.insert(ORIGIN, HeaderValue::from_str(&server.origin()).unwrap());
    headers.insert(COOKIE, HeaderValue::from_str(&user.cookie).unwrap());
    reqwest::Client::builder()
        .default_headers(headers)
        .timeout(Duration::from_secs(300))
        .build()
        .unwrap()
}

/// Sends one request and returns its status, JSON body (`null` when empty) and how long it
/// took to answer in full.
async fn call(
    client: &reqwest::Client,
    server: &TestServer,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value, Duration) {
    let started = Instant::now();
    let mut request = client.request(method, server.url(path));
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.unwrap();
    let status = response.status();
    let bytes = response.bytes().await.unwrap();
    let took = started.elapsed();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, body, took)
}

fn node(value: &Value) -> NodeId {
    NodeId(Uuid::parse_str(value.as_str().unwrap()).unwrap())
}

impl World {
    async fn new(small: bool, times: &mut Times) -> World {
        let server = spawn_server().await;
        let root = server.admin("Root").await;
        let bob = server.user("Bob").await;
        let as_root = client(&server, &root);
        let as_bob = client(&server, &bob);
        let mut world = World {
            server,
            root,
            bob,
            as_root,
            as_bob,
            big: Library::empty(),
            small: None,
        };
        world.big = world.library("Big", "Bulk").await;
        let mut batches = vec![(world.big.site, world.big.folder, FILES)];
        if small {
            let library = world.library("Small", "Few").await;
            batches.push((library.site, library.folder, SMALL_FILES));
            world.small = Some(library);
        }
        let started = Instant::now();
        let mut loaded = load(&world.server, &world.root, &batches).await;
        let count: usize = batches.iter().map(|b| b.2).sum();
        times.record(
            format!("bulk load ({count} files, one commit, one transaction)"),
            started.elapsed(),
            None,
        );
        world.big.files = loaded.remove(0);
        if let Some(library) = world.small.as_mut() {
            library.files = loaded.remove(0);
        }
        world
    }

    async fn call(
        &self,
        as_bob: bool,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value, Duration) {
        let client = if as_bob { &self.as_bob } else { &self.as_root };
        call(client, &self.server, method, path, body).await
    }

    /// Makes a site named `site` as Root, with a folder `folder` in its library, and adds Bob
    /// to its Members group.
    async fn library(&self, site: &str, folder: &str) -> Library {
        let (status, body, _) = self
            .call(
                false,
                Method::POST,
                "/api/sites",
                Some(json!({ "name": site })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let site = node(&body["id"]);
        let (_, page, _) = self
            .call(
                false,
                Method::GET,
                &format!("/api/nodes/{}/children", site.0),
                None,
            )
            .await;
        let library = node(&page["items"][0]["id"]);
        let (status, body, _) = self
            .call(
                false,
                Method::POST,
                &format!("/api/nodes/{}/children", library.0),
                Some(json!({ "kind": "folder", "name": folder })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let folder = node(&body["id"]);
        let (_, groups, _) = self
            .call(
                false,
                Method::GET,
                &format!("/api/sites/{}/groups", site.0),
                None,
            )
            .await;
        let members = groups
            .as_array()
            .unwrap()
            .iter()
            .find(|g| g["purpose"] == "members")
            .expect("a site has a Members group")["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let (status, body, _) = self
            .call(
                false,
                Method::POST,
                &format!("/api/groups/{members}/members"),
                Some(json!({ "email": self.bob.email })),
            )
            .await;
        assert!(status.is_success(), "{status} {body}");
        Library {
            site,
            library,
            folder,
            members,
            files: Vec::new(),
        }
    }

    /// Times making `node` a scope of its own, then making it inherit again.
    async fn break_and_restore(&self, times: &mut Times, what: &str, node: NodeId) {
        let path = format!("/api/nodes/{}/inheritance", node.0);
        for (inherit, verb) in [(false, "break"), (true, "restore")] {
            let (status, body, took) = self
                .call(
                    false,
                    Method::POST,
                    &path,
                    Some(json!({ "inherit": inherit })),
                )
                .await;
            assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
            times.record(format!("{verb} {what}"), took, Some(BREAK_OR_RESTORE));
        }
    }

    async fn count(&self, query: &str) -> i64 {
        sqlx::query_scalar(sqlx::AssertSqlSafe(query.to_owned()))
            .fetch_one(self.server.db.db.test_pool())
            .await
            .unwrap()
    }

    /// The number of commits on `main`.
    fn commits(&self) -> usize {
        let repo = gix::open(self.server.library()).unwrap();
        let tip = repo.find_reference("refs/heads/main").unwrap().id();
        repo.rev_walk([tip]).all().unwrap().count()
    }
}

impl Library {
    fn empty() -> Library {
        Library {
            site: NodeId(Uuid::nil()),
            library: NodeId(Uuid::nil()),
            folder: NodeId(Uuid::nil()),
            members: String::new(),
            files: Vec::new(),
        }
    }
}

/// The name of the `n`th loaded file, counting from 1.
fn file_name(n: usize) -> String {
    format!("File {n:06}.txt")
}

/// The bulk loader, for these tests only: puts `count` text files, named by [`file_name`], into
/// each `(site, folder, count)` of `batches`, created by `by`: their records in one Git commit
/// on `main` (the tree's sub-folders written directly), then their nodes and documents in one
/// database transaction, inserted from arrays with `unnest`, and the tables analysed as
/// PostgreSQL's autovacuum would soon do. Returns the files of each batch, in name order.
async fn load(
    server: &TestServer,
    by: &TestUser,
    batches: &[(NodeId, NodeId, usize)],
) -> Vec<Vec<NodeId>> {
    let author = Author {
        id: by.id.0.to_string(),
        name: "Root".into(),
    };
    let mut records = Vec::new();
    let mut files = Vec::new();
    for &(site, folder, count) in batches {
        let mut ids = Vec::with_capacity(count);
        for n in 1..=count {
            let id = Uuid::new_v4();
            ids.push(NodeId(id));
            records.push(TreeRecord {
                schema: 1,
                id,
                kind: "document".into(),
                parent: Some(folder.0),
                site: site.0,
                name: file_name(n),
                created: Stamp {
                    at: CREATED_AT.into(),
                    by: author.clone(),
                },
                deleted: None,
                bin_stage: None,
                deleted_with: None,
                purged: None,
            });
        }
        files.push(ids);
    }

    let library = server.library();
    let written = records.clone();
    let commit_author = author.clone();
    tokio::task::spawn_blocking(move || commit_records(&library, &written, &commit_author))
        .await
        .unwrap();

    let ids: Vec<Uuid> = records.iter().map(|r| r.id).collect();
    let parents: Vec<Uuid> = records.iter().map(|r| r.parent.unwrap()).collect();
    let sites: Vec<Uuid> = records.iter().map(|r| r.site).collect();
    let names: Vec<String> = records.iter().map(|r| r.name.clone()).collect();
    let pool = server.db.db.test_pool();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query(
        "insert into nodes (id, kind, parent_id, site_id, name, created_by, created_at) \
         select t.id, 'document', t.parent, t.site, t.name, $5, $6::timestamptz \
         from unnest($1::uuid[], $2::uuid[], $3::uuid[], $4::text[]) as t(id, parent, site, name)",
    )
    .bind(&ids)
    .bind(&parents)
    .bind(&sites)
    .bind(&names)
    .bind(by.id.0)
    .bind(CREATED_AT)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "insert into documents (id, name, created_by, created_at, editor) \
         select t.id, t.name, $3, $4::timestamptz, 'text' \
         from unnest($1::uuid[], $2::text[]) as t(id, name)",
    )
    .bind(&ids)
    .bind(&names)
    .bind(by.id.0)
    .bind(CREATED_AT)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    sqlx::query("analyze nodes, documents")
        .execute(pool)
        .await
        .unwrap();
    files
}

/// Writes `records` into the library repository at `library` as one commit by `author` on
/// `main`, which `refs/sharecraft/tree` follows: the blobs from several threads, then each
/// sub-folder of `tree/` and `tree/` itself as whole tree objects.
fn commit_records(library: &Path, records: &[TreeRecord], author: &Author) {
    let open = || gix::open_opts(library, gix::open::Options::isolated()).unwrap();
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get().min(16));
    let chunk = records.len().div_ceil(threads).max(1);
    let blobs: Vec<(Uuid, gix::ObjectId)> = std::thread::scope(|scope| {
        let workers: Vec<_> = records
            .chunks(chunk)
            .map(|part| {
                scope.spawn(move || {
                    let repo = open();
                    part.iter()
                        .map(|record| {
                            let mut bytes = serde_json::to_vec_pretty(record).unwrap();
                            bytes.push(b'\n');
                            (record.id, repo.write_blob(&bytes).unwrap().detach())
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|worker| worker.join().unwrap())
            .collect()
    });

    let repo = open();
    let main = repo
        .find_reference("refs/heads/main")
        .unwrap()
        .id()
        .detach();
    let tree_ref = repo
        .find_reference("refs/sharecraft/tree")
        .unwrap()
        .id()
        .detach();
    let root = repo.find_commit(main).unwrap().tree_id().unwrap().detach();
    let dir = repo
        .find_tree(root)
        .unwrap()
        .lookup_entry_by_path("tree")
        .unwrap()
        .expect("the tree has records")
        .object_id();
    let dir: gix::objs::Tree = repo.find_tree(dir).unwrap().decode().unwrap().into();
    let mut shards: BTreeMap<String, gix::objs::Tree> = BTreeMap::new();
    let mut entries = Vec::new();
    for entry in dir.entries {
        if entry.mode.is_tree() && entry.filename.len() == 2 {
            let sub = repo.find_tree(entry.oid).unwrap().decode().unwrap().into();
            shards.insert(entry.filename.to_string(), sub);
        } else {
            entries.push(entry);
        }
    }
    for (id, oid) in blobs {
        let shard = id.simple().to_string()[..2].to_owned();
        shards
            .entry(shard)
            .or_default()
            .entries
            .push(gix::objs::tree::Entry {
                mode: EntryKind::Blob.into(),
                filename: format!("{id}.json").into(),
                oid,
            });
    }
    for (name, mut sub) in shards {
        sub.entries.sort();
        let oid = repo.write_object(&sub).unwrap().detach();
        entries.push(gix::objs::tree::Entry {
            mode: EntryKind::Tree.into(),
            filename: name.into(),
            oid,
        });
    }
    entries.sort();
    let dir = repo
        .write_object(&gix::objs::Tree { entries })
        .unwrap()
        .detach();
    let mut editor = repo.edit_tree(root).unwrap();
    editor.upsert("tree", EntryKind::Tree, dir).unwrap();
    let tree = editor.write().unwrap().detach();
    let signature = gix::actor::Signature {
        name: author.name.as_str().into(),
        email: format!("{}@sharecraft.invalid", author.id).into(),
        time: gix::date::Time::now_local_or_utc(),
    };
    let commit = gix::objs::Commit {
        tree,
        parents: std::iter::once(main).collect(),
        author: signature.clone(),
        committer: signature,
        encoding: None,
        message: format!("Load {} files\n", records.len()).into(),
        extra_headers: Vec::new(),
    };
    let oid = repo.write_object(&commit).unwrap().detach();
    for (name, expected) in [
        ("refs/heads/main", main),
        ("refs/sharecraft/tree", tree_ref),
    ] {
        repo.reference(
            name,
            oid,
            PreviousValue::MustExistAndMatch(Target::Object(expected)),
            "bulk load",
        )
        .unwrap();
    }
}

/// The `after` of a children page that continues after `cursor`, as the API writes it.
fn after(cursor: &Cursor) -> String {
    serde_json::to_vec(cursor)
        .unwrap()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "takes minutes; run with `cargo xtask scale`"]
async fn library_of_100000_files() {
    let mut times = Times::default();
    let w = World::new(true, &mut times).await;
    let small = w.small.as_ref().unwrap();

    // The folder's listing, as Bob, a member: the first page and one from the middle.
    let path = format!("/api/nodes/{}/children?limit=100", w.big.folder.0);
    let (status, page, took) = w.call(true, Method::GET, &path, None).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["items"].as_array().unwrap().len(), 100);
    assert_eq!(page["items"][0]["name"], file_name(1));
    times.record(
        "listing: first page (100 of 100,000)",
        took,
        Some(LISTING_PAGE),
    );
    let middle = FILES / 2;
    let cursor = Cursor {
        kind_rank: 1,
        name: file_name(middle),
        id: w.big.files[middle - 1],
    };
    let path = format!("{path}&after={}", after(&cursor));
    let (status, page, took) = w.call(true, Method::GET, &path, None).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["items"][0]["name"], file_name(middle + 1));
    assert_eq!(page["items"].as_array().unwrap().len(), 100);
    times.record(
        "listing: middle page (after 50,000)",
        took,
        Some(LISTING_PAGE),
    );

    // Break and restore, at every level, in both libraries.
    for (label, library) in [("large", &w.big), ("small", small)] {
        let file = library.files[library.files.len() / 2];
        w.break_and_restore(&mut times, &format!("{label} library"), library.library)
            .await;
        w.break_and_restore(
            &mut times,
            &format!("{label} library's folder"),
            library.folder,
        )
        .await;
        w.break_and_restore(&mut times, &format!("one file in the {label} folder"), file)
            .await;
    }

    // The explorer and its counts with 50 breaks among the 100,000 files.
    let db = w.server.records_db();
    for file in w.big.files.iter().step_by(FILES / 50) {
        assert!(db.break_inheritance(w.root.id, *file).await.unwrap());
    }
    let (status, page, took) = w
        .call(
            false,
            Method::GET,
            &format!("/api/explorer/{}?limit=100", w.big.library.0),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["items"].as_array().unwrap().len(), 50);
    times.record("explorer: page of 50 breaks", took, Some(EXPLORER));
    let (status, counts, took) = w
        .call(
            false,
            Method::GET,
            &format!("/api/explorer/{}/counts", w.big.library.0),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{counts}");
    assert!(counts.to_string().contains("50"), "{counts}");
    times.record("explorer: counts with 50 breaks", took, Some(EXPLORER));

    // A rename in the folder: one Git commit.
    let commits = w.commits();
    let file = w.big.files[FILES / 3];
    let (status, body, took) = w
        .call(
            false,
            Method::PATCH,
            &format!("/api/nodes/{}", file.0),
            Some(json!({ "name": "Renamed.txt" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["name"], "Renamed.txt");
    assert_eq!(w.commits(), commits + 1, "one commit");
    times.record("rename a file in the folder", took, Some(RENAME));

    // The server's start-up: moving the tree records (none to move), the reconcile of
    // 100,000 records with PostgreSQL, the adoption check and the revisions.
    let started = Instant::now();
    server::prepare(config_at(w.server.data_dir.path(), &w.server.db))
        .await
        .unwrap();
    times.record(
        "server start-up (tree reconcile of 100,000+ nodes)",
        started.elapsed(),
        Some(START),
    );

    times.check();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "takes minutes; run with `cargo xtask scale`"]
async fn large_library_break_while_open() {
    let mut times = Times::default();
    let w = World::new(false, &mut times).await;
    let library = &w.big;
    let doc = DocId(library.files[FILES / 2].0);
    let assignments = "select count(*) from assignments";
    let scopes = "select count(*) from nodes where unique_scope";

    // Bob, a member of the site's Members group, edits a file in the folder.
    let mut bob = TestClient::connect_as(&w.server, doc, &w.bob).await;
    assert!(bob.can_edit);
    bob.type_at(0, "before ").await;
    bob.pump_until(|c| c.pending() == 0).await;
    let (assigned, scoped) = (w.count(assignments).await, w.count(scopes).await);

    // The library's inheritance is broken while he types: he keeps editing throughout, with
    // the copy of the site's assignments the library now has.
    let client = w.as_root.clone();
    let url = w
        .server
        .url(&format!("/api/nodes/{}/inheritance", library.library.0));
    let breaking = tokio::spawn(async move {
        let started = Instant::now();
        let response = client
            .post(url)
            .json(&json!({ "inherit": false }))
            .send()
            .await
            .unwrap();
        (response.status(), started.elapsed())
    });
    let end = bob.text().chars().count();
    bob.type_at(end, "during ").await;
    bob.pump_until(|c| c.pending() == 0).await;
    let (status, took) = breaking.await.unwrap();
    assert_eq!(status, StatusCode::NO_CONTENT);
    times.record(
        "break the library with a file open",
        took,
        Some(BREAK_OR_RESTORE),
    );
    let end = bob.text().chars().count();
    bob.type_at(end, "after").await;
    bob.pump_until(|c| c.pending() == 0).await;
    assert!(bob.can_edit && bob.removed.is_none());
    assert_eq!(bob.text(), "before during after");

    // Nothing was done per file: the library became the one new scope, holding a copy of the
    // site's three assignments.
    assert_eq!(w.count(assignments).await, assigned + 3);
    assert_eq!(w.count(scopes).await, scoped + 1);

    // Taking the Members group off the library ends Bob's access, and his session, at once.
    let (status, body, _) = w
        .call(
            false,
            Method::DELETE,
            &format!(
                "/api/nodes/{}/access/group/{}",
                library.library.0, library.members
            ),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    tokio::time::timeout(
        Duration::from_secs(1),
        bob.pump_until(|c| c.removed.is_some()),
    )
    .await
    .expect("not removed within one second");
    assert_eq!(bob.removed, Some(RemovedReason::AccessRevoked));
    let (status, _, _) = w
        .call(true, Method::GET, &format!("/api/nodes/{}", doc.0), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Restoring inheritance gives him back the site's access to every file in the library.
    let (status, body, took) = w
        .call(
            false,
            Method::POST,
            &format!("/api/nodes/{}/inheritance", library.library.0),
            Some(json!({ "inherit": true })),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    times.record(
        "restore the library with a file open",
        took,
        Some(BREAK_OR_RESTORE),
    );
    assert_eq!(w.count(assignments).await, assigned);
    assert_eq!(w.count(scopes).await, scoped);
    let mut again = TestClient::connect_as(&w.server, doc, &w.bob).await;
    assert!(again.can_edit);
    assert_eq!(again.text(), "before during after");
    again.type_at(0, "back ").await;
    again.pump_until(|c| c.pending() == 0).await;
    let (status, shown, _) = w
        .call(true, Method::GET, &format!("/api/nodes/{}", doc.0), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{shown}");

    times.check();
}
