//! A file document's bytes over HTTP: download, save by the seat holder, and upload as a new
//! document. The server never interprets the bytes and never serves them in a way a browser
//! would render.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::path::Path;
use std::sync::Arc;

use collab_core::ids::DocId;
use common::{
    FileClient, Site, TestServer, TestUser, new_doc, spawn_server, spawn_server_config,
    spawn_server_with, wait_events,
};
use reqwest::StatusCode;
use reqwest::header::{
    CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_SECURITY_POLICY, CONTENT_TYPE, ORIGIN,
    X_CONTENT_TYPE_OPTIONS,
};
use revisions::{
    Checkpoint, CheckpointError, GitError, Outcome, RevisionId, RevisionInfo, RevisionStore,
    Stored, TreeChange, TreeRecord,
};
use serde_json::Value;
use server::Checkpointer;
use uuid::Uuid;

const SEAT: &str = "x-sharecraft-seat";
const REVISION: &str = "x-sharecraft-revision";

/// Every byte value, twice, so that nothing is mistaken for text.
fn every_byte() -> Vec<u8> {
    (0..=255u8).chain(0..=255u8).collect()
}

/// A page with a script: what a browser would run if it rendered the download.
const HTML_PAGE: &[u8] =
    b"<!doctype html><html><body><script>alert(document.cookie)</script></body></html>";

/// Uploads `body` as `name` into `user`'s own folder.
async fn upload(site: &Site, user: &TestUser, name: &str, body: Vec<u8>) -> reqwest::Response {
    let parent = home(site, user).await;
    site.http(Some(user))
        .post(site.url("/api/docs/upload"))
        .query(&[("parent", parent.as_str()), ("name", name)])
        .body(body)
        .send()
        .await
        .unwrap()
}

/// Uploads `body` as `name` and returns the new document.
async fn uploaded(site: &Site, user: &TestUser, name: &str, body: Vec<u8>) -> DocId {
    let response = upload(site, user, name, body).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let json: Value = response.json().await.unwrap();
    DocId(json["id"].as_str().unwrap().parse().unwrap())
}

async fn download(
    site: &Site,
    user: Option<&TestUser>,
    doc: DocId,
    revision: Option<&str>,
) -> reqwest::Response {
    let mut request = site
        .http(user)
        .get(site.url(&format!("/api/docs/{}/content", doc.0)));
    if let Some(revision) = revision {
        request = request.query(&[("revision", revision)]);
    }
    request.send().await.unwrap()
}

async fn save(
    site: &Site,
    user: &TestUser,
    doc: DocId,
    base: Option<&str>,
    token: Option<&str>,
    body: Vec<u8>,
) -> reqwest::Response {
    let mut request = site
        .http(Some(user))
        .put(site.url(&format!("/api/docs/{}/content", doc.0)))
        .body(body);
    if let Some(base) = base {
        request = request.query(&[("base", base)]);
    }
    if let Some(token) = token {
        request = request.header(SEAT, token);
    }
    request.send().await.unwrap()
}

/// The revision header of a download.
fn revision_of(response: &reqwest::Response) -> String {
    response
        .headers()
        .get(REVISION)
        .expect("no revision header")
        .to_str()
        .unwrap()
        .to_owned()
}

