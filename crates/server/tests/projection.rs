#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use collab_core::ids::{DocId, Seq};
use common::{StoppableServer, TIMEOUT, TestClient, new_doc, spawn_server, spawn_server_config};
use protocol::{ClientMsg, SaveState, ServerMsg};
use records::test_support::TestDb;
use records::{Db, RevisionRow};
use revisions::{Author, Checkpoint, Outcome, RevisionId, RevisionStore};
use serde_json::{Map, Value, json};

/// Waits until `doc`'s recorded revisions end with `head`, and returns them.
async fn wait_recorded(db: &Db, doc: DocId, head: &str) -> Vec<RevisionRow> {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        let rows = db.revisions(doc).await.unwrap();
        if rows.last().is_some_and(|r| r.revision == head) {
            return rows;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{head} was never recorded; recorded: {rows:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Types `text` at the end of `c`'s document and waits until it is saved; returns the
/// revision.
async fn type_and_save(c: &mut TestClient, text: &str) -> String {
    let end = c.text().chars().count();
    let seq = Seq(c.seq().0 + 1);
    c.type_at(end, text).await;
    c.pump_until(|c| c.pending() == 0 && c.seq() == seq).await;
    c.wait_saved(seq).await
}

fn author(id: &str, name: &str) -> Author {
    Author {
        id: id.into(),
        name: name.into(),
    }
}

/// Writes a checkpoint of `doc` straight to the library at `library`, as another writer would.
fn write_behind(
    library: &Path,
    doc: DocId,
    bytes: &[u8],
    base: &str,
    fields: &Map<String, Value>,
) -> String {
    let store = RevisionStore::open(library).unwrap();
    let base: RevisionId = base.parse().unwrap();
    let authors = [author("someone-else", "Someone Else")];
    let blob = common::store_blob(common::data_of(library), bytes);
    let outcome = store
        .checkpoint(&Checkpoint {
            doc,
            file_name: &format!("{}.txt", doc.0),
            blob,
            base: Some(&base),
            authors: &authors,
            fields,
        })
        .unwrap();
    match outcome {
        Outcome::Written(r) => r.to_string(),
        Outcome::Unchanged(r) => panic!("expected a new revision, got Unchanged({r})"),
    }
}

/// Makes `doc`'s head in `library` a commit holding content but no metadata.
fn corrupt(library: &Path, doc: DocId) {
    let repo = gix::open(library).unwrap();
    let docs_ref = format!("refs/sharecraft/docs/{}", doc.0);
    let tip = repo.find_reference(&docs_ref).unwrap().id().detach();
    let tip = repo.find_commit(tip).unwrap();
    let mut editor = repo.edit_tree(tip.tree_id().unwrap()).unwrap();
    editor
        .remove(format!("docs/{}/meta.json", doc.0).as_str())
        .unwrap();
    let tree = editor.write().unwrap().detach();
    let sig = gix::actor::Signature {
        name: "Ann".into(),
        email: "u1@sharecraft.invalid".into(),
        time: gix::date::Time::now_utc(),
    };
    let commit = gix::objs::Commit {
        tree,
        parents: [tip.id].into_iter().collect(),
        author: sig.clone(),
        committer: sig,
        encoding: None,
        message: "Checkpoint broken.txt\n".into(),
        extra_headers: Vec::new(),
    };
    let new = repo.write_object(&commit).unwrap().detach();
    for name in ["refs/heads/main", docs_ref.as_str()] {
        repo.reference(
            name,
            new,
            gix::refs::transaction::PreviousValue::Any,
            "test",
        )
        .unwrap();
    }
}

/// Waits until the server answers at all, which it does only after reconciling.
async fn wait_listening(server: &StoppableServer) {
    let response = server
        .http(None)
        .get(server.url("/healthz"))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
}

async fn fields_of(db: &Db, doc: DocId) -> Value {
    sqlx::query_scalar("select fields from documents where id = $1")
        .bind(doc.0)
        .fetch_one(db.test_pool())
        .await
        .unwrap()
}

#[tokio::test]
async fn checkpoint_is_recorded_in_postgresql() {
    let server = spawn_server().await;
    let doc = new_doc();
    let mut a = TestClient::connect(&server, doc, "Ann").await;
    let first = type_and_save(&mut a, "abc").await;
    let second = type_and_save(&mut a, "def").await;

    let rows = wait_recorded(&server.db.db, doc, &second).await;
    let positions: Vec<_> = rows
        .iter()
        .map(|r| (r.revision.as_str(), r.position, r.parent.as_deref()))
        .collect();
    assert_eq!(
        positions,
        vec![
            (first.as_str(), 1, None),
            (second.as_str(), 2, Some(first.as_str()))
        ]
    );
    let ann = (a.user.id.0.to_string(), "Ann".to_owned());
    assert!(
        rows.iter().all(|r| r.authors == vec![ann.clone()]),
        "{rows:?}"
    );
    let document = server.db.db.document(doc).await.unwrap().unwrap();
    assert_eq!(document.revision.as_deref(), Some(second.as_str()));
    assert_eq!(document.revised_at, Some(rows[1].committed_at));
}

#[tokio::test]
async fn rebuild_reproduces_live_records() {
    // Changes typed within a second of each other are saved together.
    let server = spawn_server_config(|c| c.checkpoint_idle = Duration::from_secs(1)).await;
    let doc = new_doc();
    let mut a = TestClient::connect(&server, doc, "Ann").await;
    let mut b = TestClient::connect(&server, doc, "Bob").await;
    type_and_save(&mut a, "one ").await;
    b.pump_until(|c| c.text() == "one ").await;
    type_and_save(&mut b, "two ").await;
    a.pump_until(|c| c.text() == "one two ").await;
    a.type_at(8, "three ").await;
    a.pump_until(|c| c.pending() == 0 && c.seq() == Seq(3))
        .await;
    b.pump_until(|c| c.seq() == Seq(3)).await;
    let last = type_and_save(&mut b, "four").await;

    let live = wait_recorded(&server.db.db, doc, &last).await;
    assert_eq!(live.len(), 3, "{live:?}");
    let ann = (a.user.id.0.to_string(), "Ann".to_owned());
    let bob = (b.user.id.0.to_string(), "Bob".to_owned());
    assert_eq!(live[2].authors, vec![ann, bob]);

    let store: Arc<dyn server::Checkpointer> = Arc::new(server.store());
    let report = server::rebuild(store, &server.db.db).await.unwrap();
    assert_eq!((report.documents, report.updated), (1, 1));
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    assert_eq!(server.db.db.revisions(doc).await.unwrap(), live);
}

#[tokio::test]
async fn reconcile_at_start_reads_revisions_written_while_stopped() {
    let data_dir = tempfile::tempdir().unwrap();
    let db = TestDb::create().await;
    let library = server::library_path(data_dir.path());
    let mut first = StoppableServer::start(data_dir.path(), &db, |_| {});
    let doc = new_doc();
    let mut a = TestClient::connect(&first, doc, "Ann").await;
    let saved = type_and_save(&mut a, "abc").await;
    wait_recorded(&db.db, doc, &saved).await;
    first.stop();

    let behind = write_behind(&library, doc, b"abc and more", &saved, &Map::new());
    let second = StoppableServer::start(data_dir.path(), &db, |_| {});
    wait_listening(&second).await;
    let rows = db.db.revisions(doc).await.unwrap();
    let ids: Vec<_> = rows.iter().map(|r| r.revision.as_str()).collect();
    assert_eq!(ids, vec![saved.as_str(), behind.as_str()]);
    assert_eq!(
        rows[1].authors,
        vec![("someone-else".to_owned(), "Someone Else".to_owned())]
    );
    let document = db.db.document(doc).await.unwrap().unwrap();
    assert_eq!(document.revision.as_deref(), Some(behind.as_str()));
}

#[tokio::test]
async fn metadata_only_revision_updates_fields() {
    let data_dir = tempfile::tempdir().unwrap();
    let db = TestDb::create().await;
    let library = server::library_path(data_dir.path());
    let mut first = StoppableServer::start(data_dir.path(), &db, |_| {});
    let doc = new_doc();
    let mut a = TestClient::connect(&first, doc, "Ann").await;
    let saved = type_and_save(&mut a, "same text").await;
    wait_recorded(&db.db, doc, &saved).await;
    assert_eq!(fields_of(&db.db, doc).await, json!({}));
    first.stop();

    let mut fields = Map::new();
    fields.insert("title".into(), json!("Plan"));
    let behind = write_behind(&library, doc, b"same text", &saved, &fields);
    let second = StoppableServer::start(data_dir.path(), &db, |_| {});
    wait_listening(&second).await;
    let rows = db.db.revisions(doc).await.unwrap();
    assert_eq!(
        rows.last().map(|r| r.revision.as_str()),
        Some(behind.as_str())
    );
    assert_eq!(fields_of(&db.db, doc).await, json!({ "title": "Plan" }));
}

#[tokio::test]
async fn index_failure_does_not_fail_the_save() {
    let server = spawn_server().await;
    sqlx::query("drop table revisions")
        .execute(server.db.db.test_pool())
        .await
        .unwrap();
    let doc = new_doc();
    let mut a = TestClient::connect(&server, doc, "Ann").await;
    let first = type_and_save(&mut a, "kept").await;
    // A second save after the failed recording is not held up by it either.
    let second = type_and_save(&mut a, " in Git").await;
    assert_ne!(first, second);
    assert!(
        a.saves().iter().all(|s| s.0 != SaveState::Failed),
        "{:?}",
        a.saves()
    );
    let stored = server.store().read(doc, None).unwrap().unwrap();
    assert_eq!(server.bytes(&stored), b"kept in Git");
    assert_eq!(stored.revision.to_string(), second);
}

#[tokio::test]
async fn corrupt_document_is_reported_and_the_others_reconcile() {
    let data_dir = tempfile::tempdir().unwrap();
    let db = TestDb::create().await;
    let library = server::library_path(data_dir.path());
    let store = RevisionStore::open(&library).unwrap();
    let (good, broken) = (new_doc(), new_doc());
    let authors = [author("u1", "Ann")];
    let fields = Map::new();
    for doc in [good, broken] {
        store
            .checkpoint(&Checkpoint {
                doc,
                file_name: "notes.txt",
                blob: common::store_blob(data_dir.path(), b"text"),
                base: None,
                authors: &authors,
                fields: &fields,
            })
            .unwrap();
    }
    corrupt(&library, broken);

    let checkpointer: Arc<dyn server::Checkpointer> = Arc::new(store);
    let report = server::reconcile(Arc::clone(&checkpointer), &db.db)
        .await
        .unwrap();
    assert_eq!((report.documents, report.updated), (2, 1));
    assert_eq!(report.failed.len(), 1, "{:?}", report.failed);
    assert_eq!(report.failed[0].0, broken);
    assert_eq!(db.db.revisions(good).await.unwrap().len(), 1);
    assert!(db.db.revisions(broken).await.unwrap().is_empty());

    // Reconciling again leaves the recorded document alone.
    let again = server::reconcile(checkpointer, &db.db).await.unwrap();
    assert_eq!((again.documents, again.updated), (2, 0));
    assert_eq!(again.failed.len(), 1);

    // A server starts despite the broken document, and flags the content rows for a rebuild so
    // that nothing is erased while a document's revisions could not be read.
    db.db.rebuild_blob_refs(&[], true).await.unwrap(); // a fresh database starts flagged
    assert!(!db.db.blob_refs_flagged().await.unwrap());
    let running = StoppableServer::start(data_dir.path(), &db, |_| {});
    wait_listening(&running).await;
    assert!(db.db.blob_refs_flagged().await.unwrap());
}

fn conflicts(c: &TestClient) -> usize {
    c.saves()
        .iter()
        .filter(|s| s.0 == SaveState::Conflict)
        .count()
}

#[tokio::test]
async fn stale_checkpoint_stops_the_room_with_a_conflict() {
    let server = spawn_server().await;
    let doc = new_doc();
    let mut a = TestClient::connect(&server, doc, "Ann").await;
    let mut b = TestClient::connect(&server, doc, "Bob").await;
    let saved = type_and_save(&mut a, "abc").await;
    b.wait_saved(Seq(1)).await;

    // Another writer saves the document behind the room's back.
    let other = write_behind(&server.library(), doc, b"other text", &saved, &Map::new());
    a.type_at(3, "!").await;
    a.pump_until(|c| c.pending() == 0 && c.seq() == Seq(2))
        .await;
    a.pump_until(|c| conflicts(c) == 1).await;
    b.pump_until(|c| conflicts(c) == 1).await;

    // A new change is refused, and nothing more is saved.
    b.type_at(4, "?").await;
    b.pump_until(|c| {
        c.received
            .iter()
            .any(|m| matches!(m, ServerMsg::Refused { reason, .. } if reason == "conflict"))
    })
    .await;
    // Several idle periods pass: no further checkpoint is tried.
    tokio::time::sleep(Duration::from_millis(500)).await;
    // Anything sent before a cursor's people list has been read once that list arrives.
    a.send(&ClientMsg::Cursor { at: Some(1) }).await;
    for c in [&mut a, &mut b] {
        c.pump_until(|c| c.participants().iter().any(|p| p.cursor == Some(1)))
            .await;
        assert_eq!(conflicts(c), 1, "{:?}", c.saves());
        assert_eq!(c.seq(), Seq(2));
    }
    let head = server.store().head(doc).unwrap().unwrap();
    assert_eq!(head.to_string(), other);

    // Once empty, the room ends without saving; the next one loads the other revision.
    a.close().await;
    b.close().await;
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        let mut c = TestClient::connect(&server, doc, "Cy").await;
        if c.text() == "other text" {
            assert_eq!(c.joined_revision(), Some(other.clone()));
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the conflicted room never ended"
        );
        c.close().await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        server.store().head(doc).unwrap().unwrap().to_string(),
        other
    );
}

#[tokio::test]
async fn live_save_keeps_fields_set_by_a_metadata_only_revision() {
    let data_dir = tempfile::tempdir().unwrap();
    let db = TestDb::create().await;
    let library = server::library_path(data_dir.path());
    let mut first = StoppableServer::start(data_dir.path(), &db, |_| {});
    let doc = new_doc();
    let mut a = TestClient::connect(&first, doc, "Ann").await;
    let saved = type_and_save(&mut a, "text").await;
    wait_recorded(&db.db, doc, &saved).await;
    first.stop();

    let mut fields = Map::new();
    fields.insert("status".into(), json!("draft"));
    write_behind(&library, doc, b"text", &saved, &fields);
    let second = StoppableServer::start(data_dir.path(), &db, |_| {});
    // A new account: the first server's harness made Ann in this database already.
    let mut b = TestClient::connect(&second, doc, "Bob").await;
    assert_eq!(b.text(), "text");
    let live = type_and_save(&mut b, " more").await;

    let stored = RevisionStore::open(&library)
        .unwrap()
        .read(doc, None)
        .unwrap()
        .unwrap();
    assert_eq!(stored.revision.to_string(), live);
    assert_eq!(common::bytes_in(&library, &stored), b"text more");
    assert_eq!(stored.meta.fields, fields);
    wait_recorded(&db.db, doc, &live).await;
    assert_eq!(fields_of(&db.db, doc).await, json!({ "status": "draft" }));
}

#[tokio::test]
async fn recording_recovers_after_a_failed_append() {
    let server = spawn_server().await;
    let pool = server.db.db.test_pool();
    let doc = new_doc();
    let mut a = TestClient::connect(&server, doc, "Ann").await;
    let first = type_and_save(&mut a, "one").await;
    wait_recorded(&server.db.db, doc, &first).await;

    // Recording the next save fails: the table is out of the way.
    sqlx::query("alter table revisions rename to revisions_away")
        .execute(pool)
        .await
        .unwrap();
    type_and_save(&mut a, " two").await;
    // Long enough for that recording to have been tried and failed.
    tokio::time::sleep(Duration::from_millis(500)).await;
    sqlx::query("alter table revisions_away rename to revisions")
        .execute(pool)
        .await
        .unwrap();
    assert_eq!(server.db.db.revisions(doc).await.unwrap().len(), 1);

    // The next save's append finds the recorded head behind it and replaces the list from Git.
    let third = type_and_save(&mut a, " three").await;
    let rows = wait_recorded(&server.db.db, doc, &third).await;
    let history: Vec<String> = server
        .store()
        .history(doc)
        .unwrap()
        .into_iter()
        .map(|r| r.revision.to_string())
        .collect();
    assert_eq!(history.len(), 3);
    let recorded: Vec<String> = rows.iter().map(|r| r.revision.clone()).collect();
    assert_eq!(recorded, history);
    for (i, row) in rows.iter().enumerate() {
        assert_eq!(row.position, i32::try_from(i + 1).unwrap());
        let parent = i.checked_sub(1).map(|p| history[p].clone());
        assert_eq!(row.parent, parent);
    }
}

/// A revision whose content row cannot be written is still recorded, and a revision list out
/// of step is still replaced from Git: the failure only flags the content rows for a rebuild.
#[tokio::test]
async fn a_failed_content_row_still_repairs_the_revision_list() {
    let server = spawn_server().await;
    let db = &server.db.db;
    let doc = new_doc();
    let mut a = TestClient::connect(&server, doc, "Ann").await;
    let first = type_and_save(&mut a, "one").await;
    wait_recorded(db, doc, &first).await;

    // The recorded list falls out of step, and completed content rows can no longer be
    // written (claims, which are pending rows, still can).
    sqlx::query("delete from revisions where document_id = $1")
        .bind(doc.0)
        .execute(db.test_pool())
        .await
        .unwrap();
    sqlx::raw_sql(
        "create function no_completed_rows() returns trigger language plpgsql as $$ \
         begin if not new.pending then raise exception 'no completed rows'; end if; \
         return new; end $$; \
         create trigger no_completed_rows before insert on blob_refs \
         for each row execute function no_completed_rows();",
    )
    .execute(db.test_pool())
    .await
    .unwrap();

    let second = type_and_save(&mut a, " two").await;
    let rows = wait_recorded(db, doc, &second).await;
    let recorded: Vec<&str> = rows.iter().map(|r| r.revision.as_str()).collect();
    assert_eq!(recorded, [first.as_str(), second.as_str()]);
    assert!(db.blob_refs_flagged().await.unwrap());
}
