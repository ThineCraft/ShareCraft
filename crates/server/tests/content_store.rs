//! File bytes in the content store, pointers in Git: checkpoints of text rooms and saves of
//! file sessions write pointers (and stay refused when stale), versions saved before the store
//! still read byte for byte, saves stream through the file session without the server holding
//! the file, downloads serve byte ranges, and the content rows are rebuilt and unused content
//! erased only when it is safe.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use collab_core::ids::{DocId, Seq};
use common::{FileClient, Site, TestClient, TestServer, TestUser, new_doc, spawn_server};
use futures_util::StreamExt;
use protocol::SaveState;
use records::{BLOB_REFS_DIRTY, Editor};
use reqwest::StatusCode;
use reqwest::header::{ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, RANGE};
use revisions::{Author, BlobRef, Checkpoint, Content, Outcome, RevisionId};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

const SEAT: &str = "x-sharecraft-seat";
const MIB: u64 = 1024 * 1024;

fn someone() -> Author {
    Author {
        id: "someone-else".into(),
        name: "Someone Else".into(),
    }
}

/// The bytes Git holds at `docs/<doc>/content` in `revision`.
fn git_content(server: &TestServer, doc: DocId, revision: &str) -> Vec<u8> {
    let repo = gix::open(server.library()).unwrap();
    let id = gix::ObjectId::from_hex(revision.as_bytes()).unwrap();
    let tree = repo.find_commit(id).unwrap().tree().unwrap();
    let entry = tree
        .lookup_entry_by_path(format!("docs/{}/content", doc.0))
        .unwrap()
        .unwrap();
    entry.object().unwrap().detach().data
}

/// Writes `bytes` inline as the next version of `doc`, as a server before the content store
/// did.
fn inline_version(
    server: &TestServer,
    doc: DocId,
    name: &str,
    bytes: &[u8],
    base: Option<&RevisionId>,
) -> RevisionId {
    let outcome = server
        .store()
        .checkpoint_inline(
            &Checkpoint {
                doc,
                file_name: name,
                blob: BlobRef::of(b""),
                base,
                authors: &[someone()],
                fields: &Map::new(),
            },
            bytes,
        )
        .unwrap();
    match outcome {
        Outcome::Written(revision) => revision,
        Outcome::Unchanged(revision) => panic!("expected a new revision, got {revision}"),
    }
}

async fn download(
    site: &Site,
    user: &TestUser,
    doc: DocId,
    revision: Option<&str>,
    range: Option<&str>,
) -> reqwest::Response {
    let mut request = site
        .http(Some(user))
        .get(site.url(&format!("/api/docs/{}/content", doc.0)));
    if let Some(revision) = revision {
        request = request.query(&[("revision", revision)]);
    }
    if let Some(range) = range {
        request = request.header(RANGE, range);
    }
    request.send().await.unwrap()
}

/// Uploads `bytes` as `name` into `user`'s own folder; the new document.
async fn upload(site: &Site, user: &TestUser, name: &str, bytes: Vec<u8>) -> DocId {
    let parent = site.home_of(user).await.0.to_string();
    let response = site
        .http(Some(user))
        .post(site.url("/api/docs/upload"))
        .query(&[("parent", parent.as_str()), ("name", name)])
        .body(bytes)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let json: Value = response.json().await.unwrap();
    DocId(json["id"].as_str().unwrap().parse().unwrap())
}

/// The pending claims left in the database.
async fn pending_claims(server: &TestServer) -> i64 {
    sqlx::query_scalar("select count(*) from blob_refs where pending")
        .fetch_one(server.db.db.test_pool())
        .await
        .unwrap()
}

/// A text room's checkpoint stores a pointer in Git and the text in the content store, and
/// the content is claimed for the revision (no claim is left pending).
#[tokio::test]
async fn text_room_checkpoint_is_a_pointer() {
    let server = spawn_server().await;
    let doc = new_doc();
    let mut ann = TestClient::connect(&server, doc, "Ann").await;
    ann.type_at(0, "hello").await;
    ann.pump_until(|c| c.pending() == 0 && c.seq() == Seq(1))
        .await;
    let revision = ann.wait_saved(Seq(1)).await;

    let blob = BlobRef::of(b"hello");
    assert_eq!(git_content(&server, doc, &revision), blob.to_pointer());
    assert_eq!(
        server.blobs().read_all(&Content::Blob(blob)).unwrap(),
        b"hello"
    );
    assert_eq!(
        server.db.db.blob_refs_of(&blob.sha256).await.unwrap(),
        vec![(doc, revision)]
    );
    assert_eq!(pending_claims(&server).await, 0);
}

