//! The inheritance explorer, its counts and CSV export, and the restore preview, over HTTP.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use common::{TestServer, TestUser, role_access, spawn_server};
use reqwest::header::{CONTENT_DISPOSITION, CONTENT_TYPE};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

fn error(code: &str) -> Value {
    json!({ "error": code })
}

/// Sends one request as `user` and returns the status and JSON body (`null` when empty).
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
    let bytes = response.bytes().await.unwrap();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, body)
}

/// A site Team made by the administrator Root, with breaks: the site itself; the folder
/// Shared, which Ann owns, and Shared/Inner below it; the folder Hidden Folder and the file
/// Secret Plan.txt in it, which Ann cannot reach.
struct World {
    server: TestServer,
    root: TestUser,
    ann: TestUser,
    site: String,
    library: String,
    shared: String,
    inner: String,
    hidden: String,
    secret: String,
}

async fn world() -> World {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let ann = server.user("Ann").await;
    let (status, site) = call(
        &server,
        &root,
        Method::POST,
        "/api/sites",
        Some(json!({ "name": "Team" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let site = site["id"].as_str().unwrap().to_owned();
    let (_, page) = call(
        &server,
        &root,
        Method::GET,
        &format!("/api/nodes/{site}/children"),
        None,
    )
    .await;
    let library = page["items"][0]["id"].as_str().unwrap().to_owned();
    let mut w = World {
        server,
        root,
        ann,
        site,
        library,
        shared: String::new(),
        inner: String::new(),
        hidden: String::new(),
        secret: String::new(),
    };
    w.shared = w.folder(&w.library, "Shared").await;
    w.inner = w.folder(&w.shared, "Inner").await;
    w.hidden = w.folder(&w.library, "Hidden Folder").await;
    let (status, body) = w
        .call(
            &w.root,
            Method::POST,
            "/api/docs",
            Some(json!({ "parent": w.hidden, "name": "Secret Plan.txt" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    w.secret = body["id"].as_str().unwrap().to_owned();
    let ann_email = w.ann.email.clone();
    w.share(&w.shared, &ann_email, "owner").await;
    for node in [&w.inner, &w.hidden, &w.secret] {
        w.break_inheritance(node).await;
    }
    w
}

impl World {
    async fn call(
        &self,
        user: &TestUser,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        call(&self.server, user, method, path, body).await
    }

    async fn get(&self, user: &TestUser, path: &str) -> (StatusCode, Value) {
        self.call(user, Method::GET, path, None).await
    }

    async fn folder(&self, parent: &str, name: &str) -> String {
        let (status, body) = self
            .call(
                &self.root,
                Method::POST,
                &format!("/api/nodes/{parent}/children"),
                Some(json!({ "kind": "folder", "name": name })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        body["id"].as_str().unwrap().to_owned()
    }

    async fn share(&self, node: &str, email: &str, role: &str) {
        let (status, body) = self
            .call(
                &self.root,
                Method::PUT,
                &format!("/api/nodes/{node}/access"),
                Some(json!({ "email": email, "role": role })),
            )
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    }

    async fn break_inheritance(&self, node: &str) {
        let (status, body) = self
            .call(
                &self.root,
                Method::POST,
                &format!("/api/nodes/{node}/inheritance"),
                Some(json!({ "inherit": false })),
            )
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    }

    /// The names of the breaks in `page`.
    fn names(page: &Value) -> Vec<String> {
        page["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["node"]["name"].as_str().unwrap().to_owned())
            .collect()
    }
}

/// The text of `GET path` as `user`, which must succeed.
async fn text(w: &World, user: &TestUser, path: &str) -> String {
    let response = w
        .server
        .http(Some(user))
        .get(w.server.url(path))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "{path}");
    response.text().await.unwrap()
}

#[tokio::test]
async fn explorer_lists_breaks_in_the_order_of_the_tree() {
    let w = world().await;
    let (status, page) = w.get(&w.root, &format!("/api/explorer/{}", w.site)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        World::names(&page),
        [
            "Team",
            "Hidden Folder",
            "Secret Plan.txt",
            "Shared",
            "Inner"
        ]
    );
    assert_eq!(page["next"], Value::Null);

    // One break in full.
    let hidden = &page["items"][1];
    assert_eq!(hidden["kind"], "folder");
    assert_eq!(
        hidden["node"],
        json!({
            "id": w.hidden,
            "kind": "folder",
            "name": "Hidden Folder",
            "access": role_access("owner")
        })
    );
    let path: Vec<_> = hidden["path"]
        .as_array()
        .unwrap()
        .iter()
        .map(|crumb| crumb["name"].clone())
        .collect();
    assert_eq!(path, [json!("Team"), json!("Documents")]);
    assert_eq!(hidden["inherited_from"]["id"], w.site.as_str());
    assert_eq!(hidden["breaks_below"], 1);
    assert_eq!(hidden["broken_by"]["user"]["name"], "Root");
    assert!(hidden["broken_by"]["at"].is_string());
    assert_eq!(hidden["changed_by"]["user"]["id"], w.root.id.0.to_string());
    let assigned: Vec<_> = hidden["assignments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| {
            (
                a["principal"]["name"].clone(),
                a["principal"]["site"].clone(),
                a["role"]["id"].clone(),
            )
        })
        .collect();
    let team = json!({ "id": w.site, "name": "Team" });
    assert_eq!(
        assigned,
        vec![
            (json!("Team Members"), team.clone(), json!("editor")),
            (json!("Team Owners"), team.clone(), json!("owner")),
            (json!("Team Visitors"), team.clone(), json!("viewer")),
        ]
    );

    // Pages.
    let root = format!("/api/explorer/{}", w.site);
    let (_, first) = w.get(&w.root, &format!("{root}?limit=2")).await;
    assert_eq!(World::names(&first), ["Team", "Hidden Folder"]);
    let next = first["next"].as_str().unwrap().to_owned();
    let (status, second) = w
        .get(
            &w.root,
            &format!("{root}?limit=2&after={}", urlencode(&next)),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{second}");
    assert_eq!(World::names(&second), ["Secret Plan.txt", "Shared"]);

    // Filters.
    let (_, page) = w
        .get(&w.root, &format!("{root}?principal=user:{}", w.ann.id.0))
        .await;
    assert_eq!(World::names(&page), ["Shared", "Inner"]);
    let (_, page) = w.get(&w.root, &format!("{root}?kind=document")).await;
    assert_eq!(World::names(&page), ["Secret Plan.txt"]);
    let (_, page) = w.get(&w.root, &format!("{root}?kind=site,document")).await;
    assert_eq!(World::names(&page), ["Team", "Secret Plan.txt"]);

    // Requests that make no sense.
    let invalid = (StatusCode::UNPROCESSABLE_ENTITY, error("invalid_request"));
    for query in [
        "after=not-a-cursor",
        "after=%5B1%5D",
        "limit=0",
        "limit=501",
        "principal=ann",
        "principal=robot:00000000-0000-0000-0000-000000000000",
        "kind=shelf",
    ] {
        assert_eq!(
            w.get(&w.root, &format!("{root}?{query}")).await,
            invalid,
            "{query}"
        );
    }
    assert_eq!(w.get(&w.root, "/api/explorer/nonsense").await, invalid);
}

#[tokio::test]
async fn explorer_over_http_stays_in_scope() {
    let w = world().await;
    let ann = &w.ann;

    // Ann owns Shared: she sees its subtree and nothing else.
    let (status, page) = w.get(ann, &format!("/api/explorer/{}", w.shared)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(World::names(&page), ["Shared", "Inner"]);
    // Shared would inherit from the site, which Ann does not manage: it is not named.
    assert_eq!(page["items"][0]["inherited_from"], Value::Null);
    assert_eq!(page["items"][0]["path"], json!([]));
    let body = text(&w, ann, &format!("/api/explorer/{}", w.shared)).await;
    for unseen in ["Hidden", "Secret", w.hidden.as_str(), w.secret.as_str()] {
        assert!(!body.contains(unseen), "{unseen} in {body}");
    }

    // Anywhere else she cannot start.
    let not_found = (StatusCode::NOT_FOUND, error("not_found"));
    assert_eq!(
        w.get(ann, &format!("/api/explorer/{}", w.site)).await,
        not_found
    );
    assert_eq!(
        w.get(ann, &format!("/api/explorer/{}", w.hidden)).await,
        not_found
    );
    assert_eq!(
        w.get(ann, &format!("/api/explorer/{}/counts", w.library))
            .await,
        not_found
    );
    assert_eq!(
        w.get(ann, &format!("/api/explorer/{}/export.csv", w.site))
            .await,
        not_found
    );
    // An editor of Shared may not use it.
    let ed = w.server.user("Ed").await;
    w.share(&w.shared, &ed.email, "editor").await;
    assert_eq!(
        w.get(&ed, &format!("/api/explorer/{}", w.shared)).await,
        (StatusCode::FORBIDDEN, error("forbidden"))
    );

    // Counts: only below her root.
    let (status, counts) = w
        .get(ann, &format!("/api/explorer/{}/counts", w.shared))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(counts, json!([{ "id": w.inner, "count": 1 }]));
    assert_eq!(
        w.get(
            ann,
            &format!("/api/explorer/{}/counts?parent={}", w.shared, w.library)
        )
        .await,
        (StatusCode::UNPROCESSABLE_ENTITY, error("invalid_request"))
    );

    // The export too.
    let csv = text(&w, ann, &format!("/api/explorer/{}/export.csv", w.shared)).await;
    assert!(csv.contains("Shared / Inner"), "{csv}");
    assert!(!csv.contains("Hidden") && !csv.contains("Secret"), "{csv}");
}

#[tokio::test]
async fn counts_follow_the_tree() {
    let w = world().await;
    let (status, counts) = w
        .get(&w.root, &format!("/api/explorer/{}/counts", w.site))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(counts, json!([{ "id": w.library, "count": 4 }]));
    let (_, counts) = w
        .get(
            &w.root,
            &format!("/api/explorer/{}/counts?parent={}", w.site, w.library),
        )
        .await;
    assert_eq!(
        counts,
        json!([{ "id": w.hidden, "count": 2 }, { "id": w.shared, "count": 2 }])
    );
}

#[tokio::test]
async fn export_is_csv_attachment() {
    let w = world().await;
    let response = w
        .server
        .http(Some(&w.root))
        .get(
            w.server
                .url(&format!("/api/explorer/{}/export.csv?kind=folder", w.site)),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response.headers()[CONTENT_TYPE]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(content_type.starts_with("text/csv"), "{content_type}");
    let disposition = response.headers()[CONTENT_DISPOSITION]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(disposition.starts_with("attachment"), "{disposition}");
    assert!(disposition.contains(".csv"), "{disposition}");
    let csv = response.text().await.unwrap();
    let lines: Vec<_> = csv.split_terminator("\r\n").collect();
    assert_eq!(
        lines[0],
        "path,kind,principal kind,principal,role,permissions,broken by,broken at"
    );
    // Hidden Folder (the three site groups), Shared (Ann and the groups), Inner (the same).
    assert_eq!(lines.len(), 1 + 3 + 4 + 4, "{csv}");
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("Team / Documents / Shared,folder,person,Ann,Owner,")),
        "{csv}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("Team / Documents / Hidden Folder,folder,site group,Team Owners,Owner,view add edit delete manage_access manage_site,Root,")),
        "{csv}"
    );
    // The same refusals as the explorer.
    assert_eq!(
        w.get(
            &w.root,
            &format!("/api/explorer/{}/export.csv?kind=shelf", w.site)
        )
        .await,
        (StatusCode::UNPROCESSABLE_ENTITY, error("invalid_request"))
    );
}

/// The export holds every break, in the order of the tree, however many pages the listing
/// takes: here the listing needs three pages of two.
#[tokio::test]
async fn export_holds_every_page() {
    let w = world().await;
    let mut listed = Vec::new();
    let mut after: Option<String> = None;
    let mut pages = 0;
    loop {
        let path = match &after {
            Some(after) => format!(
                "/api/explorer/{}?limit=2&after={}",
                w.site,
                urlencode(after)
            ),
            None => format!("/api/explorer/{}?limit=2", w.site),
        };
        let (status, page) = w.get(&w.root, &path).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        pages += 1;
        listed.extend(World::names(&page));
        match page["next"].as_str() {
            Some(next) => after = Some(next.to_owned()),
            None => break,
        }
    }
    assert_eq!(pages, 3);
    let csv = text(&w, &w.root, &format!("/api/explorer/{}/export.csv", w.site)).await;
    let mut exported: Vec<String> = Vec::new();
    for line in csv
        .split_terminator(
            "
",
        )
        .skip(1)
    {
        let path = line.split(',').next().unwrap();
        let name = path.rsplit(" / ").next().unwrap().to_owned();
        if exported.last() != Some(&name) {
            exported.push(name);
        }
    }
    assert_eq!(exported, listed, "{csv}");
}

#[tokio::test]
async fn restore_preview_says_what_would_change() {
    let w = world().await;
    let path = format!("/api/nodes/{}/inheritance/preview", w.shared);
    let (status, preview) = w.get(&w.root, &path).await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    assert_eq!(preview["node"]["id"], w.shared.as_str());
    assert_eq!(
        preview["inherit_from"],
        json!({ "id": w.site, "kind": "site", "name": "Team", "access": role_access("owner") })
    );
    assert_eq!(
        preview["people"],
        json!([{
            "user": { "id": w.ann.id.0.to_string(), "name": "Ann" },
            "before": role_access("owner")["perms"],
            "after": []
        }])
    );
    assert_eq!(preview["groups"], json!([]));
    assert_eq!(preview["unchanged_breaks_below"], 1);

    // Ann manages Shared but not the site it would inherit from; Bob nothing; a site never
    // inherits.
    assert_eq!(
        w.get(&w.ann, &path).await,
        (StatusCode::FORBIDDEN, error("forbidden"))
    );
    let bob = w.server.user("Bob").await;
    assert_eq!(
        w.get(&bob, &path).await,
        (StatusCode::NOT_FOUND, error("not_found"))
    );
    assert_eq!(
        w.get(
            &w.root,
            &format!("/api/nodes/{}/inheritance/preview", w.site)
        )
        .await,
        (StatusCode::BAD_REQUEST, error("is_site"))
    );
}

/// `text` with every byte but unreserved ones percent-encoded, for a query value.
fn urlencode(text: &str) -> String {
    text.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

#[tokio::test]
async fn groups_of_unreachable_sites_are_never_named() {
    let w = world().await;
    let (status, other) = w
        .call(
            &w.root,
            Method::POST,
            "/api/sites",
            Some(json!({ "name": "Elsewhere" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let visitors = other["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["purpose"] == "visitors")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    // The other site's Visitors may view Shared; Ann, who owns Team, cannot reach that site.
    let (status, _) = w
        .call(
            &w.root,
            Method::PUT,
            &format!("/api/nodes/{}/access", w.shared),
            Some(json!({ "group": visitors, "role": "viewer" })),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let ann_email = w.ann.email.clone();
    w.share(&w.site, &ann_email, "owner").await;
    let hidden = json!({ "kind": "group", "id": visitors, "name": null, "hidden": true });

    let access = text(&w, &w.ann, &format!("/api/nodes/{}/access", w.shared)).await;
    assert!(!access.contains("Elsewhere"), "{access}");
    let access: Value = serde_json::from_str(&access).unwrap();
    assert!(
        access["assignments"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["principal"] == hidden),
        "{access}"
    );

    let explorer = text(&w, &w.ann, &format!("/api/explorer/{}", w.shared)).await;
    assert!(!explorer.contains("Elsewhere"), "{explorer}");

    let preview = text(
        &w,
        &w.ann,
        &format!("/api/nodes/{}/inheritance/preview", w.shared),
    )
    .await;
    assert!(!preview.contains("Elsewhere"), "{preview}");
    let preview: Value = serde_json::from_str(&preview).unwrap();
    assert_eq!(
        preview["groups"],
        json!([{ "group": hidden, "before": ["view"], "after": [] }])
    );
}
