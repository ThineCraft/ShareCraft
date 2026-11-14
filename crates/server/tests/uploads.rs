//! Resumable uploads: a file sent in chunks, resumed after an interruption or a restart of the
//! server, checked against its declared size and SHA-256, and made into a new file or a new
//! version saved through the document's file session; sessions belong to their owner, are
//! limited to five a person, and are swept when idle for a day.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use collab_core::ids::{DocId, NodeId};
use common::{
    FileClient, Site, StoppableServer, TestServer, TestUser, spawn_server, spawn_server_config,
};
use futures_util::StreamExt;
use protocol::{FileServerMsg, SaveState, SeatState};
use records::Principal;
use reqwest::StatusCode;
use reqwest::header::CONTENT_RANGE;
use revisions::BlobRef;
use serde_json::{Value, json};
use time::OffsetDateTime;

/// `n` bytes that are not all alike.
fn bytes(n: usize, seed: u8) -> Vec<u8> {
    (0..n)
        .map(|i| u8::try_from((i * 31 + usize::from(seed) * 7) % 251).unwrap())
        .collect()
}

/// The SHA-256 of `bytes`, as the API takes it.
fn sha_hex(bytes: &[u8]) -> String {
    BlobRef::of(bytes).hex()
}

/// Asks to start an upload session with `body`.
async fn start_raw(site: &Site, user: &TestUser, body: Value) -> reqwest::Response {
    site.http(Some(user))
        .post(site.url("/api/uploads"))
        .json(&body)
        .send()
        .await
        .unwrap()
}