/// A file document owned by Ann, with Bob an editor and Vic a viewer.
async fn shared_file(server: &TestServer) -> (DocId, TestUser, TestUser, TestUser) {
    let doc = new_doc();
    let ann = server.user("Ann").await;
    let bob = server.user("Bob").await;
    let vic = server.user("Vic").await;
    server.create_file_document(doc, &ann).await;
    for (user, role) in [(&bob, "editor"), (&vic, "viewer")] {
        let response = server
            .http(Some(&ann))
            .put(server.url(&format!("/api/docs/{}/access", doc.0)))
            .json(&serde_json::json!({ "email": user.email, "role": role }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }
    (doc, ann, bob, vic)
}

/// The id of `user`'s own folder, where their uploads go.
async fn home(site: &Site, user: &TestUser) -> String {
    site.home_of(user).await.0.to_string()
}

/// The documents in `user`'s own folder, as they list it.
async fn listed(site: &Site, user: &TestUser) -> Vec<Value> {
    let folder = home(site, user).await;
    let response = site
        .http(Some(user))
        .get(site.url(&format!("/api/nodes/{folder}/children")))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let page: Value = response.json().await.unwrap();
    page["items"].as_array().unwrap().clone()
}

/// How many documents `user` has in their own folder, where their uploads go.
async fn documents_of(site: &Site, user: &TestUser) -> usize {
    listed(site, user).await.len()
}

/// Asserts that `response` carries each header of a download once.
fn assert_safe_headers(response: &reqwest::Response) {
    let headers = response.headers();
    for (name, value) in [
        (CONTENT_TYPE, "application/octet-stream"),
        (X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (CONTENT_SECURITY_POLICY, "sandbox"),
        (CACHE_CONTROL, "no-store"),
    ] {
        let all: Vec<_> = headers.get_all(&name).iter().collect();
        assert_eq!(all, vec![value], "{name}");
    }
    let disposition: Vec<_> = headers.get_all(CONTENT_DISPOSITION).iter().collect();
    assert_eq!(disposition.len(), 1);
    assert!(
        disposition[0].to_str().unwrap().starts_with("attachment;"),
        "{disposition:?}"
    );
}

#[tokio::test]
async fn upload_then_download_round_trips() {
    let server = spawn_server().await;
    let ann = server.user("Ann").await;
    let bytes = every_byte();

    let response = upload(&server, &ann, "Budget.xlsx", bytes.clone()).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let json: Value = response.json().await.unwrap();
    assert_eq!(json["name"], "Budget.xlsx");
    assert_eq!(json["editor"], "gridcraft");
    assert_eq!(json["access"], common::role_access("owner"));
    let doc = DocId(json["id"].as_str().unwrap().parse().unwrap());

    let response = download(&server, Some(&ann), doc, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_safe_headers(&response);
    let revision = revision_of(&response);
    assert_eq!(response.bytes().await.unwrap().to_vec(), bytes);

    // The revision is in Git, and the database knows it too, before the answer came.
    let detail: Value = server
        .http(Some(&ann))
        .get(server.url(&format!("/api/docs/{}", doc.0)))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(detail["revision"], revision.as_str());
    let rows = server.records_revisions(doc).await;
    assert_eq!(rows, vec![revision.clone()]);
    assert_eq!(server.doc_commits(doc), vec![revision]);

    // One creation event, saying it was an upload and how big; never the content.
    let events = wait_events(&server.records_db(), doc, |e| {
        e.iter().any(|e| e.kind == "document.created")
    })
    .await;
    let created: Vec<_> = events
        .iter()
        .filter(|e| e.kind == "document.created")
        .collect();
    assert_eq!(created.len(), 1);
    assert_eq!(created[0].detail["uploaded"], true);
    assert_eq!(created[0].detail["bytes"], bytes.len());
    assert_eq!(created[0].detail["editor"], "gridcraft");
    assert_eq!(created[0].actor, Some(ann.id));
}

#[tokio::test]
async fn content_is_never_rendered() {
    let server = spawn_server().await;
    let ann = server.user("Ann").await;
    let doc = uploaded(&server, &ann, "x.docx", HTML_PAGE.to_vec()).await;

    let response = download(&server, Some(&ann), doc, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_safe_headers(&response);
    assert_eq!(response.bytes().await.unwrap().as_ref(), HTML_PAGE);
}

#[tokio::test]
async fn download_needs_access() {
    let server = spawn_server().await;
    let ann = server.user("Ann").await;
    let eve = server.user("Eve").await;
    let doc = uploaded(&server, &ann, "Private.xlsx", every_byte()).await;

    let stranger = download(&server, Some(&eve), doc, None).await;
    assert_eq!(stranger.status(), StatusCode::NOT_FOUND);
    assert!(stranger.headers().get(REVISION).is_none());
    let body = stranger.bytes().await.unwrap();
    assert!(!body.windows(4).any(|w| w == [0, 1, 2, 3]));

    // Signed out: the same answer every API route gives.
    let signed_out = download(&server, None, doc, None).await;
    assert_eq!(signed_out.status(), StatusCode::UNAUTHORIZED);

    // A document that does not exist looks the same as one the caller may not see.
    let missing = download(&server, Some(&ann), new_doc(), None).await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn revision_of_another_document_is_not_found() {
    let server = spawn_server().await;
    let ann = server.user("Ann").await;
    let a = uploaded(&server, &ann, "A.xlsx", b"first".to_vec()).await;
    let b = uploaded(&server, &ann, "B.xlsx", b"second".to_vec()).await;
    let of_a = revision_of(&download(&server, Some(&ann), a, None).await);

    // Its own revision is found; another document's, garbage and an unknown one are not.
    let own = download(&server, Some(&ann), a, Some(&of_a)).await;
    assert_eq!(own.status(), StatusCode::OK);
    assert_eq!(own.bytes().await.unwrap().as_ref(), b"first");
    for revision in [of_a.as_str(), "garbage", &"0".repeat(40), ""] {
        let response = download(&server, Some(&ann), b, Some(revision)).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{revision:?}");
    }
}

#[tokio::test]
async fn an_older_revision_can_be_downloaded() {
    let server = spawn_server().await;
    let ann = server.user("Ann").await;
    let doc = uploaded(&server, &ann, "Plan.xlsx", b"one".to_vec()).await;
    let first = revision_of(&download(&server, Some(&ann), doc, None).await);

    let seat = FileClient::connect_as(&server, doc, &ann).await;
    let token = seat.token().unwrap();
    let response = save(
        &server,
        &ann,
        doc,
        Some(&first),
        Some(&token),
        b"two".to_vec(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    let old = download(&server, Some(&ann), doc, Some(&first)).await;
    assert_safe_headers(&old);
    assert_eq!(revision_of(&old), first);
    assert_eq!(old.bytes().await.unwrap().as_ref(), b"one");
    let current = download(&server, Some(&ann), doc, None).await;
    assert_eq!(current.bytes().await.unwrap().as_ref(), b"two");
}

#[tokio::test]
async fn empty_document_answers_no_content() {
    let server = spawn_server().await;
    let ann = server.user("Ann").await;
    let doc = new_doc();
    server.create_file_document(doc, &ann).await;

    let response = download(&server, Some(&ann), doc, None).await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(response.headers().get(REVISION).is_none());
    assert!(response.bytes().await.unwrap().is_empty());
}

#[tokio::test]
async fn save_with_the_seat_writes_a_revision() {
    let server = spawn_server().await;
    let (doc, ann, _, _) = shared_file(&server).await;
    let seat = FileClient::connect_as(&server, doc, &ann).await;
    let token = seat.token().unwrap();

    let bytes = every_byte();
    let response = save(&server, &ann, doc, None, Some(&token), bytes.clone()).await;
    assert_eq!(response.status(), StatusCode::OK);
    let json: Value = response.json().await.unwrap();
    assert_eq!(json["changed"], true);
    let revision = json["revision"].as_str().unwrap().to_owned();

    let response = download(&server, Some(&ann), doc, None).await;
    assert_eq!(revision_of(&response), revision);
    assert_eq!(response.bytes().await.unwrap().to_vec(), bytes);
    assert_eq!(server.doc_commits(doc), vec![revision.clone()]);

    // The same bytes again, from the revision they made: nothing new is written.
    let again = save(&server, &ann, doc, Some(&revision), Some(&token), bytes).await;
    assert_eq!(again.status(), StatusCode::OK);
    let json: Value = again.json().await.unwrap();
    assert_eq!(json["changed"], false);
    assert_eq!(json["revision"], revision.as_str());
    assert_eq!(server.doc_commits(doc).len(), 1);
}

#[tokio::test]
async fn save_without_the_seat_is_refused() {
    let server = spawn_server().await;
    let (doc, ann, bob, vic) = shared_file(&server).await;

    // Nobody has the document open: no seat exists, a made-up token is no good, and nor is none.
    let made_up = "ab".repeat(32);
    let response = save(&server, &ann, doc, None, Some(&made_up), b"no".to_vec()).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = save(&server, &ann, doc, None, None, b"no".to_vec()).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    // Ann holds the seat; Bob, an editor, has neither her token nor one of his own.
    let seat = FileClient::connect_as(&server, doc, &ann).await;
    let token = seat.token().unwrap();
    let response = save(&server, &bob, doc, None, Some(&token), b"no".to_vec()).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = save(&server, &bob, doc, None, None, b"no".to_vec()).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    // A viewer, even with the holder's token, may not save.
    let response = save(&server, &vic, doc, None, Some(&token), b"no".to_vec()).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    // Nobody saw the answer carry the token back.
    let body = response.text().await.unwrap();
    assert!(!body.contains(&token));

    assert_eq!(
        download(&server, Some(&ann), doc, None).await.status(),
        StatusCode::NO_CONTENT
    );
    assert!(server.records_revisions(doc).await.is_empty());
}

#[tokio::test]
async fn save_on_an_old_revision_is_a_conflict() {
    let server = spawn_server().await;
    let (doc, ann, _, _) = shared_file(&server).await;
    let seat = FileClient::connect_as(&server, doc, &ann).await;
    let token = seat.token().unwrap();

    let first = save(&server, &ann, doc, None, Some(&token), b"one".to_vec()).await;
    let first: Value = first.json().await.unwrap();
    let first = first["revision"].as_str().unwrap().to_owned();

    // Made from "no revision yet", though there is one now.
    let response = save(&server, &ann, doc, None, Some(&token), b"two".to_vec()).await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let json: Value = response.json().await.unwrap();
    assert_eq!(json["current"], first.as_str());
    // Made from a revision that is not the current one.
    let other = "1".repeat(40);
    let response = save(
        &server,
        &ann,
        doc,
        Some(&other),
        Some(&token),
        b"two".to_vec(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);

    let current = download(&server, Some(&ann), doc, None).await;
    assert_eq!(revision_of(&current), first);
    assert_eq!(current.bytes().await.unwrap().as_ref(), b"one");
    assert_eq!(server.doc_commits(doc), vec![first.clone()]);

    // Refusing those saves did not stop the session: one made from the current revision is
    // stored.
    let response = save(
        &server,
        &ann,
        doc,
        Some(&first),
        Some(&token),
        b"two".to_vec(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let current = download(&server, Some(&ann), doc, None).await;
    assert_eq!(current.bytes().await.unwrap().as_ref(), b"two");
}

#[tokio::test]
async fn save_after_removal_is_refused() {
    let server = spawn_server().await;
    let (doc, ann, bob, _) = shared_file(&server).await;
    let seat = FileClient::connect_as(&server, doc, &bob).await;
    let token = seat.token().unwrap();

    let response = server
        .http(Some(&ann))
        .delete(server.url(&format!("/api/docs/{}/access/user/{}", doc.0, bob.id.0)))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    // The role is checked first: Bob no longer knows the document is there.
    let response = save(&server, &bob, doc, None, Some(&token), b"late".to_vec()).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let response = download(&server, Some(&bob), doc, None).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(server.records_revisions(doc).await.is_empty());
}

#[tokio::test]
async fn oversized_upload_is_refused() {
    let server = spawn_server_config(|config| config.max_file_bytes = 1024).await;
    let ann = server.user("Ann").await;

    let response = upload(&server, &ann, "Big.xlsx", vec![7; 1025]).await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(documents_of(&server, &ann).await, 0);

    // The limit itself is allowed.
    let doc = uploaded(&server, &ann, "Fits.xlsx", vec![7; 1024]).await;
    let response = download(&server, Some(&ann), doc, None).await;
    assert_eq!(response.bytes().await.unwrap().len(), 1024);
    assert_eq!(documents_of(&server, &ann).await, 1);
}

#[tokio::test]
async fn oversized_save_is_refused() {
    let server = spawn_server_config(|config| config.max_file_bytes = 1024).await;
    let (doc, ann, _, _) = shared_file(&server).await;
    let seat = FileClient::connect_as(&server, doc, &ann).await;
    let token = seat.token().unwrap();

    let response = save(&server, &ann, doc, None, Some(&token), vec![7; 1025]).await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        download(&server, Some(&ann), doc, None).await.status(),
        StatusCode::NO_CONTENT
    );
    assert!(server.records_revisions(doc).await.is_empty());
    assert!(server.store().load(doc).unwrap().is_none());

    // Other routes keep their own, much smaller, limit.
    let response = server
        .http(Some(&ann))
        .put(server.url(&format!("/api/docs/{}/access", doc.0)))
        .header(CONTENT_TYPE, "application/json")
        .body(vec![b' '; 20 * 1024])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn bad_file_names_are_refused() {
    let server = spawn_server().await;
    let ann = server.user("Ann").await;
    let long = format!("{}.xlsx", "a".repeat(300));

    // No editor opens the name: 400.
    for name in ["report", "report.exe", ".xlsx", "notes.txt", ""] {
        let response = upload(&server, &ann, name, b"data".to_vec()).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{name:?}");
    }
    // An editor opens it, but it is not a name a document may have: 422, as for any document.
    for name in ["a/b.xlsx", "a\\b.xlsx", "tab\t.xlsx", long.as_str()] {
        let response = upload(&server, &ann, name, b"data".to_vec()).await;
        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{name:?}"
        );
    }
    // No name at all.
    let parent = home(&server, &ann).await;
    let response = server
        .http(Some(&ann))
        .post(server.url("/api/docs/upload"))
        .query(&[("parent", parent.as_str())])
        .body(b"data".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    // An empty file is not a document.
    let response = upload(&server, &ann, "Empty.xlsx", Vec::new()).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    assert_eq!(documents_of(&server, &ann).await, 0);
}

#[tokio::test]
async fn non_ascii_file_name_survives_download() {
    let server = spawn_server().await;
    let ann = server.user("Ann").await;
    let doc = uploaded(
        &server,
        &ann,
        "B\u{fc}dget \u{2013} \u{65e5}\u{672c}.xlsx",
        every_byte(),
    )
    .await;

    let response = download(&server, Some(&ann), doc, None).await;
    let disposition = response
        .headers()
        .get(CONTENT_DISPOSITION)
        .unwrap()
        .to_str()
        .unwrap();
    assert_eq!(
        disposition,
        "attachment; filename=\"B_dget _ __.xlsx\"; \
         filename*=UTF-8''B%C3%BCdget%20%E2%80%93%20%E6%97%A5%E6%9C%AC.xlsx"
    );
    // The name the document list shows is the one given.
    let list = listed(&server, &ann).await;
    assert_eq!(
        list[0]["name"],
        "B\u{fc}dget \u{2013} \u{65e5}\u{672c}.xlsx"
    );
}

#[tokio::test]
async fn save_is_refused_for_a_text_document() {
    let server = spawn_server().await;
    let ann = server.user("Ann").await;
    let doc = new_doc();
    server.give_access(doc, &ann).await;

    let response = save(
        &server,
        &ann,
        doc,
        None,
        Some(&"ab".repeat(32)),
        b"text".to_vec(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // A text document can still be downloaded.
    let response = download(&server, Some(&ann), doc, None).await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn cross_origin_save_is_refused() {
    let server = spawn_server().await;
    let (doc, ann, _, _) = shared_file(&server).await;
    let seat = FileClient::connect_as(&server, doc, &ann).await;
    let token = seat.token().unwrap();

    let client = server.http(Some(&ann));
    let response = client
        .put(server.url(&format!("/api/docs/{}/content", doc.0)))
        .header(ORIGIN, "http://evil.example")
        .header(SEAT, &token)
        .body(b"cross".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let parent = home(&server, &ann).await;
    let response = client
        .post(server.url("/api/docs/upload"))
        .query(&[("parent", parent.as_str()), ("name", "Cross.xlsx")])
        .header(ORIGIN, "http://evil.example")
        .body(b"cross".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    assert_eq!(
        download(&server, Some(&ann), doc, None).await.status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(documents_of(&server, &ann).await, 1);
}

/// A store that cannot write.
struct CannotWrite(RevisionStore);

impl Checkpointer for CannotWrite {
    fn write(&self, _c: &Checkpoint<'_>) -> Result<Outcome, CheckpointError> {
        Err(CheckpointError::Git(GitError::new("disk on fire")))
    }

    fn load(&self, doc: DocId) -> Result<Option<Stored>, CheckpointError> {
        self.0.load(doc)
    }

    fn load_at(
        &self,
        doc: DocId,
        revision: &RevisionId,
    ) -> Result<Option<Stored>, CheckpointError> {
        self.0.load_at(doc, revision)
    }

    fn describe(
        &self,
        doc: DocId,
        revision: &RevisionId,
    ) -> Result<Option<RevisionInfo>, CheckpointError> {
        self.0.describe(doc, revision)
    }

    fn history(&self, doc: DocId) -> Result<Vec<RevisionInfo>, CheckpointError> {
        self.0.history(doc)
    }

    fn documents(&self) -> Result<Vec<DocId>, GitError> {
        self.0.documents()
    }

    fn write_tree(&self, change: &TreeChange<'_>) -> Result<RevisionId, CheckpointError> {
        self.0.write_tree(change)
    }

    fn read_tree(&self) -> Result<Vec<TreeRecord>, CheckpointError> {
        self.0.read_tree()
    }

    fn read_tree_record(&self, id: Uuid) -> Result<Option<TreeRecord>, CheckpointError> {
        self.0.read_tree_record(id)
    }
}

#[tokio::test]
async fn failed_upload_keeps_the_empty_document() {
    let server = spawn_server_with(|path: &Path| {
        Arc::new(CannotWrite(RevisionStore::open(path).unwrap())) as Arc<dyn Checkpointer>
    })
    .await;
    let ann = server.user("Ann").await;

    let response = upload(&server, &ann, "Lost.xlsx", b"bytes".to_vec()).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let json: Value = response.json().await.unwrap();
    let doc = DocId(json["document"].as_str().unwrap().parse().unwrap());
    assert!(!json.to_string().contains("disk on fire"));

    // The document is there, owned by Ann, with nothing in it.
    assert_eq!(documents_of(&server, &ann).await, 1);
    assert_eq!(
        download(&server, Some(&ann), doc, None).await.status(),
        StatusCode::NO_CONTENT
    );
}

/// A Git object that is a tree or a blob is no revision: asking for it is `404`, not a failure.
#[tokio::test]
async fn object_that_is_not_a_revision_is_not_found() {
    let server = spawn_server().await;
    let ann = server.user("Ann").await;
    let doc = uploaded(&server, &ann, "A.xlsx", b"first".to_vec()).await;
    let revision = revision_of(&download(&server, Some(&ann), doc, None).await);

    let repo = gix::open(server.library()).unwrap();
    let commit = repo
        .find_commit(gix::ObjectId::from_hex(revision.as_bytes()).unwrap())
        .unwrap();
    let tree = commit.tree().unwrap();
    let blob = tree
        .lookup_entry_by_path(format!("docs/{}/content", doc.0))
        .unwrap()
        .unwrap()
        .object_id();
    for object in [tree.id.to_string(), blob.to_string()] {
        let response = download(&server, Some(&ann), doc, Some(&object)).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{object}");
    }
}

/// Files far larger than the API's 16 KiB body limit for JSON pass the content routes, up to
/// `max_file_bytes`; one byte more is refused, whether or not the request says its length.
#[tokio::test]
async fn files_up_to_the_limit_pass_and_larger_ones_do_not() {
    const LIMIT: usize = 64 * 1024;
    let server = spawn_server_config(|config| config.max_file_bytes = LIMIT as u64).await;
    let ann = server.user("Ann").await;

    let first: Vec<u8> = (0..40 * 1024).map(|n| (n % 251) as u8).collect();
    let response = upload(&server, &ann, "Big.xlsx", first.clone()).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let json: Value = response.json().await.unwrap();
    let doc = DocId(json["id"].as_str().unwrap().parse().unwrap());
    let response = download(&server, Some(&ann), doc, None).await;
    let base = revision_of(&response);
    assert_eq!(response.bytes().await.unwrap().to_vec(), first);

    let seat = FileClient::connect_as(&server, doc, &ann).await;
    let token = seat.token().unwrap();
    let second: Vec<u8> = (0..40 * 1024).map(|n| (n % 241) as u8).collect();
    let response = save(
        &server,
        &ann,
        doc,
        Some(&base),
        Some(&token),
        second.clone(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let response = download(&server, Some(&ann), doc, None).await;
    assert_eq!(response.bytes().await.unwrap().to_vec(), second);

    // One byte over the limit.
    let response = upload(&server, &ann, "Over.xlsx", vec![7; LIMIT + 1]).await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let current = revision_of(&download(&server, Some(&ann), doc, None).await);
    let response = save(
        &server,
        &ann,
        doc,
        Some(&current),
        Some(&token),
        vec![7; LIMIT + 1],
    )
    .await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(documents_of(&server, &ann).await, 1);

    // Sent in chunks, with no length given up front.
    let chunks: Vec<Result<Vec<u8>, std::io::Error>> = (0..(LIMIT / 1024 + 2))
        .map(|_| Ok(vec![7u8; 1024]))
        .collect();
    let parent = home(&server, &ann).await;
    let response = server
        .http(Some(&ann))
        .post(server.url("/api/docs/upload"))
        .query(&[("parent", parent.as_str()), ("name", "Chunked.xlsx")])
        .body(reqwest::Body::wrap_stream(futures_util::stream::iter(
            chunks,
        )))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(documents_of(&server, &ann).await, 1);
}

/// A file session's events: who joined and took the seat, the save credited to them, and the
/// person gone and the seat freed when they leave, each with its actor.
#[tokio::test]
async fn file_session_events_are_recorded() {
    let server = spawn_server().await;
    let ann = server.user("Ann").await;
    let doc = new_doc();
    server.create_file_document(doc, &ann).await;
    let mut seat = FileClient::connect_as(&server, doc, &ann).await;
    let token = seat.token().unwrap();
    let response = save(&server, &ann, doc, None, Some(&token), b"one".to_vec()).await;
    assert_eq!(response.status(), StatusCode::OK);
    let json: Value = response.json().await.unwrap();
    let revision = json["revision"].as_str().unwrap().to_owned();
    seat.close().await;

    let events = wait_events(&server.db.db, doc, |e| {
        e.iter().any(|e| e.kind == "seat.freed")
    })
    .await;
    let session: Vec<(&str, Option<_>, Option<_>)> = events
        .iter()
        .filter(|e| e.kind != "document.created")
        .map(|e| (e.kind.as_str(), e.actor, e.subject))
        .collect();
    let ann_id = Some(ann.id);
    assert_eq!(
        session,
        vec![
            ("editing.joined", ann_id, ann_id),
            ("seat.taken", ann_id, ann_id),
            ("revision.saved", None, None),
            ("editing.left", ann_id, ann_id),
            ("seat.freed", ann_id, ann_id),
        ]
    );
    let saved = events.iter().find(|e| e.kind == "revision.saved").unwrap();
    assert_eq!(saved.detail["revision"], revision.as_str());
    assert_eq!(
        saved.detail["authors"],
        serde_json::json!([ann.id.0.to_string()])
    );
}
