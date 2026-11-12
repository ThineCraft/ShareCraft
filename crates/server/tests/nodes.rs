//! Sites, libraries, folders and inherited access over HTTP, and how tree and access changes
//! reach live sessions.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::collections::HashSet;
use std::time::Duration;

use collab_core::ids::{DocId, Seq};
use common::{FileClient, TestClient, TestServer, TestUser, spawn_server};
use protocol::RemovedReason;
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

/// How soon a person must be out of a session once their access ended.
const PROMPTLY: Duration = Duration::from_secs(1);

fn error(code: &str) -> Value {
    json!({ "error": code })
}

/// Sends one request as `user` (or unsigned) and returns the status and JSON body (`null` when
/// the body is empty).
async fn call(
    server: &TestServer,
    user: Option<&TestUser>,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = server.http(user).request(method, server.url(path));
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

/// A server with a site made by the administrator Root, who owns it, and its library.
struct World {
    server: TestServer,
    root: TestUser,
    site: String,
    library: String,
}

async fn world() -> World {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let (status, site) = call(
        &server,
        Some(&root),
        Method::POST,
        "/api/sites",
        Some(json!({ "name": "Team" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let site = site["id"].as_str().unwrap().to_owned();
    let (status, page) = call(
        &server,
        Some(&root),
        Method::GET,
        &format!("/api/nodes/{site}/children"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["items"][0]["kind"], "library");
    assert_eq!(page["items"][0]["name"], "Documents");
    let library = page["items"][0]["id"].as_str().unwrap().to_owned();
    World {
        server,
        root,
        site,
        library,
    }
}

impl World {
    /// The groups of the site, as the administrator sees them.
    async fn site_groups(&self) -> Vec<records::Group> {
        let site = uuid::Uuid::parse_str(&self.site).unwrap();
        self.server
            .db
            .db
            .groups_visible_to(self.root.id)
            .await
            .unwrap()
            .into_iter()
            .filter(|g| g.site.map(|s| s.0) == Some(site))
            .collect()
    }

    async fn call(
        &self,
        user: &TestUser,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        call(&self.server, Some(user), method, path, body).await
    }

    /// Creates a `kind` named `name` in `parent` as `user`; returns its id.
    async fn child(&self, user: &TestUser, parent: &str, kind: &str, name: &str) -> String {
        let (status, body) = self
            .call(
                user,
                Method::POST,
                &format!("/api/nodes/{parent}/children"),
                Some(json!({ "kind": kind, "name": name })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        body["id"].as_str().unwrap().to_owned()
    }

    async fn folder(&self, user: &TestUser, parent: &str, name: &str) -> String {
        self.child(user, parent, "folder", name).await
    }

    /// Creates a text document named `name` in `parent` as `user`; returns its id.
    async fn doc(&self, user: &TestUser, parent: &str, name: &str) -> String {
        let (status, body) = self
            .call(
                user,
                Method::POST,
                "/api/docs",
                Some(json!({ "parent": parent, "name": name })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        body["id"].as_str().unwrap().to_owned()
    }

    /// Gives `user` the role `role` on `node`, as `owner`.
    async fn grant(&self, owner: &TestUser, node: &str, user: &TestUser, role: &str) {
        let (status, body) = self
            .call(
                owner,
                Method::PUT,
                &format!("/api/nodes/{node}/access"),
                Some(json!({ "email": user.email, "role": role })),
            )
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    }

    /// `PATCH /api/nodes/{node}` with `body`, as `user`.
    async fn patch(&self, user: &TestUser, node: &str, body: Value) -> (StatusCode, Value) {
        self.call(
            user,
            Method::PATCH,
            &format!("/api/nodes/{node}"),
            Some(body),
        )
        .await
    }

    /// The names of the children of `node` that `user` sees, in order; `None` for a `404`.
    async fn names(&self, user: &TestUser, node: &str) -> Option<Vec<String>> {
        let (status, page) = self
            .call(
                user,
                Method::GET,
                &format!("/api/nodes/{node}/children"),
                None,
            )
            .await;
        if status == StatusCode::NOT_FOUND {
            return None;
        }
        assert_eq!(status, StatusCode::OK);
        Some(
            page["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item["name"].as_str().unwrap().to_owned())
                .collect(),
        )
    }
}

fn doc_id(id: &str) -> DocId {
    DocId(id.parse().unwrap())
}

/// Waits at most [`PROMPTLY`] for a text session to be removed, and returns why.
async fn removed_promptly(c: &mut TestClient) -> RemovedReason {
    tokio::time::timeout(PROMPTLY, c.pump_until(|c| c.removed.is_some()))
        .await
        .expect("not removed within one second");
    c.removed.unwrap()
}

/// Waits at most [`PROMPTLY`] for a file session to be removed, and returns why.
async fn file_removed_promptly(c: &mut FileClient) -> RemovedReason {
    tokio::time::timeout(PROMPTLY, c.pump_until(|c| c.removed.is_some()))
        .await
        .expect("not removed within one second");
    c.removed.unwrap()
}

// ---- Routes ----

#[tokio::test]
async fn sites_are_listed_and_created_by_administrators() {
    let w = world().await;
    let ann = w.server.user("Ann").await;
    let bob = w.server.user("Bob").await;

    // The creator owns the site.
    let (status, sites) = w.call(&w.root, Method::GET, "/api/sites", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        sites,
        json!([{ "id": w.site, "name": "Team", "role": "owner" }])
    );

    // Only an administrator creates one.
    let (status, body) = w
        .call(
            &ann,
            Method::POST,
            "/api/sites",
            Some(json!({ "name": "Mine" })),
        )
        .await;
    assert_eq!((status, body), (StatusCode::FORBIDDEN, error("forbidden")));
    let (status, body) = w
        .call(
            &w.root,
            Method::POST,
            "/api/sites",
            Some(json!({ "name": "TEAM" })),
        )
        .await;
    assert_eq!((status, body), (StatusCode::CONFLICT, error("name_taken")));
    let (status, body) = w
        .call(
            &w.root,
            Method::POST,
            "/api/sites",
            Some(json!({ "name": "a/b" })),
        )
        .await;
    assert_eq!(
        (status, body),
        (StatusCode::UNPROCESSABLE_ENTITY, error("invalid_name"))
    );
    let (status, site) = w
        .call(
            &w.root,
            Method::POST,
            "/api/sites",
            Some(json!({ "name": "Other" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(site["kind"], "site");
    assert_eq!(site["name"], "Other");
    assert_eq!(site["parent"], Value::Null);
    assert_eq!(site["inherits"], false);

    // A person with a role only on a file inside sees the site with limited access; a person
    // with none does not see it.
    let folder = w.folder(&w.root, &w.library, "Inside").await;
    let doc = w.doc(&w.root, &folder, "Plan").await;
    w.grant(&w.root, &doc, &ann, "viewer").await;
    let (_, sites) = w.call(&ann, Method::GET, "/api/sites", None).await;
    assert_eq!(
        sites,
        json!([{ "id": w.site, "name": "Team", "role": null }])
    );
    let (_, sites) = w.call(&bob, Method::GET, "/api/sites", None).await;
    assert_eq!(sites, json!([]));
}

#[tokio::test]
async fn node_shows_its_role_editor_and_path() {
    let w = world().await;
    let ann = w.server.user("Ann").await;
    let folder = w.folder(&w.root, &w.library, "Plans").await;
    let (status, body) = w
        .call(
            &w.root,
            Method::POST,
            "/api/docs",
            Some(json!({ "parent": folder, "name": "Budget.xlsx", "editor": "gridcraft" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let doc = body["id"].as_str().unwrap().to_owned();
    w.grant(&w.root, &folder, &ann, "editor").await;

    let (status, node) = w
        .call(&ann, Method::GET, &format!("/api/nodes/{doc}"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        node,
        json!({
            "id": doc,
            "kind": "document",
            "name": "Budget.xlsx",
            "role": "editor",
            "editor": "gridcraft",
            "path": [
                { "id": w.site, "kind": "site", "name": "Team", "role": null },
                { "id": w.library, "kind": "library", "name": "Documents", "role": null },
                { "id": folder, "kind": "folder", "name": "Plans", "role": "editor" },
                { "id": doc, "kind": "document", "name": "Budget.xlsx", "role": "editor" },
            ],
        })
    );
    // A folder has no editor.
    let (_, node) = w
        .call(&ann, Method::GET, &format!("/api/nodes/{folder}"), None)
        .await;
    assert!(node.get("editor").is_none());
    assert_eq!(node["path"].as_array().unwrap().len(), 3);

    // Someone with no role here, an unknown id and a malformed one.
    let bob = w.server.user("Bob").await;
    let not_found = (StatusCode::NOT_FOUND, error("not_found"));
    let (status, body) = w
        .call(&bob, Method::GET, &format!("/api/nodes/{doc}"), None)
        .await;
    assert_eq!((status, body), not_found);
    let unknown = uuid::Uuid::new_v4();
    let (status, body) = w
        .call(&ann, Method::GET, &format!("/api/nodes/{unknown}"), None)
        .await;
    assert_eq!((status, body), not_found);
    let (status, body) = w
        .call(&ann, Method::GET, "/api/nodes/not-an-id", None)
        .await;
    assert_eq!(
        (status, body),
        (StatusCode::UNPROCESSABLE_ENTITY, error("invalid_request"))
    );
}

#[tokio::test]
async fn children_are_listed_with_their_details() {
    let w = world().await;
    let ann = w.server.user("Ann").await;
    let folder = w.folder(&w.root, &w.library, "B folder").await;
    let doc = w.doc(&w.root, &w.library, "A note").await;
    w.grant(&w.root, &doc, &ann, "viewer").await;

    let (status, page) = w
        .call(
            &w.root,
            Method::GET,
            &format!("/api/nodes/{}/children", w.library),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    // Folders first, whatever their names.
    assert_eq!(
        page,
        json!({
            "items": [
                { "id": folder, "kind": "folder", "name": "B folder", "role": "owner",
                  "inherits": true },
                { "id": doc, "kind": "document", "name": "A note", "role": "owner",
                  "editor": "text", "inherits": false },
            ],
            "next": null,
        })
    );

    // Limited access: only what leads to the person's own role.
    let (_, page) = w
        .call(
            &ann,
            Method::GET,
            &format!("/api/nodes/{}/children", w.library),
            None,
        )
        .await;
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    assert_eq!(page["items"][0]["id"], doc.as_str());
    assert_eq!(page["items"][0]["role"], "viewer");

    // Refusals: no reach, a limit out of range, a cursor that is not one.
    let bob = w.server.user("Bob").await;
    let path = format!("/api/nodes/{}/children", w.library);
    let (status, body) = w.call(&bob, Method::GET, &path, None).await;
    assert_eq!((status, body), (StatusCode::NOT_FOUND, error("not_found")));
    let invalid = (StatusCode::UNPROCESSABLE_ENTITY, error("invalid_request"));
    for query in ["limit=0", "limit=501", "after=zz", "after=7b7d"] {
        let (status, body) = w
            .call(&w.root, Method::GET, &format!("{path}?{query}"), None)
            .await;
        assert_eq!((status, body), invalid, "{query}");
    }
    let (status, _) = w
        .call(&w.root, Method::GET, &format!("{path}?limit=many"), None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn children_paging_over_http() {
    let w = world().await;
    let mut expected = Vec::new();
    for n in (0..5).rev() {
        w.folder(&w.root, &w.library, &format!("folder {n}")).await;
    }
    for n in (0..7).rev() {
        w.doc(&w.root, &w.library, &format!("File {n}")).await;
    }
    for n in 0..5 {
        expected.push(format!("folder {n}"));
    }
    for n in 0..7 {
        expected.push(format!("File {n}"));
    }

    let mut seen = Vec::new();
    let mut after: Option<String> = None;
    let mut pages = 0;
    loop {
        let mut path = format!("/api/nodes/{}/children?limit=4", w.library);
        if let Some(cursor) = &after {
            path.push_str(&format!("&after={cursor}"));
        }
        let (status, page) = w.call(&w.root, Method::GET, &path, None).await;
        assert_eq!(status, StatusCode::OK);
        pages += 1;
        let items = page["items"].as_array().unwrap();
        assert!(items.len() <= 4);
        seen.extend(
            items
                .iter()
                .map(|item| item["name"].as_str().unwrap().to_owned()),
        );
        match page["next"].as_str() {
            Some(next) => after = Some(next.to_owned()),
            None => break,
        }
    }
    assert_eq!(pages, 3);
    assert_eq!(seen, expected);

    // The default page holds them all.
    let all = w.names(&w.root, &w.library).await.unwrap();
    assert_eq!(all, expected);
}

#[tokio::test]
async fn libraries_and_folders_are_created_where_they_belong() {
    let w = world().await;
    let ed = w.server.user("Ed").await;
    let vic = w.server.user("Vic").await;
    w.grant(&w.root, &w.site, &ed, "editor").await;
    w.grant(&w.root, &w.site, &vic, "viewer").await;
    let create = |parent: &str| format!("/api/nodes/{parent}/children");

    // A library: owners of the site.
    let (status, library) = w
        .call(
            &w.root,
            Method::POST,
            &create(&w.site),
            Some(json!({ "kind": "library", "name": "Archive" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(library["kind"], "library");
    assert_eq!(library["parent"], w.site.as_str());
    assert_eq!(library["site"], w.site.as_str());
    assert_eq!(library["inherits"], true);
    let (status, body) = w
        .call(
            &ed,
            Method::POST,
            &create(&w.site),
            Some(json!({ "kind": "library", "name": "Mine" })),
        )
        .await;
    assert_eq!((status, body), (StatusCode::FORBIDDEN, error("forbidden")));

    // A folder: editors of the library or folder it goes into.
    let folder = w.folder(&ed, &w.library, "Plans").await;
    w.folder(&ed, &folder, "Inner").await;
    let (status, body) = w
        .call(
            &vic,
            Method::POST,
            &create(&w.library),
            Some(json!({ "kind": "folder", "name": "Mine" })),
        )
        .await;
    assert_eq!((status, body), (StatusCode::FORBIDDEN, error("forbidden")));

    // Kinds that cannot go there.
    let wrong = (StatusCode::BAD_REQUEST, error("wrong_place"));
    for (parent, kind) in [
        (&w.site, "folder"),
        (&w.library, "library"),
        (&w.library, "site"),
        (&w.library, "document"),
    ] {
        let (status, body) = w
            .call(
                &w.root,
                Method::POST,
                &create(parent),
                Some(json!({ "kind": kind, "name": "X" })),
            )
            .await;
        assert_eq!((status, body), wrong, "{kind}");
    }
    let (status, body) = w
        .call(
            &w.root,
            Method::POST,
            &create(&w.library),
            Some(json!({ "kind": "drawer", "name": "X" })),
        )
        .await;
    assert_eq!(
        (status, body),
        (StatusCode::UNPROCESSABLE_ENTITY, error("invalid_request"))
    );

    // Names: taken ignoring case, or refused.
    let (status, body) = w
        .call(
            &ed,
            Method::POST,
            &create(&w.library),
            Some(json!({ "kind": "folder", "name": "PLANS" })),
        )
        .await;
    assert_eq!((status, body), (StatusCode::CONFLICT, error("name_taken")));
    let (status, body) = w
        .call(
            &ed,
            Method::POST,
            &create(&w.library),
            Some(json!({ "kind": "folder", "name": "end." })),
        )
        .await;
    assert_eq!(
        (status, body),
        (StatusCode::UNPROCESSABLE_ENTITY, error("invalid_name"))
    );

    // Someone with no role at all.
    let bob = w.server.user("Bob").await;
    let (status, body) = w
        .call(
            &bob,
            Method::POST,
            &create(&w.library),
            Some(json!({ "kind": "folder", "name": "Mine" })),
        )
        .await;
    assert_eq!((status, body), (StatusCode::NOT_FOUND, error("not_found")));
}

#[tokio::test]
async fn documents_are_created_and_uploaded_into_a_parent() {
    let w = world().await;
    let ed = w.server.user("Ed").await;
    let vic = w.server.user("Vic").await;
    w.grant(&w.root, &w.library, &ed, "editor").await;
    w.grant(&w.root, &w.library, &vic, "viewer").await;

    let (status, body) = w
        .call(
            &ed,
            Method::POST,
            "/api/docs",
            Some(json!({ "parent": w.library, "name": "Plan" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    // The document inherits: its creator has the role they have on the library.
    assert_eq!(body["role"], "editor");

    let refused = [
        (
            &ed,
            json!({ "name": "No parent" }),
            StatusCode::BAD_REQUEST,
            "bad_request",
        ),
        (
            &ed,
            json!({ "parent": "nope", "name": "X" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_request",
        ),
        (
            &w.root,
            json!({ "parent": w.site, "name": "X" }),
            StatusCode::BAD_REQUEST,
            "wrong_place",
        ),
        (
            &vic,
            json!({ "parent": w.library, "name": "X" }),
            StatusCode::FORBIDDEN,
            "forbidden",
        ),
        (
            &ed,
            json!({ "parent": w.library, "name": "plan" }),
            StatusCode::CONFLICT,
            "name_taken",
        ),
    ];
    for (user, body, status, code) in refused {
        let (got, answer) = w
            .call(user, Method::POST, "/api/docs", Some(body.clone()))
            .await;
        assert_eq!((got, answer), (status, error(code)), "{body}");
    }

    // An upload names its parent too.
    let upload = |user: &TestUser, query: String| {
        w.server
            .http(Some(user))
            .post(w.server.url(&format!("/api/docs/upload?{query}")))
            .body(b"data".to_vec())
            .send()
    };
    let response = upload(&ed, format!("parent={}&name=Sheet.xlsx", w.library))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["role"], "editor");
    let response = upload(&ed, "name=Other.xlsx".into()).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let response = upload(&vic, format!("parent={}&name=Other.xlsx", w.library))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = upload(&ed, format!("parent={}&name=SHEET.xlsx", w.library))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);

    // The home page lists sites now.
    let (status, _) = w.call(&ed, Method::GET, "/api/docs", None).await;
    assert_ne!(status, StatusCode::OK);
}

#[tokio::test]
async fn rename_needs_editor_and_owner_for_libraries_and_sites() {
    let w = world().await;
    let ed = w.server.user("Ed").await;
    let vic = w.server.user("Vic").await;
    w.grant(&w.root, &w.site, &ed, "editor").await;
    w.grant(&w.root, &w.site, &vic, "viewer").await;
    let folder = w.folder(&ed, &w.library, "Plans").await;
    let doc = w.doc(&ed, &w.library, "Notes").await;

    let (status, node) = w.patch(&ed, &folder, json!({ "name": "Ideas" })).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(node["name"], "Ideas");
    assert_eq!(node["id"], folder.as_str());
    let (status, _) = w.patch(&ed, &doc, json!({ "name": "Minutes" })).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        w.names(&ed, &w.library).await.unwrap(),
        vec!["Ideas", "Minutes"]
    );

    let forbidden = (StatusCode::FORBIDDEN, error("forbidden"));
    assert_eq!(w.patch(&vic, &doc, json!({ "name": "X" })).await, forbidden);
    assert_eq!(
        w.patch(&ed, &w.library, json!({ "name": "X" })).await,
        forbidden
    );
    assert_eq!(
        w.patch(&ed, &w.site, json!({ "name": "X" })).await,
        forbidden
    );
    let (status, node) = w
        .patch(&w.root, &w.library, json!({ "name": "Files" }))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(node["name"], "Files");
    let (status, _) = w.patch(&w.root, &w.site, json!({ "name": "Crew" })).await;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(
        w.patch(&ed, &doc, json!({ "name": "ideas" })).await,
        (StatusCode::CONFLICT, error("name_taken"))
    );
    assert_eq!(
        w.patch(&ed, &doc, json!({ "name": "a:b" })).await,
        (StatusCode::UNPROCESSABLE_ENTITY, error("invalid_name"))
    );
    assert_eq!(
        w.patch(&ed, &doc, json!({})).await,
        (StatusCode::UNPROCESSABLE_ENTITY, error("invalid_request"))
    );
    let bob = w.server.user("Bob").await;
    assert_eq!(
        w.patch(&bob, &doc, json!({ "name": "X" })).await,
        (StatusCode::NOT_FOUND, error("not_found"))
    );
}

#[tokio::test]
async fn move_needs_editor_at_both_ends_and_a_place_that_fits() {
    let w = world().await;
    let ed = w.server.user("Ed").await;
    let a = w.folder(&w.root, &w.library, "A").await;
    let b = w.folder(&w.root, &a, "B").await;
    let c = w.folder(&w.root, &w.library, "C").await;
    let doc = w.doc(&w.root, &a, "Notes").await;
    w.doc(&w.root, &c, "notes").await;
    w.grant(&w.root, &a, &ed, "editor").await;
    w.grant(&w.root, &c, &ed, "viewer").await;

    // Into a folder they edit.
    let (status, node) = w.patch(&ed, &doc, json!({ "parent": b })).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(node["parent"], b.as_str());
    assert_eq!(w.names(&ed, &b).await.unwrap(), vec!["Notes"]);

    // Into a folder they only view.
    assert_eq!(
        w.patch(&ed, &doc, json!({ "parent": c })).await,
        (StatusCode::FORBIDDEN, error("forbidden"))
    );
    // Into itself, or below itself.
    let into_itself = (StatusCode::BAD_REQUEST, error("into_itself"));
    assert_eq!(
        w.patch(&w.root, &a, json!({ "parent": a })).await,
        into_itself
    );
    assert_eq!(
        w.patch(&w.root, &a, json!({ "parent": b })).await,
        into_itself
    );
    // Into a site, or moving a library.
    let wrong = (StatusCode::BAD_REQUEST, error("wrong_place"));
    assert_eq!(
        w.patch(&w.root, &a, json!({ "parent": w.site })).await,
        wrong
    );
    assert_eq!(
        w.patch(&w.root, &w.library, json!({ "parent": a })).await,
        wrong
    );
    // Onto a name already there.
    assert_eq!(
        w.patch(&w.root, &doc, json!({ "parent": c })).await,
        (StatusCode::CONFLICT, error("name_taken"))
    );
    // Somewhere that does not exist.
    let unknown = uuid::Uuid::new_v4().to_string();
    assert_eq!(
        w.patch(&w.root, &doc, json!({ "parent": unknown })).await,
        (StatusCode::NOT_FOUND, error("not_found"))
    );
    // Rename and move at once.
    let (status, node) = w
        .patch(
            &w.root,
            &doc,
            json!({ "name": "Moved", "parent": w.library }),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(node["name"], "Moved");
    assert_eq!(node["parent"], w.library.as_str());
}

#[tokio::test]
async fn delete_takes_files_and_empty_folders_only() {
    let w = world().await;
    let w = &w;
    let ed = w.server.user("Ed").await;
    let vic = w.server.user("Vic").await;
    w.grant(&w.root, &w.site, &ed, "editor").await;
    w.grant(&w.root, &w.site, &vic, "viewer").await;
    let folder = w.folder(&ed, &w.library, "Plans").await;
    let doc = w.doc(&ed, &folder, "Notes").await;
    let delete = |user: &TestUser, node: &str| {
        let path = format!("/api/nodes/{node}");
        let user = user.clone();
        async move { w.call(&user, Method::DELETE, &path, None).await }
    };

    assert_eq!(
        delete(&vic, &doc).await,
        (StatusCode::FORBIDDEN, error("forbidden"))
    );
    assert_eq!(
        delete(&ed, &folder).await,
        (StatusCode::CONFLICT, error("not_empty"))
    );
    assert_eq!(
        delete(&w.root, &w.site).await,
        (StatusCode::BAD_REQUEST, error("is_site"))
    );
    assert_eq!(
        delete(&w.root, &w.library).await,
        (StatusCode::BAD_REQUEST, error("wrong_place"))
    );
    assert_eq!(
        delete(&ed, &doc).await,
        (StatusCode::NO_CONTENT, Value::Null)
    );
    assert_eq!(
        delete(&ed, &folder).await,
        (StatusCode::NO_CONTENT, Value::Null)
    );
    assert_eq!(
        w.names(&ed, &w.library).await.unwrap(),
        Vec::<String>::new()
    );
    let not_found = (StatusCode::NOT_FOUND, error("not_found"));
    assert_eq!(delete(&ed, &doc).await, not_found);
    let (status, body) = w
        .call(&ed, Method::GET, &format!("/api/docs/{doc}"), None)
        .await;
    assert_eq!((status, body), not_found);
}

#[tokio::test]
async fn access_is_seen_and_changed_by_owners_on_any_node() {
    let w = world().await;
    let ann = w.server.user("Ann").await;
    let ed = w.server.user("Ed").await;
    let folder = w.folder(&w.root, &w.library, "Plans").await;
    w.grant(&w.root, &w.site, &ed, "editor").await;
    let path = format!("/api/nodes/{folder}/access");

    // The folder inherits from the site.
    let (status, access) = w.call(&w.root, Method::GET, &path, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(access["inherits"], true);
    assert_eq!(
        access["source"],
        json!({ "id": w.site, "kind": "site", "name": "Team", "role": "owner" })
    );
    let emails: HashSet<_> = access["grants"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["user"]["email"].as_str().unwrap().to_owned())
        .collect();
    // The site's creator owns it as a member of its Owners group, not by a grant of their own.
    assert_eq!(emails, HashSet::from([ed.email.clone()]));
    let groups = w.site_groups().await;
    let owners = groups
        .iter()
        .find(|g| g.purpose == Some(records::SiteGroup::Owners))
        .unwrap();
    let (_, members) = w
        .server
        .db
        .db
        .group(w.root.id, owners.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        members.iter().map(|u| u.id).collect::<Vec<_>>(),
        [w.root.id]
    );

    // Granting on the folder breaks its inheritance.
    w.grant(&w.root, &folder, &ann, "owner").await;
    let (_, access) = w.call(&ann, Method::GET, &path, None).await;
    assert_eq!(access["inherits"], false);
    assert_eq!(access["source"]["id"], folder.as_str());
    assert_eq!(access["grants"].as_array().unwrap().len(), 2);

    // Editors cannot see or change it; others cannot find it.
    let forbidden = (StatusCode::FORBIDDEN, error("forbidden"));
    assert_eq!(w.call(&ed, Method::GET, &path, None).await, forbidden);
    assert_eq!(
        w.call(
            &ed,
            Method::PUT,
            &path,
            Some(json!({ "email": ann.email, "role": "viewer" }))
        )
        .await,
        forbidden
    );
    let bob = w.server.user("Bob").await;
    assert_eq!(
        w.call(&bob, Method::GET, &path, None).await,
        (StatusCode::NOT_FOUND, error("not_found"))
    );
    assert_eq!(
        w.call(
            &ann,
            Method::PUT,
            &path,
            Some(json!({ "email": "nobody@test.invalid", "role": "viewer" }))
        )
        .await,
        (StatusCode::NOT_FOUND, error("no_such_user"))
    );

    // Removing an assignment; the last owner stays. The copies of the site's groups go too
    // (through the records: until the API speaks of groups, it names people only).
    let (status, _) = w
        .call(&ann, Method::DELETE, &format!("{path}/{}", ed.id.0), None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let folder_id = collab_core::ids::NodeId(uuid::Uuid::parse_str(&folder).unwrap());
    for group in &groups {
        w.server
            .db
            .db
            .set_node_access(ann.id, folder_id, records::Principal::Group(group.id), None)
            .await
            .unwrap();
    }
    assert_eq!(
        w.call(&ann, Method::DELETE, &format!("{path}/{}", ann.id.0), None)
            .await,
        (StatusCode::CONFLICT, error("last_manager"))
    );
}

#[tokio::test]
async fn inheritance_is_broken_and_restored_by_owners() {
    let w = world().await;
    let w = &w;
    let ann = w.server.user("Ann").await;
    let folder = w.folder(&w.root, &w.library, "Plans").await;
    w.grant(&w.root, &w.site, &ann, "editor").await;
    let inherit = |user: &TestUser, node: &str, value: bool| {
        let path = format!("/api/nodes/{node}/inheritance");
        let user = user.clone();
        async move {
            w.call(
                &user,
                Method::POST,
                &path,
                Some(json!({ "inherit": value })),
            )
            .await
        }
    };
    let inherits = |node: &str| {
        let path = format!("/api/nodes/{node}/access");
        async move { w.call(&w.root, Method::GET, &path, None).await.1["inherits"].clone() }
    };

    assert_eq!(
        inherit(&ann, &folder, false).await,
        (StatusCode::FORBIDDEN, error("forbidden"))
    );
    assert_eq!(
        inherit(&w.root, &folder, false).await,
        (StatusCode::NO_CONTENT, Value::Null)
    );
    assert_eq!(inherits(&folder).await, false);
    // The copy keeps everyone's role.
    let (_, node) = w
        .call(&ann, Method::GET, &format!("/api/nodes/{folder}"), None)
        .await;
    assert_eq!(node["role"], "editor");
    // Asking again changes nothing.
    assert_eq!(
        inherit(&w.root, &folder, false).await,
        (StatusCode::NO_CONTENT, Value::Null)
    );
    assert_eq!(
        inherit(&w.root, &folder, true).await,
        (StatusCode::NO_CONTENT, Value::Null)
    );
    assert_eq!(inherits(&folder).await, true);
    assert_eq!(
        inherit(&w.root, &w.site, true).await,
        (StatusCode::BAD_REQUEST, error("is_site"))
    );
    assert_eq!(
        w.call(
            &w.root,
            Method::POST,
            &format!("/api/nodes/{folder}/inheritance"),
            Some(json!({ "inherit": "yes" }))
        )
        .await,
        (StatusCode::BAD_REQUEST, error("bad_request"))
    );
}

#[tokio::test]
async fn doc_access_alias_matches_node_route() {
    let w = world().await;
    let w = &w;
    let ann = w.server.user("Ann").await;
    let bob = w.server.user("Bob").await;
    let doc = w.doc(&w.root, &w.library, "Plan").await;
    let node_path = format!("/api/nodes/{doc}/access");
    let doc_path = format!("/api/docs/{doc}/access");
    let same = |user: &TestUser| {
        let user = user.clone();
        let (node_path, doc_path) = (node_path.clone(), doc_path.clone());
        async move {
            let by_node = w.call(&user, Method::GET, &node_path, None).await;
            let by_doc = w.call(&user, Method::GET, &doc_path, None).await;
            assert_eq!(by_doc, by_node);
            by_doc
        }
    };

    let (status, access) = same(&w.root).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(access["inherits"], true);
    let (status, _) = same(&bob).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A change through the alias is a change of the node.
    let (status, _) = w
        .call(
            &w.root,
            Method::PUT,
            &doc_path,
            Some(json!({ "email": ann.email, "role": "editor" })),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, access) = same(&w.root).await;
    assert_eq!(access["inherits"], false);
    assert!(
        access["grants"]
            .as_array()
            .unwrap()
            .iter()
            .any(|g| g["user"]["id"] == ann.id.0.to_string() && g["role"] == "editor")
    );
    assert_eq!(same(&ann).await.1, error("forbidden"));
    let (status, _) = w
        .call(
            &w.root,
            Method::DELETE,
            &format!("{doc_path}/{}", ann.id.0),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(same(&ann).await.1, error("not_found"));
}

#[tokio::test]
async fn file_reached_only_through_its_path() {
    let w = world().await;
    let bob = w.server.user("Bob").await;
    let a = w.folder(&w.root, &w.library, "A").await;
    w.folder(&w.root, &w.library, "Z sibling").await;
    let b = w.folder(&w.root, &a, "B").await;
    w.folder(&w.root, &a, "C sibling").await;
    let file = w.doc(&w.root, &b, "Target").await;
    w.doc(&w.root, &b, "Other file").await;
    w.grant(&w.root, &file, &bob, "viewer").await;

    // By link.
    let (status, doc) = w
        .call(&bob, Method::GET, &format!("/api/docs/{file}"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(doc["role"], "viewer");

    // From the sites list down, one level at a time, never seeing a sibling.
    let (_, sites) = w.call(&bob, Method::GET, "/api/sites", None).await;
    assert_eq!(
        sites,
        json!([{ "id": w.site, "name": "Team", "role": null }])
    );
    assert_eq!(w.names(&bob, &w.site).await.unwrap(), vec!["Documents"]);
    assert_eq!(w.names(&bob, &w.library).await.unwrap(), vec!["A"]);
    assert_eq!(w.names(&bob, &a).await.unwrap(), vec!["B"]);
    assert_eq!(w.names(&bob, &b).await.unwrap(), vec!["Target"]);
    let (_, page) = w
        .call(&bob, Method::GET, &format!("/api/nodes/{b}/children"), None)
        .await;
    assert_eq!(page["items"][0]["role"], "viewer");
    let (_, page) = w
        .call(&bob, Method::GET, &format!("/api/nodes/{a}/children"), None)
        .await;
    assert_eq!(page["items"][0]["role"], Value::Null);

    // The breadcrumb names the way there, with no role above the file.
    let (_, node) = w
        .call(&bob, Method::GET, &format!("/api/nodes/{file}"), None)
        .await;
    let roles: Vec<_> = node["path"]
        .as_array()
        .unwrap()
        .iter()
        .map(|crumb| (crumb["name"].clone(), crumb["role"].clone()))
        .collect();
    assert_eq!(
        roles,
        vec![
            (json!("Team"), Value::Null),
            (json!("Documents"), Value::Null),
            (json!("A"), Value::Null),
            (json!("B"), Value::Null),
            (json!("Target"), json!("viewer")),
        ]
    );
    // Limited access gives no way to change anything on the way.
    assert_eq!(
        w.patch(&bob, &a, json!({ "name": "Mine" })).await,
        (StatusCode::NOT_FOUND, error("not_found"))
    );
}

// ---- Live sessions ----

#[tokio::test]
async fn site_grant_reaches_open_file() {
    let w = world().await;
    let bob = w.server.user("Bob").await;
    let (status, body) = w
        .call(
            &w.root,
            Method::POST,
            "/api/docs",
            Some(json!({ "parent": w.library, "name": "Budget.xlsx", "editor": "gridcraft" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let doc = doc_id(body["id"].as_str().unwrap());
    w.grant(&w.root, &w.site, &bob, "viewer").await;

    let seat = FileClient::connect_as(&w.server, doc, &w.root).await;
    let viewer = FileClient::connect_as(&w.server, doc, &bob).await;
    assert!(seat.can_edit);
    assert!(!viewer.can_edit);

    // Raised on the site, the role reaches the open session.
    let mut viewer = viewer;
    w.grant(&w.root, &w.site, &bob, "editor").await;
    tokio::time::timeout(PROMPTLY, viewer.pump_until(|c| c.can_edit))
        .await
        .expect("the new role did not arrive");
}

#[tokio::test]
async fn site_revoke_removes_member() {
    let w = world().await;
    let bob = w.server.user("Bob").await;
    let doc = doc_id(&w.doc(&w.root, &w.library, "Plan").await);
    w.grant(&w.root, &w.site, &bob, "editor").await;
    let mut root = TestClient::connect_as(&w.server, doc, &w.root).await;
    let mut b = TestClient::connect_as(&w.server, doc, &bob).await;
    root.pump_until(|c| c.people().len() == 2).await;

    let (status, _) = w
        .call(
            &w.root,
            Method::DELETE,
            &format!("/api/nodes/{}/access/{}", w.site, bob.id.0),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(removed_promptly(&mut b).await, RemovedReason::AccessRevoked);
    root.pump_until(|c| c.people().len() == 1).await;
    assert!(root.removed.is_none());
}

/// A folder where Bob is an editor by a grant of the folder's own, and a document in it that
/// Bob has open.
async fn granted_folder(w: &World, bob: &TestUser) -> (String, DocId, TestClient) {
    let folder = w.folder(&w.root, &w.library, "Granted").await;
    w.grant(&w.root, &folder, bob, "editor").await;
    let doc = w.doc(&w.root, &folder, "Plan").await;
    let b = TestClient::connect_as(&w.server, doc_id(&doc), bob).await;
    assert!(b.can_edit);
    (folder, doc_id(&doc), b)
}

#[tokio::test]
async fn restore_above_open_file_removes_member() {
    let w = world().await;
    let bob = w.server.user("Bob").await;
    let (folder, _, mut b) = granted_folder(&w, &bob).await;

    let (status, _) = w
        .call(
            &w.root,
            Method::POST,
            &format!("/api/nodes/{folder}/inheritance"),
            Some(json!({ "inherit": true })),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(removed_promptly(&mut b).await, RemovedReason::AccessRevoked);
}

#[tokio::test]
async fn move_out_of_granted_folder_removes_member() {
    let w = world().await;
    let bob = w.server.user("Bob").await;
    let (_, doc, mut b) = granted_folder(&w, &bob).await;

    let (status, _) = w
        .patch(&w.root, &doc.0.to_string(), json!({ "parent": w.library }))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(removed_promptly(&mut b).await, RemovedReason::AccessRevoked);
}

#[tokio::test]
async fn move_into_granted_folder_upgrades_member() {
    let w = world().await;
    let bob = w.server.user("Bob").await;
    let folder = w.folder(&w.root, &w.library, "Granted").await;
    w.grant(&w.root, &folder, &bob, "editor").await;
    w.grant(&w.root, &w.site, &bob, "viewer").await;
    let doc = w.doc(&w.root, &w.library, "Plan").await;
    let mut b = TestClient::connect_as(&w.server, doc_id(&doc), &bob).await;
    assert!(!b.can_edit);

    let (status, _) = w.patch(&w.root, &doc, json!({ "parent": folder })).await;
    assert_eq!(status, StatusCode::OK);
    tokio::time::timeout(PROMPTLY, b.pump_until(|c| c.can_edit))
        .await
        .expect("the new role did not arrive");
    assert!(b.removed.is_none());
    b.type_at(0, "now editable").await;
    b.pump_until(|c| c.pending() == 0 && c.seq() == Seq(1))
        .await;
}

#[tokio::test]
async fn delete_open_file_removes_everyone() {
    let w = world().await;
    let bob = w.server.user("Bob").await;
    let doc = w.doc(&w.root, &w.library, "Plan").await;
    w.grant(&w.root, &w.site, &bob, "editor").await;
    let mut root = TestClient::connect_as(&w.server, doc_id(&doc), &w.root).await;
    let mut b = TestClient::connect_as(&w.server, doc_id(&doc), &bob).await;
    root.pump_until(|c| c.people().len() == 2).await;

    let (status, _) = w
        .call(&w.root, Method::DELETE, &format!("/api/nodes/{doc}"), None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(removed_promptly(&mut root).await, RemovedReason::Deleted);
    assert_eq!(removed_promptly(&mut b).await, RemovedReason::Deleted);

    // A file session too.
    let (_, body) = w
        .call(
            &w.root,
            Method::POST,
            "/api/docs",
            Some(json!({ "parent": w.library, "name": "Sheet.xlsx", "editor": "gridcraft" })),
        )
        .await;
    let file = body["id"].as_str().unwrap().to_owned();
    let mut seat = FileClient::connect_as(&w.server, doc_id(&file), &w.root).await;
    let mut other = FileClient::connect_as(&w.server, doc_id(&file), &bob).await;
    let (status, _) = w
        .call(&w.root, Method::DELETE, &format!("/api/nodes/{file}"), None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        file_removed_promptly(&mut seat).await,
        RemovedReason::Deleted
    );
    assert_eq!(
        file_removed_promptly(&mut other).await,
        RemovedReason::Deleted
    );
}

#[tokio::test]
async fn rename_open_file_then_checkpoint_uses_new_name() {
    let w = world().await;
    let doc = w.doc(&w.root, &w.library, "Plan.txt").await;
    let id = doc_id(&doc);
    let mut a = TestClient::connect_as(&w.server, id, &w.root).await;
    a.type_at(0, "abc").await;
    a.pump_until(|c| c.pending() == 0 && c.seq() == Seq(1))
        .await;
    a.wait_saved(Seq(1)).await;
    let stored = w.server.store().read(id, None).unwrap().unwrap();
    assert_eq!(stored.meta.file_name, "Plan.txt");

    let (status, _) = w.patch(&w.root, &doc, json!({ "name": "Notes.txt" })).await;
    assert_eq!(status, StatusCode::OK);
    a.type_at(3, "def").await;
    a.pump_until(|c| c.pending() == 0 && c.seq() == Seq(2))
        .await;
    // Saved, not refused as stale: the tree commit did not move the document's ref.
    a.wait_saved(Seq(2)).await;
    let stored = w.server.store().read(id, None).unwrap().unwrap();
    assert_eq!(stored.meta.file_name, "Notes.txt");
    assert_eq!(stored.bytes, b"abcdef");
    assert!(a.removed.is_none());
}

#[tokio::test]
async fn delete_repaired_after_a_database_failure_still_removes_everyone() {
    let w = world().await;
    let doc = w.doc(&w.root, &w.library, "Plan").await;
    let mut root = TestClient::connect_as(&w.server, doc_id(&doc), &w.root).await;
    // The database fails on the delete's own event, after Git recorded it; the repair from Git
    // (whose event is marked as a repair) then deletes the node in PostgreSQL.
    let pool = w.server.records_db();
    sqlx::raw_sql(
        "create function refuse_events() returns trigger language plpgsql as $$ \
         begin if new.kind = 'node.deleted' and not (new.detail ? 'repaired') then \
         raise exception 'database down'; end if; return new; end $$; \
         create trigger refuse_events before insert on events \
         for each row execute function refuse_events();",
    )
    .execute(pool.test_pool())
    .await
    .unwrap();

    let (status, _) = w
        .call(&w.root, Method::DELETE, &format!("/api/nodes/{doc}"), None)
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        pool.node(doc_id(&doc).into())
            .await
            .unwrap()
            .unwrap()
            .deleted
    );
    assert_eq!(removed_promptly(&mut root).await, RemovedReason::Deleted);
}