/// Starts a session uploading `content` as a new file `name` in `parent`; its id.
async fn start_file(
    site: &Site,
    user: &TestUser,
    parent: NodeId,
    name: &str,
    content: &[u8],
) -> String {
    let response = start_raw(
        site,
        user,
        json!({
            "parent": parent.0.to_string(),
            "name": name,
            "size": content.len(),
            "sha256": sha_hex(content),
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["received"], 0);
    body["id"].as_str().unwrap().to_owned()
}

/// Starts a session uploading `content` as a new version of `doc` made from `base`.
async fn start_version(
    site: &Site,
    user: &TestUser,
    doc: DocId,
    base: Option<&str>,
    content: &[u8],
) -> reqwest::Response {
    start_raw(
        site,
        user,
        json!({
            "doc": doc.0.to_string(),
            "size": content.len(),
            "sha256": sha_hex(content),
            "base": base,
        }),
    )
    .await
}

/// Sends `chunk` as the bytes from `start` of a file of `size` bytes.
async fn put(
    site: &Site,
    user: &TestUser,
    id: &str,
    start: usize,
    chunk: &[u8],
    size: usize,
) -> reqwest::Response {
    let end = start + chunk.len() - 1;
    site.http(Some(user))
        .put(site.url(&format!("/api/uploads/{id}")))
        .header(CONTENT_RANGE, format!("bytes {start}-{end}/{size}"))
        .body(chunk.to_vec())
        .send()
        .await
        .unwrap()
}

/// Sends all of `content` from `from`, in chunks of `chunk` bytes, each accepted.
async fn put_all(
    site: &Site,
    user: &TestUser,
    id: &str,
    content: &[u8],
    from: usize,
    chunk: usize,
) {
    put_until(site, user, id, content, from, content.len(), chunk).await;
}

/// Sends the bytes of `content` from `from` to `until`, in chunks of `chunk` bytes, each
/// accepted.
async fn put_until(
    site: &Site,
    user: &TestUser,
    id: &str,
    content: &[u8],
    from: usize,
    until: usize,
    chunk: usize,
) {
    let mut at = from;
    while at < until {
        let end = (at + chunk).min(until);
        let response = put(site, user, id, at, &content[at..end], content.len()).await;
        let status = response.status();
        let body: Value = response.json().await.unwrap();
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["received"], end);
        at = end;
    }
}

async fn finish(site: &Site, user: &TestUser, id: &str) -> reqwest::Response {
    site.http(Some(user))
        .post(site.url(&format!("/api/uploads/{id}/finish")))
        .send()
        .await
        .unwrap()
}

async fn show(site: &Site, user: &TestUser, id: &str) -> reqwest::Response {
    site.http(Some(user))
        .get(site.url(&format!("/api/uploads/{id}")))
        .send()
        .await
        .unwrap()
}

/// How many bytes the session `id` has received.
async fn received(site: &Site, user: &TestUser, id: &str) -> u64 {
    let response = show(site, user, id).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.unwrap();
    body["received"].as_u64().unwrap()
}

/// The current bytes of `doc` and the revision holding them.
async fn download(site: &Site, user: &TestUser, doc: DocId) -> (Vec<u8>, String) {
    let response = site
        .http(Some(user))
        .get(site.url(&format!("/api/docs/{}/content", doc.0)))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let revision = response.headers()["x-sharecraft-revision"]
        .to_str()
        .unwrap()
        .to_owned();
    (response.bytes().await.unwrap().to_vec(), revision)
}

/// Uploads `content` as `name` into `parent` the one-request way; the new document.
async fn upload_whole(
    site: &Site,
    user: &TestUser,
    parent: NodeId,
    name: &str,
    content: &[u8],
) -> DocId {
    let response = site
        .http(Some(user))
        .post(site.url("/api/docs/upload"))
        .query(&[("parent", parent.0.to_string().as_str()), ("name", name)])
        .body(content.to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let body: Value = response.json().await.unwrap();
    DocId(body["id"].as_str().unwrap().parse().unwrap())
}

/// The id of a document a finished upload created.
async fn created_doc(response: reqwest::Response) -> DocId {
    assert_eq!(response.status(), StatusCode::CREATED);
    let body: Value = response.json().await.unwrap();
    DocId(body["id"].as_str().unwrap().parse().unwrap())
}

/// The upload part files left in the data folder `data`.
fn parts(data: &Path) -> Vec<PathBuf> {
    match std::fs::read_dir(server::uploads_path(data)) {
        Ok(entries) => entries.map(|e| e.unwrap().path()).collect(),
        Err(_) => Vec::new(),
    }
}

/// How many items `parent` holds, deleted or not.
async fn items_in(server: &TestServer, parent: NodeId) -> i64 {
    sqlx::query_scalar("select count(*) from nodes where parent_id = $1")
        .bind(parent.0)
        .fetch_one(server.db.db.test_pool())
        .await
        .unwrap()
}

/// Whether the content store holds `content`.
fn stored(server: &TestServer, content: &[u8]) -> bool {
    server.blobs().contains(&BlobRef::of(content).sha256)
}

/// A file uploaded in three chunks, the progress asked for on the way, becomes a document with
/// exactly those bytes; the session and its part file are gone, and the upload is recorded.
#[tokio::test]
async fn upload_in_chunks_then_finish() {
    let server = spawn_server().await;
    let ann = server.user("Ann").await;
    let home = server.home_of(&ann).await;
    let content = bytes(300_000, 1);
    let id = start_file(&server, &ann, home, "Report.pdf", &content).await;
    assert_eq!(parts(server.data_dir.path()).len(), 1);

    put_until(&server, &ann, &id, &content, 0, 200_000, 100_000).await;
    assert_eq!(received(&server, &ann, &id).await, 200_000);
    let response = put(
        &server,
        &ann,
        &id,
        200_000,
        &content[200_000..],
        content.len(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    let doc = created_doc(finish(&server, &ann, &id).await).await;
    let (downloaded, _) = download(&server, &ann, doc).await;
    assert_eq!(downloaded, content);
    assert_eq!(
        server
            .records_db()
            .document(doc)
            .await
            .unwrap()
            .unwrap()
            .name,
        "Report.pdf"
    );
    assert_eq!(
        show(&server, &ann, &id).await.status(),
        StatusCode::NOT_FOUND
    );
    assert!(parts(server.data_dir.path()).is_empty());
    let events = common::wait_events(&server.records_db(), doc, |events| {
        events.iter().any(|e| e.kind == "upload.completed")
    })
    .await;
    let done = events
        .iter()
        .find(|e| e.kind == "upload.completed")
        .unwrap();
    assert_eq!(done.detail["bytes"], 300_000);
    assert_eq!(done.actor, Some(ann.id));
}

/// A chunk whose connection breaks keeps what arrived; asking for the progress and sending the
/// rest from there produces exactly the declared bytes.
#[tokio::test]
async fn resume_after_interruption() {
    let server = spawn_server().await;
    let ann = server.user("Ann").await;
    let home = server.home_of(&ann).await;
    let content = bytes(256 * 1024, 2);
    let id = start_file(&server, &ann, home, "Movie.pdf", &content).await;

    // The whole file in one chunk, whose body breaks after 100 000 bytes.
    let head = content[..100_000].to_vec();
    let body = futures_util::stream::once(async move { Ok::<_, std::io::Error>(head) }).chain(
        futures_util::stream::once(async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            Err(std::io::Error::other("the connection dropped"))
        }),
    );
    let broken = server
        .http(Some(&ann))
        .put(server.url(&format!("/api/uploads/{id}")))
        .header(
            CONTENT_RANGE,
            format!("bytes 0-{}/{}", content.len() - 1, content.len()),
        )
        .body(reqwest::Body::wrap_stream(body))
        .send()
        .await;
    assert!(broken.is_err() || !broken.unwrap().status().is_success());

    // The server finishes with the broken chunk on its own time.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut at = received(&server, &ann, &id).await;
    while at < 100_000 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
        at = received(&server, &ann, &id).await;
    }
    assert!(at > 0 && at <= 100_000, "{at}");
    let at = usize::try_from(at).unwrap();

    // Sending from anywhere else is refused, naming where to go on.
    let response = put(&server, &ann, &id, 0, &content[..10], content.len()).await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"], "wrong_offset");
    assert_eq!(body["expected"], at);
    put_all(&server, &ann, &id, &content, at, 64 * 1024).await;
    let doc = created_doc(finish(&server, &ann, &id).await).await;
    assert_eq!(download(&server, &ann, doc).await.0, content);
}

/// After the server restarts, the running hash is gone; it is computed again from the part
/// file, so the upload goes on and finishes with the right checksum.
#[tokio::test]
async fn resume_after_server_restart() {
    let data_dir = tempfile::tempdir().unwrap();
    let db = records::test_support::TestDb::create().await;
    let content = bytes(200_000, 3);
    let mut first = StoppableServer::start(data_dir.path(), &db, |_| {});
    let ann = first.user("Ann").await;
    let home = first.home_of(&ann).await;
    let id = start_file(&first, &ann, home, "Plans.pdf", &content).await;
    put_until(&first, &ann, &id, &content, 0, 120_000, 60_000).await;
    first.stop();

    let second = StoppableServer::start(data_dir.path(), &db, |_| {});
    assert_eq!(received(&second, &ann, &id).await, 120_000);
    put_all(&second, &ann, &id, &content, 120_000, 50_000).await;
    let doc = created_doc(finish(&second, &ann, &id).await).await;
    assert_eq!(download(&second, &ann, doc).await.0, content);

    // A session whose every byte arrived before the restart is hashed again at the finish.
    let other = bytes(50_000, 4);
    let other_id = start_file(&second, &ann, home, "Other.pdf", &other).await;
    put_all(&second, &ann, &other_id, &other, 0, 50_000).await;
    drop(second);
    let third = StoppableServer::start(data_dir.path(), &db, |_| {});
    let doc = created_doc(finish(&third, &ann, &other_id).await).await;
    assert_eq!(download(&third, &ann, doc).await.0, other);
}

/// A wrong checksum, a finish before every byte arrived, a chunk beyond the declared size, a
/// body longer than its range, and chunks out of order: none of them creates anything.
#[tokio::test]
async fn bad_uploads_create_nothing() {
    let server = spawn_server().await;
    let ann = server.user("Ann").await;
    let home = server.home_of(&ann).await;
    let content = bytes(100_000, 5);

    // Declared with another file's checksum.
    let wrong = bytes(100_000, 6);
    let response = start_raw(
        &server,
        &ann,
        json!({
            "parent": home.0.to_string(),
            "name": "Wrong.pdf",
            "size": content.len(),
            "sha256": sha_hex(&wrong),
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let id = response.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    put_all(&server, &ann, &id, &content, 0, 40_000).await;
    let response = finish(&server, &ann, &id).await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"],
        "checksum_mismatch"
    );
    // The session is over.
    assert_eq!(
        show(&server, &ann, &id).await.status(),
        StatusCode::NOT_FOUND
    );

    // Short: finished before every byte arrived.
    let id = start_file(&server, &ann, home, "Short.pdf", &content).await;
    put_until(&server, &ann, &id, &content, 0, 60_000, 60_000).await;
    let response = finish(&server, &ann, &id).await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"], "incomplete");
    assert_eq!(body["received"], 60_000);

    // Long: a range past the declared size, a size other than the declared one, and a body
    // longer than its range.
    let response = server
        .http(Some(&ann))
        .put(server.url(&format!("/api/uploads/{id}")))
        .header(CONTENT_RANGE, "bytes 60000-100000/100000")
        .body(vec![0u8; 40_001])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let response = server
        .http(Some(&ann))
        .put(server.url(&format!("/api/uploads/{id}")))
        .header(CONTENT_RANGE, "bytes 60000-100000/100001")
        .body(vec![0u8; 40_001])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let longer = futures_util::stream::iter([Ok::<_, std::io::Error>(vec![1u8; 50_000])]);
    let response = server
        .http(Some(&ann))
        .put(server.url(&format!("/api/uploads/{id}")))
        .header(CONTENT_RANGE, "bytes 60000-99999/100000")
        .body(reqwest::Body::wrap_stream(longer))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(received(&server, &ann, &id).await, 60_000);

    // Out of order: the last chunk before the one in between.
    let response = put(
        &server,
        &ann,
        &id,
        80_000,
        &content[80_000..],
        content.len(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"], "wrong_offset");
    assert_eq!(body["expected"], 60_000);
    let response = finish(&server, &ann, &id).await;
    assert_eq!(response.status(), StatusCode::CONFLICT);

    // A size beyond the largest file allowed is refused at the start.
    let response = start_raw(
        &server,
        &ann,
        json!({
            "parent": home.0.to_string(),
            "name": "Huge.pdf",
            "size": 1u64 << 40,
            "sha256": sha_hex(b""),
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);

    assert_eq!(items_in(&server, home).await, 0);
    assert!(!stored(&server, &content));
    assert!(!stored(&server, &wrong));
    assert!(server.blobs().list().unwrap().is_empty());
    // Only the open session's part file is left.
    assert_eq!(parts(server.data_dir.path()).len(), 1);
}

/// Someone else's session is not found, whatever they do with it.
#[tokio::test]
async fn sessions_belong_to_their_owner() {
    let server = spawn_server().await;
    let ann = server.user("Ann").await;
    let bob = server.user("Bob").await;
    let home = server.home_of(&ann).await;
    let content = bytes(1000, 7);
    let id = start_file(&server, &ann, home, "Mine.pdf", &content).await;

    assert_eq!(
        show(&server, &bob, &id).await.status(),
        StatusCode::NOT_FOUND
    );
    let response = put(&server, &bob, &id, 0, &content, content.len()).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        finish(&server, &bob, &id).await.status(),
        StatusCode::NOT_FOUND
    );
    let response = server
        .http(Some(&bob))
        .delete(server.url(&format!("/api/uploads/{id}")))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    // Nothing Bob did touched it.
    assert_eq!(received(&server, &ann, &id).await, 0);

    // Bob cannot start one into Ann's folder either.
    let response = start_raw(
        &server,
        &bob,
        json!({
            "parent": home.0.to_string(),
            "name": "Into.pdf",
            "size": 10,
            "sha256": sha_hex(&[0u8; 10]),
        }),
    )
    .await;
    assert!(response.status().is_client_error());

    put_all(&server, &ann, &id, &content, 0, 1000).await;
    created_doc(finish(&server, &ann, &id).await).await;
}

/// A person may have five sessions open; a sixth is refused until one ends.
#[tokio::test]
async fn five_open_sessions_at_most() {
    let server = spawn_server().await;
    let ann = server.user("Ann").await;
    let bob = server.user("Bob").await;
    let home = server.home_of(&ann).await;
    let content = bytes(10, 8);
    let mut ids = Vec::new();
    for n in 0..5 {
        ids.push(start_file(&server, &ann, home, &format!("F{n}.pdf"), &content).await);
    }
    let sixth = json!({
        "parent": home.0.to_string(),
        "name": "F5.pdf",
        "size": content.len(),
        "sha256": sha_hex(&content),
    });
    let response = start_raw(&server, &ann, sixth.clone()).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"],
        "too_many_uploads"
    );
    // Someone else's count is their own.
    let bobs = server.home_of(&bob).await;
    start_file(&server, &bob, bobs, "Bob.pdf", &content).await;

    let response = server
        .http(Some(&ann))
        .delete(server.url(&format!("/api/uploads/{}", ids[0])))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        start_raw(&server, &ann, sixth).await.status(),
        StatusCode::CREATED
    );
    // A finished session frees its place too.
    put_all(&server, &ann, &ids[1], &content, 0, 10).await;
    created_doc(finish(&server, &ann, &ids[1]).await).await;
    start_file(&server, &ann, home, "F6.pdf", &content).await;
}

/// A new version is saved through the document's file session: everyone in it hears of the
/// save, and the version is credited to the uploader.
#[tokio::test]
async fn new_version_goes_through_the_file_session() {
    let server = spawn_server().await;
    let ann = server.user("Ann").await;
    let bob = server.user("Bob").await;
    let home = server.home_of(&ann).await;
    let doc = upload_whole(&server, &ann, home, "Budget.xlsx", b"first version").await;
    let (_, base) = download(&server, &ann, doc).await;
    server
        .records_db()
        .set_access(ann.id, doc, Principal::User(bob.id), Some("viewer"))
        .await
        .unwrap();
    // Bob watches; a viewer never takes the seat.
    let mut watching = FileClient::connect_as(&server, doc, &bob).await;
    assert_eq!(watching.seat, Some(SeatState::Free));

    let content = bytes(150_000, 9);
    let response = start_version(&server, &ann, doc, Some(&base), &content).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let id = response.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    put_all(&server, &ann, &id, &content, 0, 50_000).await;
    let response = finish(&server, &ann, &id).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["changed"], true);
    let revision = body["revision"].as_str().unwrap().to_owned();
    assert_ne!(revision, base);

    let saved = Some(revision.clone());
    watching
        .pump_until(|c| {
            c.received.iter().any(|m| {
                matches!(m, FileServerMsg::Save { state: SaveState::Saved, revision }
                    if *revision == saved)
            })
        })
        .await;
    assert_eq!(download(&server, &ann, doc).await, (content, revision));
    let events = common::wait_events(&server.records_db(), doc, |events| {
        events.iter().any(|e| e.kind == "revision.saved")
            && events.iter().any(|e| e.kind == "upload.completed")
    })
    .await;
    let saved = events.iter().find(|e| e.kind == "revision.saved").unwrap();
    assert_eq!(
        saved.detail["authors"],
        json!([ann.id.0.to_string()]),
        "{saved:?}"
    );
}

/// While someone holds the editing seat, a new version is refused, at the start and at the
/// finish, and nothing is written.
#[tokio::test]
async fn replace_while_being_edited_refused() {
    let server = spawn_server().await;
    let ann = server.user("Ann").await;
    let home = server.home_of(&ann).await;
    let doc = upload_whole(&server, &ann, home, "Notes.docx", b"as it was").await;
    let (_, base) = download(&server, &ann, doc).await;
    let content = bytes(20_000, 10);

    // Started while nobody edits.
    let response = start_version(&server, &ann, doc, Some(&base), &content).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let id = response.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    put_all(&server, &ann, &id, &content, 0, 20_000).await;

    // Then Ann opens it in her editor and holds the seat.
    let editing = FileClient::connect_as(&server, doc, &ann).await;
    assert!(editing.token().is_some());
    let response = finish(&server, &ann, &id).await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"],
        "being_edited"
    );
    let response = start_version(&server, &ann, doc, Some(&base), &content).await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"],
        "being_edited"
    );

    assert_eq!(
        download(&server, &ann, doc).await,
        (b"as it was".to_vec(), base)
    );
    assert!(!stored(&server, &content));
    // The session is kept, to finish once the seat is free.
    assert_eq!(received(&server, &ann, &id).await, 20_000);
}

/// A new version made from a revision that is no longer the current one is refused as S4
/// refuses a save: at the start, and at the finish when another version came meanwhile.
#[tokio::test]
async fn new_version_session_refused_when_stale() {
    let server = spawn_server().await;
    let ann = server.user("Ann").await;
    let home = server.home_of(&ann).await;
    let doc = upload_whole(&server, &ann, home, "Plan.pdf", b"one").await;
    let (_, first) = download(&server, &ann, doc).await;

    let late = bytes(30_000, 11);
    let response = start_version(&server, &ann, doc, Some(&first), &late).await;
    let late_id = response.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    put_all(&server, &ann, &late_id, &late, 0, 30_000).await;

    // Another version is uploaded and finished first.
    let quick = bytes(1000, 12);
    let response = start_version(&server, &ann, doc, Some(&first), &quick).await;
    let quick_id = response.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    put_all(&server, &ann, &quick_id, &quick, 0, 1000).await;
    let response = finish(&server, &ann, &quick_id).await;
    assert_eq!(response.status(), StatusCode::OK);
    let (_, second) = download(&server, &ann, doc).await;

    let response = finish(&server, &ann, &late_id).await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        response.json::<Value>().await.unwrap()["current"],
        second.as_str()
    );
    let response = start_version(&server, &ann, doc, Some(&first), &late).await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        response.json::<Value>().await.unwrap()["current"],
        second.as_str()
    );
    assert_eq!(download(&server, &ann, doc).await, (quick, second));
    assert!(!stored(&server, &late));
}