/// After the upgrade a text document saved inline opens as before; its next live checkpoint
/// writes a pointer, and a checkpoint based on a stale revision is still refused (S4): the room
/// is in conflict and what was saved from elsewhere stays.
#[tokio::test]
async fn checkpoint_writes_pointer_and_stays_s4() {
    let server = spawn_server().await;
    let ann = server.user("Ann").await;
    let doc = new_doc();
    server
        .create_document(doc, "Old.txt", Editor::Text, &ann)
        .await;
    let old = inline_version(&server, doc, "Old.txt", b"old text", None);

    let mut a = TestClient::connect_as(&server, doc, &ann).await;
    assert_eq!(a.text(), "old text");
    a.type_at(8, " more").await;
    a.pump_until(|c| c.pending() == 0 && c.seq() == Seq(1))
        .await;
    let saved = a.wait_saved(Seq(1)).await;
    assert_eq!(
        git_content(&server, doc, &saved),
        BlobRef::of(b"old text more").to_pointer()
    );
    assert_eq!(git_content(&server, doc, &old.to_string()), b"old text");

    // Another writer saves behind the room's back.
    let saved: RevisionId = saved.parse().unwrap();
    let Outcome::Written(other) = server
        .store()
        .checkpoint(&Checkpoint {
            doc,
            file_name: "Old.txt",
            blob: server.blob(b"elsewhere"),
            base: Some(&saved),
            authors: &[someone()],
            fields: &Map::new(),
        })
        .unwrap()
    else {
        panic!("expected a new revision");
    };
    a.type_at(0, "!").await;
    a.pump_until(|c| c.saves().iter().any(|s| s.0 == SaveState::Conflict))
        .await;
    let head = server.store().read(doc, None).unwrap().unwrap();
    assert_eq!(head.revision, other);
    assert_eq!(server.bytes(&head), b"elsewhere");
    // The refused checkpoint's claim was withdrawn.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(pending_claims(&server).await, 0);
}

/// A file saved before the content store downloads byte for byte, whole, as a range and as
/// an older revision once a new version (a pointer) is saved.
#[tokio::test]
async fn old_inline_versions_still_read() {
    let server = spawn_server().await;
    let ann = server.user("Ann").await;
    let doc = new_doc();
    server.create_file_document(doc, &ann).await;
    let name = format!("{}.xlsx", doc.0);
    // Every byte value, and text that looks like a pointer but is not one.
    let mut old: Vec<u8> = (0..=255u8).collect();
    old.extend_from_slice(b"sharecraft-blob 1\nsha256 nothing\n");
    let old_revision = inline_version(&server, doc, &name, &old, None);

    let response = download(&server, &ann, doc, None, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.bytes().await.unwrap().to_vec(), old);
    let response = download(&server, &ann, doc, None, Some("bytes=10-19")).await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.bytes().await.unwrap().to_vec(), old[10..20]);

    let seat = FileClient::connect_as(&server, doc, &ann).await;
    let token = seat.token().unwrap();
    let response = server
        .http(Some(&ann))
        .put(server.url(&format!("/api/docs/{}/content", doc.0)))
        .query(&[("base", old_revision.to_string())])
        .header(SEAT, &token)
        .body(b"new bytes".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json: Value = response.json().await.unwrap();
    let new = json["revision"].as_str().unwrap().to_owned();
    assert_eq!(
        git_content(&server, doc, &new),
        BlobRef::of(b"new bytes").to_pointer()
    );

    let response = download(&server, &ann, doc, Some(&old_revision.to_string()), None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.bytes().await.unwrap().to_vec(), old);
    let response = download(&server, &ann, doc, None, None).await;
    assert_eq!(response.bytes().await.unwrap().to_vec(), b"new bytes");
}

