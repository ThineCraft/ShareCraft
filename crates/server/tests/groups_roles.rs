//! Roles, groups and access given to groups over HTTP, and how membership and role changes
//! reach live sessions.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::time::Duration;

use collab_core::ids::DocId;
use common::{TestClient, TestServer, TestUser, role_access, spawn_server};
use protocol::RemovedReason;
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

/// How soon an open session must follow a change of access.
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

/// A server with a site named Team made by the administrator Root, its library, and its
/// three groups as `POST /api/sites` answered them.
struct World {
    server: TestServer,
    root: TestUser,
    site: String,
    library: String,
    groups: Value,
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
    assert_eq!(status, StatusCode::CREATED, "{site}");
    let groups = site["groups"].clone();
    let site = site["id"].as_str().unwrap().to_owned();
    let (_, page) = call(
        &server,
        Some(&root),
        Method::GET,
        &format!("/api/nodes/{site}/children"),
        None,
    )
    .await;
    let library = page["items"][0]["id"].as_str().unwrap().to_owned();
    World {
        server,
        root,
        site,
        library,
        groups,
    }
}

impl World {
    async fn call(
        &self,
        user: &TestUser,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        call(&self.server, Some(user), method, path, body).await
    }

    /// The id of the site's group for `purpose` (`owners`, `members` or `visitors`).
    fn group(&self, purpose: &str) -> String {
        self.groups
            .as_array()
            .unwrap()
            .iter()
            .find(|g| g["purpose"] == purpose)
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    /// Creates a `kind` named `name` in `parent` as `user`; returns the status and body.
    async fn try_child(
        &self,
        user: &TestUser,
        parent: &str,
        kind: &str,
        name: &str,
    ) -> (StatusCode, Value) {
        self.call(
            user,
            Method::POST,
            &format!("/api/nodes/{parent}/children"),
            Some(json!({ "kind": kind, "name": name })),
        )
        .await
    }

    async fn folder(&self, user: &TestUser, parent: &str, name: &str) -> String {
        let (status, body) = self.try_child(user, parent, "folder", name).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        body["id"].as_str().unwrap().to_owned()
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

    /// Gives `user` the role `role` (an id) on `node`, as the administrator.
    async fn grant(&self, node: &str, user: &TestUser, role: &str) {
        let (status, body) = self
            .call(
                &self.root,
                Method::PUT,
                &format!("/api/nodes/{node}/access"),
                Some(json!({ "email": user.email, "role": role })),
            )
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    }

    /// Defines a role named `name` with `perms`, as the administrator; returns its id.
    async fn role(&self, name: &str, perms: &[&str]) -> String {
        let (status, body) = self
            .call(
                &self.root,
                Method::POST,
                "/api/roles",
                Some(json!({ "name": name, "perms": perms })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        body["id"].as_str().unwrap().to_owned()
    }

    /// `PUT /api/groups/{group}/members/{user}` as `actor`.
    async fn add(&self, actor: &TestUser, group: &str, user: &TestUser) -> (StatusCode, Value) {
        self.call(
            actor,
            Method::PUT,
            &format!("/api/groups/{group}/members/{}", user.id.0),
            None,
        )
        .await
    }

    /// `DELETE /api/groups/{group}/members/{user}` as `actor`.
    async fn remove(&self, actor: &TestUser, group: &str, user: &TestUser) -> (StatusCode, Value) {
        self.call(
            actor,
            Method::DELETE,
            &format!("/api/groups/{group}/members/{}", user.id.0),
            None,
        )
        .await
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

/// Waits at most [`PROMPTLY`] for a text session's right to edit to become `can`.
async fn can_edit_promptly(c: &mut TestClient, can: bool) {
    tokio::time::timeout(PROMPTLY, c.pump_until(|c| c.can_edit == can))
        .await
        .expect("the new access did not arrive within one second");
}

// ---- Roles ----

#[tokio::test]
async fn roles_are_listed_to_whoever_manages_access() {
    let w = world().await;
    let ann = w.server.user("Ann").await;
    let vic = w.server.user("Vic").await;

    let (status, roles) = w.call(&w.root, Method::GET, "/api/roles", None).await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<_> = roles
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].clone())
        .collect();
    assert_eq!(ids, [json!("viewer"), json!("editor"), json!("owner")]);
    assert_eq!(
        roles[1],
        json!({
            "id": "editor",
            "name": "Editor",
            "perms": ["view", "add", "edit", "delete"],
            "builtin": true
        })
    );

    // An owner of one folder manages access somewhere; a viewer does not.
    let folder = w.folder(&w.root, &w.library, "Plans").await;
    w.grant(&folder, &ann, "owner").await;
    w.grant(&w.site, &vic, "viewer").await;
    let (status, _) = w.call(&ann, Method::GET, "/api/roles", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        w.call(&vic, Method::GET, "/api/roles", None).await,
        (StatusCode::FORBIDDEN, error("forbidden"))
    );
    assert_eq!(
        call(&w.server, None, Method::GET, "/api/roles", None).await,
        (StatusCode::UNAUTHORIZED, error("not_signed_in"))
    );
}

#[tokio::test]
async fn roles_are_defined_by_administrators() {
    let w = world().await;
    let ann = w.server.user("Ann").await;
    let body = json!({ "name": "Reviewer", "perms": ["edit"] });

    let (status, role) = w
        .call(&w.root, Method::POST, "/api/roles", Some(body.clone()))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(role["name"], "Reviewer");
    assert_eq!(role["perms"], json!(["view", "edit"]));
    assert_eq!(role["builtin"], false);
    let (_, roles) = w.call(&w.root, Method::GET, "/api/roles", None).await;
    assert_eq!(roles[3]["id"], role["id"]);

    let refusals = [
        (&ann, body.clone(), StatusCode::FORBIDDEN, "forbidden"),
        (
            &w.root,
            json!({ "name": "reviewer", "perms": [] }),
            StatusCode::CONFLICT,
            "name_taken",
        ),
        (
            &w.root,
            json!({ "name": "  ", "perms": ["view"] }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_name",
        ),
        (
            &w.root,
            json!({ "name": "Other", "perms": ["fly"] }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_request",
        ),
        (
            &w.root,
            json!({ "name": "Other" }),
            StatusCode::BAD_REQUEST,
            "bad_request",
        ),
    ];
    for (user, body, status, code) in refusals {
        assert_eq!(
            w.call(user, Method::POST, "/api/roles", Some(body.clone()))
                .await,
            (status, error(code)),
            "{body}"
        );
    }
}

#[tokio::test]
async fn roles_are_changed_by_administrators() {
    let w = world().await;
    let ann = w.server.user("Ann").await;
    let id = w.role("Reviewer", &["edit"]).await;
    let path = format!("/api/roles/{id}");

    let (status, role) = w
        .call(
            &w.root,
            Method::PATCH,
            &path,
            Some(json!({ "name": "Checker", "perms": ["add"] })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        role,
        json!({ "id": id, "name": "Checker", "perms": ["view", "add"], "builtin": false })
    );
    let (_, role) = w
        .call(
            &w.root,
            Method::PATCH,
            &path,
            Some(json!({ "name": "Reader" })),
        )
        .await;
    assert_eq!(role["perms"], json!(["view", "add"]));

    let refusals = [
        (
            &ann,
            path.clone(),
            json!({ "name": "X" }),
            StatusCode::FORBIDDEN,
            "forbidden",
        ),
        (
            &w.root,
            "/api/roles/editor".to_owned(),
            json!({ "name": "X" }),
            StatusCode::BAD_REQUEST,
            "builtin_role",
        ),
        (
            &w.root,
            "/api/roles/no-such-role".to_owned(),
            json!({ "name": "X" }),
            StatusCode::NOT_FOUND,
            "not_found",
        ),
        (
            &w.root,
            path.clone(),
            json!({}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_request",
        ),
        (
            &w.root,
            path.clone(),
            json!({ "perms": ["fly"] }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_request",
        ),
        (
            &w.root,
            path.clone(),
            json!({ "name": "viewer" }),
            StatusCode::CONFLICT,
            "name_taken",
        ),
    ];
    for (user, path, body, status, code) in refusals {
        assert_eq!(
            w.call(user, Method::PATCH, &path, Some(body.clone())).await,
            (status, error(code)),
            "{path} {body}"
        );
    }
}

#[tokio::test]
async fn a_role_keeps_a_manager_on_every_scope() {
    let w = world().await;
    let ann = w.server.user("Ann").await;
    let manager = w.role("Manager", &["manage_access"]).await;
    let folder = w.folder(&w.root, &w.library, "Plans").await;
    w.grant(&folder, &ann, &manager).await;
    // Only Ann's assignment manages the folder now.
    for purpose in ["owners", "members", "visitors"] {
        let (status, body) = w
            .call(
                &w.root,
                Method::DELETE,
                &format!("/api/nodes/{folder}/access/group/{}", w.group(purpose)),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    }
    assert_eq!(
        w.call(
            &w.root,
            Method::PATCH,
            &format!("/api/roles/{manager}"),
            Some(json!({ "perms": ["view"] }))
        )
        .await,
        (StatusCode::CONFLICT, error("last_manager"))
    );
}

#[tokio::test]
async fn roles_are_deleted_by_administrators_when_unused() {
    let w = world().await;
    let ann = w.server.user("Ann").await;
    let used = w.role("Used", &["edit"]).await;
    let unused = w.role("Unused", &["edit"]).await;
    w.grant(&w.library, &ann, &used).await;

    let refusals = [
        (&ann, unused.clone(), StatusCode::FORBIDDEN, "forbidden"),
        (&w.root, used.clone(), StatusCode::CONFLICT, "role_in_use"),
        (
            &w.root,
            "owner".to_owned(),
            StatusCode::BAD_REQUEST,
            "builtin_role",
        ),
        (
            &w.root,
            "no-such-role".to_owned(),
            StatusCode::NOT_FOUND,
            "not_found",
        ),
    ];
    for (user, id, status, code) in refusals {
        assert_eq!(
            w.call(user, Method::DELETE, &format!("/api/roles/{id}"), None)
                .await,
            (status, error(code)),
            "{id}"
        );
    }
    let (status, _) = w
        .call(
            &w.root,
            Method::DELETE,
            &format!("/api/roles/{unused}"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, roles) = w.call(&w.root, Method::GET, "/api/roles", None).await;
    assert!(
        !roles
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == unused.as_str())
    );
}

// ---- Groups ----

#[tokio::test]
async fn site_creation_returns_groups() {
    let w = world().await;
    let groups = w.groups.as_array().unwrap();
    let shown: Vec<_> = groups
        .iter()
        .map(|g| {
            (
                g["name"].clone(),
                g["purpose"].clone(),
                g["site"].clone(),
                g["member_count"].clone(),
            )
        })
        .collect();
    let site = json!({ "id": w.site, "name": "Team" });
    assert_eq!(
        shown,
        vec![
            (
                json!("Team Owners"),
                json!("owners"),
                site.clone(),
                json!(1)
            ),
            (
                json!("Team Members"),
                json!("members"),
                site.clone(),
                json!(0)
            ),
            (
                json!("Team Visitors"),
                json!("visitors"),
                site.clone(),
                json!(0)
            ),
        ]
    );
    // The creator is in the Owners group, which owns the site.
    let (_, detail) = w
        .call(
            &w.root,
            Method::GET,
            &format!("/api/groups/{}", w.group("owners")),
            None,
        )
        .await;
    assert_eq!(detail["members"][0]["id"], w.root.id.0.to_string());
    let (_, access) = w
        .call(
            &w.root,
            Method::GET,
            &format!("/api/nodes/{}/access", w.site),
            None,
        )
        .await;
    let owners = access["assignments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["principal"]["id"] == w.group("owners").as_str())
        .unwrap();
    assert_eq!(
        owners,
        &json!({
            "principal": {
                "kind": "group",
                "id": w.group("owners"),
                "name": "Team Owners",
                "site": { "id": w.site, "name": "Team" }
            },
            "role": { "id": "owner", "name": "Owner" },
            "perms": role_access("owner")["perms"]
        })
    );
}

#[tokio::test]
async fn site_groups_are_listed_for_the_site() {
    let w = world().await;
    let path = format!("/api/sites/{}/groups", w.site);
    let (status, groups) = w.call(&w.root, Method::GET, &path, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(groups, w.groups);

    // Someone who manages access only on a folder sees the groups but may not change them.
    let ann = w.server.user("Ann").await;
    let folder = w.folder(&w.root, &w.library, "Plans").await;
    w.grant(&folder, &ann, "owner").await;
    // Limited access to the site: nothing of its own there.
    assert_eq!(
        w.call(&ann, Method::GET, &path, None).await,
        (StatusCode::NOT_FOUND, error("not_found"))
    );
    w.grant(&w.site, &ann, "viewer").await;
    let (status, groups) = w.call(&ann, Method::GET, &path, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        groups
            .as_array()
            .unwrap()
            .iter()
            .all(|g| g["can_manage"] == false)
    );

    // A viewer who manages access nowhere may not see them; others cannot find the site.
    let vic = w.server.user("Vic").await;
    w.grant(&w.site, &vic, "viewer").await;
    assert_eq!(
        w.call(&vic, Method::GET, &path, None).await,
        (StatusCode::FORBIDDEN, error("forbidden"))
    );
    let bob = w.server.user("Bob").await;
    assert_eq!(
        w.call(&bob, Method::GET, &path, None).await,
        (StatusCode::NOT_FOUND, error("not_found"))
    );
    let (status, _) = w
        .call(
            &w.root,
            Method::GET,
            &format!("/api/sites/{}/groups", w.library),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = w
        .call(&w.root, Method::GET, "/api/sites/nonsense/groups", None)
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn organisation_groups_are_created_and_deleted_by_administrators() {
    let w = world().await;
    let ann = w.server.user("Ann").await;

    let (status, group) = w
        .call(
            &w.root,
            Method::POST,
            "/api/groups",
            Some(json!({ "name": "Finance" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(group["name"], "Finance");
    assert_eq!(group["site"], Value::Null);
    assert_eq!(group["purpose"], Value::Null);
    assert_eq!(group["member_count"], 0);
    let id = group["id"].as_str().unwrap().to_owned();
    let (_, groups) = w.call(&w.root, Method::GET, "/api/groups", None).await;
    let names: Vec<_> = groups
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["name"].clone())
        .collect();
    assert_eq!(
        names,
        [
            json!("Finance"),
            json!("Team Members"),
            json!("Team Owners"),
            json!("Team Visitors")
        ]
    );

    for (user, body, status, code) in [
        (
            &ann,
            json!({ "name": "Mine" }),
            StatusCode::FORBIDDEN,
            "forbidden",
        ),
        (
            &w.root,
            json!({ "name": "FINANCE" }),
            StatusCode::CONFLICT,
            "name_taken",
        ),
        (
            &w.root,
            json!({ "name": "" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_name",
        ),
    ] {
        assert_eq!(
            w.call(user, Method::POST, "/api/groups", Some(body.clone()))
                .await,
            (status, error(code)),
            "{body}"
        );
    }

    // Deleting: not a site's group, not while assigned, administrators only.
    w.call(
        &w.root,
        Method::PUT,
        &format!("/api/nodes/{}/access", w.library),
        Some(json!({ "group": id, "role": "viewer" })),
    )
    .await;
    let path = format!("/api/groups/{id}");
    assert_eq!(
        w.call(&w.root, Method::DELETE, &path, None).await,
        (StatusCode::CONFLICT, error("group_in_use"))
    );
    assert_eq!(
        w.call(&ann, Method::DELETE, &path, None).await,
        (StatusCode::FORBIDDEN, error("forbidden"))
    );
    assert_eq!(
        w.call(
            &w.root,
            Method::DELETE,
            &format!("/api/groups/{}", w.group("members")),
            None
        )
        .await,
        (StatusCode::BAD_REQUEST, error("site_group"))
    );
    assert_eq!(
        w.call(
            &w.root,
            Method::DELETE,
            &format!("/api/groups/{}", uuid::Uuid::new_v4()),
            None
        )
        .await,
        (StatusCode::NOT_FOUND, error("not_found"))
    );
    let (status, _) = w
        .call(
            &w.root,
            Method::DELETE,
            &format!("/api/nodes/{}/access/group/{id}", w.library),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = w.call(&w.root, Method::DELETE, &path, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        w.call(&w.root, Method::GET, &path, None).await,
        (StatusCode::NOT_FOUND, error("not_found"))
    );
}

#[tokio::test]
async fn group_members_are_shown_to_those_who_manage_the_group() {
    let w = world().await;
    let ann = w.server.user("Ann").await;
    let bob = w.server.user("Bob").await;
    let members = w.group("members");
    let path = format!("/api/groups/{members}");
    assert_eq!(
        w.add(&w.root, &members, &bob).await.0,
        StatusCode::NO_CONTENT
    );

    let (status, detail) = w.call(&w.root, Method::GET, &path, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["can_manage"], true);
    assert_eq!(detail["member_count"], 1);
    assert_eq!(
        detail["members"],
        json!([{ "id": bob.id.0.to_string(), "email": bob.email, "name": "Bob" }])
    );

    // An owner of a folder may see the group, but not its members.
    let folder = w.folder(&w.root, &w.library, "Plans").await;
    w.grant(&folder, &ann, "owner").await;
    w.grant(&w.site, &ann, "viewer").await;
    let (status, detail) = w.call(&ann, Method::GET, &path, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["can_manage"], false);
    assert_eq!(detail["members"], json!([]));

    // Someone who manages access nowhere cannot find it.
    assert_eq!(
        w.call(&bob, Method::GET, &path, None).await,
        (StatusCode::NOT_FOUND, error("not_found"))
    );
    assert_eq!(
        w.call(&w.root, Method::GET, "/api/groups/nonsense", None)
            .await,
        (StatusCode::UNPROCESSABLE_ENTITY, error("invalid_request"))
    );
}

#[tokio::test]
async fn members_are_added_and_removed_by_site_managers() {
    let w = world().await;
    let ann = w.server.user("Ann").await;
    let bob = w.server.user("Bob").await;
    let cy = w.server.user("Cy").await;
    // Ann manages the site's groups but holds only view and manage_site on it.
    let site_admin = w.role("Site admin", &["manage_site"]).await;
    w.grant(&w.site, &ann, &site_admin).await;
    let visitors = w.group("visitors");
    let owners = w.group("owners");

    // By id and by email.
    assert_eq!(w.add(&ann, &visitors, &bob).await.0, StatusCode::NO_CONTENT);
    let (status, _) = w
        .call(
            &ann,
            Method::POST,
            &format!("/api/groups/{visitors}/members"),
            Some(json!({ "email": cy.email })),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    // Adding again changes nothing.
    assert_eq!(w.add(&ann, &visitors, &bob).await.0, StatusCode::NO_CONTENT);
    let (_, detail) = w
        .call(
            &w.root,
            Method::GET,
            &format!("/api/groups/{visitors}"),
            None,
        )
        .await;
    assert_eq!(detail["member_count"], 2);
    assert_eq!(
        w.remove(&ann, &visitors, &cy).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        w.remove(&ann, &visitors, &cy).await.0,
        StatusCode::NO_CONTENT
    );

    // Not into a group that gives more than Ann holds.
    assert_eq!(
        w.add(&ann, &owners, &cy).await,
        (StatusCode::FORBIDDEN, error("beyond_your_access"))
    );
    // Nobody without manage_site on the site, and not an organisation group.
    assert_eq!(
        w.add(&bob, &visitors, &cy).await,
        (StatusCode::FORBIDDEN, error("forbidden"))
    );
    let (_, finance) = w
        .call(
            &w.root,
            Method::POST,
            "/api/groups",
            Some(json!({ "name": "Finance" })),
        )
        .await;
    let finance = finance["id"].as_str().unwrap().to_owned();
    assert_eq!(
        w.add(&ann, &finance, &cy).await,
        (StatusCode::FORBIDDEN, error("forbidden"))
    );
    assert_eq!(
        w.add(&w.root, &finance, &cy).await.0,
        StatusCode::NO_CONTENT
    );
    // Unknown people and groups.
    let nobody = format!("/api/groups/{visitors}/members/{}", uuid::Uuid::new_v4());
    assert_eq!(
        w.call(&ann, Method::PUT, &nobody, None).await,
        (StatusCode::NOT_FOUND, error("no_such_user"))
    );
    assert_eq!(
        w.call(
            &ann,
            Method::POST,
            &format!("/api/groups/{visitors}/members"),
            Some(json!({ "email": "nobody@test.invalid" }))
        )
        .await,
        (StatusCode::NOT_FOUND, error("no_such_user"))
    );
    assert_eq!(
        w.call(
            &w.root,
            Method::PUT,
            &format!("/api/groups/{}/members/{}", uuid::Uuid::new_v4(), cy.id.0),
            None
        )
        .await,
        (StatusCode::NOT_FOUND, error("not_found"))
    );
    // Someone who may not change the group cannot learn which emails have accounts.
    let folder = w.folder(&w.root, &w.library, "Plans").await;
    w.grant(&folder, &bob, "owner").await;
    assert_eq!(
        w.call(
            &bob,
            Method::POST,
            &format!("/api/groups/{visitors}/members"),
            Some(json!({ "email": "nobody@test.invalid" }))
        )
        .await,
        (StatusCode::FORBIDDEN, error("forbidden"))
    );
}

#[tokio::test]
async fn group_list_never_names_unreachable_sites() {
    let w = world().await;
    let (status, other) = w
        .call(
            &w.root,
            Method::POST,
            "/api/sites",
            Some(json!({ "name": "Secret Plans" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let other_groups = other["groups"].clone();
    w.call(
        &w.root,
        Method::POST,
        "/api/groups",
        Some(json!({ "name": "Finance" })),
    )
    .await;

    // Ann owns a folder in Team and has no way into the other site.
    let ann = w.server.user("Ann").await;
    let folder = w.folder(&w.root, &w.library, "Plans").await;
    w.grant(&folder, &ann, "owner").await;
    let response = w
        .server
        .http(Some(&ann))
        .get(w.server.url("/api/groups"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let text = response.text().await.unwrap();
    assert!(!text.contains("Secret"), "{text}");
    assert!(!text.contains(other["id"].as_str().unwrap()), "{text}");
    let groups: Value = serde_json::from_str(&text).unwrap();
    let names: Vec<_> = groups
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["name"].clone())
        .collect();
    assert_eq!(
        names,
        [
            json!("Finance"),
            json!("Team Members"),
            json!("Team Owners"),
            json!("Team Visitors")
        ]
    );
    // Nor one at a time, nor to give it a role.
    for group in other_groups.as_array().unwrap() {
        let id = group["id"].as_str().unwrap();
        assert_eq!(
            w.call(&ann, Method::GET, &format!("/api/groups/{id}"), None)
                .await,
            (StatusCode::NOT_FOUND, error("not_found"))
        );
        assert_eq!(
            w.call(
                &ann,
                Method::PUT,
                &format!("/api/nodes/{folder}/access"),
                Some(json!({ "group": id, "role": "viewer" }))
            )
            .await,
            (StatusCode::NOT_FOUND, error("not_found"))
        );
    }
    // Someone who manages access nowhere sees no group at all.
    let bob = w.server.user("Bob").await;
    w.grant(&w.site, &bob, "editor").await;
    assert_eq!(
        w.call(&bob, Method::GET, "/api/groups", None).await,
        (StatusCode::OK, json!([]))
    );
}

// ---- Access with groups ----

#[tokio::test]
async fn access_is_given_to_groups_and_taken_away() {
    let w = world().await;
    let ann = w.server.user("Ann").await;
    let bob = w.server.user("Bob").await;
    let folder = w.folder(&w.root, &w.library, "Plans").await;
    let path = format!("/api/nodes/{folder}/access");
    let (_, finance) = w
        .call(
            &w.root,
            Method::POST,
            "/api/groups",
            Some(json!({ "name": "Finance" })),
        )
        .await;
    let finance = finance["id"].as_str().unwrap().to_owned();
    w.add(&w.root, &finance, &bob).await;

    // Given to a group, the folder breaks inheritance and its members get the role.
    let (status, body) = w
        .call(
            &w.root,
            Method::PUT,
            &path,
            Some(json!({ "group": finance, "role": "editor" })),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (_, access) = w.call(&w.root, Method::GET, &path, None).await;
    assert_eq!(access["inherits"], false);
    let row = access["assignments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["principal"]["id"] == finance.as_str())
        .unwrap()
        .clone();
    assert_eq!(
        row,
        json!({
            "principal": { "kind": "group", "id": finance, "name": "Finance", "site": null },
            "role": { "id": "editor", "name": "Editor" },
            "perms": ["view", "add", "edit", "delete"]
        })
    );
    let (_, node) = w
        .call(&bob, Method::GET, &format!("/api/nodes/{folder}"), None)
        .await;
    assert_eq!(node["access"], role_access("editor"));

    // Refusals.
    let ann_manages = w.role("Access manager", &["manage_access"]).await;
    w.grant(&folder, &ann, &ann_manages).await;
    let refusals = [
        (
            &w.root,
            json!({ "role": "viewer" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_request",
        ),
        (
            &w.root,
            json!({ "email": bob.email, "group": finance, "role": "viewer" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_request",
        ),
        (
            &w.root,
            json!({ "group": uuid::Uuid::new_v4(), "role": "viewer" }),
            StatusCode::NOT_FOUND,
            "not_found",
        ),
        (
            &w.root,
            json!({ "group": finance, "role": "boss" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_request",
        ),
        // Ann manages access but may not give edit, which she lacks.
        (
            &ann,
            json!({ "group": finance, "role": "editor" }),
            StatusCode::FORBIDDEN,
            "beyond_your_access",
        ),
        (
            &ann,
            json!({ "email": bob.email, "role": "owner" }),
            StatusCode::FORBIDDEN,
            "beyond_your_access",
        ),
        (
            &bob,
            json!({ "group": finance, "role": "viewer" }),
            StatusCode::FORBIDDEN,
            "forbidden",
        ),
    ];
    for (user, body, status, code) in refusals {
        assert_eq!(
            w.call(user, Method::PUT, &path, Some(body.clone())).await,
            (status, error(code)),
            "{body}"
        );
    }
    // What Ann holds she may give.
    let (status, _) = w
        .call(
            &ann,
            Method::PUT,
            &path,
            Some(json!({ "group": finance, "role": "viewer" })),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Taken away, by someone who manages access there.
    let group_path = format!("{path}/group/{finance}");
    assert_eq!(
        w.call(&bob, Method::DELETE, &group_path, None).await,
        (StatusCode::FORBIDDEN, error("forbidden"))
    );
    let (status, _) = w.call(&w.root, Method::DELETE, &group_path, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = w
        .call(&bob, Method::GET, &format!("/api/nodes/{folder}"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        w.call(
            &w.root,
            Method::DELETE,
            &format!("{path}/robot/{finance}"),
            None
        )
        .await,
        (StatusCode::NOT_FOUND, error("not_found"))
    );
    assert_eq!(
        w.call(
            &w.root,
            Method::DELETE,
            &format!("{path}/group/nonsense"),
            None
        )
        .await,
        (StatusCode::UNPROCESSABLE_ENTITY, error("invalid_request"))
    );
}

// ---- Live sessions ----

#[tokio::test]
async fn leaving_one_group_keeps_the_other() {
    let w = world().await;
    let bob = w.server.user("Bob").await;
    let doc = w.doc(&w.root, &w.library, "Plan").await;
    let members = w.group("members");
    let visitors = w.group("visitors");
    w.add(&w.root, &members, &bob).await;
    w.add(&w.root, &visitors, &bob).await;
    let mut b = TestClient::connect_as(&w.server, doc_id(&doc), &bob).await;
    assert!(b.can_edit);

    // Out of Members, Bob is still a visitor: read-only, and still there.
    assert_eq!(
        w.remove(&w.root, &members, &bob).await.0,
        StatusCode::NO_CONTENT
    );
    can_edit_promptly(&mut b, false).await;
    assert!(b.removed.is_none());
    let (_, node) = w
        .call(&bob, Method::GET, &format!("/api/nodes/{doc}"), None)
        .await;
    assert_eq!(node["access"], role_access("viewer"));

    // Out of Visitors too, he has nothing left.
    assert_eq!(
        w.remove(&w.root, &visitors, &bob).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(removed_promptly(&mut b).await, RemovedReason::AccessRevoked);
}

#[tokio::test]
async fn removed_from_group_leaves_open_session() {
    let w = world().await;
    let bob = w.server.user("Bob").await;
    let first = w.doc(&w.root, &w.library, "First").await;
    let second = w.doc(&w.root, &w.library, "Second").await;
    let members = w.group("members");
    w.add(&w.root, &members, &bob).await;
    let mut root = TestClient::connect_as(&w.server, doc_id(&first), &w.root).await;
    let mut one = TestClient::connect_as(&w.server, doc_id(&first), &bob).await;
    let mut two = TestClient::connect_as(&w.server, doc_id(&second), &bob).await;
    root.pump_until(|c| c.people().len() == 2).await;

    assert_eq!(
        w.remove(&w.root, &members, &bob).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        removed_promptly(&mut one).await,
        RemovedReason::AccessRevoked
    );
    assert_eq!(
        removed_promptly(&mut two).await,
        RemovedReason::AccessRevoked
    );
    root.pump_until(|c| c.people().len() == 1).await;
    assert!(root.removed.is_none());
}

#[tokio::test]
async fn added_to_group_reaches_open_session() {
    let w = world().await;
    let bob = w.server.user("Bob").await;
    let doc = w.doc(&w.root, &w.library, "Plan").await;
    w.add(&w.root, &w.group("visitors"), &bob).await;
    let mut b = TestClient::connect_as(&w.server, doc_id(&doc), &bob).await;
    assert!(!b.can_edit);

    assert_eq!(
        w.add(&w.root, &w.group("members"), &bob).await.0,
        StatusCode::NO_CONTENT
    );
    can_edit_promptly(&mut b, true).await;
}

#[tokio::test]
async fn role_change_reaches_open_sessions() {
    let w = world().await;
    let bob = w.server.user("Bob").await;
    let writer = w.role("Writer", &["add", "edit"]).await;
    // Bob holds the role in two places, one of them through a group.
    let a = w.folder(&w.root, &w.library, "A").await;
    let b = w.folder(&w.root, &w.library, "B").await;
    w.grant(&a, &bob, &writer).await;
    let (_, team) = w
        .call(
            &w.root,
            Method::POST,
            "/api/groups",
            Some(json!({ "name": "Writers" })),
        )
        .await;
    let team = team["id"].as_str().unwrap().to_owned();
    w.add(&w.root, &team, &bob).await;
    let (status, _) = w
        .call(
            &w.root,
            Method::PUT,
            &format!("/api/nodes/{b}/access"),
            Some(json!({ "group": team, "role": writer })),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let in_a = w.doc(&w.root, &a, "In A").await;
    let in_b = w.doc(&w.root, &b, "In B").await;
    let mut one = TestClient::connect_as(&w.server, doc_id(&in_a), &bob).await;
    let mut two = TestClient::connect_as(&w.server, doc_id(&in_b), &bob).await;
    assert!(one.can_edit && two.can_edit);

    // The role loses edit: Bob is read-only at once, everywhere.
    let (status, _) = w
        .call(
            &w.root,
            Method::PATCH,
            &format!("/api/roles/{writer}"),
            Some(json!({ "perms": ["view"] })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    can_edit_promptly(&mut one, false).await;
    can_edit_promptly(&mut two, false).await;
    assert!(one.removed.is_none() && two.removed.is_none());

    // And back.
    w.call(
        &w.root,
        Method::PATCH,
        &format!("/api/roles/{writer}"),
        Some(json!({ "perms": ["edit"] })),
    )
    .await;
    can_edit_promptly(&mut one, true).await;
    can_edit_promptly(&mut two, true).await;
}

// ---- Each operation's own permission, with custom roles ----

#[tokio::test]
async fn edit_without_add_cannot_create_or_move_in() {
    let w = world().await;
    let ann = w.server.user("Ann").await;
    let edit_only = w.role("Edit only", &["edit"]).await;
    let target = w.folder(&w.root, &w.library, "Target").await;
    let source = w.folder(&w.root, &w.library, "Source").await;
    w.grant(&target, &ann, &edit_only).await;
    w.grant(&source, &ann, "editor").await;
    let file = w.doc(&ann, &source, "Plan").await;

    let forbidden = (StatusCode::FORBIDDEN, error("forbidden"));
    assert_eq!(w.try_child(&ann, &target, "folder", "New").await, forbidden);
    assert_eq!(
        w.call(
            &ann,
            Method::POST,
            "/api/docs",
            Some(json!({ "parent": target, "name": "New" }))
        )
        .await,
        forbidden
    );
    assert_eq!(
        w.call(
            &ann,
            Method::PATCH,
            &format!("/api/nodes/{file}"),
            Some(json!({ "parent": target }))
        )
        .await,
        forbidden
    );
    // Edit is enough to rename what is there.
    let (status, _) = w
        .call(
            &ann,
            Method::PATCH,
            &format!("/api/nodes/{target}"),
            Some(json!({ "name": "Renamed" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn add_without_edit_cannot_rename() {
    let w = world().await;
    let ann = w.server.user("Ann").await;
    let add_only = w.role("Add only", &["add"]).await;
    let folder = w.folder(&w.root, &w.library, "Inbox").await;
    w.grant(&folder, &ann, &add_only).await;

    // Ann may add here...
    let created = w.folder(&ann, &folder, "Mine").await;
    let doc = w.doc(&ann, &folder, "Note").await;
    // ...but not rename, neither the folder nor what is in it.
    for node in [&folder, &created, &doc] {
        assert_eq!(
            w.call(
                &ann,
                Method::PATCH,
                &format!("/api/nodes/{node}"),
                Some(json!({ "name": "Renamed" }))
            )
            .await,
            (StatusCode::FORBIDDEN, error("forbidden")),
            "{node}"
        );
    }
}

#[tokio::test]
async fn library_creation_needs_manage_site() {
    let w = world().await;
    let ann = w.server.user("Ann").await;
    let bob = w.server.user("Bob").await;
    let site_admin = w.role("Site admin", &["manage_site"]).await;
    let builder = w
        .role("Builder", &["add", "edit", "delete", "manage_access"])
        .await;
    w.grant(&w.site, &ann, &site_admin).await;
    w.grant(&w.site, &bob, &builder).await;

    assert_eq!(
        w.try_child(&bob, &w.site, "library", "Archive").await,
        (StatusCode::FORBIDDEN, error("forbidden"))
    );
    let (status, body) = w.try_child(&ann, &w.site, "library", "Archive").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

#[tokio::test]
async fn delete_needs_delete() {
    let w = world().await;
    let ann = w.server.user("Ann").await;
    let no_delete = w.role("No delete", &["add", "edit"]).await;
    let deleter = w.role("Deleter", &["delete"]).await;
    let folder = w.folder(&w.root, &w.library, "Plans").await;
    let doc = w.doc(&w.root, &folder, "Plan").await;
    w.grant(&folder, &ann, &no_delete).await;

    assert_eq!(
        w.call(&ann, Method::DELETE, &format!("/api/nodes/{doc}"), None)
            .await,
        (StatusCode::FORBIDDEN, error("forbidden"))
    );
    w.grant(&folder, &ann, &deleter).await;
    let (status, _) = w
        .call(&ann, Method::DELETE, &format!("/api/nodes/{doc}"), None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}
