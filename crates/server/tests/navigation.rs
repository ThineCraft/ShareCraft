//! Hubs and navigation over HTTP: administrators make a site a hub and unregister it; a site's
//! owners join it to a hub they can see and leave it; joining never changes access; hub lists
//! name only sites the viewer can see; the global, hub and site navigation are answered
//! already resolved for the viewer, so nothing they cannot see is named; audience targeting
//! hides links without granting or refusing access, and marks targeted items for the people
//! who manage that navigation; bad navigation is refused; and a node target needs `view` from
//! the person saving it.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use collab_core::ids::NodeId;
use common::{TestServer, TestUser, spawn_server};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use uuid::Uuid;

/// A server with the administrator Root and the site Team, made by Root, with its Documents
/// library; Olive owns Team (Owner on it) without being an administrator.
struct World {
    server: TestServer,
    root: TestUser,
    olive: TestUser,
    team: String,
    docs: String,
}

async fn world() -> World {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let olive = server.user("Olive").await;
    let mut w = World {
        server,
        root,
        olive,
        team: String::new(),
        docs: String::new(),
    };
    w.team = w.create_site("Team").await;
    w.docs = w.first_library(&w.team).await;
    w.grant(&w.team, &w.olive, "owner").await;
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
        let mut request = self
            .server
            .http(Some(user))
            .request(method, self.server.url(path));
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        let status = response.status();
        let bytes = response.bytes().await.unwrap();
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, body)
    }

    async fn create_site(&self, name: &str) -> String {
        let (status, body) = self
            .call(
                &self.root,
                Method::POST,
                "/api/sites",
                Some(json!({ "name": name })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        body["id"].as_str().unwrap().to_owned()
    }

    async fn first_library(&self, site: &str) -> String {
        let (status, page) = self
            .call(
                &self.root,
                Method::GET,
                &format!("/api/nodes/{site}/children"),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{page}");
        page["items"][0]["id"].as_str().unwrap().to_owned()
    }

    /// A child of `parent` of `kind` named `name`, made by Root.
    async fn child(&self, parent: &str, kind: &str, name: &str) -> String {
        let (status, body) = self
            .call(
                &self.root,
                Method::POST,
                &format!("/api/nodes/{parent}/children"),
                Some(json!({ "kind": kind, "name": name })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        body["id"].as_str().unwrap().to_owned()
    }

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

    /// Makes `node` a scope of its own and takes every listed person's own assignment off it.
    async fn restrict(&self, node: &str, away: &[&TestUser]) {
        let (status, body) = self
            .call(
                &self.root,
                Method::POST,
                &format!("/api/nodes/{node}/inheritance"),
                Some(json!({ "inherit": false })),
            )
            .await;
        assert!(status.is_success(), "{status} {body}");
        for user in away {
            let (status, body) = self
                .call(
                    &self.root,
                    Method::DELETE,
                    &format!("/api/nodes/{node}/access/user/{}", user.id.0),
                    None,
                )
                .await;
            assert!(status.is_success(), "{status} {body}");
        }
    }

    /// The id of `site`'s group with this purpose.
    async fn group(&self, site: &str, purpose: &str) -> String {
        let (status, groups) = self
            .call(
                &self.root,
                Method::GET,
                &format!("/api/sites/{site}/groups"),
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

    /// A new organisation group named `name`.
    async fn org_group(&self, name: &str) -> String {
        let (status, group) = self
            .call(
                &self.root,
                Method::POST,
                "/api/groups",
                Some(json!({ "name": name })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{group}");
        group["id"].as_str().unwrap().to_owned()
    }

    async fn add_member(&self, group: &str, user: &TestUser) {
        let (status, body) = self
            .call(
                &self.root,
                Method::POST,
                &format!("/api/groups/{group}/members"),
                Some(json!({ "email": user.email })),
            )
            .await;
        assert!(status.is_success(), "{status} {body}");
    }

    async fn delete_node(&self, node: &str) {
        let (status, body) = self
            .call(
                &self.root,
                Method::DELETE,
                &format!("/api/nodes/{node}"),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    }

    async fn set_hub(&self, user: &TestUser, site: &str, items: Value) -> (StatusCode, Value) {
        self.call(
            user,
            Method::PUT,
            &format!("/api/sites/{site}/hub"),
            Some(json!({ "navigation": items })),
        )
        .await
    }

    async fn join(&self, user: &TestUser, site: &str, hub: Option<&str>) -> (StatusCode, Value) {
        self.call(
            user,
            Method::PUT,
            &format!("/api/sites/{site}/joined-hub"),
            Some(json!({ "hub": hub })),
        )
        .await
    }

    async fn set_local(&self, user: &TestUser, site: &str, items: Value) -> (StatusCode, Value) {
        self.call(
            user,
            Method::PUT,
            &format!("/api/sites/{site}/navigation"),
            Some(json!({ "items": items })),
        )
        .await
    }

    async fn set_global(&self, user: &TestUser, items: Value) -> (StatusCode, Value) {
        self.call(
            user,
            Method::PUT,
            "/api/navigation",
            Some(json!({ "items": items })),
        )
        .await
    }

    /// `GET /api/sites/{site}/navigation` as `user`, which must succeed.
    async fn nav(&self, user: &TestUser, site: &str) -> Value {
        let (status, body) = self
            .call(
                user,
                Method::GET,
                &format!("/api/sites/{site}/navigation"),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body
    }

    /// The number of commits on `main`.
    fn commits(&self) -> usize {
        let repo = gix::open(self.server.library()).unwrap();
        let tip = repo.find_reference("refs/heads/main").unwrap();
        repo.rev_walk([tip.id()]).all().unwrap().count()
    }

    /// The details of the events of `kind`, oldest first.
    async fn events(&self, kind: &str) -> Vec<Value> {
        sqlx::query_scalar("select detail from events where kind = $1 order by id")
            .bind(kind)
            .fetch_all(self.server.records_db().test_pool())
            .await
            .unwrap()
    }

    /// The hub `site`'s record in Git says it joined.
    fn joined_in_git(&self, site: &str) -> Option<Uuid> {
        self.server
            .store()
            .read_site_record(uuid(site))
            .unwrap()
            .and_then(|record| record.joined_hub)
    }
}

fn uuid(text: &str) -> Uuid {
    text.parse().unwrap()
}

fn node_id(text: &str) -> NodeId {
    NodeId(uuid(text))
}

fn item(label: &str, target: Value, audience: &[&str]) -> Value {
    json!({
        "id": Uuid::new_v4(),
        "label": label,
        "target": target,
        "audience": audience,
        "children": [],
    })
}

/// A link to the node `node`.
fn to_node(label: &str, node: &str) -> Value {
    item(label, json!({ "node": node }), &[])
}

/// A link to the address `url`.
fn to_url(label: &str, url: &str) -> Value {
    item(label, json!({ "url": url }), &[])
}

/// A heading over `children`.
fn heading(label: &str, children: Vec<Value>) -> Value {
    let mut heading = item(label, Value::Null, &[]);
    heading["children"] = Value::Array(children);
    heading
}

/// `item` aimed at `groups`.
fn aimed(mut item: Value, groups: &[&str]) -> Value {
    item["audience"] = json!(groups);
    item
}

/// The labels of a resolved list, in order.
fn labels(items: &Value) -> Vec<String> {
    items
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["label"].as_str().unwrap().to_owned())
        .collect()
}

/// The names in a list of sites or hubs, in order.
fn names(list: &Value) -> Vec<String> {
    list.as_array()
        .unwrap()
        .iter()
        .map(|site| site["name"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn make_join_leave_and_unregister_a_hub() {
    let w = world().await;
    let hub = w.create_site("Hub").await;
    let beta = w.create_site("Beta").await;

    // Olive cannot see Hub, then can but it is not a hub yet; and she may not make one.
    let (status, _) = w.join(&w.olive, &w.team, Some(&hub)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    w.grant(&hub, &w.olive, "viewer").await;
    let (status, body) = w.join(&w.olive, &w.team, Some(&hub)).await;
    assert_eq!(
        (status, body["error"].as_str()),
        (StatusCode::CONFLICT, Some("not_a_hub"))
    );
    let (status, body) = w.set_hub(&w.olive, &hub, json!([])).await;
    assert_eq!(
        (status, body["error"].as_str()),
        (StatusCode::FORBIDDEN, Some("forbidden"))
    );

    // Root makes Hub a hub: one commit, `hub.registered`; the same bar again writes nothing.
    let bar = json!([to_url("Intranet", "https://intranet.example.invalid/")]);
    let commits = w.commits();
    let (status, body) = w.set_hub(&w.root, &hub, bar.clone()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["is_hub"], true);
    assert_eq!(w.commits(), commits + 1);
    let (status, _) = w.set_hub(&w.root, &hub, bar).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(w.commits(), commits + 1);
    assert_eq!(w.events("hub.registered").await.len(), 1);
    let record = w
        .server
        .store()
        .read_site_record(uuid(&hub))
        .unwrap()
        .unwrap();
    assert_eq!(record.hub.unwrap().navigation[0].label, "Intranet");

    // Changing the bar is `navigation.changed` for the hub.
    let bar = json!([to_url("Intranet home", "https://intranet.example.invalid/")]);
    let (status, _) = w.set_hub(&w.root, &hub, bar).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(w.commits(), commits + 2);
    let changed = w.events("navigation.changed").await;
    assert_eq!(changed.len(), 1, "{changed:?}");
    assert_eq!(changed[0]["which"], "hub");

    // Olive joins Team to it: one commit, `hub.joined`, recorded in Git and PostgreSQL.
    let (status, body) = w.join(&w.olive, &w.team, Some(&hub)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["hub"], json!({ "id": hub, "name": "Hub" }));
    assert_eq!(w.commits(), commits + 3);
    assert_eq!(w.joined_in_git(&w.team), Some(uuid(&hub)));
    let joined = w.events("hub.joined").await;
    assert_eq!(joined.len(), 1);
    assert_eq!(joined[0]["hub"], hub.as_str());
    let nav = w.nav(&w.olive, &w.team).await;
    assert_eq!(nav["hub"]["id"], hub.as_str());
    assert_eq!(nav["hub"]["name"], "Hub");
    assert_eq!(labels(&nav["hub"]["items"]), vec!["Intranet home"]);
    // The hub shows its own bar too.
    let nav = w.nav(&w.root, &hub).await;
    assert_eq!(labels(&nav["hub"]["items"]), vec!["Intranet home"]);

    // Leaving: `hub.left`; joining again.
    let (status, body) = w.join(&w.olive, &w.team, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["hub"], Value::Null);
    assert_eq!(w.joined_in_git(&w.team), None);
    assert_eq!(w.events("hub.left").await.len(), 1);
    assert_eq!(w.nav(&w.olive, &w.team).await["hub"], Value::Null);
    let (status, _) = w.join(&w.olive, &w.team, Some(&hub)).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = w.join(&w.root, &beta, Some(&hub)).await;
    assert_eq!(status, StatusCode::OK);
    let (_, members) = w
        .call(
            &w.root,
            Method::GET,
            &format!("/api/hubs/{hub}/sites"),
            None,
        )
        .await;
    assert_eq!(names(&members), vec!["Beta", "Team"]);

    // Unregistering releases both members in the one commit, in Git and PostgreSQL.
    let commits = w.commits();
    let (status, body) = w
        .call(
            &w.root,
            Method::DELETE,
            &format!("/api/sites/{hub}/hub"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["is_hub"], false);
    assert_eq!(w.commits(), commits + 1);
    assert_eq!(w.joined_in_git(&w.team), None);
    assert_eq!(w.joined_in_git(&beta), None);
    let record = w
        .server
        .store()
        .read_site_record(uuid(&hub))
        .unwrap()
        .unwrap();
    assert!(record.hub.is_none());
    let db = w.server.records_db();
    for site in [&w.team, &beta] {
        let row = db.site_settings(node_id(site)).await.unwrap().unwrap();
        assert_eq!(row.joined_hub, None);
    }
    assert!(
        !db.site_settings(node_id(&hub))
            .await
            .unwrap()
            .unwrap()
            .is_hub
    );
    assert_eq!(w.events("hub.unregistered").await.len(), 1);
    let released = w
        .events("hub.left")
        .await
        .into_iter()
        .filter(|d| d["released"] == true)
        .count();
    assert_eq!(released, 2);
    let (status, body) = w
        .call(
            &w.root,
            Method::DELETE,
            &format!("/api/sites/{hub}/hub"),
            None,
        )
        .await;
    assert_eq!(
        (status, body["error"].as_str()),
        (StatusCode::CONFLICT, Some("not_a_hub"))
    );
    let (status, body) = w.join(&w.olive, &w.team, Some(&hub)).await;
    assert_eq!(
        (status, body["error"].as_str()),
        (StatusCode::CONFLICT, Some("not_a_hub"))
    );

    // A deleted site answers 404 to every one of these routes, as to every site route.
    let (status, _) = w
        .call(&w.root, Method::DELETE, &format!("/api/sites/{beta}"), None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    for (method, path, body) in [
        (
            Method::PUT,
            format!("/api/sites/{beta}/hub"),
            Some(json!({ "navigation": [] })),
        ),
        (Method::DELETE, format!("/api/sites/{beta}/hub"), None),
        (
            Method::PUT,
            format!("/api/sites/{beta}/joined-hub"),
            Some(json!({ "hub": null })),
        ),
        (
            Method::PUT,
            format!("/api/sites/{beta}/navigation"),
            Some(json!({ "items": null })),
        ),
        (Method::GET, format!("/api/sites/{beta}/navigation"), None),
    ] {
        let (status, answer) = w.call(&w.root, method.clone(), &path, body).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path}: {answer}");
    }
}

#[tokio::test]
async fn hub_cannot_join_a_hub() {
    let w = world().await;
    let first = w.create_site("First hub").await;
    let second = w.create_site("Second hub").await;
    for hub in [&first, &second] {
        let (status, body) = w.set_hub(&w.root, hub, json!([])).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let commits = w.commits();
    let (status, body) = w.join(&w.root, &second, Some(&first)).await;
    assert_eq!(
        (status, body["error"].as_str()),
        (StatusCode::CONFLICT, Some("is_hub"))
    );

    // A site that joined a hub cannot become one; nor can a site join itself.
    let (status, _) = w.join(&w.root, &w.team, Some(&first)).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = w.set_hub(&w.root, &w.team, json!([])).await;
    assert_eq!(
        (status, body["error"].as_str()),
        (StatusCode::CONFLICT, Some("is_hub"))
    );
    let other = w.create_site("Other").await;
    let (status, body) = w.join(&w.root, &other, Some(&other)).await;
    assert_eq!(
        (status, body["error"].as_str()),
        (StatusCode::CONFLICT, Some("not_a_hub"))
    );
    assert_eq!(w.commits(), commits + 2, "the join and the site Other");
    assert_eq!(w.joined_in_git(&second), None);
}

/// What each of `users` gets for each of `nodes`: the status and the access answered.
async fn access_of(
    w: &World,
    users: &[&TestUser],
    nodes: &[&String],
) -> Vec<(String, String, StatusCode, Value)> {
    let mut seen = Vec::new();
    for user in users {
        for node in nodes {
            let (status, body) = w
                .call(user, Method::GET, &format!("/api/nodes/{node}"), None)
                .await;
            seen.push((
                user.email.clone(),
                (*node).clone(),
                status,
                body["access"].clone(),
            ));
        }
    }
    seen
}

#[tokio::test]
async fn joining_never_changes_access() {
    let w = world().await;
    let hub = w.create_site("Hub").await;
    let hub_docs = w.first_library(&hub).await;
    let uma = w.server.user("Uma").await;
    let vic = w.server.user("Vic").await;
    w.grant(&hub, &uma, "viewer").await;
    w.grant(&w.team, &vic, "viewer").await;
    w.grant(&hub, &w.olive, "viewer").await;
    let (status, _) = w
        .set_hub(
            &w.root,
            &hub,
            json!([to_url("Wiki", "https://wiki.example.invalid/")]),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let users = [&uma, &vic, &w.olive];
    let nodes = [&hub, &w.team, &hub_docs, &w.docs];
    let before = access_of(&w, &users, &nodes).await;
    let (status, body) = w.join(&w.olive, &w.team, Some(&hub)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(access_of(&w, &users, &nodes).await, before);

    // Uma still cannot see Team, Vic still cannot see Hub.
    let (status, _) = w
        .call(&uma, Method::GET, &format!("/api/sites/{}", w.team), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = w
        .call(&vic, Method::GET, &format!("/api/sites/{hub}"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Vic sees the hub's bar on Team, but neither the hub's id nor its name.
    let nav = w.nav(&vic, &w.team).await;
    assert_eq!(nav["hub"]["id"], Value::Null);
    assert_eq!(nav["hub"]["name"], Value::Null);
    assert_eq!(labels(&nav["hub"]["items"]), vec!["Wiki"]);
    assert!(!nav.to_string().contains(&hub));
    let (_, site) = w
        .call(&vic, Method::GET, &format!("/api/sites/{}", w.team), None)
        .await;
    assert_eq!(site["hub"], Value::Null);
}

#[tokio::test]
async fn hub_lists_only_visible_member_sites() {
    let w = world().await;
    let hub = w.create_site("Hub").await;
    let alpha = w.create_site("Alpha").await;
    let beta = w.create_site("Beta").await;
    let uma = w.server.user("Uma").await;
    let vic = w.server.user("Vic").await;
    w.grant(&hub, &uma, "viewer").await;
    w.grant(&alpha, &uma, "viewer").await;
    w.grant(&alpha, &vic, "viewer").await;
    let (status, _) = w.set_hub(&w.root, &hub, json!([])).await;
    assert_eq!(status, StatusCode::OK);
    for site in [&alpha, &beta] {
        let (status, _) = w.join(&w.root, site, Some(&hub)).await;
        assert_eq!(status, StatusCode::OK);
    }

    let (_, hubs) = w.call(&uma, Method::GET, "/api/hubs", None).await;
    assert_eq!(names(&hubs), vec!["Hub"]);
    let (status, members) = w
        .call(&uma, Method::GET, &format!("/api/hubs/{hub}/sites"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{members}");
    assert_eq!(names(&members), vec!["Alpha"]);
    assert!(!members.to_string().contains(&beta));

    let (_, hubs) = w.call(&vic, Method::GET, "/api/hubs", None).await;
    assert_eq!(names(&hubs), Vec::<String>::new());
    let (status, _) = w
        .call(&vic, Method::GET, &format!("/api/hubs/{hub}/sites"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, site) = w
        .call(&vic, Method::GET, &format!("/api/sites/{alpha}"), None)
        .await;
    assert_eq!(site["hub"], Value::Null);

    let (_, members) = w
        .call(
            &w.root,
            Method::GET,
            &format!("/api/hubs/{hub}/sites"),
            None,
        )
        .await;
    assert_eq!(names(&members), vec!["Alpha", "Beta"]);
    let (status, body) = w
        .call(&uma, Method::GET, &format!("/api/hubs/{alpha}/sites"), None)
        .await;
    assert_eq!(
        (status, body["error"].as_str()),
        (StatusCode::CONFLICT, Some("not_a_hub"))
    );
}

#[tokio::test]
async fn navigation_never_names_what_viewer_cannot_see() {
    let w = world().await;
    let uma = w.server.user("Uma").await;
    w.grant(&w.team, &uma, "viewer").await;
    let secret = w.child(&w.team, "library", "Secret").await;
    w.restrict(&secret, &[&uma, &w.olive]).await;
    let gone = w.child(&w.docs, "folder", "Gone").await;
    let hidden = w.create_site("Hidden Place").await;
    let hidden_docs = w.first_library(&hidden).await;
    let hidden_members = w.group(&hidden, "members").await;
    let faraway = w.create_site("Faraway Hub").await;

    let (status, body) = w
        .set_global(
            &w.root,
            json!([
                to_node("", &secret),
                to_node("", &hidden_docs),
                to_node("", &w.docs),
                heading("Teams", vec![to_node("", &secret)]),
                to_node("", &gone),
            ]),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = w
        .set_hub(
            &w.root,
            &faraway,
            json!([
                to_node("", &secret),
                to_url("Wiki", "https://wiki.example.invalid/")
            ]),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = w.join(&w.root, &w.team, Some(&faraway)).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = w
        .set_local(
            &w.root,
            &w.team,
            json!([
                to_node("", &w.docs),
                to_node("", &secret),
                to_node("", &gone),
                aimed(to_url("Board", "/board"), &[&hidden_members]),
            ]),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    w.delete_node(&gone).await;

    // Uma sees only what she may see, and nothing names the rest.
    let nav = w.nav(&uma, &w.team).await;
    assert_eq!(labels(&nav["global"]), vec!["Documents"]);
    assert_eq!(nav["hub"]["id"], Value::Null);
    assert_eq!(nav["hub"]["name"], Value::Null);
    assert_eq!(labels(&nav["hub"]["items"]), vec!["Wiki"]);
    assert_eq!(labels(&nav["local"]), vec!["Documents"]);
    assert_eq!(nav["automatic"], false);
    assert_eq!(nav["local"][0]["target"]["node"], w.docs.as_str());
    assert_eq!(nav["local"][0]["target"]["kind"], "library");
    let text = nav.to_string();
    for unseen in [
        "Secret",
        "Hidden Place",
        "Faraway",
        "Gone",
        "Board",
        "Teams",
    ] {
        assert!(!text.contains(unseen), "{unseen} in {text}");
    }
    for unseen in [&secret, &hidden_docs, &hidden_members, &faraway, &gone] {
        assert!(!text.contains(unseen.as_str()), "{unseen} in {text}");
    }
    let (status, global) = w.call(&uma, Method::GET, "/api/navigation", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(labels(&global["items"]), vec!["Documents"]);

    // Root sees every live target, by its current name; the deleted one is left out.
    let nav = w.nav(&w.root, &w.team).await;
    assert_eq!(
        labels(&nav["global"]),
        vec!["Secret", "Documents", "Documents", "Teams"]
    );
    assert_eq!(labels(&nav["global"][3]["children"]), vec!["Secret"]);
    assert_eq!(nav["hub"]["name"], "Faraway Hub");
    assert_eq!(labels(&nav["hub"]["items"]), vec!["Secret", "Wiki"]);
    assert_eq!(labels(&nav["local"]), vec!["Documents", "Secret", "Board"]);
    assert_eq!(nav["local"][2]["targeted"], true);

    // Olive edits Team's navigation: a target and a group she cannot see are not named.
    let (status, edit) = w
        .call(
            &w.olive,
            Method::GET,
            &format!("/api/sites/{}/navigation?edit=1", w.team),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{edit}");
    assert!(edit.get("global").is_none(), "{edit}");
    let local = edit["local"].as_array().unwrap();
    assert_eq!(local[0]["target"]["name"], "Documents");
    assert_eq!(local[1]["target"]["node"], secret.as_str());
    assert_eq!(local[1]["target"]["name"], Value::Null);
    assert_eq!(local[3]["audience"][0]["id"], hidden_members.as_str());
    assert_eq!(local[3]["audience"][0]["name"], "A group you cannot see");
    let text = edit.to_string();
    for unseen in ["Secret", "Hidden Place", "Gone"] {
        assert!(!text.contains(unseen), "{unseen} in {text}");
    }
    // Uma may change none of it.
    let (status, _) = w
        .call(
            &uma,
            Method::GET,
            &format!("/api/sites/{}/navigation?edit=1", w.team),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = w
        .call(&uma, Method::GET, "/api/navigation?edit=1", None)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn audience_hides_but_never_grants() {
    let w = world().await;
    let uma = w.server.user("Uma").await;
    let vic = w.server.user("Vic").await;
    let rae = w.server.user("Rae").await;
    w.grant(&w.team, &uma, "viewer").await;
    let members = w.group(&w.team, "members").await;
    w.add_member(&members, &vic).await;
    let board = w.org_group("Board").await;
    w.add_member(&board, &rae).await;
    let secret = w.child(&w.team, "library", "Secret").await;
    w.restrict(&secret, &[&uma]).await;

    let (status, body) = w
        .set_local(
            &w.root,
            &w.team,
            json!([
                aimed(to_node("Members area", &w.docs), &[&members]),
                aimed(to_node("Board room", &secret), &[&board]),
                to_url("Everyone", "/everyone"),
            ]),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let nav = w.nav(&vic, &w.team).await;
    assert_eq!(labels(&nav["local"]), vec!["Members area", "Everyone"]);
    let nav = w.nav(&uma, &w.team).await;
    assert_eq!(labels(&nav["local"]), vec!["Everyone"]);
    // Hidden from Uma, but she still opens it by its address.
    let (status, _) = w
        .call(&uma, Method::GET, &format!("/api/nodes/{}", w.docs), None)
        .await;
    assert_eq!(status, StatusCode::OK);

    // Being in the audience gives Rae nothing: she sees neither the site nor the library.
    let (status, _) = w
        .call(
            &rae,
            Method::GET,
            &format!("/api/sites/{}/navigation", w.team),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = w
        .call(&rae, Method::GET, &format!("/api/nodes/{secret}"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, body) = w
        .set_global(
            &w.root,
            json!([
                aimed(to_node("Board room", &secret), &[&board]),
                aimed(to_url("Board page", "/board"), &[&board]),
            ]),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, global) = w.call(&rae, Method::GET, "/api/navigation", None).await;
    assert_eq!(labels(&global["items"]), vec!["Board page"]);
    let (_, global) = w.call(&uma, Method::GET, "/api/navigation", None).await;
    assert_eq!(labels(&global["items"]), Vec::<String>::new());
}

#[tokio::test]
async fn targeted_items_marked_for_managers() {
    let w = world().await;
    let uma = w.server.user("Uma").await;
    let vic = w.server.user("Vic").await;
    w.grant(&w.team, &uma, "viewer").await;
    let members = w.group(&w.team, "members").await;
    w.add_member(&members, &vic).await;

    let (status, body) = w
        .set_local(
            &w.root,
            &w.team,
            json!([
                aimed(to_url("For members", "/members"), &[&members]),
                to_url("For all", "/all"),
            ]),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = w
        .set_global(
            &w.root,
            json!([aimed(to_url("Global members", "/gm"), &[&members])]),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = w
        .set_hub(
            &w.root,
            &w.team,
            json!([aimed(to_url("Hub members", "/hm"), &[&members])]),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let marks = |nav: &Value, list: &str| -> Vec<(String, bool)> {
        let items = if list == "hub" {
            &nav["hub"]["items"]
        } else {
            &nav[list]
        };
        items
            .as_array()
            .unwrap()
            .iter()
            .map(|i| {
                (
                    i["label"].as_str().unwrap().to_owned(),
                    i["targeted"] == true,
                )
            })
            .collect()
    };
    let s = |text: &str, targeted: bool| (text.to_owned(), targeted);

    // Olive manages the site's own navigation, not the hub's or the global one.
    let nav = w.nav(&w.olive, &w.team).await;
    assert_eq!(
        marks(&nav, "local"),
        vec![s("For members", true), s("For all", false)]
    );
    assert_eq!(marks(&nav, "global"), vec![]);
    assert_eq!(marks(&nav, "hub"), vec![]);
    // Root manages all three.
    let nav = w.nav(&w.root, &w.team).await;
    assert_eq!(
        marks(&nav, "local"),
        vec![s("For members", true), s("For all", false)]
    );
    assert_eq!(marks(&nav, "global"), vec![s("Global members", true)]);
    assert_eq!(marks(&nav, "hub"), vec![s("Hub members", true)]);
    // Vic is in the audience: nothing is marked.
    let nav = w.nav(&vic, &w.team).await;
    assert_eq!(
        marks(&nav, "local"),
        vec![s("For members", false), s("For all", false)]
    );
    assert_eq!(marks(&nav, "global"), vec![s("Global members", false)]);
    assert_eq!(marks(&nav, "hub"), vec![s("Hub members", false)]);
    // Uma is in none and manages nothing.
    let nav = w.nav(&uma, &w.team).await;
    assert_eq!(marks(&nav, "local"), vec![s("For all", false)]);
    assert_eq!(marks(&nav, "global"), vec![]);
    assert_eq!(nav["hub"]["items"], json!([]));
}

#[tokio::test]
async fn bad_navigation_refused() {
    let w = world().await;
    let other = w.create_site("Other").await;
    let other_docs = w.first_library(&other).await;
    let (status, _) = w.set_hub(&w.root, &other, json!([])).await;
    assert_eq!(status, StatusCode::OK);
    let commits = w.commits();

    let refused = |status: StatusCode, body: &Value, what: &str| {
        assert_eq!(
            (status, body["error"].as_str()),
            (StatusCode::UNPROCESSABLE_ENTITY, Some("invalid_navigation")),
            "{what}: {body}"
        );
    };
    for url in [
        "javascript:alert(1)",
        "JavaScript:alert(1)",
        " javascript:alert(1)",
        "data:text/html,hello",
        "vbscript:msgbox",
        "file:///etc/passwd",
        "ftp://example.invalid/",
        "//evil.example.invalid/",
        "/\\evil.example.invalid/",
        "relative/path",
    ] {
        let items = json!([to_url("Bad", url)]);
        let (status, body) = w.set_local(&w.root, &w.team, items.clone()).await;
        refused(status, &body, url);
        let (status, body) = w.set_global(&w.root, items.clone()).await;
        refused(status, &body, url);
        let (status, body) = w.set_hub(&w.root, &other, items).await;
        refused(status, &body, url);
    }

    let many: Vec<Value> = (0..101)
        .map(|n| to_url(&format!("Item {n}"), "/x"))
        .collect();
    let (status, body) = w.set_local(&w.root, &w.team, json!(many)).await;
    refused(status, &body, "101 items");

    let mut child = to_url("Child", "/child");
    child["children"] = json!([to_url("Grandchild", "/grandchild")]);
    let (status, body) = w
        .set_local(&w.root, &w.team, json!([heading("Top", vec![child])]))
        .await;
    refused(status, &body, "two levels of children");

    let groups: Vec<String> = (0..11).map(|_| Uuid::new_v4().to_string()).collect();
    let groups: Vec<&str> = groups.iter().map(String::as_str).collect();
    let (status, body) = w
        .set_local(
            &w.root,
            &w.team,
            json!([aimed(to_url("Wide", "/w"), &groups)]),
        )
        .await;
    refused(status, &body, "an 11-group audience");
    let (status, body) = w
        .set_local(
            &w.root,
            &w.team,
            json!([aimed(to_url("Nobody", "/n"), &groups[..1])]),
        )
        .await;
    refused(status, &body, "a group that does not exist");

    let (status, body) = w
        .set_local(&w.root, &w.team, json!([to_node("", &other_docs)]))
        .await;
    refused(status, &body, "a node in another site");
    assert_eq!(w.commits(), commits, "nothing was written");
    assert_eq!(w.nav(&w.root, &w.team).await["automatic"], true);

    // Hub and global navigation may aim at any site.
    let (status, body) = w
        .set_global(&w.root, json!([to_node("", &other_docs)]))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = w
        .set_hub(&w.root, &other, json!([to_node("", &w.docs)]))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn automatic_navigation_lists_libraries() {
    let w = world().await;
    let uma = w.server.user("Uma").await;
    w.grant(&w.team, &uma, "viewer").await;
    let plans = w.child(&w.team, "library", "Plans").await;
    let archive = w.child(&w.team, "library", "archive").await;
    let secret = w.child(&w.team, "library", "Secret").await;
    w.restrict(&secret, &[&uma]).await;

    let nav = w.nav(&uma, &w.team).await;
    assert_eq!(nav["automatic"], true);
    assert_eq!(labels(&nav["local"]), vec!["archive", "Documents", "Plans"]);
    let targets: Vec<&str> = nav["local"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["target"]["node"].as_str().unwrap())
        .collect();
    assert_eq!(
        targets,
        vec![archive.as_str(), w.docs.as_str(), plans.as_str()]
    );
    assert_eq!(nav["local"][0]["target"]["kind"], "library");
    let nav = w.nav(&w.root, &w.team).await;
    assert_eq!(
        labels(&nav["local"]),
        vec!["archive", "Documents", "Plans", "Secret"]
    );

    // Set, then automatic again: one commit each, `navigation.changed` for the local one.
    let commits = w.commits();
    let (status, body) = w
        .set_local(&w.olive, &w.team, json!([to_url("Home", "/")]))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["automatic"], false);
    assert_eq!(labels(&body["local"]), vec!["Home"]);
    let (status, body) = w.set_local(&w.olive, &w.team, Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["automatic"], true);
    assert_eq!(
        labels(&body["local"]),
        vec!["archive", "Documents", "Plans", "Secret"]
    );
    assert_eq!(w.commits(), commits + 2);
    let changed = w.events("navigation.changed").await;
    assert_eq!(changed.len(), 2);
    assert!(changed.iter().all(|d| d["which"] == "local"), "{changed:?}");
    let record = w
        .server
        .store()
        .read_site_record(uuid(&w.team))
        .unwrap()
        .unwrap();
    assert!(record.navigation.is_none());

    // Only people with manage_site set it.
    let (status, body) = w.set_local(&uma, &w.team, json!([])).await;
    assert_eq!(
        (status, body["error"].as_str()),
        (StatusCode::FORBIDDEN, Some("forbidden"))
    );
}

#[tokio::test]
async fn saving_a_target_needs_view() {
    let w = world().await;
    let secret = w.child(&w.team, "library", "Secret").await;
    w.restrict(&secret, &[&w.olive]).await;
    let (status, _) = w
        .call(&w.olive, Method::GET, &format!("/api/nodes/{secret}"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let commits = w.commits();
    for label in ["", "Looks harmless"] {
        let (status, body) = w
            .set_local(&w.olive, &w.team, json!([to_node(label, &secret)]))
            .await;
        assert_eq!(
            (status, body["error"].as_str()),
            (StatusCode::UNPROCESSABLE_ENTITY, Some("invalid_navigation")),
            "{body}"
        );
        assert!(!body.to_string().contains("Secret"));
    }
    let (status, body) = w
        .set_local(
            &w.olive,
            &w.team,
            json!([heading("Places", vec![to_node("", &secret)])]),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(w.commits(), commits, "nothing was written");

    let (status, body) = w
        .set_local(&w.olive, &w.team, json!([to_node("", &w.docs)]))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(labels(&body["local"]), vec!["Documents"]);
    // Root may view it, so Root may save it.
    let (status, body) = w
        .set_local(&w.root, &w.team, json!([to_node("", &secret)]))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(labels(&body["local"]), vec!["Secret"]);
}