/// One byte range is served: from the start, from the middle, a suffix, to the end; a range
/// beyond the end is `416`. Every answer keeps the download headers.
#[tokio::test]
async fn download_supports_a_range() {
    let server = spawn_server().await;
    let ann = server.user("Ann").await;
    let bytes: Vec<u8> = (0..1000u32).map(|n| (n % 251) as u8).collect();
    let doc = upload(&server, &ann, "Range.xlsx", bytes.clone()).await;

    let whole = download(&server, &ann, doc, None, None).await;
    assert_eq!(whole.status(), StatusCode::OK);
    assert_eq!(whole.headers()[ACCEPT_RANGES], "bytes");
    assert_eq!(whole.headers()[CONTENT_LENGTH], "1000");
    assert!(whole.headers().get(CONTENT_RANGE).is_none());
    assert_eq!(whole.bytes().await.unwrap().to_vec(), bytes);

    for (range, from, to) in [
        ("bytes=0-9", 0, 9),
        ("bytes=500-599", 500, 599),
        ("bytes=-100", 900, 999),
        ("bytes=990-", 990, 999),
        ("bytes=995-5000", 995, 999),
    ] {
        let response = download(&server, &ann, doc, None, Some(range)).await;
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT, "{range}");
        let headers = response.headers();
        assert_eq!(
            headers[CONTENT_RANGE],
            format!("bytes {from}-{to}/1000").as_str(),
            "{range}"
        );
        assert_eq!(headers[ACCEPT_RANGES], "bytes");
        assert_eq!(headers["content-type"], "application/octet-stream");
        assert_eq!(headers["content-security-policy"], "sandbox");
        assert!(headers.contains_key("x-sharecraft-revision"));
        assert_eq!(
            response.bytes().await.unwrap().to_vec(),
            bytes[from..=to],
            "{range}"
        );
    }

    let response = download(&server, &ann, doc, None, Some("bytes=1000-")).await;
    assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(response.headers()[CONTENT_RANGE], "bytes */1000");

    // Several ranges, or a range that is not one, are ignored: the whole file is sent.
    for ignored in ["bytes=0-1,5-6", "lines=1-2", "bytes=9-3"] {
        let response = download(&server, &ann, doc, None, Some(ignored)).await;
        assert_eq!(response.status(), StatusCode::OK, "{ignored}");
        assert_eq!(response.bytes().await.unwrap().to_vec(), bytes);
    }
}

/// How many bytes are in the content store's `tmp` folder: what the server has written of the
/// body it is receiving.
fn written_to_tmp(data_dir: &Path) -> u64 {
    let tmp = server::blobs_path(data_dir).join("tmp");
    std::fs::read_dir(tmp)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                // Opened by path: a directory entry's own size can lag behind on Windows.
                .filter_map(|entry| std::fs::metadata(entry.path()).ok())
                .map(|metadata| metadata.len())
                .sum()
        })
        .unwrap_or(0)
}

