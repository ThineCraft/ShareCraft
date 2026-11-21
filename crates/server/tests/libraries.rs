//! Library records over HTTP: each library's kind and settings, recorded in Git beside its tree
//! record (in the same commit when the library is made) and mirrored into PostgreSQL; a library
//! from before library records reads as the defaults until its first change; changing the
//! settings needs `manage_site`, and the asset kind an administrator; the rules the settings
//! keep; libraries with folders off; site templates that carry each library's kind and
//! settings; the asset libraries; archived and deleted sites; and the reconcile at start that
//! rebuilds the mirror from Git.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use collab_core::ids::NodeId;
use common::{Site, StoppableServer, TestServer, TestUser, spawn_server};
use records::test_support::TestDb;
use reqwest::{Method, StatusCode};
use revisions::{Author, LibraryRecord, RevisionStore, SitesChange};
use serde_json::{Value, json};
use uuid::Uuid;

async fn call(
    site: &Site,
    user: &TestUser,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = site.http(Some(user)).request(method, site.url(path));
    if let Some(body) = body {
        request = request.json(&body);
    }
    answer(request.send().await.unwrap()).await
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

/// Creates the site `name` as `admin` (from `template` when given); its id.
async fn create_site(site: &Site, admin: &TestUser, name: &str) -> String {
    let (status, body) = call(
        site,
        admin,
        Method::POST,
        "/api/sites",
        Some(json!({ "name": name })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["id"].as_str().unwrap().to_owned()
}

/// The id of the first library of the site `site`.
async fn first_library(site: &Site, admin: &TestUser, id: &str) -> String {
    let (status, page) = call(
        site,
        admin,
        Method::GET,
        &format!("/api/nodes/{id}/children"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    page["items"][0]["id"].as_str().unwrap().to_owned()
}

/// Makes a library of `kind` named `name` in the site `site` as `user`; the status and answer.
async fn new_library(
    server: &Site,
    user: &TestUser,
    site: &str,
    name: &str,
    kind: Option<&str>,
) -> (StatusCode, Value) {
    let mut body = json!({ "name": name });
    if let Some(kind) = kind {
        body["kind"] = json!(kind);
    }
    call(
        server,
        user,
        Method::POST,
        &format!("/api/sites/{site}/libraries"),
        Some(body),
    )
    .await
}

/// Makes a folder or library named `name` in `parent` as `user`; the status and answer.
async fn child(
    server: &Site,
    user: &TestUser,
    parent: &str,
    kind: &str,
    name: &str,
) -> (StatusCode, Value) {
    call(
        server,
        user,
        Method::POST,
        &format!("/api/nodes/{parent}/children"),
        Some(json!({ "kind": kind, "name": name })),
    )
    .await
}

/// Changes the library's settings as `user`; the status and answer.
async fn patch(server: &Site, user: &TestUser, id: &str, body: Value) -> (StatusCode, Value) {
    call(
        server,
        user,
        Method::PATCH,
        &format!("/api/libraries/{id}"),
        Some(body),
    )
    .await
}

/// The library as `GET /api/libraries/{id}` answers it for `user`.
async fn show(server: &Site, user: &TestUser, id: &str) -> (StatusCode, Value) {
    call(
        server,
        user,
        Method::GET,
        &format!("/api/libraries/{id}"),
        None,
    )
    .await
}

/// Gives `user` the role `role` on `node`, as `admin`.
async fn grant(server: &Site, admin: &TestUser, node: &str, user: &TestUser, role: &str) {
    let (status, body) = call(
        server,
        admin,
        Method::PUT,
        &format!("/api/nodes/{node}/access"),
        Some(json!({ "email": user.email, "role": role })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
}

fn node(id: &str) -> NodeId {
    NodeId(Uuid::parse_str(id).unwrap())
}

fn shard(id: &str) -> String {
    id.chars().take(2).collect()
}

/// The id `main`, `refs/sharecraft/tree` or `refs/sharecraft/sites` points at, if it exists.
fn ref_of(server: &TestServer, name: &str) -> Option<gix::ObjectId> {
    let repo = gix::open(server.library()).unwrap();
    repo.try_find_reference(name)
        .unwrap()
        .and_then(|r| r.try_id().map(gix::Id::detach))
}

/// Whether the commit `commit` holds a blob at `path`.
fn holds(repo: &gix::Repository, commit: gix::ObjectId, path: &str) -> bool {
    let tree = repo.find_commit(commit).unwrap().tree().unwrap();
    tree.lookup_entry_by_path(path).unwrap().is_some()
}

/// The details of the events of `kind`, oldest first.
async fn events(server: &Site, kind: &str) -> Vec<Value> {
    sqlx::query_scalar("select detail from events where kind = $1 order by id")
        .bind(kind)
        .fetch_all(server.records_db().test_pool())
        .await
        .unwrap()
}

fn author() -> Author {
    Author {
        id: "test".into(),
        name: "Test".into(),
    }
}

#[tokio::test]
async fn new_library_writes_both_records_in_one_commit() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let site = create_site(&server, &root, "Team").await;

    // The site's own library got its record with the site.
    let documents = first_library(&server, &root, &site).await;
    assert_eq!(
        server
            .store()
            .read_library_record(node(&documents).0)
            .unwrap(),
        Some(LibraryRecord::defaults(node(&documents).0))
    );

    let (status, body) = new_library(&server, &root, &site, "Pictures", Some("image")).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["id"].as_str().unwrap().to_owned();
    assert_eq!(body["kind"], "image");
    assert_eq!(body["name"], "Pictures");
    assert_eq!(body["site"], site);
    assert_eq!(body["folders"], true);
    assert_eq!(body["allowed_types"], Value::Null);
    assert_eq!(
        body["effective_allowed_types"],
        json!([
            ".png", ".jpg", ".jpeg", ".gif", ".webp", ".svg", ".avif", ".bmp", ".tif", ".tiff",
            ".heic"
        ])
    );

    // One commit holds both records and moves all three refs.
    let tip = ref_of(&server, "refs/heads/main").unwrap();
    assert_eq!(ref_of(&server, "refs/sharecraft/tree"), Some(tip));
    assert_eq!(ref_of(&server, "refs/sharecraft/sites"), Some(tip));
    let repo = gix::open(server.library()).unwrap();
    let tree_path = format!("tree/{}/{id}.json", shard(&id));
    let library_path = format!("libraries/{}/{id}.json", shard(&id));
    assert!(holds(&repo, tip, &tree_path));
    assert!(holds(&repo, tip, &library_path));
    let parent = repo
        .find_commit(tip)
        .unwrap()
        .parent_ids()
        .next()
        .unwrap()
        .detach();
    assert!(!holds(&repo, parent, &tree_path));
    assert!(!holds(&repo, parent, &library_path));
    let record = server
        .store()
        .read_library_record(node(&id).0)
        .unwrap()
        .unwrap();
    assert_eq!(record.kind, "image");
    let settings = server
        .records_db()
        .library_settings(node(&id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(settings.kind.as_str(), "image");

    // The existing new-library path makes document libraries, with their record too.
    let (status, body) = child(&server, &root, &site, "library", "Files").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let files = body["id"].as_str().unwrap();
    assert_eq!(
        server.store().read_library_record(node(files).0).unwrap(),
        Some(LibraryRecord::defaults(node(files).0))
    );
    let (_, body) = show(&server, &root, files).await;
    assert_eq!(body["kind"], "document");
    assert_eq!(body["effective_allowed_types"], Value::Null);

    // An unknown kind makes nothing.
    let (status, body) = new_library(&server, &root, &site, "Wiki", Some("wiki")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"], "invalid_library_settings");
    assert_eq!(body["reason"], "invalid_kind");
}

#[tokio::test]
async fn old_library_reads_defaults_and_writes_on_first_change() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let site = create_site(&server, &root, "Old").await;
    let id = first_library(&server, &root, &site).await;
    // Made as a server before library records made it: no record in Git.
    let author = author();
    let removed = [node(&id).0];
    let mut change = SitesChange::new(&author, "Remove the record");
    change.removed_libraries = &removed;
    server.store().write_sites(&change).unwrap();
    assert_eq!(
        server.store().read_library_record(node(&id).0).unwrap(),
        None
    );

    let (status, body) = show(&server, &root, &id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!({
            "id": id,
            "name": "Documents",
            "site": site,
            "kind": "document",
            "description": "",
            "folders": true,
            "allowed_types": null,
            "blocked_types": [],
            "max_file_bytes": null,
            "effective_allowed_types": null,
            "hero_image": null,
        })
    );

    let tip = ref_of(&server, "refs/heads/main").unwrap();
    let (status, body) = patch(&server, &root, &id, json!({ "description": "Ours" })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["description"], "Ours");
    let record = server
        .store()
        .read_library_record(node(&id).0)
        .unwrap()
        .unwrap();
    assert_eq!(
        record,
        LibraryRecord {
            schema: 1,
            id: node(&id).0,
            kind: "document".into(),
            description: "Ours".into(),
            folders: true,
            allowed_types: None,
            blocked_types: Vec::new(),
            max_file_bytes: None,
            hero_image: None,
        }
    );
    // One commit, moving the sites ref and not the tree ref.
    let now = ref_of(&server, "refs/heads/main").unwrap();
    let repo = gix::open(server.library()).unwrap();
    assert_eq!(
        repo.find_commit(now)
            .unwrap()
            .parent_ids()
            .next()
            .unwrap()
            .detach(),
        tip
    );
    assert_eq!(ref_of(&server, "refs/sharecraft/sites"), Some(now));
    assert_ne!(ref_of(&server, "refs/sharecraft/tree"), Some(now));
    let changed = events(&server, "library.changed").await;
    assert_eq!(changed.len(), 1);
    assert_eq!(
        changed[0],
        json!({
            "name": "Documents",
            "fields": { "description": { "from": "", "to": "Ours" } },
        })
    );

    // Setting what is already there writes nothing.
    let (status, _) = patch(&server, &root, &id, json!({ "description": "Ours" })).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ref_of(&server, "refs/heads/main"), Some(now));
    assert_eq!(events(&server, "library.changed").await.len(), 1);

    // Every setting at once, with the old and new values recorded.
    let (status, body) = patch(
        &server,
        &root,
        &id,
        json!({
            "kind": "template",
            "folders": false,
            "allowed_types": [".DOCX", " .xlsx"],
            "blocked_types": [".exe"],
            "max_file_bytes": 1_048_576,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["kind"], "template");
    assert_eq!(body["folders"], false);
    assert_eq!(body["allowed_types"], json!([".docx", ".xlsx"]));
    assert_eq!(body["blocked_types"], json!([".exe"]));
    assert_eq!(body["max_file_bytes"], 1_048_576);
    let changed = events(&server, "library.changed").await;
    assert_eq!(
        changed[1]["fields"],
        json!({
            "kind": { "from": "document", "to": "template" },
            "folders": { "from": true, "to": false },
            "allowed_types": { "from": null, "to": [".docx", ".xlsx"] },
            "blocked_types": { "from": [], "to": [".exe"] },
            "max_file_bytes": { "from": null, "to": 1_048_576 },
        })
    );
    // `null` sets every type and no limit again.
    let (status, body) = patch(
        &server,
        &root,
        &id,
        json!({ "allowed_types": null, "max_file_bytes": null }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["allowed_types"], Value::Null);
    assert_eq!(body["max_file_bytes"], Value::Null);
}

#[tokio::test]
async fn settings_need_manage_site_and_asset_needs_admin() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let owner = server.user("Olive").await;
    let member = server.user("Mia").await;
    let stranger = server.user("Stan").await;
    let site = create_site(&server, &root, "Team").await;
    let id = first_library(&server, &root, &site).await;
    grant(&server, &root, &site, &owner, "owner").await;
    grant(&server, &root, &site, &member, "editor").await;

    // Viewing needs view; strangers see nothing.
    let (status, _) = show(&server, &member, &id).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = show(&server, &stranger, &id).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    // Changing needs manage_site.
    let (status, body) = patch(&server, &member, &id, json!({ "description": "Mine" })).await;
    assert_eq!(
        (status, body["error"].as_str()),
        (StatusCode::FORBIDDEN, Some("forbidden")),
        "{body}"
    );
    let (status, body) = patch(&server, &stranger, &id, json!({ "description": "Mine" })).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, body) = patch(&server, &owner, &id, json!({ "description": "Ours" })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = patch(&server, &owner, &id, json!({ "kind": "image" })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = new_library(&server, &member, &site, "Theirs", None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body) = new_library(&server, &owner, &site, "Ours", Some("template")).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // The asset kind is an administrator's to set, and to take away.
    let (status, body) = patch(&server, &owner, &id, json!({ "kind": "asset" })).await;
    assert_eq!(
        (status, body["error"].as_str()),
        (StatusCode::FORBIDDEN, Some("forbidden")),
        "{body}"
    );
    let (status, body) = new_library(&server, &owner, &site, "Brand", Some("asset")).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body) = patch(&server, &root, &id, json!({ "kind": "asset" })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["kind"], "asset");
    // Its managers still change its other settings.
    let (status, body) = patch(&server, &owner, &id, json!({ "description": "Logos" })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = patch(&server, &owner, &id, json!({ "kind": "document" })).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body) = patch(&server, &root, &id, json!({ "kind": "document" })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = new_library(&server, &root, &site, "Brand", Some("asset")).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["kind"], "asset");

    // Refusals wrote nothing: only the accepted changes were recorded.
    let kinds: Vec<Value> = events(&server, "library.changed")
        .await
        .iter()
        .map(|e| e["fields"].clone())
        .collect();
    assert_eq!(kinds.len(), 5, "{kinds:?}");
}

#[tokio::test]
async fn bad_settings_refused() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let site = create_site(&server, &root, "Team").await;
    let id = first_library(&server, &root, &site).await;
    let (status, body) = new_library(&server, &root, &site, "Pictures", Some("image")).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let pictures = body["id"].as_str().unwrap().to_owned();
    let tip = ref_of(&server, "refs/heads/main");

    let many: Vec<String> = (0..101).map(|n| format!(".x{n}")).collect();
    let cases = [
        (&id, json!({ "kind": "wiki" }), "invalid_kind"),
        (
            &id,
            json!({ "description": "x".repeat(501) }),
            "invalid_description",
        ),
        (&id, json!({ "allowed_types": many }), "too_many_types"),
        (&id, json!({ "blocked_types": many }), "too_many_types"),
        (
            &id,
            json!({ "blocked_types": ["exe"] }),
            "invalid_extension",
        ),
        (
            &id,
            json!({ "blocked_types": [".e x e"] }),
            "invalid_extension",
        ),
        (
            &id,
            json!({ "allowed_types": [".tar..gz"] }),
            "invalid_extension",
        ),
        (
            &id,
            json!({ "allowed_types": [format!(".{}", "a".repeat(20))] }),
            "invalid_extension",
        ),
        (
            &id,
            json!({ "blocked_types": [".exe", ".EXE"] }),
            "duplicate_type",
        ),
        (&id, json!({ "allowed_types": [] }), "no_types"),
        (
            &id,
            json!({ "max_file_bytes": 0 }),
            "invalid_max_file_bytes",
        ),
        (
            &id,
            json!({ "max_file_bytes": u64::MAX }),
            "invalid_max_file_bytes",
        ),
        // An image library never widens its list.
        (
            &pictures,
            json!({ "allowed_types": [".png", ".pdf"] }),
            "not_an_image_type",
        ),
        (
            &id,
            json!({ "kind": "image", "allowed_types": [".docx"] }),
            "not_an_image_type",
        ),
    ];
    for (library, body, reason) in cases {
        let (status, answer) = patch(&server, &root, library, body.clone()).await;
        assert_eq!(
            (status, answer["error"].as_str(), answer["reason"].as_str()),
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                Some("invalid_library_settings"),
                Some(reason)
            ),
            "{body}: {answer}"
        );
    }
    // Not a setting at all.
    let (status, _) = patch(&server, &root, &id, json!({ "colour": "red" })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(ref_of(&server, "refs/heads/main"), tip);
    assert!(events(&server, "library.changed").await.is_empty());

    // At the limits, accepted; an image library may narrow its list.
    let hundred: Vec<String> = (0..100).map(|n| format!(".x{n}")).collect();
    let (status, body) = patch(
        &server,
        &root,
        &id,
        json!({
            "description": "x".repeat(500),
            "blocked_types": hundred,
            "allowed_types": [format!(".{}", "a".repeat(19)), ".tar.gz"],
            "max_file_bytes": 1,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = patch(
        &server,
        &root,
        &pictures,
        json!({ "allowed_types": [".png", ".jpg"] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["effective_allowed_types"], json!([".png", ".jpg"]));
}

#[tokio::test]
async fn folders_off_refuses_new_folders_and_folder_moves() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let site = create_site(&server, &root, "Team").await;
    let open = first_library(&server, &root, &site).await;
    let (_, body) = new_library(&server, &root, &site, "Flat", None).await;
    let flat = body["id"].as_str().unwrap().to_owned();
    let (_, body) = child(&server, &root, &flat, "folder", "Kept").await;
    let kept = body["id"].as_str().unwrap().to_owned();
    let (_, body) = child(&server, &root, &kept, "folder", "Inner").await;
    let inner = body["id"].as_str().unwrap().to_owned();
    let (_, body) = child(&server, &root, &open, "folder", "Plans").await;
    let plans = body["id"].as_str().unwrap().to_owned();

    let (status, body) = patch(&server, &root, &flat, json!({ "folders": false })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["folders"], false);

    let refused = |(status, body): (StatusCode, Value)| {
        assert_eq!(
            (status, body["error"].as_str()),
            (StatusCode::CONFLICT, Some("folders_off")),
            "{body}"
        );
    };
    // No new folder, in the library or in a folder it keeps.
    refused(child(&server, &root, &flat, "folder", "New").await);
    refused(child(&server, &root, &kept, "folder", "New").await);
    // No folder moved or copied in from elsewhere.
    refused(
        call(
            &server,
            &root,
            Method::PATCH,
            &format!("/api/nodes/{plans}"),
            Some(json!({ "parent": flat })),
        )
        .await,
    );
    refused(
        call(
            &server,
            &root,
            Method::POST,
            &format!("/api/nodes/{plans}/copy"),
            Some(json!({ "parent": kept })),
        )
        .await,
    );
    refused(
        call(
            &server,
            &root,
            Method::POST,
            &format!("/api/nodes/{inner}/copy"),
            Some(json!({ "parent": flat })),
        )
        .await,
    );

    // Files still go in; existing folders stay, move about inside, and can be emptied,
    // deleted and restored.
    let (status, body) = call(
        &server,
        &root,
        Method::POST,
        "/api/docs",
        Some(json!({ "parent": kept, "name": "Notes" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = call(
        &server,
        &root,
        Method::PATCH,
        &format!("/api/nodes/{inner}"),
        Some(json!({ "parent": flat })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = call(
        &server,
        &root,
        Method::DELETE,
        &format!("/api/nodes/{inner}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, body) = call(
        &server,
        &root,
        Method::POST,
        &format!("/api/bin/{inner}/restore"),
        None,
    )
    .await;
    assert!(status.is_success(), "{status} {body}");
    // A folder moves out of it.
    let (status, body) = call(
        &server,
        &root,
        Method::PATCH,
        &format!("/api/nodes/{inner}"),
        Some(json!({ "parent": open })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Folders on again: new folders are fine.
    let (status, _) = patch(&server, &root, &flat, json!({ "folders": true })).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = child(&server, &root, &flat, "folder", "New").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

#[tokio::test]
async fn reconcile_rebuilds_libraries_from_git() {
    let data_dir = tempfile::tempdir().unwrap();
    let db = TestDb::create().await;
    let mut first = StoppableServer::start(data_dir.path(), &db, |_| {});
    let root = first.admin("Root").await;
    let site = create_site(&first, &root, "Team").await;
    let id = first_library(&first, &root, &site).await;
    let (status, body) = patch(
        &first,
        &root,
        &id,
        json!({ "description": "Kept in Git", "blocked_types": [".exe"] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, body) = new_library(&first, &root, &site, "Other", None).await;
    let other = body["id"].as_str().unwrap().to_owned();
    let (_, body) = new_library(&first, &root, &site, "Bare", None).await;
    let bare = body["id"].as_str().unwrap().to_owned();
    first.stop();

    let pool = db.db.test_pool();
    sqlx::query(
        "update libraries set description = 'Wrong', folders = false, blocked_types = '{}' \
         where id = $1",
    )
    .bind(node(&id).0)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("delete from libraries where id = $1")
        .bind(node(&other).0)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("update libraries set kind = 'image' where id = $1")
        .bind(node(&bare).0)
        .execute(pool)
        .await
        .unwrap();
    // A record changed in Git while the server was stopped, and one taken away (a library
    // from before library records, which reads as the defaults).
    let store = RevisionStore::open(&server::library_path(data_dir.path())).unwrap();
    let mut record = store.read_library_record(node(&other).0).unwrap().unwrap();
    record.kind = "template".into();
    let author = author();
    let records = [record];
    let removed = [node(&bare).0];
    let mut change = SitesChange::new(&author, "Change libraries");
    change.libraries = &records;
    change.removed_libraries = &removed;
    store.write_sites(&change).unwrap();

    let second = StoppableServer::start(data_dir.path(), &db, |_| {});
    let root = second.admin("Rita").await;
    let (status, body) = show(&second, &root, &id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["description"], "Kept in Git");
    assert_eq!(body["folders"], true);
    assert_eq!(body["blocked_types"], json!([".exe"]));
    let (_, body) = show(&second, &root, &other).await;
    assert_eq!(body["kind"], "template");
    let (_, body) = show(&second, &root, &bare).await;
    assert_eq!(body["kind"], "document");
    let repaired = events(&second, "library.changed").await;
    assert_eq!(
        repaired
            .iter()
            .filter(|detail| detail["repaired"] == true)
            .count(),
        3,
        "{repaired:?}"
    );
}

#[tokio::test]
async fn library_change_refused_by_database_is_taken_back() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let site = create_site(&server, &root, "Team").await;
    let id = first_library(&server, &root, &site).await;
    let before = server.store().read_library_record(node(&id).0).unwrap();
    // PostgreSQL refuses the description "Refused", as its own checks refuse a row.
    sqlx::raw_sql(
        "create function refuse_library() returns trigger language plpgsql as $$ \
         begin if new.description = 'Refused' then \
         raise exception using errcode = 'check_violation', message = 'refused'; end if; \
         return new; end $$; \
         create trigger refuse_library before insert or update on libraries \
         for each row execute function refuse_library();",
    )
    .execute(server.records_db().test_pool())
    .await
    .unwrap();
    let tip = ref_of(&server, "refs/heads/main").unwrap();

    let (status, body) = patch(
        &server,
        &root,
        &id,
        json!({ "description": "Refused", "folders": false }),
    )
    .await;
    assert_eq!(
        (status, body["error"].as_str(), body["reason"].as_str()),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            Some("invalid_library_settings"),
            Some("refused")
        ),
        "{body}"
    );
    // The change went into Git and a second commit took it back.
    let repo = gix::open(server.library()).unwrap();
    let now = ref_of(&server, "refs/heads/main").unwrap();
    let undo = repo.find_commit(now).unwrap();
    assert!(
        String::from_utf8_lossy(undo.message_raw().unwrap()).starts_with("Undo: "),
        "the last commit takes the change back"
    );
    let change = undo.parent_ids().next().unwrap().detach();
    assert_eq!(
        repo.find_commit(change)
            .unwrap()
            .parent_ids()
            .next()
            .unwrap()
            .detach(),
        tip
    );
    assert_eq!(ref_of(&server, "refs/sharecraft/sites"), Some(now));
    assert_eq!(
        server.store().read_library_record(node(&id).0).unwrap(),
        before
    );
    let (_, body) = show(&server, &root, &id).await;
    assert_eq!(
        (&body["description"], &body["folders"]),
        (&json!(""), &json!(true))
    );
    assert!(events(&server, "library.changed").await.is_empty());

    // Nothing is out of step: the next change goes through.
    let (status, body) = patch(&server, &root, &id, json!({ "description": "Fine" })).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // A library with no record taken back loses the record again.
    let removed = [node(&id).0];
    let author = author();
    let mut change = SitesChange::new(&author, "Remove the record");
    change.removed_libraries = &removed;
    server.store().write_sites(&change).unwrap();
    let (status, _) = patch(&server, &root, &id, json!({ "description": "Refused" })).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        server.store().read_library_record(node(&id).0).unwrap(),
        None
    );
}

#[tokio::test]
async fn templates_carry_library_kinds() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let site = create_site(&server, &root, "Source").await;
    let (_, body) = new_library(&server, &root, &site, "Pictures", Some("image")).await;
    let pictures = body["id"].as_str().unwrap().to_owned();
    let (status, body) = patch(
        &server,
        &root,
        &pictures,
        json!({
            "description": "Photos",
            "folders": false,
            "allowed_types": [".png"],
            "blocked_types": [".svg"],
            "max_file_bytes": 5_000_000,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = new_library(&server, &root, &site, "Brand", Some("asset")).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let (status, saved) = call(
        &server,
        &root,
        Method::POST,
        &format!("/api/sites/{site}/template"),
        Some(json!({ "name": "Gallery", "description": "" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{saved}");
    let template = saved["id"].as_str().unwrap().to_owned();
    let pictures_settings = json!({
        "kind": "image",
        "description": "Photos",
        "folders": false,
        "allowed_types": [".png"],
        "blocked_types": [".svg"],
        "max_file_bytes": 5_000_000,
    });
    let expected_libraries = json!([
        { "name": "Brand", "settings": {
            "kind": "asset", "description": "", "folders": true, "allowed_types": null,
            "blocked_types": [], "max_file_bytes": null }, "folders": [] },
        // A document library with the defaults carries no settings.
        { "name": "Documents", "folders": [] },
        { "name": "Pictures", "settings": pictures_settings, "folders": [] },
    ]);
    let records = server.store().read_templates().unwrap();
    let body = serde_json::to_value(&records[0].body).unwrap();
    assert_eq!(body["libraries"], expected_libraries);

    // Its package carries them, and a site made from it gets them.
    let response = server
        .http(Some(&root))
        .get(server.url(&format!("/api/templates/{template}/package")))
        .send()
        .await
        .unwrap();
    let (status, package) = answer(response).await;
    assert_eq!(status, StatusCode::OK, "{package}");
    assert_eq!(package["body"]["libraries"], expected_libraries);
    let (status, body) = call(
        &server,
        &root,
        Method::POST,
        "/api/sites",
        Some(json!({ "name": "Copy", "template": template })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let copy = body["id"].as_str().unwrap().to_owned();
    let (_, page) = call(
        &server,
        &root,
        Method::GET,
        &format!("/api/nodes/{copy}/children"),
        None,
    )
    .await;
    let mut seen = Vec::new();
    for item in page["items"].as_array().unwrap() {
        let id = item["id"].as_str().unwrap();
        let (status, library) = show(&server, &root, id).await;
        assert_eq!(status, StatusCode::OK, "{library}");
        // Recorded in Git with the site, and mirrored.
        let record = server
            .store()
            .read_library_record(node(id).0)
            .unwrap()
            .unwrap();
        assert_eq!(json!(record.kind), library["kind"]);
        seen.push((
            library["name"].as_str().unwrap().to_owned(),
            library["kind"].as_str().unwrap().to_owned(),
        ));
        if library["name"] == "Pictures" {
            for field in [
                "description",
                "folders",
                "allowed_types",
                "blocked_types",
                "max_file_bytes",
            ] {
                assert_eq!(library[field], pictures_settings[field], "{field}");
            }
        }
    }
    seen.sort();
    assert_eq!(
        seen,
        vec![
            ("Brand".to_owned(), "asset".to_owned()),
            ("Documents".to_owned(), "document".to_owned()),
            ("Pictures".to_owned(), "image".to_owned()),
        ]
    );

    // A package with no settings makes document libraries with the defaults; one whose
    // settings break a rule is refused.
    let package = |libraries: Value| {
        serde_json::to_vec(&json!({
            "sharecraft_template": 1,
            "name": "Imported",
            "body": {
                "kind": "team", "language": "en", "time_zone": "UTC",
                "libraries": libraries, "navigation": null
            }
        }))
        .unwrap()
    };
    let import = |bytes: Vec<u8>| {
        let request = server
            .http(Some(&root))
            .post(server.url("/api/templates/import"))
            .header("content-type", "application/json")
            .body(bytes);
        async move { answer(request.send().await.unwrap()).await }
    };
    let (status, imported) = import(package(json!([{ "name": "Plain", "folders": [] }]))).await;
    assert_eq!(status, StatusCode::CREATED, "{imported}");
    let (status, body) = call(
        &server,
        &root,
        Method::POST,
        "/api/sites",
        Some(json!({ "name": "Plain site", "template": imported["id"] })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let plain = first_library(&server, &root, body["id"].as_str().unwrap()).await;
    let (_, library) = show(&server, &root, &plain).await;
    assert_eq!(
        (
            &library["kind"],
            &library["folders"],
            &library["allowed_types"]
        ),
        (&json!("document"), &json!(true), &Value::Null)
    );
    for bad in [
        json!({ "kind": "wiki" }),
        json!({ "kind": "image", "allowed_types": [".exe"] }),
        json!({ "blocked_types": ["exe"] }),
        json!({ "max_file_bytes": 0 }),
    ] {
        let (status, body) = import(package(
            json!([{ "name": "Bad", "settings": bad, "folders": [] }]),
        ))
        .await;
        assert_eq!(
            (status, body["reason"].as_str()),
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                Some("invalid_library_settings")
            ),
            "{body}"
        );
    }
    let (status, body) = import(package(
        json!([{ "name": "Bad", "settings": { "colour": "red" }, "folders": [] }]),
    ))
    .await;
    assert_eq!(
        (status, body["reason"].as_str()),
        (StatusCode::UNPROCESSABLE_ENTITY, Some("unknown_field")),
        "{body}"
    );
}

#[tokio::test]
async fn assets_listed_only_where_visible() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let viewer = server.user("Vic").await;
    let ours = create_site(&server, &root, "Ours").await;
    let theirs = create_site(&server, &root, "Theirs").await;
    grant(&server, &root, &ours, &viewer, "viewer").await;
    let (_, body) = new_library(&server, &root, &ours, "Logos", Some("asset")).await;
    let logos = body["id"].as_str().unwrap().to_owned();
    let (_, body) = new_library(&server, &root, &theirs, "Brand", Some("asset")).await;
    let brand = body["id"].as_str().unwrap().to_owned();
    let (_, body) = new_library(&server, &root, &ours, "Old logos", Some("asset")).await;
    let old = body["id"].as_str().unwrap().to_owned();
    new_library(&server, &root, &ours, "Plain", None).await;
    let (status, _) = call(
        &server,
        &root,
        Method::DELETE,
        &format!("/api/nodes/{old}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let listed = |user: &TestUser| {
        let request = server.http(Some(user)).get(server.url("/api/assets"));
        async move {
            let (status, body) = answer(request.send().await.unwrap()).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            body.as_array()
                .unwrap()
                .iter()
                .map(|l| l["id"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(listed(&root).await, vec![brand.clone(), logos.clone()]);
    assert_eq!(listed(&viewer).await, vec![logos.clone()]);
    let stranger = server.user("Stan").await;
    assert!(listed(&stranger).await.is_empty());
    // A library with its own permissions that shut the viewer out is not listed to them.
    let (status, body) = call(
        &server,
        &root,
        Method::POST,
        &format!("/api/nodes/{logos}/inheritance"),
        Some(json!({ "inherit": false })),
    )
    .await;
    assert!(status.is_success(), "{status} {body}");
    let (status, body) = call(
        &server,
        &root,
        Method::DELETE,
        &format!("/api/nodes/{logos}/access/user/{}", viewer.id.0),
        None,
    )
    .await;
    assert!(status.is_success(), "{status} {body}");
    assert!(listed(&viewer).await.is_empty());
    // A deleted site's asset libraries are not listed.
    let (status, _) = call(
        &server,
        &root,
        Method::DELETE,
        &format!("/api/sites/{theirs}"),
        None,
    )
    .await;
    assert!(status.is_success());
    assert_eq!(listed(&root).await, vec![logos]);
}

#[tokio::test]
async fn archived_site_refuses_library_changes() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let owner = server.user("Olive").await;
    let site = create_site(&server, &root, "Team").await;
    let id = first_library(&server, &root, &site).await;
    grant(&server, &root, &site, &owner, "owner").await;
    let (status, body) = call(
        &server,
        &root,
        Method::POST,
        &format!("/api/sites/{site}/archive"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let tip = ref_of(&server, "refs/heads/main");
    for user in [&root, &owner] {
        for (status, body) in [
            patch(&server, user, &id, json!({ "description": "Changed" })).await,
            patch(&server, user, &id, json!({ "kind": "asset" })).await,
            new_library(&server, user, &site, "More", None).await,
            new_library(&server, user, &site, "Brand", Some("asset")).await,
        ] {
            assert_eq!(
                (status, body["error"].as_str()),
                (StatusCode::FORBIDDEN, Some("archived")),
                "as {}: {body}",
                user.email
            );
        }
        // Reading still works.
        let (status, _) = show(&server, user, &id).await;
        assert_eq!(status, StatusCode::OK);
    }
    assert_eq!(ref_of(&server, "refs/heads/main"), tip);

    // On a deleted site, the library is not there at all.
    let (status, _) = call(
        &server,
        &root,
        Method::DELETE,
        &format!("/api/sites/{site}"),
        None,
    )
    .await;
    assert!(status.is_success());
    for (status, body) in [
        show(&server, &root, &id).await,
        patch(&server, &root, &id, json!({ "description": "Changed" })).await,
        new_library(&server, &root, &site, "More", None).await,
    ] {
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    }
}
