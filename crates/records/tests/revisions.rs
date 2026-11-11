#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use collab_core::ids::DocId;
use common::{put, site, user};
use records::test_support::TestDb;
use records::{AccessError, Editor, NodeKind, RevisionRecord, Role};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

fn doc_id() -> DocId {
    DocId(Uuid::new_v4())
}

/// A revision of `doc` with a 40-character id built from `n`, committed `n` minutes after
/// a fixed time, by Ann.
fn record(doc: DocId, n: u8) -> RevisionRecord {
    RevisionRecord {
        doc,
        revision: format!("{n:040x}"),
        committed_at: OffsetDateTime::from_unix_timestamp(1_800_000_000 + 60 * i64::from(n))
            .unwrap(),
        authors: vec![("u1".into(), "Ann".into())],
        name: format!("notes-{n}.txt"),
        fields: json!({ "n": n }),
    }
}

/// `(revision, position, parent)` of each recorded revision of `doc`.
async fn listed(test: &TestDb, doc: DocId) -> Vec<(String, i32, Option<String>)> {
    test.db
        .revisions(doc)
        .await
        .unwrap()
        .into_iter()
        .map(|r| (r.revision, r.position, r.parent))
        .collect()
}

#[tokio::test]
async fn append_requires_the_recorded_head() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    let doc = doc_id();
    test.db
        .create_document(ann.id, doc, "Notes", Editor::Text)
        .await
        .unwrap();
    let site = site(&test, "Site", Some(ann.id)).await;
    put(
        &test,
        doc.into(),
        NodeKind::Document,
        Some(site),
        site,
        "Notes",
    )
    .await;
    let (r1, r2, r3) = (record(doc, 1), record(doc, 2), record(doc, 3));

    // The first revision has no parent; naming one is refused.
    assert!(
        !test
            .db
            .append_revision(Some(&r2.revision), &r1)
            .await
            .unwrap()
    );
    assert!(listed(&test, doc).await.is_empty());
    assert!(test.db.append_revision(None, &r1).await.unwrap());
    // Not after the head: refused, nothing changes.
    assert!(!test.db.append_revision(None, &r2).await.unwrap());
    assert!(
        !test
            .db
            .append_revision(Some(&r3.revision), &r2)
            .await
            .unwrap()
    );
    assert!(
        test.db
            .append_revision(Some(&r1.revision), &r2)
            .await
            .unwrap()
    );

    assert_eq!(
        listed(&test, doc).await,
        vec![
            (r1.revision.clone(), 1, None),
            (r2.revision.clone(), 2, Some(r1.revision.clone())),
        ]
    );
    let rows = test.db.revisions(doc).await.unwrap();
    assert_eq!(rows[1].committed_at, r2.committed_at);
    assert_eq!(rows[1].authors, r2.authors);

    // The document record follows the newest revision; its creator and access stay.
    let document = test.db.document(doc).await.unwrap().unwrap();
    assert_eq!(document.revision.as_deref(), Some(r2.revision.as_str()));
    assert_eq!(document.revised_at, Some(r2.committed_at));
    assert_eq!(document.name, "notes-2.txt");
    assert_eq!(document.created_by, Some(ann.id));
    assert_eq!(test.db.role(ann.id, doc).await.unwrap(), Some(Role::Owner));
    let fields: serde_json::Value =
        sqlx::query_scalar("select fields from documents where id = $1")
            .bind(doc.0)
            .fetch_one(test.db.test_pool())
            .await
            .unwrap();
    assert_eq!(fields, json!({ "n": 2 }));
}

#[tokio::test]
async fn append_twice_records_once() {
    let test = TestDb::create().await;
    let doc = doc_id();
    let (r1, r2) = (record(doc, 1), record(doc, 2));
    assert!(test.db.append_revision(None, &r1).await.unwrap());
    assert!(
        test.db
            .append_revision(Some(&r1.revision), &r2)
            .await
            .unwrap()
    );
    assert!(
        !test
            .db
            .append_revision(Some(&r1.revision), &r2)
            .await
            .unwrap()
    );
    assert!(!test.db.append_revision(None, &r1).await.unwrap());
    assert_eq!(listed(&test, doc).await.len(), 2);
}

#[tokio::test]
async fn replace_is_all_or_nothing() {
    let test = TestDb::create().await;
    let doc = doc_id();
    let (r1, r2, r3) = (record(doc, 1), record(doc, 2), record(doc, 3));
    test.db
        .replace_revisions(doc, &[r1.clone(), r2.clone()])
        .await
        .unwrap();
    let before = listed(&test, doc).await;
    assert_eq!(
        before,
        vec![
            (r1.revision.clone(), 1, None),
            (r2.revision.clone(), 2, Some(r1.revision.clone())),
        ]
    );

    // The same revision twice cannot be stored: nothing of the new list is kept.
    let err = test
        .db
        .replace_revisions(doc, &[r3.clone(), r1.clone(), r1.clone()])
        .await;
    assert!(err.is_err());
    assert_eq!(listed(&test, doc).await, before);
    // Neither is a list holding another document's revision.
    let other = record(doc_id(), 4);
    assert!(
        test.db
            .replace_revisions(doc, &[r3.clone(), other])
            .await
            .is_err()
    );
    assert_eq!(listed(&test, doc).await, before);
    let document = test.db.document(doc).await.unwrap().unwrap();
    assert_eq!(document.revision.as_deref(), Some(r2.revision.as_str()));

    // A good list replaces the old one entirely.
    test.db
        .replace_revisions(doc, &[r1.clone(), r3.clone()])
        .await
        .unwrap();
    assert_eq!(
        listed(&test, doc).await,
        vec![
            (r1.revision.clone(), 1, None),
            (r3.revision.clone(), 2, Some(r1.revision.clone())),
        ]
    );
    let document = test.db.document(doc).await.unwrap().unwrap();
    assert_eq!(document.revision.as_deref(), Some(r3.revision.as_str()));
    assert_eq!(document.name, "notes-3.txt");
}

#[tokio::test]
async fn git_only_document_gets_a_record_nobody_can_open_until_placed() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    let ada = user(&test, "Ada", true).await;
    let (appended, replaced) = (doc_id(), doc_id());
    let r1 = record(appended, 1);
    assert!(test.db.append_revision(None, &r1).await.unwrap());
    test.db
        .replace_revisions(replaced, &[record(replaced, 2)])
        .await
        .unwrap();

    for (doc, name) in [(appended, "notes-1.txt"), (replaced, "notes-2.txt")] {
        let document = test.db.document(doc).await.unwrap().unwrap();
        assert_eq!(document.name, name);
        assert_eq!(document.created_by, None);
        assert!(document.revision.is_some());
        assert_eq!(test.db.role(ann.id, doc).await.unwrap(), None);
        // Outside the tree there is no node to decide access, even for an administrator.
        assert_eq!(test.db.role(ada.id, doc).await.unwrap(), None);
        assert_eq!(
            test.db.grants(ada.id, doc).await,
            Err(AccessError::NotFound)
        );
    }
    let site = site(&test, "Site", None).await;
    put(
        &test,
        appended.into(),
        NodeKind::Document,
        Some(site),
        site,
        "notes-1.txt",
    )
    .await;
    assert_eq!(test.db.role(ann.id, appended).await.unwrap(), None);
    assert_eq!(
        test.db.role(ada.id, appended).await.unwrap(),
        Some(Role::Owner)
    );
    assert!(test.db.grants(ada.id, appended).await.unwrap().is_empty());
}