/// A 200 MiB save streams through the file session: the body is produced only as fast as the
/// server writes it to the content store, and the producer fails if the server ever holds more
/// than 1 MiB of it unwritten. The version is a pointer to exactly those bytes, and downloads
/// back the same.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn saves_stream_through_the_file_session() {
    const TOTAL: u64 = 200 * MIB;
    const CHUNK: usize = 64 * 1024;
    let server = common::spawn_server_config(|config| config.max_file_bytes = 300 * MIB).await;
    let ann = server.user("Ann").await;
    let doc = new_doc();
    server.create_file_document(doc, &ann).await;
    let seat = FileClient::connect_as(&server, doc, &ann).await;
    let token = seat.token().unwrap();

    let data_dir = server.data_dir.path().to_path_buf();
    let base: Vec<u8> = (0..CHUNK).map(|n| (n % 253) as u8).collect();
    let produced = Arc::new(AtomicU64::new(0));
    let hasher = Arc::new(std::sync::Mutex::new(Sha256::new()));
    let body = futures_util::stream::unfold(0u64, {
        let produced = Arc::clone(&produced);
        let hasher = Arc::clone(&hasher);
        move |index| {
            let data_dir = data_dir.clone();
            let produced = Arc::clone(&produced);
            let hasher = Arc::clone(&hasher);
            let mut chunk = base.clone();
            async move {
                let sent = index * CHUNK as u64;
                if sent >= TOTAL {
                    return None;
                }
                // Wait until the server has written all but 1 MiB of what it was sent.
                let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
                while sent > MIB && written_to_tmp(&data_dir) < sent - MIB {
                    assert!(
                        tokio::time::Instant::now() < deadline,
                        "the server holds more than 1 MiB of the body unwritten \
                         ({} of {sent} bytes written)",
                        written_to_tmp(&data_dir)
                    );
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
                chunk[..8].copy_from_slice(&index.to_le_bytes());
                hasher.lock().unwrap().update(&chunk);
                produced.fetch_add(chunk.len() as u64, Ordering::SeqCst);
                Some((Ok::<_, std::io::Error>(chunk), index + 1))
            }
        }
    });

    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()
        .unwrap()
        .put(server.url(&format!("/api/docs/{}/content", doc.0)))
        .header("origin", server.origin())
        .header("cookie", &ann.cookie)
        .header(SEAT, &token)
        .body(reqwest::Body::wrap_stream(body))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json: Value = response.json().await.unwrap();
    assert_eq!(json["changed"], true);
    let revision = json["revision"].as_str().unwrap().to_owned();
    assert_eq!(produced.load(Ordering::SeqCst), TOTAL);
    let sha256: [u8; 32] = hasher.lock().unwrap().clone().finalize().into();
    let expected = BlobRef {
        sha256,
        size: TOTAL,
    };
    assert_eq!(git_content(&server, doc, &revision), expected.to_pointer());
    assert_eq!(
        server.db.db.blob_refs_of(&sha256).await.unwrap(),
        vec![(doc, revision)]
    );

    let response = download(&server, &ann, doc, None, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut stream = response.bytes_stream();
    let mut downloaded = Sha256::new();
    let mut size = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.unwrap();
        size += chunk.len() as u64;
        downloaded.update(&chunk);
    }
    assert_eq!(size, TOTAL);
    let downloaded: [u8; 32] = downloaded.finalize().into();
    assert_eq!(downloaded, sha256);
}

/// The content rows are rebuilt from Git only when a flag asks for it, and the rebuild clears
/// the flag.
#[tokio::test]
async fn rebuild_only_when_flagged() {
    let server = spawn_server().await;
    let ann = server.user("Ann").await;
    let state = server.state().await;
    let db = &server.db.db;
    // The first start rebuilt for the upgrade and cleared its marker.
    assert!(!db.blob_refs_flagged().await.unwrap());

    let doc = upload(&server, &ann, "Kept.xlsx", b"kept".to_vec()).await;
    let blob = BlobRef::of(b"kept");
    let rows = db.blob_refs_of(&blob.sha256).await.unwrap();
    assert_eq!(rows.len(), 1);
    // An old inline version is not a row: its bytes are in Git.
    let old = new_doc();
    server.create_file_document(old, &ann).await;
    inline_version(&server, old, &format!("{}.xlsx", old.0), b"inline", None);

    sqlx::query("delete from blob_refs")
        .execute(db.test_pool())
        .await
        .unwrap();
    let report = server::reconcile_blobs(state, OffsetDateTime::now_utc())
        .await
        .unwrap();
    assert!(!report.rebuilt);
    assert!(db.blob_refs_of(&blob.sha256).await.unwrap().is_empty());

    db.set_blob_refs_flag(BLOB_REFS_DIRTY).await.unwrap();
    let report = server::reconcile_blobs(state, OffsetDateTime::now_utc())
        .await
        .unwrap();
    assert!(report.rebuilt);
    assert_eq!(report.versions, 1);
    assert!(report.failed.is_empty());
    assert_eq!(db.blob_refs_of(&blob.sha256).await.unwrap(), rows);
    assert_eq!(rows[0].0, doc);
    assert!(!db.blob_refs_flagged().await.unwrap());
}