/// A plain-text document is edited in its room; it takes no uploaded versions.
#[tokio::test]
async fn text_documents_refuse_new_version_uploads() {
    let server = spawn_server().await;
    let ann = server.user("Ann").await;
    let doc = common::new_doc();
    server.give_access(doc, &ann).await;
    let response = start_version(&server, &ann, doc, None, b"replacement").await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"],
        "text_document"
    );
}

/// Whoever loses the right to edit, or to add, while uploading cannot finish: nothing is
/// created or written.
#[tokio::test]
async fn permission_lost_during_upload_refuses_finish() {
    let server = spawn_server().await;
    let ann = server.user("Ann").await;
    let bob = server.user("Bob").await;
    let home = server.home_of(&ann).await;
    let db = server.records_db();

    // A new version of Ann's file, by Bob while he edits it.
    let doc = upload_whole(&server, &ann, home, "Shared.xlsx", b"Ann's").await;
    let (_, base) = download(&server, &ann, doc).await;
    db.set_access(ann.id, doc, Principal::User(bob.id), Some("editor"))
        .await
        .unwrap();
    let content = bytes(10_000, 13);
    let response = start_version(&server, &bob, doc, Some(&base), &content).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let id = response.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    put_all(&server, &bob, &id, &content, 0, 10_000).await;
    db.set_access(ann.id, doc, Principal::User(bob.id), Some("viewer"))
        .await
        .unwrap();
    assert_eq!(
        finish(&server, &bob, &id).await.status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        download(&server, &ann, doc).await,
        (b"Ann's".to_vec(), base)
    );

    // A new file in a folder Bob may add to, until he may not.
    db.set_node_access(ann.id, home, Principal::User(bob.id), Some("editor"))
        .await
        .unwrap();
    let before = items_in(&server, home).await;
    let other = bytes(5000, 14);
    let id = start_file(&server, &bob, home, "Bob's.pdf", &other).await;
    put_all(&server, &bob, &id, &other, 0, 5000).await;
    db.set_node_access(ann.id, home, Principal::User(bob.id), Some("viewer"))
        .await
        .unwrap();
    assert_eq!(
        finish(&server, &bob, &id).await.status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(items_in(&server, home).await, before);
    assert!(!stored(&server, &content));
    assert!(!stored(&server, &other));
}

/// A session that has received nothing for a day is ended by the sweeper, and its part file
/// removed; a younger one is kept. The sweeper never removes a part file while a chunk is being
/// written, and never waits for one: it skips the busy session.
#[tokio::test]
async fn expired_sessions_are_swept() {
    let server = spawn_server().await;
    let ann = server.user("Ann").await;
    let home = server.home_of(&ann).await;
    let state = server.app_state().await;
    let content = bytes(4000, 15);
    let id = start_file(&server, &ann, home, "Idle.pdf", &content).await;
    put_until(&server, &ann, &id, &content, 0, 1000, 1000).await;

    // Not yet a day.
    server::sweep_once(
        &state,
        OffsetDateTime::now_utc() + time::Duration::hours(23),
    )
    .await;
    assert_eq!(received(&server, &ann, &id).await, 1000);

    // A chunk being written when the sweep comes: the sweep skips the busy session at once.
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let tail = content[1000..].to_vec();
    let body = futures_util::stream::once(async { Ok::<_, std::io::Error>(vec![7u8; 1000]) })
        .chain(futures_util::stream::once(async move {
            released.await.unwrap();
            Ok(tail[1000..].to_vec())
        }));
    let slow = tokio::spawn(
        server
            .http(Some(&ann))
            .put(server.url(&format!("/api/uploads/{id}")))
            .header(CONTENT_RANGE, "bytes 1000-3999/4000")
            .body(reqwest::Body::wrap_stream(body))
            .send(),
    );
    let part = parts(server.data_dir.path()).pop().unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while std::fs::metadata(&part).unwrap().len() < 2000 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the chunk never started"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let later = OffsetDateTime::now_utc() + time::Duration::hours(25);
    tokio::time::timeout(Duration::from_secs(5), server::sweep_once(&state, later))
        .await
        .expect("the sweep waited for a busy session");
    assert!(part.exists());
    release.send(()).unwrap();
    let response = slow.await.unwrap().unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(received(&server, &ann, &id).await, 4000);
    server::sweep_once(&state, later).await;

    // A day after the chunk, the session is gone with its part file.
    assert_eq!(
        show(&server, &ann, &id).await.status(),
        StatusCode::NOT_FOUND
    );
    assert!(!part.exists());
    assert!(parts(server.data_dir.path()).is_empty());

    // A part file left without a session is removed once an hour old.
    let stray = server::uploads_path(server.data_dir.path())
        .join(format!("{}.part", uuid::Uuid::new_v4().simple()));
    std::fs::write(&stray, b"left behind").unwrap();
    server::sweep_once(&state, OffsetDateTime::now_utc()).await;
    assert!(stray.exists());
    server::sweep_once(&state, OffsetDateTime::now_utc() + time::Duration::hours(2)).await;
    assert!(!stray.exists());
}

/// A chunk whose body stalls without breaking is cut off once it has sent nothing for the idle
/// time: what arrived is kept, the session's turn is free again, and the upload resumes.
#[tokio::test]
async fn stalled_chunk_is_cut_off_and_resumable() {
    let server = spawn_server_config(|c| c.upload_idle = Duration::from_millis(300)).await;
    let ann = server.user("Ann").await;
    let home = server.home_of(&ann).await;
    let content = bytes(5000, 16);
    let id = start_file(&server, &ann, home, "Stalled.pdf", &content).await;

    // 1000 bytes, then nothing, with the connection left open.
    let head = content[..1000].to_vec();
    let body = futures_util::stream::once(async move { Ok::<_, std::io::Error>(head) })
        .chain(futures_util::stream::pending());
    let stalled = server
        .http(Some(&ann))
        .put(server.url(&format!("/api/uploads/{id}")))
        .header(CONTENT_RANGE, "bytes 0-4999/5000")
        .body(reqwest::Body::wrap_stream(body))
        .send()
        .await
        .unwrap();
    assert_eq!(stalled.status(), StatusCode::BAD_REQUEST);
    let answer: Value = stalled.json().await.unwrap();
    assert_eq!(answer["error"], "incomplete_chunk");
    assert_eq!(answer["received"], 1000);

    assert_eq!(received(&server, &ann, &id).await, 1000);
    put_all(&server, &ann, &id, &content, 1000, 4000).await;
    let doc = created_doc(finish(&server, &ann, &id).await).await;
    assert_eq!(download(&server, &ann, doc).await.0, content);
}
