//! Deleting, restoring and purging whole sites over HTTP: a deleted site answers `404` to
//! everyone and everything in it is unreachable, an administrator restores it as it was (its
//! bin, links, groups, members, quota and hub), and a purge (by an administrator, or by the
//! sweeper 93 days after the deletion) tombstones every record of the site, deletes what it
//! owned and erases only the contents nothing else names. A purge interrupted by a stop is
//! finished at the next start. Link tokens are kept in variables and never printed.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::time::Duration;

use collab_core::ids::{DocId, NodeId};
use common::{Site, StoppableServer, TestClient, TestServer, TestUser, spawn_server};
use protocol::RemovedReason;
use records::test_support::TestDb;
use reqwest::{Method, StatusCode};
use revisions::{Author, BlobRef, HubRecord, SitesChange};
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

/// How soon a person must be out of a session once the site is deleted.
const PROMPTLY: Duration = Duration::from_secs(1);

/// A server with the site Team made by the administrator Root, its library, and Olive, who owns
/// Team (Owner on it) without being an administrator.
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
    let site = create_site(&server, &root, "Team").await;
    let library = first_library(&server, &root, &site).await;
    let w = World {
        server,
        root,
        olive,
        site,
        library,
    };
    w.grant(&w.site, &w.olive, "owner").await;
    w
}