/// Points `doc`'s ref (and `main`) at a commit whose `meta.json` is missing, so its history
/// cannot be read; returns the tip it had, to put back.
fn break_history(library: &Path, doc: DocId) -> gix::ObjectId {
    let repo = gix::open(library).unwrap();
    let docs_ref = format!("refs/sharecraft/docs/{}", doc.0);
    let tip = repo.find_reference(&docs_ref).unwrap().id().detach();
    let commit = repo.find_commit(tip).unwrap();
    let mut editor = repo.edit_tree(commit.tree_id().unwrap()).unwrap();
    editor
        .remove(format!("docs/{}/meta.json", doc.0).as_str())
        .unwrap();
    let tree = editor.write().unwrap().detach();
    let sig = gix::actor::Signature {
        name: "Ann".into(),
        email: "u1@sharecraft.invalid".into(),
        time: gix::date::Time::now_utc(),
    };
    let broken = repo
        .write_object(&gix::objs::Commit {
            tree,
            parents: [tip].into_iter().collect(),
            author: sig.clone(),
            committer: sig,
            encoding: None,
            message: "Checkpoint broken\n".into(),
            extra_headers: Vec::new(),
        })
        .unwrap()
        .detach();
    set_ref(library, &docs_ref, broken);
    tip
}

fn set_ref(library: &Path, name: &str, to: gix::ObjectId) {
    let repo = gix::open(library).unwrap();
    repo.reference(name, to, gix::refs::transaction::PreviousValue::Any, "test")
        .unwrap();
}

/// Unused content is erased at a start only after a rebuild that read every document, and only
/// once it is older than 24 hours; content a version names is never erased.
#[tokio::test]
async fn startup_erasure_waits_for_a_clean_rebuild() {
    let server = spawn_server().await;
    let ann = server.user("Ann").await;
    let state = server.state().await;
    let db = &server.db.db;
    let blobs = server.blobs();

    let kept = BlobRef::of(b"named by a version");
    let doc = upload(&server, &ann, "Kept.xlsx", b"named by a version".to_vec()).await;
    // Content nothing names: a file with no row, and one with a row but no claim left.
    let loose = server.blob(b"stored, never saved");
    let withdrawn = server.blob(b"save that failed");
    let claim = db.claim_blob(withdrawn, doc).await.unwrap();
    db.settle_claim(claim, None).await.unwrap();
    let later = OffsetDateTime::now_utc() + time::Duration::days(2);

    // A rebuild with a failure erases nothing, however old, and keeps the flag.
    let tip = break_history(&server.library(), doc);
    db.set_blob_refs_flag(BLOB_REFS_DIRTY).await.unwrap();
    let report = server::reconcile_blobs(state, later).await.unwrap();
    assert!(report.rebuilt);
    assert_eq!(report.failed.len(), 1, "{:?}", report.failed);
    assert_eq!(report.erased, 0);
    assert!(db.blob_refs_flagged().await.unwrap());
    for blob in [kept, loose, withdrawn] {
        assert!(blobs.contains(&blob.sha256), "{blob:?}");
    }
    set_ref(
        &server.library(),
        &format!("refs/sharecraft/docs/{}", doc.0),
        tip,
    );

    // A clean rebuild today: nothing is old enough yet.
    let report = server::reconcile_blobs(state, OffsetDateTime::now_utc())
        .await
        .unwrap();
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    assert_eq!(report.erased, 0);
    assert!(!db.blob_refs_flagged().await.unwrap());

    // Without a flag, nothing is rebuilt nor erased.
    let report = server::reconcile_blobs(state, later).await.unwrap();
    assert!(!report.rebuilt);
    assert!(blobs.contains(&loose.sha256));

    // A clean rebuild two days on erases the unused content, and only that.
    db.set_blob_refs_flag(BLOB_REFS_DIRTY).await.unwrap();
    let report = server::reconcile_blobs(state, later).await.unwrap();
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    assert_eq!(report.erased, 2);
    assert!(blobs.contains(&kept.sha256));
    assert!(!blobs.contains(&loose.sha256));
    assert!(!blobs.contains(&withdrawn.sha256));
    assert!(
        db.blob_created_at(&withdrawn.sha256)
            .await
            .unwrap()
            .is_none()
    );
    let response = download(&server, &ann, doc, None, None).await;
    assert_eq!(
        response.bytes().await.unwrap().to_vec(),
        b"named by a version"
    );
}

