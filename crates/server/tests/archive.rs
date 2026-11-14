//! Deleting libraries to the recycle bin and restoring them, and archiving and reactivating
//! sites over HTTP: an archived site is read-only for everyone, administrators included, until
//! an administrator reactivates it; every write route under `/api` answers `403 archived`,
//! reads and downloads keep working, open sessions end, upload sessions are abandoned, anyone
//! links stop working and the directory lists it only to its managers. Link tokens are kept in
//! variables and never printed.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::time::Duration;

use collab_core::ids::{DocId, NodeId};
use common::{FileClient, TestClient, TestServer, TestUser, spawn_server};
use protocol::RemovedReason;
use records::{NewUpload, UploadTarget};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

/// How soon a person must be out of a session once the site is archived.
const PROMPTLY: Duration = Duration::from_secs(1);

/// The SHA-256 of `abc`, the bytes every upload here declares.
const ABC_SHA256: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

/// A server with the site Team made by the administrator Root, its library, and the person
/// Olive, who owns Team (Owner on it) without being an administrator.
struct World {
    server: TestServer,
    root: TestUser,
    olive: TestUser,
    site: String,
    library: String,
}

async fn world() -> World {
    let server = spawn_server().await;
    let root = server.admin("Root").await;
    let olive = server.user("Olive").await;
    let w = World {
        server,
        root,
        olive,
        site: String::new(),
        library: String::new(),
    };
    let site = w.create_site("Team").await;
    let library = w.first_library(&site).await;
    let w = World { site, library, ..w };
    w.grant(&w.site, &w.olive, "owner").await;
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
        answer(request.send().await.unwrap()).await
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

    /// A text document named `name` in `parent`, made by Root.
    async fn doc(&self, parent: &str, name: &str) -> String {
        self.make(json!({ "parent": parent, "name": name })).await
    }

    /// A GridCraft file named `name` in `parent`, made by Root.
    async fn file(&self, parent: &str, name: &str) -> String {
        self.make(json!({ "parent": parent, "name": name, "editor": "gridcraft" }))
            .await
    }

    async fn make(&self, body: Value) -> String {
        let (status, body) = self
            .call(&self.root, Method::POST, "/api/docs", Some(body))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        body["id"].as_str().unwrap().to_owned()
    }

    /// A file uploaded by Root into `parent`, holding `abc`.
    async fn uploaded(&self, parent: &str, name: &str) -> String {
        let response = self
            .server
            .http(Some(&self.root))
            .post(
                self.server
                    .url(&format!("/api/docs/upload?parent={parent}&name={name}")),
            )
            .body("abc")
            .send()
            .await
            .unwrap();
        let (status, body) = answer(response).await;
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
        assert_eq!(status, StatusCode::OK, "{page}");
        Some(
            page["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item["name"].as_str().unwrap().to_owned())
                .collect(),
        )
    }

    /// The first stage of Team's bin as `user` sees it.
    async fn bin(&self, user: &TestUser) -> Vec<Value> {
        let (status, page) = self
            .call(
                user,
                Method::GET,
                &format!("/api/sites/{}/bin", self.site),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{page}");
        page["items"].as_array().unwrap().clone()
    }

    async fn archive(&self, user: &TestUser) -> (StatusCode, Value) {
        self.call(
            user,
            Method::POST,
            &format!("/api/sites/{}/archive", self.site),
            None,
        )
        .await
    }

    async fn reactivate(&self, user: &TestUser) -> (StatusCode, Value) {
        self.call(
            user,
            Method::POST,
            &format!("/api/sites/{}/reactivate", self.site),
            None,
        )
        .await
    }

    /// The id of Team's group with this purpose.
    async fn group(&self, purpose: &str) -> String {
        let (status, groups) = self
            .call(
                &self.root,
                Method::GET,
                &format!("/api/sites/{}/groups", self.site),
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
}

async fn answer(response: reqwest::Response) -> (StatusCode, Value) {
    let status = response.status();
    let bytes = response.bytes().await.unwrap();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, body)
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

fn sha_abc() -> [u8; 32] {
    revisions::parse_hex(ABC_SHA256).unwrap()
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
async fn delete_and_restore_a_library() {
    let w = world().await;
    let plans = w.child(&w.root, &w.site, "library", "Plans").await;
    let folder = w.child(&w.root, &plans, "folder", "Drafts").await;
    let notes = w.doc(&folder, "Notes").await;
    let sheet = w.file(&plans, "Sheet.xlsx").await;
    let mut text = TestClient::connect_as(&w.server, doc_id(&notes), &w.root).await;
    let mut seat = FileClient::connect_as(&w.server, doc_id(&sheet), &w.olive).await;

    // An owner (manage_site) deletes it, with everything in it, as one entry, in one commit.
    let commits = w.commits();
    let (status, body) = w
        .call(
            &w.olive,
            Method::DELETE,
            &format!("/api/nodes/{plans}"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(w.commits(), commits + 1);
    assert_eq!(removed_promptly(&mut text).await, RemovedReason::Deleted);
    assert_eq!(
        file_removed_promptly(&mut seat).await,
        RemovedReason::Deleted
    );
    assert_eq!(
        w.names(&w.root, &w.site).await.unwrap(),
        vec!["Documents".to_owned()]
    );
    for node in [&plans, &folder, &notes, &sheet] {
        let (status, _) = w
            .call(&w.root, Method::GET, &format!("/api/nodes/{node}"), None)
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let record = w
            .server
            .store()
            .read_tree_record(uuid(node))
            .unwrap()
            .unwrap();
        assert!(record.deleted.is_some());
        assert_eq!(record.deleted_with, Some(uuid(&plans)));
    }

    // The bin shows one entry, the library, with its kind.
    let entries = w.bin(&w.olive).await;
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(entries[0]["id"], plans.as_str());
    assert_eq!(entries[0]["kind"], "library");
    assert_eq!(entries[0]["name"], "Plans");
    assert_eq!(entries[0]["path"], json!(["Team"]));
    assert_eq!(entries[0]["stage"], "first");

    // Restored as a folder is, with everything deleted with it.
    let (status, body) = w
        .call(
            &w.olive,
            Method::POST,
            &format!("/api/bin/{plans}/restore"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["node"]["kind"], "library");
    assert_eq!(body["renamed"], Value::Null);
    assert_eq!(
        w.names(&w.root, &w.site).await.unwrap(),
        vec!["Documents".to_owned(), "Plans".to_owned()]
    );
    assert_eq!(
        w.names(&w.root, &plans).await.unwrap(),
        vec!["Drafts".to_owned(), "Sheet.xlsx".to_owned()]
    );
    assert_eq!(
        w.names(&w.root, &folder).await.unwrap(),
        vec!["Notes".to_owned()]
    );
    assert!(w.bin(&w.root).await.is_empty());
    let deleted = w.events("node.deleted").await;
    assert!(deleted.iter().any(|d| d["name"] == "Plans"), "{deleted:?}");
    let restored = w.events("node.restored").await;
    assert!(
        restored.iter().any(|d| d["name"] == "Plans"),
        "{restored:?}"
    );

    // The site itself cannot be deleted by its owner (administrators delete whole sites: see
    // tests/site_lifecycle.rs).
    let (status, _) = w
        .call(
            &w.olive,
            Method::DELETE,
            &format!("/api/nodes/{}", w.site),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn library_delete_needs_delete_below() {
    let w = world().await;
    let ed = w.server.user("Ed").await;
    w.grant(&w.site, &ed, "editor").await;
    let plans = w.child(&w.root, &w.site, "library", "Plans").await;
    let secret = w.child(&w.root, &plans, "folder", "Secret").await;
    // Secret has permissions of its own, where Olive only views.
    w.grant(&secret, &w.olive, "viewer").await;

    // An editor deletes folders and files, not libraries: that needs manage_site.
    let (status, body) = w
        .call(&ed, Method::DELETE, &format!("/api/nodes/{plans}"), None)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"], "forbidden");
    // Olive manages the site but may not delete inside Secret.
    let commits = w.commits();
    let (status, body) = w
        .call(
            &w.olive,
            Method::DELETE,
            &format!("/api/nodes/{plans}"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"], "forbidden_below");
    assert_eq!(w.commits(), commits, "nothing written");
    assert_eq!(
        w.names(&w.root, &w.site).await.unwrap(),
        vec!["Documents".to_owned(), "Plans".to_owned()]
    );

    // Root may.
    let (status, body) = w
        .call(
            &w.root,
            Method::DELETE,
            &format!("/api/nodes/{plans}"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(
        w.names(&w.root, &w.site).await.unwrap(),
        vec!["Documents".to_owned()]
    );
}

#[tokio::test]
async fn restored_library_name_taken_gets_number() {
    let w = world().await;
    let plans = w.child(&w.root, &w.site, "library", "Plans").await;
    let (status, _) = w
        .call(
            &w.root,
            Method::DELETE,
            &format!("/api/nodes/{plans}"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    // Its name is free meanwhile.
    let again = w.child(&w.root, &w.site, "library", "plans").await;

    let (status, body) = w
        .call(
            &w.root,
            Method::POST,
            &format!("/api/bin/{plans}/restore"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["node"]["name"], "Plans (2)");
    assert_eq!(body["renamed"]["from"], "Plans");
    assert_eq!(body["renamed"]["to"], "Plans (2)");
    assert_eq!(
        w.names(&w.root, &w.site).await.unwrap(),
        vec![
            "Documents".to_owned(),
            "plans".to_owned(),
            "Plans (2)".to_owned()
        ]
    );
    let record = w
        .server
        .store()
        .read_tree_record(uuid(&plans))
        .unwrap()
        .unwrap();
    assert_eq!(record.name, "Plans (2)");
    assert!(record.deleted.is_none());
    assert_ne!(again, plans);
}

/// One write tried against the archived site.
struct Write {
    method: Method,
    path: String,
    body: Option<Value>,
}

fn write(method: Method, path: String, body: Option<Value>) -> Write {
    Write { method, path, body }
}

#[tokio::test]
async fn archived_site_refuses_every_write() {
    let w = world().await;
    let uma = w.server.user("Uma").await;
    let vic = w.server.user("Vic").await;
    let rae = w.server.user("Rae").await;
    let other = w.create_site("Other").await;
    let other_library = w.first_library(&other).await;
    let outsider = w.doc(&other_library, "Outsider").await;
    let other_folder = w
        .child(&w.root, &other_library, "folder", "Elsewhere")
        .await;
    // Olive edits in Other, so that moving and copying from there into Team is hers to try.
    w.grant(&other, &w.olive, "editor").await;

    let folder = w.child(&w.root, &w.library, "folder", "Plans").await;
    let notes = w.doc(&w.library, "Notes").await;
    let sheet = w.uploaded(&w.library, "Sheet.xlsx").await;
    let gone = w.doc(&w.library, "Gone").await;
    let (status, _) = w
        .call(&w.root, Method::DELETE, &format!("/api/nodes/{gone}"), None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    w.grant(&w.site, &uma, "viewer").await;
    let members = w.group("members").await;
    let (status, _) = w
        .call(
            &w.root,
            Method::POST,
            &format!("/api/groups/{members}/members"),
            Some(json!({ "email": vic.email })),
        )
        .await;
    assert!(status.is_success());
    let (status, link) = w
        .call(
            &w.root,
            Method::POST,
            &format!("/api/nodes/{folder}/links"),
            Some(json!({ "kind": "signed_in", "role": "viewer" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let link_id = link["id"].as_str().unwrap().to_owned();
    let token = link["url"]
        .as_str()
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap()
        .to_owned();
    let (status, _) = w
        .call(
            &rae,
            Method::POST,
            &format!("/api/nodes/{folder}/requests"),
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let (_, open) = w.call(&w.root, Method::GET, "/api/requests", None).await;
    let request = open[0]["id"].as_str().unwrap().to_owned();

    let (status, body) = w.archive(&w.root).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["archived"]["at"].is_string(), "{body}");
    assert_eq!(body["archived"]["by"], "Root");

    let site = w.site.clone();
    let library = w.library.clone();
    for actor in [&w.root, &w.olive] {
        // An upload session that escaped the abandoning (as one started in the same instant
        // would): its chunks and its finish are refused all the same.
        let escaped = Uuid::new_v4();
        w.server
            .records_db()
            .start_upload(NewUpload {
                id: escaped,
                owner: actor.id,
                site: node_id(&site),
                target: UploadTarget::NewFile {
                    parent: node_id(&library),
                },
                name: "Escaped.xlsx",
                size: 3,
                sha256: sha_abc(),
                base: None,
                on_conflict: None,
            })
            .await
            .unwrap();

        let writes = vec![
            write(
                Method::PATCH,
                format!("/api/sites/{site}"),
                Some(json!({ "description": "Changed" })),
            ),
            write(
                Method::PUT,
                format!("/api/sites/{site}/quota"),
                Some(json!({ "bytes": 1_000_000 })),
            ),
            write(
                Method::POST,
                format!("/api/nodes/{site}/children"),
                Some(json!({ "kind": "library", "name": "More" })),
            ),
            write(
                Method::POST,
                format!("/api/nodes/{library}/children"),
                Some(json!({ "kind": "folder", "name": "New" })),
            ),
            write(
                Method::PATCH,
                format!("/api/nodes/{folder}"),
                Some(json!({ "name": "Renamed" })),
            ),
            write(
                Method::PATCH,
                format!("/api/nodes/{site}"),
                Some(json!({ "name": "Renamed site" })),
            ),
            // Moving out of the site, and into it.
            write(
                Method::PATCH,
                format!("/api/nodes/{folder}"),
                Some(json!({ "parent": other_library })),
            ),
            write(
                Method::PATCH,
                format!("/api/nodes/{other_folder}"),
                Some(json!({ "parent": library })),
            ),
            write(Method::DELETE, format!("/api/nodes/{folder}"), None),
            write(Method::DELETE, format!("/api/nodes/{library}"), None),
            // Copying into the site.
            write(
                Method::POST,
                format!("/api/nodes/{outsider}/copy"),
                Some(json!({ "parent": library })),
            ),
            write(
                Method::PUT,
                format!("/api/nodes/{folder}/access"),
                Some(json!({ "email": rae.email, "role": "viewer" })),
            ),
            write(
                Method::DELETE,
                format!("/api/nodes/{site}/access/user/{}", uma.id.0),
                None,
            ),
            write(
                Method::POST,
                format!("/api/nodes/{folder}/inheritance"),
                Some(json!({ "inherit": false })),
            ),
            write(
                Method::PUT,
                format!("/api/docs/{notes}/access"),
                Some(json!({ "email": rae.email, "role": "viewer" })),
            ),
            write(
                Method::DELETE,
                format!("/api/docs/{notes}/access/user/{}", uma.id.0),
                None,
            ),
            write(
                Method::POST,
                format!("/api/nodes/{folder}/links"),
                Some(json!({ "kind": "signed_in", "role": "viewer" })),
            ),
            write(Method::DELETE, format!("/api/links/{link_id}"), None),
            write(Method::POST, format!("/api/l/{token}/redeem"), None),
            write(
                Method::POST,
                format!("/api/nodes/{folder}/requests"),
                Some(json!({})),
            ),
            write(
                Method::POST,
                format!("/api/requests/{request}/approve"),
                Some(json!({ "role": "viewer" })),
            ),
            write(
                Method::POST,
                format!("/api/requests/{request}/decline"),
                Some(json!({})),
            ),
            write(
                Method::POST,
                "/api/docs".to_owned(),
                Some(json!({ "parent": library, "name": "New" })),
            ),
            write(
                Method::POST,
                "/api/uploads".to_owned(),
                Some(json!({
                    "parent": library, "name": "Up.xlsx", "size": 3, "sha256": ABC_SHA256,
                })),
            ),
            write(
                Method::POST,
                "/api/uploads".to_owned(),
                Some(json!({ "doc": sheet, "size": 3, "sha256": ABC_SHA256 })),
            ),
            write(Method::POST, format!("/api/uploads/{escaped}/finish"), None),
            write(Method::POST, format!("/api/bin/{gone}/restore"), None),
            write(Method::DELETE, format!("/api/bin/{gone}"), None),
            write(
                Method::POST,
                format!("/api/groups/{members}/members"),
                Some(json!({ "email": rae.email })),
            ),
            write(
                Method::PUT,
                format!("/api/groups/{members}/members/{}", rae.id.0),
                None,
            ),
            write(
                Method::DELETE,
                format!("/api/groups/{members}/members/{}", vic.id.0),
                None,
            ),
        ];
        for tried in writes {
            let (status, body) = w
                .call(actor, tried.method.clone(), &tried.path, tried.body.clone())
                .await;
            assert_eq!(
                (status, body["error"].as_str()),
                (StatusCode::FORBIDDEN, Some("archived")),
                "{} {} as {}: {body}",
                tried.method,
                tried.path,
                actor.email
            );
        }

        // Writes with bodies that are not JSON.
        let saved = w
            .server
            .http(Some(actor))
            .put(w.server.url(&format!("/api/docs/{sheet}/content")))
            .header("x-sharecraft-seat", "no-seat")
            .body("xyz")
            .send()
            .await
            .unwrap();
        let (status, body) = answer(saved).await;
        assert_eq!(
            (status, body["error"].as_str()),
            (StatusCode::FORBIDDEN, Some("archived")),
            "save as {}: {body}",
            actor.email
        );
        let uploaded = w
            .server
            .http(Some(actor))
            .post(
                w.server
                    .url(&format!("/api/docs/upload?parent={library}&name=Up.xlsx")),
            )
            .body("abc")
            .send()
            .await
            .unwrap();
        let (status, body) = answer(uploaded).await;
        assert_eq!(
            (status, body["error"].as_str()),
            (StatusCode::FORBIDDEN, Some("archived")),
            "upload as {}: {body}",
            actor.email
        );
        let chunk = w
            .server
            .http(Some(actor))
            .put(w.server.url(&format!("/api/uploads/{escaped}")))
            .header("content-range", "bytes 0-2/3")
            .body("abc")
            .send()
            .await
            .unwrap();
        let (status, body) = answer(chunk).await;
        assert_eq!(
            (status, body["error"].as_str()),
            (StatusCode::FORBIDDEN, Some("archived")),
            "chunk as {}: {body}",
            actor.email
        );

        // Bulk operations answer each item as its own route would.
        for (op, items, parent) in [
            ("delete", vec![folder.clone(), notes.clone()], None),
            ("restore", vec![gone.clone()], None),
            ("move", vec![folder.clone()], Some(other_library.clone())),
            ("move", vec![other_folder.clone()], Some(library.clone())),
            ("copy", vec![outsider.clone()], Some(library.clone())),
        ] {
            let mut body = json!({ "op": op, "items": items });
            if let Some(parent) = parent {
                body["parent"] = json!(parent);
            }
            let (status, results) = w.call(actor, Method::POST, "/api/bulk", Some(body)).await;
            assert_eq!(status, StatusCode::OK, "{results}");
            for result in results.as_array().unwrap() {
                assert_eq!(result["ok"], false, "{op}: {results}");
                assert_eq!(result["error"], "archived", "{op}: {results}");
            }
        }
    }

    // Nothing changed meanwhile.
    assert_eq!(
        w.names(&w.root, &w.library).await.unwrap(),
        vec![
            "Plans".to_owned(),
            "Notes".to_owned(),
            "Sheet.xlsx".to_owned()
        ]
    );
    assert_eq!(
        w.names(&w.root, &w.site).await.unwrap(),
        vec!["Documents".to_owned()]
    );
    let usage = w
        .server
        .records_db()
        .site_usage(node_id(&w.site))
        .await
        .unwrap();
    assert_eq!(usage.quota, None);

    // Reads, downloads, the explorer, the bin and the access pages keep working.
    for actor in [&w.root, &w.olive] {
        let (status, body) = w
            .call(actor, Method::GET, &format!("/api/sites/{site}"), None)
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["access"]["perms"], json!(["view"]), "{body}");
        let (status, body) = w
            .call(actor, Method::GET, &format!("/api/nodes/{folder}"), None)
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["access"]["perms"], json!(["view"]), "{body}");
        let downloaded = w
            .server
            .http(Some(actor))
            .get(w.server.url(&format!("/api/docs/{sheet}/content")))
            .send()
            .await
            .unwrap();
        assert_eq!(downloaded.status(), StatusCode::OK);
        assert_eq!(downloaded.bytes().await.unwrap().as_ref(), b"abc");
        for path in [
            format!("/api/explorer/{site}"),
            format!("/api/nodes/{folder}/access"),
            format!("/api/nodes/{folder}/links"),
            format!("/api/nodes/{folder}/inheritance/preview"),
            format!("/api/sites/{site}/bin?stage=second"),
            format!("/api/sites/{site}/groups"),
            format!("/api/docs/{notes}"),
            "/api/requests".to_owned(),
        ] {
            let (status, body) = w.call(actor, Method::GET, &path, None).await;
            assert_eq!(status, StatusCode::OK, "{path} as {}: {body}", actor.email);
        }
        let names: Vec<Value> = w
            .bin(actor)
            .await
            .iter()
            .map(|e| e["name"].clone())
            .collect();
        assert_eq!(names, vec![json!("Gone")]);
    }
    // A text document opens read-only.
    let reader = TestClient::connect_as(&w.server, doc_id(&notes), &w.root).await;
    assert!(!reader.can_edit);
    let seat = FileClient::connect_as(&w.server, doc_id(&sheet), &w.olive).await;
    assert!(!seat.can_edit);
    assert!(seat.token().is_none());
    // The open request is still open.
    let (_, open) = w.call(&w.root, Method::GET, "/api/requests", None).await;
    assert_eq!(open[0]["id"], request.as_str());
}

#[tokio::test]
async fn archiving_ends_open_sessions() {
    let w = world().await;
    let bob = w.server.user("Bob").await;
    w.grant(&w.site, &bob, "editor").await;
    let other = w.create_site("Other").await;
    let other_library = w.first_library(&other).await;
    let notes = w.doc(&w.library, "Notes").await;
    let sheet = w.file(&w.library, "Sheet.xlsx").await;
    let outside = w.doc(&other_library, "Outside").await;

    let mut a = TestClient::connect_as(&w.server, doc_id(&notes), &w.root).await;
    let mut b = TestClient::connect_as(&w.server, doc_id(&notes), &bob).await;
    a.pump_until(|c| c.people().len() == 2).await;
    let mut seat = FileClient::connect_as(&w.server, doc_id(&sheet), &bob).await;
    assert!(seat.token().is_some(), "Bob holds the seat");
    let mut watcher = FileClient::connect_as(&w.server, doc_id(&sheet), &w.olive).await;
    let mut away = TestClient::connect_as(&w.server, doc_id(&outside), &w.root).await;
    // An upload under way into the site holds its size reserved.
    let (status, started) = w
        .call(
            &bob,
            Method::POST,
            "/api/uploads",
            Some(json!({
                "parent": w.library, "name": "Big.xlsx", "size": 3, "sha256": ABC_SHA256,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{started}");
    let upload = started["id"].as_str().unwrap().to_owned();
    let db = w.server.records_db();
    assert_eq!(db.site_usage(node_id(&w.site)).await.unwrap().reserved, 3);

    let (status, body) = w.archive(&w.root).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(removed_promptly(&mut a).await, RemovedReason::Archived);
    assert_eq!(removed_promptly(&mut b).await, RemovedReason::Archived);
    assert_eq!(
        file_removed_promptly(&mut seat).await,
        RemovedReason::Archived
    );
    assert_eq!(
        file_removed_promptly(&mut watcher).await,
        RemovedReason::Archived
    );
    // The upload session is gone and its reservation released.
    let (status, _) = w
        .call(&bob, Method::GET, &format!("/api/uploads/{upload}"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(db.upload_session(uuid(&upload)).await.unwrap().is_none());
    assert_eq!(db.site_usage(node_id(&w.site)).await.unwrap().reserved, 0);
    let archived = w.events("site.archived").await;
    assert_eq!(archived.len(), 1, "{archived:?}");
    assert_eq!(archived[0]["name"], "Team");

    // A document in another site stays open.
    away.type_at(0, "still here").await;
    away.pump_until(|c| c.pending() == 0).await;
    assert!(away.removed.is_none());
}

#[tokio::test]
async fn archived_anyone_link_answers_expired() {
    let w = world().await;
    let (status, _) = w
        .call(
            &w.root,
            Method::PUT,
            "/api/settings/sharing",
            Some(json!({ "anyone_links": true })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let folder = w.child(&w.root, &w.library, "folder", "Public").await;
    let notes = w.doc(&folder, "Notes").await;
    let expires = (OffsetDateTime::now_utc() + time::Duration::days(3))
        .format(&Rfc3339)
        .unwrap();
    let (status, link) = w
        .call(
            &w.root,
            Method::POST,
            &format!("/api/nodes/{folder}/links"),
            Some(json!({ "kind": "anyone", "role": "viewer", "expires_at": expires })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{link}");
    let token = link["url"]
        .as_str()
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap()
        .to_owned();
    let anonymous = reqwest::Client::new();
    let fetch = |path: String| {
        let anonymous = anonymous.clone();
        let url = w.server.url(&path);
        async move { answer(anonymous.get(url).send().await.unwrap()).await }
    };
    let (status, _) = fetch(format!("/api/l/{token}")).await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = w.archive(&w.root).await;
    assert_eq!(status, StatusCode::OK);
    for path in [
        format!("/api/l/{token}"),
        format!("/api/l/{token}/children/{folder}"),
        format!("/api/l/{token}/text/{notes}"),
    ] {
        let (status, body) = fetch(path.clone()).await;
        assert_eq!(
            (status, body["error"].as_str()),
            (StatusCode::NOT_FOUND, Some("link_not_found")),
            "{path}"
        );
    }
    // The link itself is not revoked: it works again once the site is reactivated.
    let (_, listed) = w
        .call(
            &w.root,
            Method::GET,
            &format!("/api/nodes/{folder}/links"),
            None,
        )
        .await;
    assert_eq!(listed[0]["revoked"], Value::Null, "{listed}");
    let (status, _) = w.reactivate(&w.root).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = fetch(format!("/api/l/{token}")).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn archived_site_hidden_from_directory_except_managers() {
    let w = world().await;
    let mia = w.server.user("Mia").await;
    w.grant(&w.site, &mia, "editor").await;
    let (status, _) = w.archive(&w.root).await;
    assert_eq!(status, StatusCode::OK);

    for (user, seen) in [(&w.root, true), (&w.olive, true), (&mia, false)] {
        let (status, listed) = w.call(user, Method::GET, "/api/sites", None).await;
        assert_eq!(status, StatusCode::OK, "{listed}");
        let team: Vec<&Value> = listed
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| s["id"] == w.site.as_str())
            .collect();
        assert_eq!(
            team.len(),
            usize::from(seen),
            "{} sees {listed}",
            user.email
        );
        if seen {
            assert_eq!(team[0]["archived"], true);
            assert_eq!(team[0]["access"]["perms"], json!(["view"]));
        }
    }
    // Hidden from discovery, not from the people who could read it: Mia still opens it.
    let (status, body) = w
        .call(&mia, Method::GET, &format!("/api/sites/{}", w.site), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["archived"].is_object());
    // Only administrators archive and reactivate.
    let (status, body) = w.reactivate(&w.olive).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body) = w.archive(&w.olive).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
}

#[tokio::test]
async fn reactivate_restores_writes() {
    let w = world().await;
    let commits = w.commits();
    let (status, body) = w.archive(&w.root).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(w.commits(), commits + 1, "one sites commit");
    let record = w
        .server
        .store()
        .read_site_record(uuid(&w.site))
        .unwrap()
        .unwrap();
    let stamp = record.archived.unwrap();
    assert_eq!(stamp.by.name, "Root");
    // Archiving again changes nothing and writes nothing.
    let (status, again) = w.archive(&w.root).await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again["archived"], body["archived"]);
    assert_eq!(w.commits(), commits + 1);
    assert_eq!(w.events("site.archived").await.len(), 1);
    let (status, body) = w
        .call(
            &w.olive,
            Method::POST,
            &format!("/api/nodes/{}/children", w.library),
            Some(json!({ "kind": "folder", "name": "Plans" })),
        )
        .await;
    assert_eq!(
        (status, body["error"].as_str()),
        (StatusCode::FORBIDDEN, Some("archived"))
    );

    let (status, body) = w.reactivate(&w.root).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["archived"], Value::Null);
    assert_eq!(w.commits(), commits + 2);
    let record = w
        .server
        .store()
        .read_site_record(uuid(&w.site))
        .unwrap()
        .unwrap();
    assert!(record.archived.is_none());
    let reactivated = w.events("site.reactivated").await;
    assert_eq!(reactivated.len(), 1);
    // Reactivating an active site changes nothing.
    let (status, _) = w.reactivate(&w.root).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(w.commits(), commits + 2);
    assert_eq!(w.events("site.reactivated").await.len(), 1);

    // Writes work again, for the owner and for the administrator.
    let folder = w.child(&w.olive, &w.library, "folder", "Plans").await;
    let (status, body) = w
        .call(
            &w.root,
            Method::PATCH,
            &format!("/api/sites/{}", w.site),
            Some(json!({ "description": "Back" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = w
        .call(&w.root, Method::GET, &format!("/api/nodes/{folder}"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body["access"]["perms"]
            .as_array()
            .unwrap()
            .contains(&json!("manage_site"))
    );
    let notes = w.doc(&w.library, "Notes").await;
    let mut editor = TestClient::connect_as(&w.server, doc_id(&notes), &w.olive).await;
    assert!(editor.can_edit);
    editor.type_at(0, "editable again").await;
    editor.pump_until(|c| c.pending() == 0).await;
}