/// `method path` with `body` as `user` on `site`.
async fn call_on(
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

async fn create_site(site: &Site, root: &TestUser, name: &str) -> String {
    let (status, body) = call_on(
        site,
        root,
        Method::POST,
        "/api/sites",
        Some(json!({ "name": name })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["id"].as_str().unwrap().to_owned()
}

async fn first_library(site: &Site, root: &TestUser, id: &str) -> String {
    let (status, page) = call_on(
        site,
        root,
        Method::GET,
        &format!("/api/nodes/{id}/children"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    page["items"][0]["id"].as_str().unwrap().to_owned()
}

/// A file uploaded by `user` into `parent`, holding `bytes`.
async fn upload(site: &Site, user: &TestUser, parent: &str, name: &str, bytes: &[u8]) -> String {
    let response = site
        .http(Some(user))
        .post(site.url(&format!("/api/docs/upload?parent={parent}&name={name}")))
        .body(bytes.to_vec())
        .send()
        .await
        .unwrap();
    let (status, body) = answer(response).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["id"].as_str().unwrap().to_owned()
}

impl World {
    async fn call(
        &self,
        user: &TestUser,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        call_on(&self.server, user, method, path, body).await
    }

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

    /// A text document named `name` in `parent`, made by Root.
    async fn doc(&self, parent: &str, name: &str) -> String {
        let (status, body) = self
            .call(
                &self.root,
                Method::POST,
                "/api/docs",
                Some(json!({ "parent": parent, "name": name })),
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

    /// `DELETE /api/sites/{site}` as `user`.
    async fn delete_site(&self, user: &TestUser, site: &str) -> (StatusCode, Value) {
        self.call(user, Method::DELETE, &format!("/api/sites/{site}"), None)
            .await
    }

    /// `POST /api/admin/sites/{site}/restore` as `user`.
    async fn restore_site(&self, user: &TestUser, site: &str) -> (StatusCode, Value) {
        self.call(
            user,
            Method::POST,
            &format!("/api/admin/sites/{site}/restore"),
            None,
        )
        .await
    }

    /// `DELETE /api/admin/sites/{site}` as `user`.
    async fn purge_site(&self, user: &TestUser, site: &str) -> (StatusCode, Value) {
        self.call(
            user,
            Method::DELETE,
            &format!("/api/admin/sites/{site}"),
            None,
        )
        .await
    }

    /// The row of `site` in `GET /api/admin/sites` as Root; `None` if it is not listed.
    async fn admin_row(&self, site: &str) -> Option<Value> {
        let (status, listed) = self
            .call(&self.root, Method::GET, "/api/admin/sites", None)
            .await;
        assert_eq!(status, StatusCode::OK, "{listed}");
        listed
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == site)
            .cloned()
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

    /// The names in a stage of `site`'s bin as Root, with when each was deleted.
    async fn bin(&self, site: &str, stage: &str) -> Vec<(String, String)> {
        let (status, page) = self
            .call(
                &self.root,
                Method::GET,
                &format!("/api/sites/{site}/bin?stage={stage}"),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{page}");
        page["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| {
                (
                    item["name"].as_str().unwrap().to_owned(),
                    item["deleted_at"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    }

    /// The number of commits on `main`.
    fn commits(&self) -> usize {
        let repo = gix::open(self.server.library()).unwrap();
        let tip = repo.find_reference("refs/heads/main").unwrap();
        repo.rev_walk([tip.id()]).all().unwrap().count()
    }

    /// The details and actors of the events of `kind` about `node`, oldest first.
    async fn events(&self, kind: &str, node: &str) -> Vec<(Option<Uuid>, Value)> {
        sqlx::query_as(
            "select actor, detail from events where kind = $1 and node_id = $2 order by id",
        )
        .bind(kind)
        .bind(uuid(node))
        .fetch_all(self.server.records_db().test_pool())
        .await
        .unwrap()
    }

    /// Makes `hub` a hub and `member` a site that joined it, in Git and in PostgreSQL, as
    /// registering a hub and joining it will.
    async fn join_hub(&self, member: &str, hub: &str) {
        join_hub(&self.server, self.server.store(), member, hub).await;
    }
}

/// Makes `hub` a hub and `member` a site that joined it, in Git (`store`) and in `site`'s
/// database.
async fn join_hub(site: &Site, store: revisions::RevisionStore, member: &str, hub: &str) {
    let mut hub_record = store.read_site_record(uuid(hub)).unwrap().unwrap();
    hub_record.hub = Some(HubRecord {
        navigation: Vec::new(),
    });
    let mut member_record = store.read_site_record(uuid(member)).unwrap().unwrap();
    member_record.joined_hub = Some(uuid(hub));
    let author = Author {
        id: "test".to_owned(),
        name: "Test".to_owned(),
    };
    let records = [hub_record, member_record];
    let mut change = SitesChange::new(&author, "Join a hub");
    change.sites = &records;
    store.write_sites(&change).unwrap();
    let pool = site.records_db();
    sqlx::query("update sites set is_hub = true, hub_navigation = '[]' where id = $1")
        .bind(uuid(hub))
        .execute(pool.test_pool())
        .await
        .unwrap();
    sqlx::query("update sites set joined_hub = $2 where id = $1")
        .bind(uuid(member))
        .bind(uuid(hub))
        .execute(pool.test_pool())
        .await
        .unwrap();
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

fn uuid(text: &str) -> Uuid {
    text.parse().unwrap()
}

fn doc_id(text: &str) -> DocId {
    DocId(text.parse().unwrap())
}

fn node_id(text: &str) -> NodeId {
    NodeId(text.parse().unwrap())
}

/// The token at the end of a link's `url`.
fn token_of(link: &Value) -> String {
    link["url"]
        .as_str()
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap()
        .to_owned()
}

/// The node as PostgreSQL holds it, with its bin state.
async fn row(site: &Site, node: &str) -> records::NodeRow {
    let id = node_id(node);
    site.records_db()
        .node_rows()
        .await
        .unwrap()
        .into_iter()
        .find(|row| row.id == id)
        .unwrap()
}

/// How many rows of `table` name the site `site` in `column`.
async fn count(site: &Site, table: &str, column: &str, id: &str) -> i64 {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "select count(*) from {table} where {column} = $1"
    )))
    .bind(uuid(id))
    .fetch_one(site.records_db().test_pool())
    .await
    .unwrap()
}

/// Checks that everything `site` owned in PostgreSQL is gone and every record of it in Git is a
/// tombstone purged by `by`, its site record removed.
async fn assert_purged(site: &Site, store: &revisions::RevisionStore, id: &str, by: &str) {
    let rows: Vec<records::NodeRow> = site
        .records_db()
        .node_rows()
        .await
        .unwrap()
        .into_iter()
        .filter(|row| row.site == node_id(id))
        .collect();
    assert!(rows.len() > 1, "the site's nodes stay as tombstones");
    for row in &rows {
        assert!(row.purged_at.is_some(), "{} is purged", row.name);
        let record = store.read_tree_record(row.id.0).unwrap().unwrap();
        assert_eq!(record.purged.as_ref().unwrap().by.id, by, "{}", row.name);
    }
    assert!(store.read_site_record(uuid(id)).unwrap().is_none());
    assert_eq!(count(site, "groups", "site_id", id).await, 0);
    assert_eq!(count(site, "site_usage", "site_id", id).await, 0);
    assert_eq!(count(site, "upload_sessions", "site_id", id).await, 0);
    let left: i64 = sqlx::query_scalar(
        "select (select count(*) from assignments a join nodes n on n.id = a.node_id \
                 where n.site_id = $1) \
              + (select count(*) from links l join nodes n on n.id = l.node_id \
                 where n.site_id = $1) \
              + (select count(*) from access_requests r join nodes n on n.id = r.node_id \
                 where n.site_id = $1)",
    )
    .bind(uuid(id))
    .fetch_one(site.records_db().test_pool())
    .await
    .unwrap();
    assert_eq!(
        left, 0,
        "assignments, links and requests of the site are gone"
    );
    let purging: bool = sqlx::query_scalar("select purging from sites where id = $1")
        .bind(uuid(id))
        .fetch_one(site.records_db().test_pool())
        .await
        .unwrap();
    assert!(!purging, "the purge is finished");
}

#[tokio::test]
async fn deleted_site_unreachable_by_everyone() {
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
    let folder = w.child(&w.library, "folder", "Public").await;
    let notes = w.doc(&folder, "Notes").await;
    let sheet = upload(&w.server, &w.root, &w.library, "Sheet.xlsx", b"sheet").await;
    let vic = w.server.user("Vic").await;
    let rae = w.server.user("Rae").await;
    let members = w.group(&w.site, "members").await;
    let (status, _) = w
        .call(
            &w.root,
            Method::POST,
            &format!("/api/groups/{members}/members"),
            Some(json!({ "email": vic.email })),
        )
        .await;
    assert!(status.is_success());
    let expires = (OffsetDateTime::now_utc() + time::Duration::days(3))
        .format(&Rfc3339)
        .unwrap();
    let (status, anyone) = w
        .call(
            &w.root,
            Method::POST,
            &format!("/api/nodes/{folder}/links"),
            Some(json!({ "kind": "anyone", "role": "viewer", "expires_at": expires })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{anyone}");
    let anyone = token_of(&anyone);
    let (status, signed_in) = w
        .call(
            &w.root,
            Method::POST,
            &format!("/api/nodes/{folder}/links"),
            Some(json!({ "kind": "signed_in", "role": "viewer" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let signed_in = token_of(&signed_in);
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
    let mut text = TestClient::connect_as(&w.server, doc_id(&notes), &w.olive).await;

    // One commit, recorded as `site.deleted`; the open session ends.
    let commits = w.commits();
    let (status, body) = w.delete_site(&w.root, &w.site).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(w.commits(), commits + 1);
    let deleted = w.events("site.deleted", &w.site).await;
    assert_eq!(deleted.len(), 1);
    assert_eq!(deleted[0].0, Some(w.root.id.0));
    assert_eq!(deleted[0].1["name"], "Team");
    tokio::time::timeout(PROMPTLY, text.pump_until(|c| c.removed.is_some()))
        .await
        .expect("not removed within one second");
    assert_eq!(text.removed, Some(RemovedReason::Deleted));
    // Its site node is soft-deleted with no bin stage; nothing in it is marked deleted.
    let site_row = row(&w.server, &w.site).await;
    assert!(site_row.deleted_at.is_some());
    assert_eq!(site_row.deleted_with, Some(node_id(&w.site)));
    assert_eq!(site_row.bin_stage, None);
    assert!(row(&w.server, &folder).await.deleted_at.is_none());
    let record = w
        .server
        .store()
        .read_tree_record(uuid(&w.site))
        .unwrap()
        .unwrap();
    assert!(record.deleted.is_some() && record.bin_stage.is_none());

    // Pages, files, groups, explorer, bin: `404` to everyone, administrators included.
    for user in [&w.root, &w.olive, &vic] {
        for path in [
            format!("/api/sites/{}", w.site),
            format!("/api/nodes/{}", w.site),
            format!("/api/nodes/{}/children", w.site),
            format!("/api/nodes/{folder}"),
            format!("/api/nodes/{folder}/children"),
            format!("/api/nodes/{folder}/links"),
            format!("/api/nodes/{folder}/access"),
            format!("/api/docs/{notes}"),
            format!("/api/docs/{sheet}/content"),
            format!("/api/sites/{}/groups", w.site),
            format!("/api/sites/{}/bin", w.site),
            format!("/api/explorer/{}", w.site),
            format!("/api/groups/{members}"),
        ] {
            let (status, body) = w.call(user, Method::GET, &path, None).await;
            assert_eq!(
                status,
                StatusCode::NOT_FOUND,
                "{path} for {}: {body}",
                user.email
            );
        }
        // Not in the directory, and its groups in no list.
        let (_, listed) = w.call(user, Method::GET, "/api/sites", None).await;
        assert!(
            !listed
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s["id"] == w.site.as_str()),
            "{listed}"
        );
        let (_, groups) = w.call(user, Method::GET, "/api/groups", None).await;
        assert!(
            !groups
                .as_array()
                .unwrap()
                .iter()
                .any(|g| g["site"]["id"] == w.site.as_str()),
            "{groups}"
        );
    }
    // Writes to it answer as if it were missing.
    for (method, path, body) in [
        (
            Method::PATCH,
            format!("/api/sites/{}", w.site),
            Some(json!({ "description": "x" })),
        ),
        (
            Method::POST,
            format!("/api/nodes/{folder}/children"),
            Some(json!({ "kind": "folder", "name": "New" })),
        ),
        (
            Method::POST,
            format!("/api/groups/{members}/members"),
            Some(json!({ "email": rae.email })),
        ),
        (
            Method::POST,
            format!("/api/requests/{request}/approve"),
            Some(json!({ "role": "viewer" })),
        ),
    ] {
        let (status, answer) = w.call(&w.root, method, &path, body).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}: {answer}");
    }
    // Links and requests answer as if missing.
    let anonymous = reqwest::Client::new();
    for path in [
        format!("/api/l/{anyone}"),
        format!("/api/l/{anyone}/children/{folder}"),
        format!("/api/l/{anyone}/text/{notes}"),
    ] {
        let (status, body) = answer(anonymous.get(w.server.url(&path)).send().await.unwrap()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "link_not_found");
    }
    let (status, body) = w
        .call(
            &vic,
            Method::POST,
            &format!("/api/l/{signed_in}/redeem"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "link_not_found");
    let (_, open) = w.call(&w.root, Method::GET, "/api/requests", None).await;
    assert_eq!(open, json!([]));

    // Only the administrators' list names it, as deleted, with when it will be purged.
    let listed = w
        .admin_row(&w.site)
        .await
        .expect("listed for administrators");
    assert_eq!(listed["state"], "deleted");
    assert_eq!(listed["name"], "Team");
    assert_eq!(listed["deleted"]["by"], "Root");
    let at = OffsetDateTime::parse(listed["deleted"]["at"].as_str().unwrap(), &Rfc3339).unwrap();
    let purge_at = OffsetDateTime::parse(listed["purge_at"].as_str().unwrap(), &Rfc3339).unwrap();
    assert_eq!(purge_at - at, time::Duration::days(93));
    // Lifecycle routes other than restore and purge say it is deleted.
    let (status, body) = w.delete_site(&w.root, &w.site).await;
    assert_eq!(
        (status, body["error"].as_str()),
        (StatusCode::CONFLICT, Some("site_deleted"))
    );
    let (status, body) = w
        .call(
            &w.root,
            Method::POST,
            &format!("/api/sites/{}/archive", w.site),
            None,
        )
        .await;
    assert_eq!(
        (status, body["error"].as_str()),
        (StatusCode::CONFLICT, Some("site_deleted"))
    );
    // A site entry is no recycle bin entry.
    let (status, _) = w
        .call(
            &w.root,
            Method::POST,
            &format!("/api/bin/{}/restore", w.site),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn restore_site_brings_back_everything() {
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
    let hub = create_site(&w.server, &w.root, "Hub").await;
    w.join_hub(&w.site, &hub).await;
    let folder = w.child(&w.library, "folder", "Public").await;
    let notes = w.doc(&folder, "Notes").await;
    upload(&w.server, &w.root, &w.library, "Sheet.xlsx", b"sheet bytes").await;
    let gone = w.doc(&w.library, "Gone").await;
    let older = w.child(&w.library, "folder", "Older").await;
    for node in [&gone, &older] {
        let (status, _) = w
            .call(&w.root, Method::DELETE, &format!("/api/nodes/{node}"), None)
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }
    let (status, _) = w
        .call(&w.root, Method::DELETE, &format!("/api/bin/{older}"), None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "Older to the second stage");
    let vic = w.server.user("Vic").await;
    let members = w.group(&w.site, "members").await;
    let (status, _) = w
        .call(
            &w.root,
            Method::POST,
            &format!("/api/groups/{members}/members"),
            Some(json!({ "email": vic.email })),
        )
        .await;
    assert!(status.is_success());
    let (status, _) = w
        .call(
            &w.root,
            Method::PUT,
            &format!("/api/sites/{}/quota", w.site),
            Some(json!({ "bytes": 5_000_000 })),
        )
        .await;
    assert!(status.is_success());
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
    let token = token_of(&link);
    // An archived site may be deleted, and comes back archived.
    let (status, _) = w
        .call(
            &w.root,
            Method::POST,
            &format!("/api/sites/{}/archive", w.site),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (_, before) = w
        .call(
            &w.root,
            Method::GET,
            &format!("/api/sites/{}", w.site),
            None,
        )
        .await;
    let first = w.bin(&w.site, "first").await;
    let second = w.bin(&w.site, "second").await;
    assert_eq!(first.len(), 1);
    assert_eq!(second.len(), 1);

    let (status, body) = w.delete_site(&w.root, &w.site).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let commits = w.commits();
    let (status, body) = w.restore_site(&w.root, &w.site).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "archived", "{body}");
    assert_eq!(w.commits(), commits + 1);
    let restored = w.events("site.restored", &w.site).await;
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].0, Some(w.root.id.0));

    // Everything as it was: settings, storage, owners, hub, bin with its stages and times.
    let (_, after) = w
        .call(
            &w.root,
            Method::GET,
            &format!("/api/sites/{}", w.site),
            None,
        )
        .await;
    assert_eq!(after, before);
    assert_eq!(after["hub"]["id"], hub.as_str(), "{after}");
    assert!(after["archived"]["at"].is_string());
    assert_eq!(after["quota"], 5_000_000);
    assert_eq!(w.bin(&w.site, "first").await, first);
    assert_eq!(w.bin(&w.site, "second").await, second);
    // Groups and their members.
    let (status, group) = w
        .call(
            &w.root,
            Method::GET,
            &format!("/api/groups/{members}"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        group["members"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["email"] == vic.email.as_str()),
        "{group}"
    );
    let (status, _) = w
        .call(&vic, Method::GET, &format!("/api/docs/{notes}"), None)
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "Vic reads through the Members group again"
    );
    assert_eq!(w.admin_row(&w.site).await.unwrap()["state"], "archived");
    // The link was never revoked: once the site is reactivated it works again.
    let (_, listed) = w
        .call(
            &w.root,
            Method::GET,
            &format!("/api/nodes/{folder}/links"),
            None,
        )
        .await;
    assert_eq!(listed[0]["revoked"], Value::Null, "{listed}");
    let (status, _) = w
        .call(
            &w.root,
            Method::POST,
            &format!("/api/sites/{}/reactivate", w.site),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let anonymous = reqwest::Client::new();
    let (status, _) = answer(
        anonymous
            .get(w.server.url(&format!("/api/l/{token}")))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Restoring or purging a site that is not deleted is refused.
    let (status, body) = w.restore_site(&w.root, &w.site).await;
    assert_eq!(
        (status, body["error"].as_str()),
        (StatusCode::CONFLICT, Some("not_deleted"))
    );
    let (status, body) = w.purge_site(&w.root, &w.site).await;
    assert_eq!(
        (status, body["error"].as_str()),
        (StatusCode::CONFLICT, Some("not_deleted"))
    );
}

#[tokio::test]
async fn restored_site_name_taken_gets_number() {
    let w = world().await;
    let (status, _) = w.delete_site(&w.root, &w.site).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    // Its name is free while it is deleted.
    let other = create_site(&w.server, &w.root, "Team").await;
    let (status, body) = w.restore_site(&w.root, &w.site).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["name"], "Team (2)");
    assert_eq!(body["renamed_from"], "Team");
    let (_, site) = w
        .call(
            &w.root,
            Method::GET,
            &format!("/api/sites/{}", w.site),
            None,
        )
        .await;
    assert_eq!(site["name"], "Team (2)");
    let (_, kept) = w
        .call(&w.root, Method::GET, &format!("/api/sites/{other}"), None)
        .await;
    assert_eq!(kept["name"], "Team");
    let record = w
        .server
        .store()
        .read_tree_record(uuid(&w.site))
        .unwrap()
        .unwrap();
    assert_eq!(record.name, "Team (2)");
    assert!(record.deleted.is_none());
    let restored = w.events("site.restored", &w.site).await;
    assert_eq!(restored[0].1["from_name"], "Team");
}

#[tokio::test]
async fn deleted_site_purged_after_93_days() {
    let w = world().await;
    let folder = w.child(&w.library, "folder", "Plans").await;
    w.doc(&folder, "Notes").await;
    let only = b"bytes only Team names";
    upload(&w.server, &w.root, &folder, "Only.xlsx", only).await;
    let (status, _) = w.delete_site(&w.root, &w.site).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let state = w.server.app_state().await;
    let now = OffsetDateTime::now_utc();

    server::sweep_once(&state, now + time::Duration::days(92)).await;
    assert!(row(&w.server, &w.site).await.purged_at.is_none());
    assert!(w.server.blobs().contains(&BlobRef::of(only).sha256));

    let commits = w.commits();
    server::sweep_once(&state, now + time::Duration::days(94)).await;
    assert_purged(&w.server, &w.server.store(), &w.site, "sharecraft").await;
    assert_eq!(
        w.commits(),
        commits + 1,
        "one commit tombstones the whole site"
    );
    assert!(!w.server.blobs().contains(&BlobRef::of(only).sha256));
    let purged = w.events("site.purged", &w.site).await;
    assert_eq!(purged.len(), 1);
    assert_eq!(purged[0].0, None, "purged by the server");
    assert_eq!(purged[0].1["name"], "Team");
    assert!(w.admin_row(&w.site).await.is_none());
    // A second sweep finds nothing more to do.
    server::sweep_once(&state, now + time::Duration::days(95)).await;
    assert_eq!(w.commits(), commits + 1);
}

#[tokio::test]
async fn site_purge_keeps_blobs_other_sites_name() {
    let w = world().await;
    let other = create_site(&w.server, &w.root, "Other").await;
    let other_library = first_library(&w.server, &w.root, &other).await;
    let shared = b"bytes a copy in Other names too";
    let only = b"bytes only Team names";
    let copied = upload(&w.server, &w.root, &w.library, "Shared.xlsx", shared).await;
    upload(&w.server, &w.root, &w.library, "Only.xlsx", only).await;
    let (status, copy) = w
        .call(
            &w.root,
            Method::POST,
            &format!("/api/nodes/{copied}/copy"),
            Some(json!({ "parent": other_library })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{copy}");
    let copy = copy["node"]["id"].as_str().unwrap().to_owned();

    // Only a deleted site is purged, and only by an administrator.
    let (status, body) = w.purge_site(&w.root, &w.site).await;
    assert_eq!(
        (status, body["error"].as_str()),
        (StatusCode::CONFLICT, Some("not_deleted"))
    );
    let (status, _) = w.delete_site(&w.root, &w.site).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, body) = w.purge_site(&w.root, &w.site).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    assert_purged(
        &w.server,
        &w.server.store(),
        &w.site,
        &w.root.id.0.to_string(),
    )
    .await;
    let blobs = w.server.blobs();
    assert!(
        blobs.contains(&BlobRef::of(shared).sha256),
        "the copy names it"
    );
    assert!(
        !blobs.contains(&BlobRef::of(only).sha256),
        "nothing names it"
    );
    let response = w
        .server
        .http(Some(&w.root))
        .get(w.server.url(&format!("/api/docs/{copy}/content")))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.bytes().await.unwrap().as_ref(), shared);
    let purged = w.events("site.purged", &w.site).await;
    assert_eq!(purged.len(), 1);
    assert_eq!(purged[0].0, Some(w.root.id.0));
    assert_eq!(purged[0].1["bytes_freed"], only.len());
    // Purged is gone: nothing more to restore or purge.
    let (status, _) = w.restore_site(&w.root, &w.site).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = w.purge_site(&w.root, &w.site).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn refused_purge_leaves_site_restorable() {
    let w = world().await;
    let (status, _) = w.delete_site(&w.root, &w.site).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    // PostgreSQL refuses to mark the site's nodes purged, as a change refused in the judged
    // transaction would be; the purge's commit is then taken back.
    sqlx::raw_sql(
        "create function refuse_purge() returns trigger language plpgsql as $$          begin if new.purged_at is not null and old.purged_at is null then          raise exception using errcode = 'unique_violation', message = 'refused'; end if;          return new; end $$;          create trigger refuse_purge before update on nodes          for each row execute function refuse_purge();",
    )
    .execute(w.server.records_db().test_pool())
    .await
    .unwrap();

    let (status, body) = w.purge_site(&w.root, &w.site).await;
    assert!(status.is_client_error(), "{status} {body}");
    let purging: bool = sqlx::query_scalar("select purging from sites where id = $1")
        .bind(uuid(&w.site))
        .fetch_one(w.server.records_db().test_pool())
        .await
        .unwrap();
    assert!(!purging, "a refused purge leaves the site unmarked");
    assert!(
        w.server
            .store()
            .read_tree_record(uuid(&w.site))
            .unwrap()
            .unwrap()
            .purged
            .is_none(),
        "the purge was taken back in Git"
    );
    assert!(w.events("site.purged", &w.site).await.is_empty());

    // The site is still deleted, and can be restored.
    sqlx::raw_sql("drop trigger refuse_purge on nodes")
        .execute(w.server.records_db().test_pool())
        .await
        .unwrap();
    let (status, body) = w.restore_site(&w.root, &w.site).await;
    assert!(status.is_success(), "{status} {body}");
    assert!(row(&w.server, &w.site).await.deleted_at.is_none());
}

#[tokio::test]
async fn site_purge_resumes_after_restart() {
    let data_dir = tempfile::tempdir().unwrap();
    let db = TestDb::create().await;
    let only = b"bytes only Team names";
    let mut first = StoppableServer::start(data_dir.path(), &db, |_| {});
    let root = first.admin("Root").await;
    let site = create_site(&first, &root, "Team").await;
    let library = first_library(&first, &root, &site).await;
    upload(&first, &root, &library, "Only.xlsx", only).await;
    let (status, _) = call_on(
        &first,
        &root,
        Method::DELETE,
        &format!("/api/sites/{site}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    // The server stops just after a purge marked the site, before anything else.
    sqlx::query("update sites set purging = true where id = $1")
        .bind(uuid(&site))
        .execute(db.db.test_pool())
        .await
        .unwrap();
    first.stop();

    // The sweeper's first pass at the next start finishes it.
    let second = StoppableServer::start(data_dir.path(), &db, |_| {});
    let library_path = server::library_path(data_dir.path());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let purging: bool = sqlx::query_scalar("select purging from sites where id = $1")
            .bind(uuid(&site))
            .fetch_one(db.db.test_pool())
            .await
            .unwrap();
        if !purging {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the purge was not finished after the restart"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let store = revisions::RevisionStore::open(&library_path).unwrap();
    assert_purged(&second, &store, &site, "sharecraft").await;
    let blobs = revisions::BlobStore::open(&server::blobs_path(data_dir.path())).unwrap();
    assert!(!blobs.contains(&BlobRef::of(only).sha256));
}

#[tokio::test]
async fn purging_a_hub_releases_its_members() {
    let w = world().await;
    let member = create_site(&w.server, &w.root, "Member").await;
    w.join_hub(&member, &w.site).await;
    let (status, _) = w.delete_site(&w.root, &w.site).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    // While the hub is only deleted, its member still names it.
    let joined: Option<Uuid> = sqlx::query_scalar("select joined_hub from sites where id = $1")
        .bind(uuid(&member))
        .fetch_one(w.server.records_db().test_pool())
        .await
        .unwrap();
    assert_eq!(joined, Some(uuid(&w.site)));

    let commits = w.commits();
    let (status, body) = w.purge_site(&w.root, &w.site).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(
        w.commits(),
        commits + 1,
        "the member leaves in the purge's commit"
    );
    let row: (Option<Uuid>,) = sqlx::query_as("select joined_hub from sites where id = $1")
        .bind(uuid(&member))
        .fetch_one(w.server.records_db().test_pool())
        .await
        .unwrap();
    assert_eq!(row.0, None);
    let hub_row: (bool, Option<Uuid>) =
        sqlx::query_as("select is_hub, joined_hub from sites where id = $1")
            .bind(uuid(&w.site))
            .fetch_one(w.server.records_db().test_pool())
            .await
            .unwrap();
    assert_eq!(hub_row, (false, None));
    let record = w
        .server
        .store()
        .read_site_record(uuid(&member))
        .unwrap()
        .unwrap();
    assert_eq!(record.joined_hub, None);
    let (_, site) = w
        .call(&w.root, Method::GET, &format!("/api/sites/{member}"), None)
        .await;
    assert_eq!(site["hub"], Value::Null, "{site}");
    assert_purged(
        &w.server,
        &w.server.store(),
        &w.site,
        &w.root.id.0.to_string(),
    )
    .await;
}

#[tokio::test]
async fn only_administrators_delete_restore_and_purge_sites() {
    let w = world().await;
    // An owner may not delete their site, through either route.
    let (status, body) = w.delete_site(&w.olive, &w.site).await;
    assert_eq!(
        (status, body["error"].as_str()),
        (StatusCode::FORBIDDEN, Some("forbidden"))
    );
    let (status, body) = w
        .call(
            &w.olive,
            Method::DELETE,
            &format!("/api/nodes/{}", w.site),
            None,
        )
        .await;
    assert_eq!(
        (status, body["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("is_site"))
    );
    let (status, _) = w
        .call(&w.olive, Method::GET, "/api/admin/sites", None)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // An administrator deletes it through the node route too.
    let commits = w.commits();
    let (status, body) = w
        .call(
            &w.root,
            Method::DELETE,
            &format!("/api/nodes/{}", w.site),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(w.commits(), commits + 1);
    assert_eq!(w.admin_row(&w.site).await.unwrap()["state"], "deleted");

    for user in [&w.olive] {
        let (status, _) = w.restore_site(user, &w.site).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) = w.purge_site(user, &w.site).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }
    assert!(row(&w.server, &w.site).await.deleted_at.is_some());
    assert_eq!(w.commits(), commits + 1, "nothing changed");
    let (status, _) = w.restore_site(&w.root, &w.site).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = w.restore_site(&w.root, &Uuid::new_v4().to_string()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // The owner has their site back.
    let (status, _) = w
        .call(
            &w.olive,
            Method::GET,
            &format!("/api/sites/{}", w.site),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
}