/// How many files the content store holds, finished or still in `tmp`.
fn stored_files(server: &TestServer) -> usize {
    let tmp = std::fs::read_dir(server.blobs().tmp_dir()).unwrap().count();
    server.blobs().list().unwrap().len() + tmp
}

/// An upload the caller may not make is refused before its body is read: nothing is stored.
#[tokio::test]
async fn refused_upload_stores_nothing() {
    let server = spawn_server().await;
    let ann = server.user("Ann").await;
    let bob = server.user("Bob").await;
    let anns = server.home_of(&ann).await.0.to_string();
    let response = server
        .http(Some(&bob))
        .post(server.url("/api/docs/upload"))
        .query(&[("parent", anns.as_str()), ("name", "Into.xlsx")])
        .body(vec![1u8; 2 * MIB as usize])
        .send()
        .await;
    // The server may answer before the client has sent the whole body.
    if let Ok(response) = response {
        assert!(response.status().is_client_error(), "{}", response.status());
    }
    assert_eq!(stored_files(&server), 0);

    // A name that is taken is refused the same way.
    upload(&server, &ann, "Taken.xlsx", b"first".to_vec()).await;
    let before = stored_files(&server);
    let response = server
        .http(Some(&ann))
        .post(server.url("/api/docs/upload"))
        .query(&[("parent", anns.as_str()), ("name", "Taken.xlsx")])
        .body(b"second".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(stored_files(&server), before);
}

/// A save without the seat, or from a revision that is not the current one, is refused before
/// its body is read: nothing is stored.
#[tokio::test]
async fn save_without_the_seat_stores_nothing() {
    let server = spawn_server().await;
    let ann = server.user("Ann").await;
    let doc = new_doc();
    server.create_file_document(doc, &ann).await;
    let put = |token: String, base: Option<String>| {
        let mut request = server
            .http(Some(&ann))
            .put(server.url(&format!("/api/docs/{}/content", doc.0)))
            .header(SEAT, token)
            .body(b"refused bytes".to_vec());
        if let Some(base) = base {
            request = request.query(&[("base", base)]);
        }
        request.send()
    };
    // No session open.
    let response = put("0".repeat(64), None).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let seat = FileClient::connect_as(&server, doc, &ann).await;
    let token = seat.token().unwrap();
    // A wrong token.
    let response = put("0".repeat(64), None).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    // A base that is not the current revision.
    let response = put(token, Some("1".repeat(40))).await.unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(stored_files(&server), 0);
}

/// A save that passes the check before its body but is refused when it arrives (another save
/// made a new revision meanwhile) has the content it stored erased.
#[tokio::test]
async fn refusal_after_storing_leaves_no_blob() {
    let server = spawn_server().await;
    let ann = server.user("Ann").await;
    let doc = new_doc();
    server.create_file_document(doc, &ann).await;
    let seat = FileClient::connect_as(&server, doc, &ann).await;
    let token = seat.token().unwrap();
    let url = server.url(&format!("/api/docs/{}/content", doc.0));

    // A slow save from no revision: the end of its body waits until released.
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let body = futures_util::stream::once(async { Ok::<_, std::io::Error>(b"slow ".to_vec()) })
        .chain(futures_util::stream::once(async move {
            released.await.unwrap();
            Ok(b"bytes".to_vec())
        }));
    let slow = tokio::spawn(
        server
            .http(Some(&ann))
            .put(url.clone())
            .header(SEAT, token.clone())
            .body(reqwest::Body::wrap_stream(body))
            .send(),
    );
    // Wait until the slow body is being written.
    let tmp = server.blobs().tmp_dir();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while std::fs::read_dir(&tmp).unwrap().count() == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the slow save never started"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Another save from no revision is made first.
    let response = server
        .http(Some(&ann))
        .put(url)
        .header(SEAT, token)
        .body(b"quick bytes".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    release.send(()).unwrap();
    let response = slow.await.unwrap().unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let blobs = server.blobs();
    assert!(!blobs.contains(&BlobRef::of(b"slow bytes").sha256));
    assert!(blobs.contains(&BlobRef::of(b"quick bytes").sha256));
    assert_eq!(stored_files(&server), 1);
}
