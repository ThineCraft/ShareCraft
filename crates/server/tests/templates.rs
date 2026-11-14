//! Site templates over HTTP: the built-in Team and Communication templates, saving a site's
//! shape as a template (libraries and folders, never files or what is in a bin), its package
//! for carrying it to another installation, importing a package (refused or cleaned when it
//! breaks a rule), deleting a saved template, and creating a site from any of them in one
//! commit. Only administrators work with templates.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::collections::BTreeMap;

use common::{Site, TestServer, TestUser, spawn_server};
use reqwest::{Method, StatusCode};
use revisions::{Author, NavItem, NavTarget, SiteRecord, SitesChange, TreeRecord};
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

/// Creates a site from `body` as `admin`; its id.
async fn create_site(site: &Site, admin: &TestUser, body: Value) -> String {
    let (status, answer) = call(site, admin, Method::POST, "/api/sites", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{answer}");
    assert_eq!(answer["groups"].as_array().unwrap().len(), 3, "{answer}");
    answer["id"].as_str().unwrap().to_owned()
}

/// Creates a library or folder named `name` in `parent`; its id.
async fn child(site: &Site, user: &TestUser, parent: &str, kind: &str, name: &str) -> String {
    let (status, body) = call(
        site,
        user,
        Method::POST,
        &format!("/api/nodes/{parent}/children"),
        Some(json!({ "kind": kind, "name": name })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["id"].as_str().unwrap().to_owned()
}

/// Imports the package `bytes` as `user`.
async fn import(site: &Site, user: &TestUser, bytes: Vec<u8>) -> (StatusCode, Value) {
    let response = site
        .http(Some(user))
        .post(site.url("/api/templates/import"))
        .header("content-type", "application/json")
        .body(bytes)
        .send()
        .await
        .unwrap();
    answer(response).await
}

/// Saves the site `id` as the template `name`; the answer.
async fn save(site: &Site, user: &TestUser, id: &str, name: &str) -> (StatusCode, Value) {
    call(
        site,
        user,
        Method::POST,
        &format!("/api/sites/{id}/template"),
        Some(json!({ "name": name, "description": "For projects" })),
    )
    .await
}

/// The details of the events of `kind`, oldest first.
async fn events(server: &Site, kind: &str) -> Vec<Value> {
    sqlx::query_scalar("select detail from events where kind = $1 order by id")
        .bind(kind)
        .fetch_all(server.records_db().test_pool())
        .await
        .unwrap()
}

/// The live tree records of the site `id` in Git, as `path -> kind`, where a path is the
/// names from the library down, joined with `/`.
fn shape(server: &TestServer, id: &str) -> BTreeMap<String, String> {
    let site = Uuid::parse_str(id).unwrap();
    let tree = server.store().read_tree().unwrap();
    let records: Vec<&TreeRecord> = tree
        .iter()
        .filter(|r| r.site == site && r.id != site && r.deleted.is_none())
        .collect();
    let path = |record: &TreeRecord| {
        let mut names = vec![record.name.clone()];
        let mut parent = record.parent;
        while let Some(id) = parent.filter(|p| *p != site) {
            let up = records.iter().find(|r| r.id == id).unwrap();
            names.push(up.name.clone());
            parent = up.parent;
        }
        names.reverse();
        names.join("/")
    };
    records.iter().map(|r| (path(r), r.kind.clone())).collect()
}

/// The id of the live node of site `id` at `path` (see [`shape`]).
fn node_at(server: &TestServer, id: &str, path: &str) -> Uuid {
    let site = Uuid::parse_str(id).unwrap();
    let tree = server.store().read_tree().unwrap();
    let mut parent = site;
    let mut found = site;
    for name in path.split('/') {
        found = tree
            .iter()
            .find(|r| r.parent == Some(parent) && r.name == name && r.deleted.is_none())
            .unwrap_or_else(|| panic!("no {path}"))
            .id;
        parent = found;
    }
    found
}

fn record_of(server: &TestServer, id: &str) -> SiteRecord {
    server
        .store()
        .read_site_record(Uuid::parse_str(id).unwrap())
        .unwrap()
        .unwrap()
}

/// The id of the site's group with this purpose.
async fn group_of(site: &Site, admin: &TestUser, id: &str, purpose: &str) -> Uuid {
    let (status, groups) = call(
        site,
        admin,
        Method::GET,
        &format!("/api/sites/{id}/groups"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{groups}");
    let text = groups
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["purpose"] == purpose)
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    Uuid::parse_str(&text).unwrap()
}

/// The templates `GET /api/templates` lists, as `(id, name, built_in)`.
async fn listed(site: &Site, admin: &TestUser) -> Vec<(String, String, bool)> {
    let (status, body) = call(site, admin, Method::GET, "/api/templates", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body.as_array()
        .unwrap()
        .iter()
        .map(|t| {
            (
                t["id"].as_str().unwrap().to_owned(),
                t["name"].as_str().unwrap().to_owned(),
                t["built_in"].as_bool().unwrap(),
            )
        })
        .collect()
}

/// A package that breaks no rule.
fn good_package() -> Value {
    json!({
        "sharecraft_template": 1,
        "name": "Imported",
        "description": "From elsewhere",
        "body": {
            "kind": "team",
            "language": "en",
            "time_zone": "UTC",
            "libraries": [
                { "name": "Documents", "folders": [ { "name": "Plans", "folders": [] } ] }
            ],
            "navigation": [
                { "label": "Files", "target": { "library": "Documents" },
                  "audience": ["members"], "children": [] }
            ]
        }
    })
}

/// Writes the site record `record` straight into Git, as a later task's navigation editor
/// will.
fn write_record(server: &TestServer, record: SiteRecord) {
    let author = Author {
        id: "test".into(),
        name: "Test".into(),
    };
    let records = [record];
    let mut change = SitesChange::new(&author, "Set navigation");
    change.sites = &records;
    server.store().write_sites(&change).unwrap();
}

fn nav(label: &str, target: Option<NavTarget>, audience: Vec<Uuid>) -> NavItem {
    NavItem {
        id: Uuid::new_v4(),
        label: label.to_owned(),
        target,
        audience,
        children: Vec::new(),
    }
}

#[tokio::test]
async fn site_from_team_and_communication() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;

    // No template is the Team template.
    let team = create_site(&server, &root, json!({ "name": "Team A" })).await;
    let record = record_of(&server, &team);
    assert_eq!(record.kind, "team");
    assert_eq!(record.template.as_deref(), Some("team"));
    assert_eq!(
        (record.language.as_str(), record.time_zone.as_str()),
        ("en", "UTC")
    );
    assert_eq!(record.navigation, None);
    assert_eq!(
        shape(&server, &team),
        BTreeMap::from([("Documents".to_owned(), "library".to_owned())])
    );

    // The built-ins take the organisation's defaults as they are when the site is made.
    let (status, body) = call(
        &server,
        &root,
        Method::PUT,
        "/api/settings/regional",
        Some(json!({ "default_language": "fr-CA", "default_time_zone": "America/Toronto" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let news = create_site(
        &server,
        &root,
        json!({ "name": "News", "template": "communication" }),
    )
    .await;
    let record = record_of(&server, &news);
    assert_eq!(record.kind, "communication");
    assert_eq!(record.template.as_deref(), Some("communication"));
    assert_eq!(
        (record.language.as_str(), record.time_zone.as_str()),
        ("fr-CA", "America/Toronto")
    );
    assert_eq!(
        shape(&server, &news),
        BTreeMap::from([("Documents".to_owned(), "library".to_owned())])
    );
    let (status, body) = call(
        &server,
        &root,
        Method::GET,
        &format!("/api/sites/{news}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["kind"], "communication");
    assert_eq!(body["language"], "fr-CA");
    assert_eq!(body["owners"], json!(["Root"]));

    // Fields given override the template's.
    let docs = create_site(
        &server,
        &root,
        json!({ "name": "Docs", "template": "team", "description": "Ours",
                "language": "de", "time_zone": "Europe/Berlin" }),
    )
    .await;
    let record = record_of(&server, &docs);
    assert_eq!(
        (
            record.kind.as_str(),
            record.description.as_str(),
            record.language.as_str(),
            record.time_zone.as_str()
        ),
        ("team", "Ours", "de", "Europe/Berlin")
    );
    let settings = server
        .records_db()
        .site_settings(collab_core::ids::NodeId(Uuid::parse_str(&docs).unwrap()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(settings.template.as_deref(), Some("team"));
    assert_eq!(settings.description, "Ours");

    // Bad values and unknown templates make nothing.
    for (body, status, code) in [
        (
            json!({ "name": "X", "template": "wiki" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_request",
        ),
        (
            json!({ "name": "X", "template": Uuid::new_v4().to_string() }),
            StatusCode::NOT_FOUND,
            "not_found",
        ),
        (
            json!({ "name": "X", "language": "english" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_language",
        ),
        (
            json!({ "name": "X", "time_zone": "Mars/Olympus" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_time_zone",
        ),
        (
            json!({ "name": "X", "description": "x".repeat(501) }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_request",
        ),
    ] {
        let (got, answer) = call(&server, &root, Method::POST, "/api/sites", Some(body)).await;
        assert_eq!(
            (got, answer["error"].as_str()),
            (status, Some(code)),
            "{answer}"
        );
    }
    let (_, sites) = call(&server, &root, Method::GET, "/api/sites", None).await;
    assert!(
        !sites.as_array().unwrap().iter().any(|s| s["name"] == "X"),
        "{sites}"
    );
}

#[tokio::test]
async fn site_from_saved_template_has_its_shape_and_no_files() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let source = create_site(&server, &root, json!({ "name": "Source" })).await;
    let documents = node_at(&server, &source, "Documents").to_string();
    let plans = child(&server, &root, &source, "library", "Plans").await;
    let year = child(&server, &root, &plans, "folder", "2027").await;
    child(&server, &root, &year, "folder", "Q1").await;
    let old = child(&server, &root, &documents, "folder", "Old").await;
    child(&server, &root, &old, "folder", "Inside old").await;
    let (status, body) = call(
        &server,
        &root,
        Method::DELETE,
        &format!("/api/nodes/{old}"),
        None,
    )
    .await;
    assert!(status.is_success(), "{body}");
    let (status, body) = call(
        &server,
        &root,
        Method::POST,
        "/api/docs",
        Some(json!({ "parent": year, "name": "Notes.txt" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = call(
        &server,
        &root,
        Method::PATCH,
        &format!("/api/sites/{source}"),
        Some(json!({ "language": "en-GB", "time_zone": "Europe/London" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, saved) = save(&server, &root, &source, "Project").await;
    assert_eq!(status, StatusCode::CREATED, "{saved}");
    assert_eq!(saved["name"], "Project");
    assert_eq!(saved["description"], "For projects");
    assert_eq!(saved["built_in"], false);
    let template = saved["id"].as_str().unwrap().to_owned();

    // Recorded in Git, mirrored, and logged.
    let records = server.store().read_templates().unwrap();
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record.id.to_string(), template);
    assert_eq!(record.created.by.name, "Root");
    let body = serde_json::to_value(&record.body).unwrap();
    assert_eq!(
        body,
        json!({
            "kind": "team",
            "language": "en-GB",
            "time_zone": "Europe/London",
            "libraries": [
                { "name": "Documents", "folders": [] },
                { "name": "Plans", "folders": [
                    { "name": "2027", "folders": [ { "name": "Q1", "folders": [] } ] }
                ] }
            ],
            "navigation": null
        })
    );
    let rows = server.records_db().template_rows().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].body, body);
    let saved_events = events(&server, "template.saved").await;
    assert_eq!(saved_events.len(), 1);
    assert_eq!(saved_events[0]["name"], "Project");
    assert_eq!(saved_events[0]["template"], template);

    // A site made from it has its libraries and folders, and nothing else.
    let copy = create_site(
        &server,
        &root,
        json!({ "name": "Copy", "template": template }),
    )
    .await;
    let expected: BTreeMap<String, String> = [
        ("Documents", "library"),
        ("Plans", "library"),
        ("Plans/2027", "folder"),
        ("Plans/2027/Q1", "folder"),
    ]
    .into_iter()
    .map(|(path, kind)| (path.to_owned(), kind.to_owned()))
    .collect();
    assert_eq!(shape(&server, &copy), expected);
    let record = record_of(&server, &copy);
    assert_eq!(record.template.as_deref(), Some(template.as_str()));
    assert_eq!(
        (record.language.as_str(), record.time_zone.as_str()),
        ("en-GB", "Europe/London")
    );
    let (_, bin) = call(
        &server,
        &root,
        Method::GET,
        &format!("/api/sites/{copy}/bin"),
        None,
    )
    .await;
    assert_eq!(bin["items"], json!([]), "{bin}");

    // In one commit: the commit before it holds none of the new site.
    let repo = gix::open(server.library()).unwrap();
    let tip = repo
        .find_reference("refs/heads/main")
        .unwrap()
        .id()
        .detach();
    for name in ["refs/sharecraft/tree", "refs/sharecraft/sites"] {
        assert_eq!(
            repo.find_reference(name).unwrap().id().detach(),
            tip,
            "{name}"
        );
    }
    let commit = repo.find_commit(tip).unwrap();
    let parent = commit.parent_ids().next().unwrap().detach();
    let before = repo.find_commit(parent).unwrap().tree().unwrap();
    let shard: String = copy.chars().take(2).collect();
    assert!(
        before
            .lookup_entry_by_path(format!("sites/{shard}/{copy}.json"))
            .unwrap()
            .is_none()
    );
    let q1 = node_at(&server, &copy, "Plans/2027/Q1");
    let q1_shard: String = q1.to_string().chars().take(2).collect();
    assert!(
        before
            .lookup_entry_by_path(format!("tree/{q1_shard}/{q1}.json"))
            .unwrap()
            .is_none()
    );
    // Its creator owns it, and each node's creation is logged.
    let (status, body) = call(
        &server,
        &root,
        Method::GET,
        &format!("/api/sites/{copy}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["owners"], json!(["Root"]));
    let created = events(&server, "node.created").await;
    for name in ["Copy", "Plans", "2027", "Q1"] {
        assert!(created.iter().any(|e| e["name"] == name), "{name}");
    }
}

#[tokio::test]
async fn save_then_export_then_import_round_trip() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let source = create_site(&server, &root, json!({ "name": "Source" })).await;
    let other = create_site(&server, &root, json!({ "name": "Other" })).await;
    let plans = child(&server, &root, &source, "library", "Plans").await;
    let q1 = child(&server, &root, &plans, "folder", "Q1").await;
    let (status, body) = call(
        &server,
        &root,
        Method::POST,
        "/api/groups",
        Some(json!({ "name": "Finance" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let finance = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();
    let members = group_of(&server, &root, &source, "members").await;
    let others_members = group_of(&server, &root, &other, "members").await;

    // The site's navigation names its own library, a folder inside it, another site's
    // library, an address, and groups of its own and of others.
    let mut record = record_of(&server, &source);
    let node = |id: &str| Some(NavTarget::Node(Uuid::parse_str(id).unwrap()));
    let mut heading = nav("More", None, Vec::new());
    heading.children = vec![
        nav("Help", Some(NavTarget::Url("/help".into())), Vec::new()),
        nav(
            "Theirs",
            Some(NavTarget::Node(node_at(&server, &other, "Documents"))),
            Vec::new(),
        ),
    ];
    record.navigation = Some(vec![
        nav("", node(&plans), vec![members, finance]),
        nav("Quarter", node(&q1), vec![others_members]),
        nav(
            "Elsewhere",
            Some(NavTarget::Node(node_at(&server, &other, "Documents"))),
            Vec::new(),
        ),
        nav(
            "Web",
            Some(NavTarget::Url("https://example.invalid/".into())),
            vec![members],
        ),
        heading,
    ]);
    write_record(&server, record);

    let (status, saved) = save(&server, &root, &source, "Project: 2027").await;
    assert_eq!(status, StatusCode::CREATED, "{saved}");
    let id = saved["id"].as_str().unwrap().to_owned();

    let response = server
        .http(Some(&root))
        .get(server.url(&format!("/api/templates/{id}/package")))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let disposition = response
        .headers()
        .get("content-disposition")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert!(
        disposition.starts_with("attachment;") && disposition.contains("Project_ 2027.json"),
        "{disposition}"
    );
    assert!(
        response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("application/json")
    );
    let bytes = response.bytes().await.unwrap().to_vec();
    let package: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        package,
        json!({
            "sharecraft_template": 1,
            "name": "Project: 2027",
            "description": "For projects",
            "body": {
                "kind": "team",
                "language": "en",
                "time_zone": "UTC",
                "libraries": [
                    { "name": "Documents", "folders": [] },
                    { "name": "Plans", "folders": [ { "name": "Q1", "folders": [] } ] }
                ],
                "navigation": [
                    { "label": "", "target": { "library": "Plans" },
                      "audience": ["members"], "children": [] },
                    { "label": "Quarter", "target": { "library": "Plans" },
                      "audience": [], "children": [] },
                    { "label": "Web", "target": { "url": "https://example.invalid/" },
                      "audience": ["members"], "children": [] },
                    { "label": "More", "target": null, "audience": [], "children": [
                        { "label": "Help", "target": { "url": "/help" },
                          "audience": [], "children": [] }
                    ] }
                ]
            }
        })
    );
    // No identifier of this installation goes out.
    let text = String::from_utf8(bytes.clone()).unwrap();
    for id in [&source, &other, &plans, &q1, &id] {
        assert!(!text.contains(id.as_str()), "{id} in {text}");
    }
    assert!(!text.contains(&members.to_string()));

    // Imported again, it is the same template under a new id.
    let (status, imported) = import(&server, &root, bytes).await;
    assert_eq!(status, StatusCode::CREATED, "{imported}");
    assert_eq!(imported["cleaned"], json!([]));
    let second = imported["id"].as_str().unwrap().to_owned();
    assert_ne!(second, id);
    let response = server
        .http(Some(&root))
        .get(server.url(&format!("/api/templates/{second}/package")))
        .send()
        .await
        .unwrap();
    let again: Value = response.json().await.unwrap();
    assert_eq!(again, package);
    assert_eq!(events(&server, "template.imported").await.len(), 1);
    assert_eq!(server.store().read_templates().unwrap().len(), 2);

    let list = listed(&server, &root).await;
    assert_eq!(
        list.iter()
            .map(|(_, name, built_in)| (name.as_str(), *built_in))
            .collect::<Vec<_>>(),
        vec![
            ("Team site", true),
            ("Communication site", true),
            ("Project: 2027", false),
            ("Project: 2027", false),
        ]
    );
    assert_eq!(list[0].0, "team");
    assert_eq!(list[1].0, "communication");

    // A built-in's package can be downloaded too.
    let (status, team) = call(
        &server,
        &root,
        Method::GET,
        "/api/templates/team/package",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{team}");
    assert_eq!(
        team["body"]["libraries"],
        json!([{ "name": "Documents", "folders": [] }])
    );
}

#[tokio::test]
async fn hostile_template_package_refused() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let refused =
        |reason: &'static str, package: Value| (reason, serde_json::to_vec(&package).unwrap());
    let mut cases = Vec::new();

    let mut many = good_package();
    many["body"]["libraries"] = (0..101)
        .map(|n| json!({ "name": format!("Library {n}"), "folders": [] }))
        .collect();
    cases.push(refused("too_many_libraries", many));

    let mut wide = good_package();
    wide["body"]["libraries"][0]["folders"] = (0..5_001)
        .map(|n| json!({ "name": format!("{n}"), "folders": [] }))
        .collect();
    cases.push(refused("too_many_folders", wide));

    let mut deep = json!({ "name": "Level 21", "folders": [] });
    for level in (1..21).rev() {
        deep = json!({ "name": format!("Level {level}"), "folders": [deep] });
    }
    let mut too_deep = good_package();
    too_deep["body"]["libraries"][0]["folders"] = json!([deep]);
    cases.push(refused("too_deep", too_deep));

    for url in ["javascript:alert(1)", "//evil.invalid/", "data:text/html,x"] {
        let mut bad = good_package();
        bad["body"]["navigation"][0]["target"] = json!({ "url": url });
        cases.push(refused("invalid_navigation", bad));
    }
    let mut unknown = good_package();
    unknown["files"] = json!([]);
    cases.push(refused("unknown_field", unknown));
    let mut unknown = good_package();
    unknown["body"]["libraries"][0]["members"] = json!(["ann@example.invalid"]);
    cases.push(refused("unknown_field", unknown));
    let mut node = good_package();
    node["body"]["navigation"][0]["target"] = json!({ "node": Uuid::new_v4() });
    cases.push(refused("identifiers", node));
    let mut group = good_package();
    group["body"]["navigation"][0]["audience"] = json!([Uuid::new_v4()]);
    cases.push(refused("identifiers", group));
    let mut version = good_package();
    version["sharecraft_template"] = json!(2);
    cases.push(refused("unknown_version", version));
    let mut kind = good_package();
    kind["body"]["kind"] = json!("wiki");
    cases.push(refused("invalid_kind", kind));
    let mut language = good_package();
    language["body"]["language"] = json!("english");
    cases.push(refused("invalid_language", language));
    let mut zone = good_package();
    zone["body"]["time_zone"] = json!("Mars/Olympus");
    cases.push(refused("invalid_time_zone", zone));
    let mut name = good_package();
    name["name"] = json!("x".repeat(81));
    cases.push(refused("invalid_name", name));

    let mut large = serde_json::to_vec(&good_package()).unwrap();
    large.resize(1024 * 1024 + 1, b' ');
    cases.push(("too_large", large));
    let mut not_utf8 = serde_json::to_vec(&good_package()).unwrap();
    let at = not_utf8.len() - 1;
    not_utf8.insert(at, 0xff);
    cases.push(("not_utf8", not_utf8));
    cases.push(("unreadable", b"{\"sharecraft_template\": 1,".to_vec()));

    for (reason, bytes) in cases {
        let (status, body) = import(&server, &root, bytes).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{reason}: {body}");
        assert_eq!(body["error"], "invalid_template", "{reason}: {body}");
        assert_eq!(body["reason"], reason, "{body}");
    }
    assert!(server.store().read_templates().unwrap().is_empty());
    assert!(events(&server, "template.imported").await.is_empty());

    // Bad names are cleaned, made unique, and reported.
    let mut names = good_package();
    names["body"]["libraries"] = json!([
        { "name": "Bad:Name", "folders": [
            { "name": "trail.", "folders": [] },
            { "name": "CON", "folders": [] },
            { "name": "con_", "folders": [] }
        ] },
        { "name": "bad_name", "folders": [] }
    ]);
    names["body"]["navigation"] = json!(null);
    let (status, body) = import(&server, &root, serde_json::to_vec(&names).unwrap()).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        body["cleaned"],
        json!([
            { "from": "Bad:Name", "to": "Bad_Name" },
            { "from": "trail.", "to": "trail_" },
            { "from": "CON", "to": "CON_" },
            { "from": "con_", "to": "con_ (2)" },
            { "from": "bad_name", "to": "bad_name (2)" }
        ])
    );
    let records = server.store().read_templates().unwrap();
    assert_eq!(records.len(), 1);
    let libraries = serde_json::to_value(&records[0].body.libraries).unwrap();
    assert_eq!(
        libraries,
        json!([
            { "name": "Bad_Name", "folders": [
                { "name": "trail_", "folders": [] },
                { "name": "CON_", "folders": [] },
                { "name": "con_ (2)", "folders": [] }
            ] },
            { "name": "bad_name (2)", "folders": [] }
        ])
    );
    let imported = events(&server, "template.imported").await;
    assert_eq!(imported.len(), 1);
    assert_eq!(imported[0]["cleaned"].as_array().unwrap().len(), 5);

    // The largest package allowed is taken.
    let mut edge = serde_json::to_vec(&good_package()).unwrap();
    edge.resize(1024 * 1024, b' ');
    let (status, body) = import(&server, &root, edge).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

#[tokio::test]
async fn template_navigation_resolved_by_name() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    // The package's Reports library was renamed, but a link still names it.
    let mut package = good_package();
    package["body"]["libraries"] = json!([
        { "name": "Documents", "folders": [] },
        { "name": "Reports 2027", "folders": [] }
    ]);
    package["body"]["navigation"] = json!([
        { "label": "Files", "target": { "library": "documents" },
          "audience": ["members", "owners"], "children": [] },
        { "label": "Reports", "target": { "library": "Reports" },
          "audience": [], "children": [] },
        { "label": "Help", "target": { "url": "https://example.invalid/help" },
          "audience": ["visitors"], "children": [] },
        { "label": "Teams", "target": null, "audience": [], "children": [
            { "label": "", "target": { "library": "Reports 2027" },
              "audience": [], "children": [] },
            { "label": "Old reports", "target": { "library": "Reports" },
              "audience": [], "children": [] }
        ] }
    ]);
    let (status, body) = import(&server, &root, serde_json::to_vec(&package).unwrap()).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let template = body["id"].as_str().unwrap().to_owned();

    let id = create_site(
        &server,
        &root,
        json!({ "name": "From package", "template": template }),
    )
    .await;
    let documents = node_at(&server, &id, "Documents");
    let reports = node_at(&server, &id, "Reports 2027");
    let owners = group_of(&server, &root, &id, "owners").await;
    let members = group_of(&server, &root, &id, "members").await;
    let visitors = group_of(&server, &root, &id, "visitors").await;
    let record = record_of(&server, &id);
    let navigation = record.navigation.unwrap();
    let simple: Vec<(String, Option<NavTarget>, Vec<Uuid>, usize)> = navigation
        .iter()
        .map(|item| {
            (
                item.label.clone(),
                item.target.clone(),
                item.audience.clone(),
                item.children.len(),
            )
        })
        .collect();
    assert_eq!(
        simple,
        vec![
            (
                "Files".to_owned(),
                Some(NavTarget::Node(documents)),
                vec![members, owners],
                0
            ),
            (
                "Help".to_owned(),
                Some(NavTarget::Url("https://example.invalid/help".into())),
                vec![visitors],
                0
            ),
            ("Teams".to_owned(), None, Vec::new(), 1),
        ]
    );
    assert_eq!(
        navigation[2].children[0].target,
        Some(NavTarget::Node(reports))
    );
    assert_eq!(navigation[2].children[0].label, "");
    // Each item has an id of its own.
    let mut ids: Vec<Uuid> = navigation
        .iter()
        .flat_map(|item| std::iter::once(item.id).chain(item.children.iter().map(|c| c.id)))
        .collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 4);
    // The row mirrors the record.
    let settings = server
        .records_db()
        .site_settings(collab_core::ids::NodeId(Uuid::parse_str(&id).unwrap()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(settings.navigation, Some(navigation));
}

#[tokio::test]
async fn templates_are_for_administrators() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let owner = server.user("Olive").await;
    let site = create_site(&server, &root, json!({ "name": "Team" })).await;
    let owners = group_of(&server, &root, &site, "owners").await;
    let (status, body) = call(
        &server,
        &root,
        Method::POST,
        &format!("/api/groups/{owners}/members"),
        Some(json!({ "email": owner.email })),
    )
    .await;
    assert!(status.is_success(), "{body}");
    let (status, saved) = save(&server, &root, &site, "Project").await;
    assert_eq!(status, StatusCode::CREATED, "{saved}");
    let template = saved["id"].as_str().unwrap().to_owned();

    // The site's owner may manage it, but not work with templates.
    let package = serde_json::to_vec(&good_package()).unwrap();
    let mut answers = vec![
        call(&server, &owner, Method::GET, "/api/templates", None).await,
        save(&server, &owner, &site, "Mine").await,
        call(
            &server,
            &owner,
            Method::GET,
            &format!("/api/templates/{template}/package"),
            None,
        )
        .await,
        call(
            &server,
            &owner,
            Method::GET,
            "/api/templates/team/package",
            None,
        )
        .await,
        call(
            &server,
            &owner,
            Method::DELETE,
            &format!("/api/templates/{template}"),
            None,
        )
        .await,
        call(
            &server,
            &owner,
            Method::POST,
            "/api/sites",
            Some(json!({ "name": "Mine", "template": template })),
        )
        .await,
    ];
    answers.push(import(&server, &owner, package).await);
    for (status, body) in answers {
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    }
    let response = server
        .http(None)
        .get(server.url("/api/templates"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    assert_eq!(server.store().read_templates().unwrap().len(), 1);
    assert_eq!(listed(&server, &root).await.len(), 3);
}

#[tokio::test]
async fn built_ins_cannot_be_deleted() {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    for name in ["team", "communication"] {
        let (status, body) = call(
            &server,
            &root,
            Method::DELETE,
            &format!("/api/templates/{name}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    }
    let site = create_site(&server, &root, json!({ "name": "Team" })).await;
    let (status, saved) = save(&server, &root, &site, "Project").await;
    assert_eq!(status, StatusCode::CREATED, "{saved}");
    let template = saved["id"].as_str().unwrap().to_owned();

    let (status, body) = call(
        &server,
        &root,
        Method::DELETE,
        &format!("/api/templates/{template}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let list = listed(&server, &root).await;
    assert_eq!(
        list.iter()
            .map(|(id, _, _)| id.as_str())
            .collect::<Vec<_>>(),
        vec!["team", "communication"]
    );
    // Soft-deleted in Git and in the mirror, and logged.
    let records = server.store().read_templates().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].deleted.as_ref().unwrap().by.name, "Root");
    let rows = server.records_db().template_rows().await.unwrap();
    assert!(rows[0].deleted_at.is_some());
    let deleted = events(&server, "template.deleted").await;
    assert_eq!(deleted.len(), 1);
    assert_eq!(deleted[0]["name"], "Project");

    // A deleted template makes no site, has no package, and cannot be deleted again.
    for (method, path, body) in [
        (
            Method::POST,
            "/api/sites".to_owned(),
            Some(json!({ "name": "Late", "template": template })),
        ),
        (
            Method::GET,
            format!("/api/templates/{template}/package"),
            None,
        ),
        (Method::DELETE, format!("/api/templates/{template}"), None),
        (
            Method::DELETE,
            format!("/api/templates/{}", Uuid::new_v4()),
            None,
        ),
    ] {
        let (status, answer) = call(&server, &root, method, &path, body).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}: {answer}");
    }
    // The built-ins are still there.
    create_site(
        &server,
        &root,
        json!({ "name": "Still", "template": "team" }),
    )
    .await;
}
