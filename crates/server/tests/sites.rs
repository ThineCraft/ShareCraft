//! Site records over HTTP: each site's kind, description, language and time zone, recorded in
//! Git beside its tree record (in the same commit when the site is made) and mirrored into
//! PostgreSQL; a site from before site records reads as the defaults until its first change;
//! changing the settings needs `manage_site`; the site directory with its filters; a site's
//! owners; and the reconcile at start that rebuilds the mirror from Git.

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
use revisions::{Author, RevisionStore, SiteRecord, SitesChange};
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
    let response = request.send().await.unwrap();
    let status = response.status();
    let text = response.text().await.unwrap();
    let body = if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).unwrap_or(Value::String(text))
    };
    (status, body)
}

/// Creates the site `name` as `admin`; its id.
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

/// Changes the site's settings; the status and answer.
async fn patch(site: &Site, user: &TestUser, id: &str, body: Value) -> (StatusCode, Value) {
    call(
        site,
        user,
        Method::PATCH,
        &format!("/api/sites/{id}"),
        Some(body),
    )
    .await
}

/// The site as `GET /api/sites/{id}` answers it for `user`.
async fn show(site: &Site, user: &TestUser, id: &str) -> (StatusCode, Value) {
    call(site, user, Method::GET, &format!("/api/sites/{id}"), None).await
}

