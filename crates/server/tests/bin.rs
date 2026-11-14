//! The two-stage recycle bin over HTTP: deleting folders with their contents, restoring,
//! moving to the second stage, purging (by people and by the sweeper), tombstones, and the
//! start-up reconcile keeping the bin's state. Link tokens are kept in variables and never
//! printed.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use collab_core::ids::{DocId, NodeId};
use common::{FileClient, TestClient, TestServer, TestUser, spawn_server};
use protocol::RemovedReason;
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use server::Checkpointer;
use time::OffsetDateTime;
use uuid::Uuid;

/// How soon a person must be out of a session once their access ended.
const PROMPTLY: Duration = Duration::from_secs(1);

fn error(code: &str) -> Value {
    json!({ "error": code })
}

/// A server with a site made by the administrator Root, and its library.
struct World {
    server: TestServer,
    root: TestUser,
    site: String,
    library: String,
}

async fn world() -> World {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let w = World {
        server,
        root,
        site: String::new(),
        library: String::new(),
    };
    let (status, site) = w
        .call(
            &w.root,
            Method::POST,
            "/api/sites",
            Some(json!({ "name": "Team" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let site = site["id"].as_str().unwrap().to_owned();
    let (_, page) = w
        .call(
            &w.root,
            Method::GET,
            &format!("/api/nodes/{site}/children"),
            None,
        )
        .await;
    let library = page["items"][0]["id"].as_str().unwrap().to_owned();
    World { site, library, ..w }
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
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, body)
    }

    async fn folder(&self, user: &TestUser, parent: &str, name: &str) -> String {
        let (status, body) = self
            .call(
                user,
                Method::POST,
                &format!("/api/nodes/{parent}/children"),
                Some(json!({ "kind": "folder", "name": name })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        body["id"].as_str().unwrap().to_owned()
    }

    /// A text document named `name` in `parent`, made by `user`.
    async fn doc(&self, user: &TestUser, parent: &str, name: &str) -> String {
        self.make(user, json!({ "parent": parent, "name": name }))
            .await
    }

    /// A GridCraft file named `name` in `parent`, made by `user`.
    async fn file(&self, user: &TestUser, parent: &str, name: &str) -> String {
        self.make(
            user,
            json!({ "parent": parent, "name": name, "editor": "gridcraft" }),
        )
        .await
    }

    async fn make(&self, user: &TestUser, body: Value) -> String {
        let (status, body) = self.call(user, Method::POST, "/api/docs", Some(body)).await;
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

    async fn delete(&self, user: &TestUser, node: &str) -> StatusCode {
        self.call(user, Method::DELETE, &format!("/api/nodes/{node}"), None)
            .await
            .0
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

    /// `GET /api/sites/{site}/bin?stage=` as `user`.
    async fn bin_page(&self, user: &TestUser, stage: &str) -> (StatusCode, Value) {
        self.call(
            user,
            Method::GET,
            &format!("/api/sites/{}/bin?stage={stage}", self.site),
            None,
        )
        .await
    }

    /// The names of the entries of a stage of the bin that `user` sees, newest first.
    async fn bin(&self, user: &TestUser, stage: &str) -> Vec<String> {
        let (status, page) = self.bin_page(user, stage).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        page["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["name"].as_str().unwrap().to_owned())
            .collect()
    }

    async fn restore(&self, user: &TestUser, node: &str) -> (StatusCode, Value) {
        self.call(
            user,
            Method::POST,
            &format!("/api/bin/{node}/restore"),
            None,
        )
        .await
    }

    /// `DELETE /api/bin/{node}` as `user`.
    async fn bin_remove(&self, user: &TestUser, node: &str) -> StatusCode {
        self.call(user, Method::DELETE, &format!("/api/bin/{node}"), None)
            .await
            .0
    }

    /// Purges the entry `node` as Root: to the second stage, then out of it.
    async fn purge(&self, node: &str) {
        assert_eq!(
            self.bin_remove(&self.root, node).await,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            self.bin_remove(&self.root, node).await,
            StatusCode::NO_CONTENT
        );
    }

    /// The node as PostgreSQL holds it, with its bin state.
    async fn row(&self, node: &str) -> records::NodeRow {
        let id = node_id(node);
        self.server
            .records_db()
            .node_rows()
            .await
            .unwrap()
            .into_iter()
            .find(|row| row.id == id)
            .unwrap()
    }

    /// The node's record in Git.
    fn record(&self, node: &str) -> revisions::TreeRecord {
        self.server
            .store()
            .read_tree_record(node_id(node).0)
            .unwrap()
            .unwrap()
    }

    /// The number of commits on `main`.
    fn commits(&self) -> usize {
        let repo = gix::open(self.server.library()).unwrap();
        let tip = repo.find_reference("refs/heads/main").unwrap();
        repo.rev_walk([tip.id()]).all().unwrap().count()
    }
}

fn node_id(text: &str) -> NodeId {
    NodeId(text.parse().unwrap())
}

fn doc_id(text: &str) -> DocId {
    DocId(text.parse().unwrap())
}

fn uuid(text: &str) -> Uuid {
    text.parse().unwrap()
}

async fn removed_promptly(c: &mut TestClient) -> RemovedReason {
    tokio::time::timeout(PROMPTLY, c.pump_until(|c| c.removed.is_some()))
        .await
        .expect("not removed within one second");
    c.removed.unwrap()
}

async fn file_removed_promptly(c: &mut FileClient) -> RemovedReason {
    tokio::time::timeout(PROMPTLY, c.pump_until(|c| c.removed.is_some()))
        .await
        .expect("not removed within one second");
    c.removed.unwrap()
}

#[tokio::test]
async fn delete_folder_with_contents() {
    let w = world().await;
    let plans = w.folder(&w.root, &w.library, "Plans").await;
    let a = w.doc(&w.root, &plans, "A").await;
    let sub = w.folder(&w.root, &plans, "Sub").await;
    let b = w.file(&w.root, &sub, "B.xlsx").await;
    let c = w.doc(&w.root, &plans, "C").await;
    // C was deleted on its own before the folder.
    assert_eq!(w.delete(&w.root, &c).await, StatusCode::NO_CONTENT);

    let commits = w.commits();
    assert_eq!(w.delete(&w.root, &plans).await, StatusCode::NO_CONTENT);
    assert_eq!(
        w.commits(),
        commits + 1,
        "one commit for the folder and its contents"
    );
    assert_eq!(
        w.names(&w.root, &w.library).await.unwrap(),
        Vec::<String>::new()
    );
    for node in [&plans, &a, &sub, &b] {
        let row = w.row(node).await;
        assert!(row.deleted_at.is_some());
        assert_eq!(row.deleted_with, Some(node_id(&plans)));
        let record = w.record(node);
        assert!(record.deleted.is_some());
        assert_eq!(record.deleted_with, Some(uuid(&plans)));
        assert_eq!(record.bin_stage, (*node == plans).then_some(1));
    }
    assert_eq!(w.row(&plans).await.bin_stage, Some(1));
    assert_eq!(w.row(&c).await.deleted_with, Some(node_id(&c)));
    for node in [&a, &b, &sub] {
        let (status, _) = w
            .call(&w.root, Method::GET, &format!("/api/nodes/{node}"), None)
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    // Two entries: the folder and the file deleted before it.
    let (status, page) = w.bin_page(&w.root, "first").await;
    assert_eq!(status, StatusCode::OK);
    let items = page["items"].as_array().unwrap();
    let names: Vec<&str> = items.iter().map(|i| i["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["Plans", "C"]);
    let folder = &items[0];
    assert_eq!(folder["id"], plans.as_str());
    assert_eq!(folder["kind"], "folder");
    assert_eq!(folder["path"], json!(["Team", "Documents"]));
    assert_eq!(folder["deleted_by"]["id"], w.root.id.0.to_string());
    assert_eq!(folder["stage"], "first");
    assert_eq!(folder["days_left"], 93);
    assert_eq!(items[1]["path"], json!(["Team", "Documents", "Plans"]));
    assert!(page["next"].is_null());

    // A page of one continues after it.
    let (_, first) = w
        .call(
            &w.root,
            Method::GET,
            &format!("/api/sites/{}/bin?limit=1", w.site),
            None,
        )
        .await;
    assert_eq!(first["items"][0]["name"], "Plans");
    let next = first["next"].as_str().unwrap();
    let (_, second) = w
        .call(
            &w.root,
            Method::GET,
            &format!("/api/sites/{}/bin?limit=1&after={next}", w.site),
            None,
        )
        .await;
    assert_eq!(second["items"][0]["name"], "C");
    assert!(second["next"].is_null());

    // The site and the library cannot be deleted.
    assert_eq!(w.delete(&w.root, &w.library).await, StatusCode::BAD_REQUEST);
    assert_eq!(w.delete(&w.root, &w.site).await, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn deleting_a_folder_ends_sessions_inside_it() {
    let w = world().await;
    let bob = w.server.user("Bob").await;
    w.grant(&w.site, &bob, "editor").await;
    let folder = w.folder(&w.root, &w.library, "Plans").await;
    let inner = w.folder(&w.root, &folder, "Inner").await;
    let text = w.doc(&w.root, &folder, "Notes").await;
    let sheet = w.file(&w.root, &inner, "Sheet.xlsx").await;
    let outside = w.doc(&w.root, &w.library, "Outside").await;

    let mut root = TestClient::connect_as(&w.server, doc_id(&text), &w.root).await;
    let mut b = TestClient::connect_as(&w.server, doc_id(&text), &bob).await;
    root.pump_until(|c| c.people().len() == 2).await;
    let mut seat = FileClient::connect_as(&w.server, doc_id(&sheet), &w.root).await;
    let mut other = FileClient::connect_as(&w.server, doc_id(&sheet), &bob).await;
    let mut away = TestClient::connect_as(&w.server, doc_id(&outside), &bob).await;

    assert_eq!(w.delete(&bob, &folder).await, StatusCode::NO_CONTENT);
    assert_eq!(removed_promptly(&mut root).await, RemovedReason::Deleted);
    assert_eq!(removed_promptly(&mut b).await, RemovedReason::Deleted);
    assert_eq!(
        file_removed_promptly(&mut seat).await,
        RemovedReason::Deleted
    );
    assert_eq!(
        file_removed_promptly(&mut other).await,
        RemovedReason::Deleted
    );

    // A document elsewhere stays open.
    away.type_at(0, "still here").await;
    away.pump_until(|c| c.pending() == 0).await;
    assert!(away.removed.is_none());
}

#[tokio::test]
async fn restore_folder_brings_back_its_own() {
    let w = world().await;
    let bob = w.server.user("Bob").await;
    let vic = w.server.user("Vic").await;
    let plans = w.folder(&w.root, &w.library, "Plans").await;
    // The folder has permissions of its own: Bob views it.
    w.grant(&plans, &bob, "viewer").await;
    let a = w.doc(&w.root, &plans, "A").await;
    let sub = w.folder(&w.root, &plans, "Sub").await;
    let b = w.doc(&w.root, &sub, "B").await;
    let earlier = w.doc(&w.root, &plans, "Earlier").await;
    // Vic reaches A through a signed-in link.
    let (status, made) = w
        .call(
            &w.root,
            Method::POST,
            &format!("/api/nodes/{a}/links"),
            Some(json!({ "kind": "signed_in", "role": "viewer" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let link = made["id"].as_str().unwrap().to_owned();
    let token = made["url"]
        .as_str()
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap()
        .to_owned();
    let (status, _) = w
        .call(&vic, Method::POST, &format!("/api/l/{token}/redeem"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = w
        .call(&vic, Method::GET, &format!("/api/nodes/{a}"), None)
        .await;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(w.delete(&w.root, &earlier).await, StatusCode::NO_CONTENT);
    assert_eq!(w.delete(&w.root, &plans).await, StatusCode::NO_CONTENT);
    // Bob's only access was the folder's.
    assert_eq!(w.names(&bob, &w.library).await, None);

    let commits = w.commits();
    let (status, body) = w.restore(&w.root, &plans).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(w.commits(), commits + 1, "one commit");
    assert_eq!(body["node"]["id"], plans.as_str());
    assert_eq!(body["node"]["parent"], w.library.as_str());
    assert!(body["renamed"].is_null());
    for node in [&plans, &a, &sub, &b] {
        let row = w.row(node).await;
        assert_eq!(
            (row.deleted_at, row.deleted_with, row.bin_stage),
            (None, None, None)
        );
        assert!(w.record(node).deleted.is_none());
    }
    assert_eq!(
        w.names(&w.root, &plans).await.unwrap(),
        ["Sub", "A"],
        "what was deleted on its own before stays in the bin"
    );
    assert_eq!(w.bin(&w.root, "first").await, ["Earlier"]);

    // Its own permissions are as they were: Bob views it, and nobody else was added.
    assert_eq!(w.names(&bob, &plans).await.unwrap(), ["Sub", "A"]);
    let (_, access) = w
        .call(
            &w.root,
            Method::GET,
            &format!("/api/nodes/{plans}/access"),
            None,
        )
        .await;
    assert_eq!(access["inherits"], false);
    assert!(
        access["assignments"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["principal"]["id"] == bob.id.0.to_string() && a["role"]["id"] == "viewer")
    );

    // The link revoked by the deletion stays revoked.
    let (_, links) = w
        .call(&w.root, Method::GET, &format!("/api/nodes/{a}/links"), None)
        .await;
    let listed = links
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["id"] == link.as_str())
        .unwrap();
    assert!(!listed["revoked"].is_null());
    let (status, _) = w
        .call(&vic, Method::GET, &format!("/api/nodes/{a}"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, body) = w
        .call(&vic, Method::POST, &format!("/api/l/{token}/redeem"), None)
        .await;
    assert_eq!(
        (status, body),
        (StatusCode::NOT_FOUND, error("link_not_found"))
    );

    // Restored, it is no longer in the bin.
    assert_eq!(w.restore(&w.root, &plans).await.0, StatusCode::NOT_FOUND);
    // An item deleted with a folder is not an entry of its own.
    assert_eq!(w.delete(&w.root, &plans).await, StatusCode::NO_CONTENT);
    assert_eq!(w.restore(&w.root, &a).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn restore_never_beyond_your_rights() {
    let w = world().await;
    let ann = w.server.user("Ann").await;

    // Ann views the folder F and edits the file D in it; Root deletes F after Ann deleted D.
    let f = w.folder(&w.root, &w.library, "F").await;
    let d = w.file(&w.root, &f, "D.xlsx").await;
    w.grant(&f, &ann, "viewer").await;
    w.grant(&d, &ann, "editor").await;
    assert_eq!(w.delete(&ann, &d).await, StatusCode::NO_CONTENT);
    assert_eq!(w.delete(&w.root, &f).await, StatusCode::NO_CONTENT);
    assert_eq!(w.bin(&ann, "first").await, ["D.xlsx"]);
    let (status, body) = w.restore(&ann, &d).await;
    assert_eq!(
        (status, body),
        (StatusCode::CONFLICT, error("parent_in_bin"))
    );
    assert!(w.row(&f).await.deleted_at.is_some(), "F stays in the bin");
    assert!(w.row(&d).await.deleted_at.is_some());
    // She cannot restore F itself either.
    assert_eq!(w.restore(&ann, &f).await.0, StatusCode::FORBIDDEN);

    // Ann edits the folder G; Root deletes it, with Z, after Ann deleted E from it.
    let g = w.folder(&w.root, &w.library, "G").await;
    w.grant(&g, &ann, "editor").await;
    let e = w.doc(&w.root, &g, "E").await;
    let z = w.doc(&w.root, &g, "Z").await;
    assert_eq!(w.delete(&ann, &e).await, StatusCode::NO_CONTENT);
    assert_eq!(w.delete(&w.root, &g).await, StatusCode::NO_CONTENT);
    let (status, body) = w.restore(&ann, &e).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["node"]["parent"], g.as_str());
    // G alone came back: Z, deleted with it, is in the bin as an entry of its own.
    assert_eq!(w.names(&w.root, &w.library).await.unwrap(), ["G"]);
    assert_eq!(w.names(&w.root, &g).await.unwrap(), ["E"]);
    assert!(w.row(&z).await.deleted_at.is_some());
    assert_eq!(w.row(&z).await.deleted_with, Some(node_id(&z)));
    assert_eq!(w.row(&z).await.bin_stage, Some(1));
    assert_eq!(w.record(&z).deleted_with, Some(uuid(&z)));
    let bin = w.bin(&w.root, "first").await;
    assert!(bin.contains(&"Z".to_owned()), "{bin:?}");
    assert!(!bin.contains(&"G".to_owned()), "{bin:?}");
    let (status, _) = w.restore(&w.root, &z).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(w.names(&w.root, &g).await.unwrap(), ["E", "Z"]);
}

#[tokio::test]
async fn restore_under_a_purged_folder_goes_to_the_library_top() {
    let w = world().await;
    let plans = w.folder(&w.root, &w.library, "Plans").await;
    let inner = w.folder(&w.root, &plans, "Inner").await;
    let a = w.doc(&w.root, &inner, "A").await;
    assert_eq!(w.delete(&w.root, &a).await, StatusCode::NO_CONTENT);
    assert_eq!(w.delete(&w.root, &plans).await, StatusCode::NO_CONTENT);
    w.purge(&plans).await;
    assert!(w.row(&inner).await.purged_at.is_some());

    let (status, body) = w.restore(&w.root, &a).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["node"]["parent"], w.library.as_str());
    assert_eq!(w.names(&w.root, &w.library).await.unwrap(), ["A"]);
    assert_eq!(w.record(&a).parent, Some(uuid(&w.library)));
    let (status, _) = w
        .call(&w.root, Method::GET, &format!("/api/nodes/{a}"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn restore_into_a_taken_name_gets_a_suffix() {
    let w = world().await;
    let sheet = w.file(&w.root, &w.library, "Plan.xlsx").await;
    let folder = w.folder(&w.root, &w.library, "Plans").await;
    assert_eq!(w.delete(&w.root, &sheet).await, StatusCode::NO_CONTENT);
    assert_eq!(w.delete(&w.root, &folder).await, StatusCode::NO_CONTENT);
    w.file(&w.root, &w.library, "plan.xlsx").await;
    w.folder(&w.root, &w.library, "Plans").await;

    let (status, body) = w.restore(&w.root, &sheet).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["node"]["name"], "Plan (2).xlsx");
    assert_eq!(
        body["renamed"],
        json!({ "id": sheet, "from": "Plan.xlsx", "to": "Plan (2).xlsx" })
    );
    assert_eq!(w.record(&sheet).name, "Plan (2).xlsx");
    let (status, body) = w.restore(&w.root, &folder).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["node"]["name"], "Plans (2)");
    let mut names = w.names(&w.root, &w.library).await.unwrap();
    names.sort();
    assert_eq!(names, ["Plan (2).xlsx", "Plans", "Plans (2)", "plan.xlsx"]);
    // Restored under its own name when it is free.
    let other = w.doc(&w.root, &w.library, "Free").await;
    assert_eq!(w.delete(&w.root, &other).await, StatusCode::NO_CONTENT);
    let (_, body) = w.restore(&w.root, &other).await;
    assert_eq!(body["node"]["name"], "Free");
    assert!(body["renamed"].is_null());
}

#[tokio::test]
async fn second_stage_only_for_site_managers() {
    let w = world().await;
    let ann = w.server.user("Ann").await;
    let olga = w.server.user("Olga").await;
    w.grant(&w.site, &ann, "editor").await;
    w.grant(&w.site, &olga, "owner").await;
    let x = w.doc(&ann, &w.library, "X").await;
    assert_eq!(w.delete(&ann, &x).await, StatusCode::NO_CONTENT);
    assert_eq!(w.bin(&ann, "first").await, ["X"]);
    assert_eq!(
        w.bin_page(&ann, "second").await,
        (StatusCode::FORBIDDEN, error("forbidden"))
    );
    assert_eq!(w.bin(&olga, "second").await, Vec::<String>::new());

    // Ann removes it from the first stage: it is out of her sight.
    assert_eq!(w.bin_remove(&ann, &x).await, StatusCode::NO_CONTENT);
    assert_eq!(w.row(&x).await.bin_stage, Some(2));
    assert_eq!(w.record(&x).bin_stage, Some(2));
    assert_eq!(w.bin(&ann, "first").await, Vec::<String>::new());
    assert_eq!(w.restore(&ann, &x).await.0, StatusCode::NOT_FOUND);
    assert_eq!(w.bin_remove(&ann, &x).await, StatusCode::NOT_FOUND);

    // A site owner sees it in the second stage and restores it.
    let (status, page) = w.bin_page(&olga, "second").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["items"][0]["name"], "X");
    assert_eq!(page["items"][0]["stage"], "second");
    let (status, body) = w.restore(&olga, &x).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(w.names(&ann, &w.library).await.unwrap(), ["X"]);

    // Removed from the second stage, it is purged.
    assert_eq!(w.delete(&ann, &x).await, StatusCode::NO_CONTENT);
    assert_eq!(w.bin_remove(&ann, &x).await, StatusCode::NO_CONTENT);
    assert_eq!(w.bin_remove(&olga, &x).await, StatusCode::NO_CONTENT);
    assert!(w.row(&x).await.purged_at.is_some());
    assert_eq!(w.bin(&olga, "second").await, Vec::<String>::new());
    assert_eq!(w.bin_remove(&olga, &x).await, StatusCode::NOT_FOUND);
    assert_eq!(w.restore(&olga, &x).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn purge_after_93_days() {
    let w = world().await;
    let plans = w.folder(&w.root, &w.library, "Plans").await;
    let a = w.doc(&w.root, &plans, "A").await;
    let kept = w.doc(&w.root, &w.library, "Kept").await;
    assert_eq!(w.delete(&w.root, &plans).await, StatusCode::NO_CONTENT);
    let state = w.server.app_state().await;
    let now = OffsetDateTime::now_utc();

    server::sweep_once(&state, now + time::Duration::days(92)).await;
    assert!(w.row(&plans).await.purged_at.is_none());
    assert_eq!(w.bin(&w.root, "first").await, ["Plans"]);

    // Moving it to the second stage does not restart its time.
    assert_eq!(w.bin_remove(&w.root, &plans).await, StatusCode::NO_CONTENT);
    server::sweep_once(&state, now + time::Duration::days(94)).await;
    for node in [&plans, &a] {
        assert!(w.row(node).await.purged_at.is_some());
        let record = w.record(node);
        assert!(record.purged.is_some());
        assert_eq!(record.purged.unwrap().by.id, "sharecraft");
    }
    assert!(w.row(&kept).await.purged_at.is_none());
    assert_eq!(w.bin(&w.root, "second").await, Vec::<String>::new());
    let events: Vec<(Option<Uuid>, Value)> = sqlx::query_as(
        "select actor, detail from events where kind = 'node.purged' and node_id = $1",
    )
    .bind(uuid(&plans))
    .fetch_all(w.server.records_db().test_pool())
    .await
    .unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].0, None, "purged by the server");
    assert_eq!(events[0].1["items"], 2);
}

#[tokio::test]
async fn purging_a_folder_keeps_separate_entries() {
    let w = world().await;
    let plans = w.folder(&w.root, &w.library, "Plans").await;
    let a = w.doc(&w.root, &plans, "A").await;
    let b = w.doc(&w.root, &plans, "B").await;
    assert_eq!(w.delete(&w.root, &a).await, StatusCode::NO_CONTENT);
    assert_eq!(w.delete(&w.root, &plans).await, StatusCode::NO_CONTENT);
    w.purge(&plans).await;

    assert!(w.row(&plans).await.purged_at.is_some());
    assert!(w.row(&b).await.purged_at.is_some());
    assert!(w.row(&a).await.purged_at.is_none());
    assert_eq!(w.bin(&w.root, "first").await, ["A"]);
    assert_eq!(w.bin(&w.root, "second").await, Vec::<String>::new());
    // Its own entry purges on its own.
    w.purge(&a).await;
    assert!(w.row(&a).await.purged_at.is_some());
}

#[tokio::test]
async fn purged_items_are_tombstones_without_access() {
    let w = world().await;
    let bob = w.server.user("Bob").await;
    let vic = w.server.user("Vic").await;
    let folder = w.folder(&w.root, &w.library, "Plans").await;
    let doc = w.doc(&w.root, &folder, "A").await;
    w.grant(&doc, &bob, "editor").await;
    let (status, made) = w
        .call(
            &w.root,
            Method::POST,
            &format!("/api/nodes/{doc}/links"),
            Some(json!({ "kind": "signed_in", "role": "viewer" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let link = uuid(made["id"].as_str().unwrap());
    let token = made["url"]
        .as_str()
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap()
        .to_owned();
    let (status, _) = w
        .call(&vic, Method::POST, &format!("/api/l/{token}/redeem"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let group: Uuid = sqlx::query_scalar("select group_id from links where id = $1")
        .bind(link)
        .fetch_one(w.server.records_db().test_pool())
        .await
        .unwrap();

    assert_eq!(w.delete(&w.root, &folder).await, StatusCode::NO_CONTENT);
    w.purge(&folder).await;

    let db = w.server.records_db();
    let pool = db.test_pool();
    for node in [&folder, &doc] {
        // The row and the record stay, marked purged.
        let row = w.row(node).await;
        assert!(row.purged_at.is_some() && row.deleted_at.is_some());
        assert!(w.record(node).purged.is_some());
        assert_eq!(
            db.access_as_if_live(w.root.id, node_id(node))
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            db.effective_access(w.root.id, node_id(node)).await.unwrap(),
            None
        );
        let (status, _) = w
            .call(&w.root, Method::GET, &format!("/api/nodes/{node}"), None)
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(w.restore(&w.root, node).await.0, StatusCode::NOT_FOUND);
        assert_eq!(w.bin_remove(&w.root, node).await, StatusCode::NOT_FOUND);
        let assignments: i64 =
            sqlx::query_scalar("select count(*) from assignments where node_id = $1")
                .bind(uuid(node))
                .fetch_one(pool)
                .await
                .unwrap();
        assert_eq!(assignments, 0, "no assignments left");
    }
    let links: i64 = sqlx::query_scalar("select count(*) from links where node_id = $1")
        .bind(uuid(&doc))
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(links, 0, "its links are gone");
    let groups: i64 = sqlx::query_scalar("select count(*) from groups where id = $1")
        .bind(group)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(groups, 0, "and their hidden groups");
    let (status, _) = w
        .call(&vic, Method::POST, &format!("/api/l/{token}/redeem"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = w
        .call(&bob, Method::GET, &format!("/api/docs/{doc}/content"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        w.names(&w.root, &w.library).await.unwrap(),
        Vec::<String>::new()
    );
    assert_eq!(w.bin(&w.root, "first").await, Vec::<String>::new());
    assert_eq!(w.bin(&w.root, "second").await, Vec::<String>::new());
    // Its name is free.
    w.folder(&w.root, &w.library, "Plans").await;
}

#[tokio::test]
async fn reconcile_keeps_bin_state() {
    let w = world().await;
    let plans = w.folder(&w.root, &w.library, "Plans").await;
    let a = w.doc(&w.root, &plans, "A").await;
    let second = w.doc(&w.root, &w.library, "Second").await;
    let gone = w.doc(&w.root, &w.library, "Gone").await;
    assert_eq!(w.delete(&w.root, &plans).await, StatusCode::NO_CONTENT);
    assert_eq!(w.delete(&w.root, &second).await, StatusCode::NO_CONTENT);
    assert_eq!(w.bin_remove(&w.root, &second).await, StatusCode::NO_CONTENT);
    assert_eq!(w.delete(&w.root, &gone).await, StatusCode::NO_CONTENT);
    w.purge(&gone).await;
    let nodes = [&plans, &a, &second, &gone];
    let mut before = Vec::new();
    for node in nodes {
        before.push(w.row(node).await);
    }

    // PostgreSQL loses the bin state: every node live again.
    let ids: Vec<Uuid> = nodes.iter().map(|n| uuid(n)).collect();
    sqlx::query(
        "update nodes set deleted_at = null, deleted_by = null, deleted_with = null, \
         bin_stage = null, purged_at = null where id = any($1)",
    )
    .bind(&ids)
    .execute(w.server.records_db().test_pool())
    .await
    .unwrap();
    let store: Arc<dyn Checkpointer> = Arc::new(w.server.store());
    let db = w.server.records_db();
    let report = server::tree::reconcile(Arc::clone(&store), &db)
        .await
        .unwrap();
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    assert_eq!(report.updated, 4);
    for (node, was) in nodes.iter().zip(&before) {
        let now = w.row(node).await;
        assert_eq!(now.deleted_with, was.deleted_with, "{node}");
        assert_eq!(now.bin_stage, was.bin_stage, "{node}");
        assert_eq!(now.purged_at.is_some(), was.purged_at.is_some(), "{node}");
        assert_eq!(now.deleted_at.is_some(), was.deleted_at.is_some(), "{node}");
    }
    assert_eq!(w.bin(&w.root, "first").await, ["Plans"]);
    assert_eq!(w.bin(&w.root, "second").await, ["Second"]);

    // Only the stage differs: that is repaired too.
    sqlx::query("update nodes set bin_stage = 1 where id = $1")
        .bind(uuid(&second))
        .execute(db.test_pool())
        .await
        .unwrap();
    let report = server::tree::reconcile(store, &db).await.unwrap();
    assert_eq!(report.updated, 1);
    assert_eq!(w.row(&second).await.bin_stage, Some(2));
    // A second reconcile finds nothing to do.
    let report = server::tree::reconcile(Arc::new(w.server.store()), &db)
        .await
        .unwrap();
    assert_eq!(report.updated, 0);
}