/// The id of the site's group with this purpose.
async fn group_of(site: &Site, admin: &TestUser, id: &str, purpose: &str) -> String {
    let (status, groups) = call(
        site,
        admin,
        Method::GET,
        &format!("/api/sites/{id}/groups"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{groups}");
    groups
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["purpose"] == purpose)
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// Adds `member` to the group `group`, as `admin`.
async fn join(site: &Site, admin: &TestUser, group: &str, member: &TestUser) {
    let (status, body) = call(
        site,
        admin,
        Method::POST,
        &format!("/api/groups/{group}/members"),
        Some(json!({ "email": member.email })),
    )
    .await;
    assert!(status.is_success(), "{status} {body}");
}

fn node(id: &str) -> NodeId {
    NodeId(Uuid::parse_str(id).unwrap())
}

/// The id `main`, `refs/sharecraft/tree` or `refs/sharecraft/sites` points at, if it exists.
fn ref_of(server: &TestServer, name: &str) -> Option<gix::ObjectId> {
    let repo = gix::open(server.library()).unwrap();
    repo.try_find_reference(name)
        .unwrap()
        .and_then(|r| r.try_id().map(gix::Id::detach))
}

/// The paths of the blobs a commit's tree holds below `dir`.
fn paths_below(repo: &gix::Repository, commit: gix::ObjectId, dir: &str) -> Vec<String> {
    let tree = repo.find_commit(commit).unwrap().tree().unwrap();
    let Some(entry) = tree.lookup_entry_by_path(dir).unwrap() else {
        return Vec::new();
    };
    let mut found = Vec::new();
    let top = entry.object().unwrap().into_tree();
    for shard in top.iter() {
        let shard = shard.unwrap();
        let sub = repo.find_object(shard.oid()).unwrap().into_tree();
        for file in sub.iter() {
            let file = file.unwrap();
            found.push(format!("{dir}/{}/{}", shard.filename(), file.filename()));
        }
    }
    found.sort();
    found
}

/// The details of the events of `kind`, oldest first.
async fn events(server: &Site, kind: &str) -> Vec<Value> {
    sqlx::query_scalar("select detail from events where kind = $1 order by id")
        .bind(kind)
        .fetch_all(server.records_db().test_pool())
        .await
        .unwrap()
}

/// The site directory as `user` sees it with the query `query`.
async fn directory(server: &Site, user: &TestUser, query: String) -> Vec<Value> {
    let (status, body) = call(
        server,
        user,
        Method::GET,
        &format!("/api/sites{query}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body.as_array().unwrap().clone()
}

fn shard(id: &str) -> String {
    id.chars().take(2).collect()
}

#[tokio::test]
async fn new_site_writes_tree_and_site_record_in_one_commit() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let id = create_site(&server, &root, "Team").await;

    let tip = ref_of(&server, "refs/heads/main").unwrap();
    assert_eq!(ref_of(&server, "refs/sharecraft/tree"), Some(tip));
    assert_eq!(ref_of(&server, "refs/sharecraft/sites"), Some(tip));
    let repo = gix::open(server.library()).unwrap();
    let sites = paths_below(&repo, tip, "sites");
    assert_eq!(sites, vec![format!("sites/{}/{id}.json", shard(&id))]);
    let tree = paths_below(&repo, tip, "tree");
    assert_eq!(tree.len(), 2, "the site and its library: {tree:?}");
    let commit = repo.find_commit(tip).unwrap();
    let parent = commit.parent_ids().next().map(gix::Id::detach);
    if let Some(parent) = parent {
        assert!(paths_below(&repo, parent, "sites").is_empty());
        assert!(paths_below(&repo, parent, "tree").is_empty());
    }

    let record = server
        .store()
        .read_site_record(node(&id).0)
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            record.kind.as_str(),
            record.description.as_str(),
            record.language.as_str(),
            record.time_zone.as_str()
        ),
        ("team", "", "en", "UTC")
    );
    assert_eq!(record.navigation, None);
    let (status, body) = show(&server, &root, &id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["kind"], "team");
    assert_eq!(body["description"], "");
    assert_eq!(body["language"], "en");
    assert_eq!(body["time_zone"], "UTC");
    assert_eq!(body["is_hub"], false);
    assert_eq!(body["hub"], Value::Null);
    assert_eq!(body["archived"], Value::Null);
    assert_eq!(body["owners"], json!(["Root"]));

    // A new site follows the organisation's defaults as they are when it is made.
    let (status, body) = call(
        &server,
        &root,
        Method::PUT,
        "/api/settings/regional",
        Some(json!({ "default_language": "fr-CA", "default_time_zone": "America/Toronto" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!({ "default_language": "fr-CA", "default_time_zone": "America/Toronto" })
    );
    let other = create_site(&server, &root, "Équipe").await;
    let record = server
        .store()
        .read_site_record(node(&other).0)
        .unwrap()
        .unwrap();
    assert_eq!(
        (record.language.as_str(), record.time_zone.as_str()),
        ("fr-CA", "America/Toronto")
    );
    let settings = server
        .records_db()
        .site_settings(node(&other))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (settings.language.as_str(), settings.time_zone.as_str()),
        ("fr-CA", "America/Toronto")
    );
}

#[tokio::test]
async fn old_site_reads_defaults_and_writes_on_first_change() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let id = create_site(&server, &root, "Old").await;
    // Made as a server before site records made it: no record in Git.
    let author = Author {
        id: "test".into(),
        name: "Test".into(),
    };
    let removed = [node(&id).0];
    let mut change = SitesChange::new(&author, "Remove the record");
    change.removed_sites = &removed;
    server.store().write_sites(&change).unwrap();
    assert_eq!(server.store().read_site_record(node(&id).0).unwrap(), None);

    let (status, body) = show(&server, &root, &id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        (
            &body["kind"],
            &body["description"],
            &body["language"],
            &body["time_zone"]
        ),
        (&json!("team"), &json!(""), &json!("en"), &json!("UTC"))
    );

    let (status, body) = patch(&server, &root, &id, json!({ "description": "Ours" })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["description"], "Ours");
    let record = server
        .store()
        .read_site_record(node(&id).0)
        .unwrap()
        .unwrap();
    assert_eq!(
        record,
        SiteRecord {
            schema: 1,
            id: node(&id).0,
            kind: "team".into(),
            description: "Ours".into(),
            language: "en".into(),
            time_zone: "UTC".into(),
            template: None,
            archived: None,
            hub: None,
            joined_hub: None,
            navigation: None,
        }
    );
    let (_, body) = show(&server, &root, &id).await;
    assert_eq!(body["description"], "Ours");
    let changed = events(&server, "site.changed").await;
    assert_eq!(changed.len(), 1);
    assert_eq!(
        changed[0]["fields"],
        json!({ "description": { "from": "", "to": "Ours" } })
    );
}

#[tokio::test]
async fn settings_change_needs_manage_site() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let owner = server.user("Olive").await;
    let member = server.user("Mia").await;
    let visitor = server.user("Vic").await;
    let stranger = server.user("Stan").await;
    let id = create_site(&server, &root, "Team").await;
    for (purpose, user) in [
        ("owners", &owner),
        ("members", &member),
        ("visitors", &visitor),
    ] {
        let group = group_of(&server, &root, &id, purpose).await;
        join(&server, &root, &group, user).await;
    }
    let sites_ref = ref_of(&server, "refs/sharecraft/sites");

    for user in [&member, &visitor] {
        let (status, body) = patch(&server, user, &id, json!({ "description": "Mine" })).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    }
    let (status, _) = patch(&server, &stranger, &id, json!({ "description": "Mine" })).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(ref_of(&server, "refs/sharecraft/sites"), sites_ref);
    assert!(events(&server, "site.changed").await.is_empty());

    // Everyone who can see the site sees its settings.
    let (status, body) = show(&server, &visitor, &id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["language"], "en");

    let (status, body) = patch(
        &server,
        &owner,
        &id,
        json!({
            "name": "Team room",
            "description": "Where the team works",
            "language": "en-GB",
            "time_zone": "Europe/London",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["name"], "Team room");
    assert_eq!(body["language"], "en-GB");
    // One commit for the name and the settings.
    let tip = ref_of(&server, "refs/heads/main");
    assert_eq!(ref_of(&server, "refs/sharecraft/sites"), tip);
    assert_eq!(ref_of(&server, "refs/sharecraft/tree"), tip);
    let store = server.store();
    assert_eq!(
        store.read_tree_record(node(&id).0).unwrap().unwrap().name,
        "Team room"
    );
    let record = store.read_site_record(node(&id).0).unwrap().unwrap();
    assert_eq!(
        (record.description.as_str(), record.time_zone.as_str()),
        ("Where the team works", "Europe/London")
    );
    let changed = events(&server, "site.changed").await;
    assert_eq!(changed.len(), 1);
    assert_eq!(
        changed[0]["fields"],
        json!({
            "name": { "from": "Team", "to": "Team room" },
            "description": { "from": "", "to": "Where the team works" },
            "language": { "from": "en", "to": "en-GB" },
            "time_zone": { "from": "UTC", "to": "Europe/London" },
        })
    );

    // The same values again change nothing.
    let tip = ref_of(&server, "refs/heads/main");
    let (status, _) = patch(&server, &root, &id, json!({ "language": "en-GB" })).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ref_of(&server, "refs/heads/main"), tip);
    assert_eq!(events(&server, "site.changed").await.len(), 1);
}

#[tokio::test]
async fn bad_language_and_time_zone_refused() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let id = create_site(&server, &root, "Team").await;
    let tip = ref_of(&server, "refs/heads/main");
    let cases = [
        (json!({ "language": "english" }), "invalid_language"),
        (json!({ "language": "en_GB" }), "invalid_language"),
        (json!({ "language": 7 }), "bad_request"),
        (json!({ "time_zone": "Mars/Olympus" }), "invalid_time_zone"),
        (json!({ "time_zone": "" }), "invalid_time_zone"),
        (json!({ "description": "x".repeat(501) }), "invalid_request"),
        (json!({ "name": "a/b" }), "invalid_name"),
        (json!({ "colour": "red" }), "bad_request"),
    ];
    for (body, code) in cases {
        let (status, answer) = patch(&server, &root, &id, body.clone()).await;
        assert_eq!(answer["error"], code, "{body}");
        assert!(status.is_client_error(), "{body}");
    }
    assert_eq!(ref_of(&server, "refs/heads/main"), tip);

    for (body, code) in [
        (json!({ "default_language": "xx-" }), "invalid_language"),
        (
            json!({ "default_time_zone": "Atlantis" }),
            "invalid_time_zone",
        ),
    ] {
        let (status, answer) = call(
            &server,
            &root,
            Method::PUT,
            "/api/settings/regional",
            Some(body.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(answer["error"], code, "{body}");
    }
    let (status, body) = call(&server, &root, Method::GET, "/api/settings/regional", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({ "default_language": "en", "default_time_zone": "UTC" })
    );
    // Only administrators change the defaults.
    let someone = server.user("Sam").await;
    let (status, _) = call(
        &server,
        &someone,
        Method::PUT,
        "/api/settings/regional",
        Some(json!({ "default_language": "de" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // The global navigation is not a setting.
    let refused = server
        .records_db()
        .set_setting(root.id, records::GLOBAL_NAVIGATION, json!([]))
        .await;
    assert_eq!(refused, Err(records::AccessError::InvalidSetting));
}

#[tokio::test]
async fn reconcile_rebuilds_sites_from_git() {
    let data_dir = tempfile::tempdir().unwrap();
    let db = TestDb::create().await;
    let mut first = StoppableServer::start(data_dir.path(), &db, |_| {});
    let root = first.admin("Root").await;
    let id = create_site(&first, &root, "Team").await;
    let (status, body) = patch(
        &first,
        &root,
        &id,
        json!({ "description": "Kept in Git", "time_zone": "Asia/Tokyo" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let other = create_site(&first, &root, "Other").await;
    first.stop();

    let pool = db.db.test_pool();
    sqlx::query(
        "update sites set description = 'Wrong', language = 'de', is_hub = true where id = $1",
    )
    .bind(node(&id).0)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("delete from sites where id = $1")
        .bind(node(&other).0)
        .execute(pool)
        .await
        .unwrap();
    // A record written to Git while the server was stopped.
    let store = RevisionStore::open(&server::library_path(data_dir.path())).unwrap();
    let mut record = store.read_site_record(node(&other).0).unwrap().unwrap();
    record.kind = "communication".into();
    let author = Author {
        id: "elsewhere".into(),
        name: "Elsewhere".into(),
    };
    let records = [record];
    let mut change = SitesChange::new(&author, "Change site Other");
    change.sites = &records;
    store.write_sites(&change).unwrap();

    let second = StoppableServer::start(data_dir.path(), &db, |_| {});
    // The harness of the new server keeps its own accounts.
    let root = second.admin("Rita").await;
    let (status, body) = show(&second, &root, &id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["description"], "Kept in Git");
    assert_eq!(body["language"], "en");
    assert_eq!(body["time_zone"], "Asia/Tokyo");
    assert_eq!(body["is_hub"], false);
    let (_, body) = show(&second, &root, &other).await;
    assert_eq!(body["kind"], "communication");
    let repaired = events(&second, "site.changed").await;
    assert_eq!(
        repaired
            .iter()
            .filter(|detail| detail["repaired"] == true)
            .count(),
        2,
        "{repaired:?}"
    );
}

#[tokio::test]
async fn site_change_refused_by_database_is_taken_back() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let id = create_site(&server, &root, "Team").await;
    let before = server.store().read_site_record(node(&id).0).unwrap();
    // PostgreSQL refuses the name "Mine" as taken, as a name taken outside the server's lock
    // would be.
    sqlx::raw_sql(
        "create function refuse_name() returns trigger language plpgsql as $$ \
         begin if new.name = 'Mine' then \
         raise exception using errcode = 'unique_violation', message = 'name taken'; end if; \
         return new; end $$; \
         create trigger refuse_name before insert or update on nodes \
         for each row execute function refuse_name();",
    )
    .execute(server.records_db().test_pool())
    .await
    .unwrap();
    let tip = ref_of(&server, "refs/heads/main").unwrap();

    let (status, body) = patch(
        &server,
        &root,
        &id,
        json!({ "name": "Mine", "description": "Refused" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "name_taken");
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
        server.store().read_site_record(node(&id).0).unwrap(),
        before
    );
    assert_eq!(
        server
            .store()
            .read_tree_record(node(&id).0)
            .unwrap()
            .unwrap()
            .name,
        "Team"
    );
    let (_, body) = show(&server, &root, &id).await;
    assert_eq!(
        (&body["name"], &body["description"]),
        (&json!("Team"), &json!(""))
    );
    assert!(events(&server, "site.changed").await.is_empty());

    // Nothing is out of step: the next change goes through.
    let (status, body) = patch(&server, &root, &id, json!({ "description": "Fine" })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn directory_filters_by_name_kind_and_hub() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let reader = server.user("Rae").await;
    let hub = create_site(&server, &root, "Hub of news").await;
    let hidden_hub = create_site(&server, &root, "Secret hub").await;
    let alpha = create_site(&server, &root, "Alpha team").await;
    let beta = create_site(&server, &root, "Beta news").await;
    let gamma = create_site(&server, &root, "Gamma").await;
    let archived = create_site(&server, &root, "Old news").await;
    for site in [&hub, &alpha, &beta, &gamma, &archived] {
        let visitors = group_of(&server, &root, site, "visitors").await;
        join(&server, &root, &visitors, &reader).await;
    }
    let (status, body) = patch(
        &server,
        &root,
        &beta,
        json!({ "description": "Company news" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // Hubs, kinds and archiving have their own routes in later work; set them as Git would.
    let pool = server.records_db().test_pool().clone();
    sqlx::query("update sites set is_hub = true, hub_navigation = '[]' where id = any($1)")
        .bind(vec![node(&hub).0, node(&hidden_hub).0])
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("update sites set joined_hub = $1 where id = $2")
        .bind(node(&hub).0)
        .bind(node(&alpha).0)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("update sites set joined_hub = $1, kind = 'communication' where id = $2")
        .bind(node(&hidden_hub).0)
        .bind(node(&beta).0)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("update sites set archived_at = now(), archived_by = $1 where id = $2")
        .bind(root.id.0)
        .bind(node(&archived).0)
        .execute(&pool)
        .await
        .unwrap();

    let names = |rows: &[Value]| -> Vec<String> {
        rows.iter()
            .map(|row| row["name"].as_str().unwrap().to_owned())
            .collect()
    };

    // The reader sees the sites they are in, but not the archived one or the hidden hub.
    let all = directory(&server, &reader, String::new()).await;
    assert_eq!(
        names(&all),
        ["Alpha team", "Beta news", "Gamma", "Hub of news"]
    );
    let alpha_row = all.iter().find(|r| r["id"] == alpha.as_str()).unwrap();
    assert_eq!(
        alpha_row["hub"],
        json!({ "id": hub, "name": "Hub of news" })
    );
    assert_eq!(alpha_row["kind"], "team");
    assert_eq!(alpha_row["is_hub"], false);
    let beta_row = all.iter().find(|r| r["id"] == beta.as_str()).unwrap();
    assert_eq!(beta_row["kind"], "communication");
    assert_eq!(beta_row["description"], "Company news");
    // A hub the reader cannot see is not named.
    assert_eq!(beta_row["hub"], Value::Null);
    let hub_row = all.iter().find(|r| r["id"] == hub.as_str()).unwrap();
    assert_eq!(hub_row["is_hub"], true);

    assert_eq!(
        names(&directory(&server, &reader, "?q=NEWS".into()).await),
        ["Beta news", "Hub of news"]
    );
    assert_eq!(
        names(&directory(&server, &reader, "?kind=communication".into()).await),
        ["Beta news"]
    );
    assert_eq!(
        names(&directory(&server, &reader, "?kind=team&q=a".into()).await),
        ["Alpha team", "Gamma"]
    );
    assert_eq!(
        names(&directory(&server, &reader, format!("?hub={hub}")).await),
        ["Alpha team"]
    );

    // An administrator, who manages every site, also sees the archived one, marked.
    let everything = directory(&server, &root, "?q=news".into()).await;
    assert_eq!(names(&everything), ["Beta news", "Hub of news", "Old news"]);
    let old = everything
        .iter()
        .find(|r| r["id"] == archived.as_str())
        .unwrap();
    assert_eq!(old["archived"], true);
    assert_eq!(everything[0]["archived"], false);
    let (status, body) = call(&server, &root, Method::GET, "/api/sites?kind=wiki", None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
}

#[tokio::test]
async fn owners_listed_without_hidden_groups() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let olive = server.user("Olive").await;
    let otto = server.user("Otto").await;
    let gone = server.user("Gone").await;
    let grace = server.user("Grace").await;
    let viewer = server.user("Vera").await;
    let id = create_site(&server, &root, "Team").await;
    let owners = group_of(&server, &root, &id, "owners").await;
    let visitors = group_of(&server, &root, &id, "visitors").await;
    join(&server, &root, &owners, &olive).await;
    join(&server, &root, &owners, &gone).await;
    join(&server, &root, &visitors, &viewer).await;
    // Otto is given Owner on the site himself.
    let (status, body) = call(
        &server,
        &root,
        Method::PUT,
        &format!("/api/nodes/{id}/access"),
        Some(json!({ "email": otto.email, "role": "owner" })),
    )
    .await;
    assert!(status.is_success(), "{status} {body}");
    // Grace is in an organisation group given Owner on the site: its members are not named.
    let (status, group) = call(
        &server,
        &root,
        Method::POST,
        "/api/groups",
        Some(json!({ "name": "Facilities" })),
    )
    .await;
    assert!(status.is_success(), "{status} {group}");
    let group = group["id"].as_str().unwrap().to_owned();
    join(&server, &root, &group, &grace).await;
    let (status, body) = call(
        &server,
        &root,
        Method::PUT,
        &format!("/api/nodes/{id}/access"),
        Some(json!({ "group": group, "role": "owner" })),
    )
    .await;
    assert!(status.is_success(), "{status} {body}");
    // Gone's account is disabled.
    let (status, body) = call(
        &server,
        &root,
        Method::POST,
        &format!("/api/admin/users/{}/disable", gone.id.0),
        None,
    )
    .await;
    assert!(status.is_success(), "{status} {body}");

    // Anyone who can see the site sees its owners.
    let (status, body) = show(&server, &viewer, &id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["owners"], json!(["Olive", "Otto", "Root"]));
    let stranger = server.user("Stan").await;
    let (status, _) = show(&server, &stranger, &id).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
