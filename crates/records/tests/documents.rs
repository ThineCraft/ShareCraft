#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use collab_core::ids::{DocId, UserId};
use common::{document_in, new_id, put, site, user};
use records::test_support::TestDb;
use records::{AccessError, Editor, NodeKind, Role};
use serde_json::json;
use uuid::Uuid;

fn doc_id() -> DocId {
    DocId(Uuid::new_v4())
}

/// A text document "Notes" in the library of a new site that `owner` owns.
async fn document(test: &TestDb, owner: UserId) -> DocId {
    let site = site(test, "Site", Some(owner)).await;
    let library = put(
        test,
        new_id(),
        NodeKind::Library,
        Some(site),
        site,
        "Documents",
    )
    .await;
    document_in(test, owner, library, site, "Notes").await
}

#[tokio::test]
async fn created_document_takes_its_access_from_the_tree() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    let id = doc_id();
    let created = test
        .db
        .create_document(ann.id, id, "  Plan  ", Editor::Text)
        .await
        .unwrap();
    assert_eq!(created.id, id);
    assert_eq!(created.name, "Plan");
    assert_eq!(created.created_by, Some(ann.id));
    assert_eq!(created.revision, None);
    assert_eq!(test.db.document(id).await.unwrap(), Some(created.clone()));
    // Not yet in the tree: nobody has a role on it.
    assert_eq!(test.db.role(ann.id, id).await.unwrap(), None);
    let site = site(&test, "Site", Some(ann.id)).await;
    let library = put(
        &test,
        new_id(),
        NodeKind::Library,
        Some(site),
        site,
        "Documents",
    )
    .await;
    put(
        &test,
        id.into(),
        NodeKind::Document,
        Some(library),
        site,
        "Plan",
    )
    .await;
    assert_eq!(test.db.role(ann.id, id).await.unwrap(), Some(Role::Owner));
    let grants = test.db.grants(ann.id, id).await.unwrap();
    assert_eq!(grants.len(), 1);
    assert_eq!((grants[0].user.id, grants[0].role), (ann.id, Role::Owner));
    let again = test
        .db
        .create_document(ann.id, id, "Other", Editor::Text)
        .await;
    assert_eq!(again, Err(AccessError::Exists));
    assert_eq!(test.db.document(doc_id()).await.unwrap(), None);
}

#[tokio::test]
async fn stranger_has_no_role_and_does_not_list_the_document() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    let bob = user(&test, "Bob", false).await;
    let id = document(&test, ann.id).await;
    assert_eq!(test.db.role(bob.id, id).await.unwrap(), None);
    assert!(test.db.documents_for(bob.id).await.unwrap().is_empty());
    assert_eq!(
        test.db.role(UserId(Uuid::new_v4()), id).await.unwrap(),
        None
    );
    assert_eq!(test.db.role(ann.id, doc_id()).await.unwrap(), None);
    let listed = test.db.documents_for(ann.id).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!((listed[0].0.id, listed[0].1), (id, Role::Owner));
}

#[tokio::test]
async fn grant_change_and_revoke_are_reflected_by_role() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    let bob = user(&test, "Bob", false).await;
    let id = document(&test, ann.id).await;

    let result = test
        .db
        .set_access(ann.id, id, bob.id, Some(Role::Viewer))
        .await;
    assert_eq!(result, Ok((None, Some(Role::Viewer))));
    assert_eq!(test.db.role(bob.id, id).await.unwrap(), Some(Role::Viewer));
    let listed = test.db.documents_for(bob.id).await.unwrap();
    assert_eq!((listed[0].0.id, listed[0].1), (id, Role::Viewer));

    let result = test
        .db
        .set_access(ann.id, id, bob.id, Some(Role::Editor))
        .await;
    assert_eq!(result, Ok((Some(Role::Viewer), Some(Role::Editor))));
    assert_eq!(test.db.role(bob.id, id).await.unwrap(), Some(Role::Editor));

    let result = test.db.set_access(ann.id, id, bob.id, None).await;
    assert_eq!(result, Ok((Some(Role::Editor), None)));
    assert_eq!(test.db.role(bob.id, id).await.unwrap(), None);
    assert!(test.db.documents_for(bob.id).await.unwrap().is_empty());

    let missing = test
        .db
        .set_access(ann.id, id, UserId(Uuid::new_v4()), Some(Role::Viewer))
        .await;
    assert_eq!(missing, Err(AccessError::NoSuchUser));

    assert!(!Role::Viewer.can_edit() && Role::Editor.can_edit() && Role::Owner.can_edit());
    assert!(!Role::Editor.can_manage() && Role::Owner.can_manage());
    assert!(Role::Viewer < Role::Editor && Role::Editor < Role::Owner);
    assert_eq!(serde_json::to_value(Role::Editor).unwrap(), json!("editor"));
}

#[tokio::test]
async fn admin_is_owner_everywhere_and_lists_everything() {
    let test = TestDb::create().await;
    let root = user(&test, "Root", true).await;
    let ann = user(&test, "Ann", false).await;
    let first = document(&test, ann.id).await;
    let second = document(&test, ann.id).await;
    assert_eq!(
        test.db.role(root.id, first).await.unwrap(),
        Some(Role::Owner)
    );
    let listed = test.db.documents_for(root.id).await.unwrap();
    let ids: Vec<DocId> = listed.iter().map(|(d, _)| d.id).collect();
    assert_eq!(ids, vec![second, first], "newest first");
    assert!(listed.iter().all(|(_, role)| *role == Role::Owner));
    // The administrator may manage access without holding a grant.
    let result = test
        .db
        .set_access(root.id, first, ann.id, Some(Role::Editor))
        .await;
    assert_eq!(result, Err(AccessError::LastOwner));
    let bob = user(&test, "Bob", false).await;
    let result = test
        .db
        .set_access(root.id, first, bob.id, Some(Role::Viewer))
        .await;
    assert_eq!(result, Ok((None, Some(Role::Viewer))));
    assert_eq!(test.db.grants(root.id, first).await.unwrap().len(), 2);
}

#[tokio::test]
async fn disabled_account_has_no_role() {
    let test = TestDb::create().await;
    let root = user(&test, "Root", true).await;
    let ann = user(&test, "Ann", false).await;
    let admin2 = user(&test, "Zed", true).await;
    let id = document(&test, ann.id).await;
    test.db.set_disabled(root.id, ann.id, true).await.unwrap();
    assert_eq!(test.db.role(ann.id, id).await.unwrap(), None);
    assert!(test.db.documents_for(ann.id).await.unwrap().is_empty());
    assert_eq!(test.db.grants(ann.id, id).await, Err(AccessError::NotFound));
    test.db
        .set_disabled(admin2.id, root.id, true)
        .await
        .unwrap();
    assert_eq!(test.db.role(root.id, id).await.unwrap(), None);
    assert_eq!(
        test.db
            .create_document(ann.id, doc_id(), "X", Editor::Text)
            .await,
        Err(AccessError::Forbidden)
    );
    test.db
        .set_disabled(admin2.id, ann.id, false)
        .await
        .unwrap();
    assert_eq!(test.db.role(ann.id, id).await.unwrap(), Some(Role::Owner));
}

#[tokio::test]
async fn editor_cannot_manage() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    let bob = user(&test, "Bob", false).await;
    let cy = user(&test, "Cy", false).await;
    let id = document(&test, ann.id).await;
    test.db
        .set_access(ann.id, id, bob.id, Some(Role::Editor))
        .await
        .unwrap();
    assert_eq!(
        test.db.grants(bob.id, id).await,
        Err(AccessError::Forbidden)
    );
    let result = test
        .db
        .set_access(bob.id, id, cy.id, Some(Role::Viewer))
        .await;
    assert_eq!(result, Err(AccessError::Forbidden));
    assert_eq!(test.db.role(cy.id, id).await.unwrap(), None);
}

#[tokio::test]
async fn stranger_cannot_tell_the_document_exists() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    let bob = user(&test, "Bob", false).await;
    let id = document(&test, ann.id).await;
    assert_eq!(test.db.grants(bob.id, id).await, Err(AccessError::NotFound));
    assert_eq!(
        test.db.grants(bob.id, doc_id()).await,
        Err(AccessError::NotFound)
    );
    let real = test
        .db
        .set_access(bob.id, id, bob.id, Some(Role::Owner))
        .await;
    let missing = test
        .db
        .set_access(bob.id, doc_id(), bob.id, Some(Role::Owner))
        .await;
    assert_eq!(real, Err(AccessError::NotFound));
    assert_eq!(missing, Err(AccessError::NotFound));
    assert_eq!(test.db.role(bob.id, id).await.unwrap(), None);
}

#[tokio::test]
async fn last_owner_cannot_be_removed_or_demoted() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    let bob = user(&test, "Bob", false).await;
    let id = document(&test, ann.id).await;
    assert_eq!(
        test.db.set_access(ann.id, id, ann.id, None).await,
        Err(AccessError::LastOwner)
    );
    assert_eq!(
        test.db
            .set_access(ann.id, id, ann.id, Some(Role::Editor))
            .await,
        Err(AccessError::LastOwner)
    );
    assert_eq!(test.db.role(ann.id, id).await.unwrap(), Some(Role::Owner));
    // With a second owner the first may step down.
    test.db
        .set_access(ann.id, id, bob.id, Some(Role::Owner))
        .await
        .unwrap();
    let result = test
        .db
        .set_access(ann.id, id, ann.id, Some(Role::Editor))
        .await;
    assert_eq!(result, Ok((Some(Role::Owner), Some(Role::Editor))));
    assert_eq!(
        test.db.set_access(bob.id, id, bob.id, None).await,
        Err(AccessError::LastOwner)
    );
}

#[tokio::test]
async fn two_removals_racing_leave_one_owner() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    let bob = user(&test, "Bob", false).await;
    let id = document(&test, ann.id).await;
    test.db
        .set_access(ann.id, id, bob.id, Some(Role::Owner))
        .await
        .unwrap();
    for round in 0..10 {
        // Hold the document's node while both calls start, so that both have started
        // and are queued behind it; they then run strictly one after the
        // other, whatever the scheduler does.
        let held = common::gate(&test, id.into()).await;
        let first = test.db.clone();
        let second = test.db.clone();
        let a = tokio::spawn(async move { first.set_access(ann.id, id, bob.id, None).await });
        let b = tokio::spawn(async move { second.set_access(bob.id, id, ann.id, None).await });
        common::wait_blocked(&test, held.pid, 2).await;
        held.open().await;
        let results = [a.await.unwrap(), b.await.unwrap()];
        // The calls take turns. The loser was an owner when it started, but the winner has
        // removed it by the time it holds the lock, so it is refused as someone with no access
        // (it must not get to make one more change). LastOwner would also be acceptable.
        let allowed = results.iter().filter(|r| r.is_ok()).count();
        let refused = results
            .iter()
            .filter(|r| matches!(r, Err(AccessError::NotFound | AccessError::LastOwner)))
            .count();
        assert_eq!((allowed, refused), (1, 1), "round {round}: {results:?}");
        let ann_has = test.db.role(ann.id, id).await.unwrap().is_some();
        let bob_has = test.db.role(bob.id, id).await.unwrap().is_some();
        assert!(
            ann_has != bob_has,
            "round {round}: exactly one owner remains"
        );
        let (remaining, removed) = if ann_has {
            (ann.id, bob.id)
        } else {
            (bob.id, ann.id)
        };
        test.db
            .set_access(remaining, id, removed, Some(Role::Owner))
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn same_role_again_records_nothing() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    let bob = user(&test, "Bob", false).await;
    let id = document(&test, ann.id).await;
    test.db
        .set_access(ann.id, id, bob.id, Some(Role::Editor))
        .await
        .unwrap();
    let before = test.db.events_for(id).await.unwrap().len();
    let result = test
        .db
        .set_access(ann.id, id, bob.id, Some(Role::Editor))
        .await;
    assert_eq!(result, Ok((Some(Role::Editor), Some(Role::Editor))));
    let result = test
        .db
        .set_access(ann.id, id, UserId(ann.id.0), Some(Role::Owner))
        .await;
    assert_eq!(result, Ok((Some(Role::Owner), Some(Role::Owner))));
    let carl = user(&test, "Carl", false).await;
    assert_eq!(
        test.db.set_access(ann.id, id, carl.id, None).await,
        Ok((None, None))
    );
    assert_eq!(test.db.events_for(id).await.unwrap().len(), before);
}

#[tokio::test]
async fn bad_document_names_are_refused() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    let long = "n".repeat(121);
    for name in ["", "   ", "a/b", "a\\b", "a\u{0}b", "a\nb", long.as_str()] {
        let result = test
            .db
            .create_document(ann.id, doc_id(), name, Editor::Text)
            .await;
        assert_eq!(result, Err(AccessError::InvalidName), "{name:?}");
    }
    assert!(test.db.documents_for(ann.id).await.unwrap().is_empty());
    let longest = "n".repeat(120);
    assert!(
        test.db
            .create_document(ann.id, doc_id(), &longest, Editor::Text)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn access_changes_are_recorded() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    let bob = user(&test, "Bob", false).await;
    let id = document(&test, ann.id).await;
    let site = test.db.node(id.into()).await.unwrap().unwrap().site;
    test.db
        .set_access(ann.id, id, bob.id, Some(Role::Viewer))
        .await
        .unwrap();
    test.db
        .set_access(ann.id, id, bob.id, Some(Role::Editor))
        .await
        .unwrap();
    test.db.set_access(ann.id, id, bob.id, None).await.unwrap();
    let events = test.db.events_for(id).await.unwrap();
    let summary: Vec<_> = events
        .iter()
        .map(|e| {
            (
                e.kind.as_str(),
                e.actor,
                e.subject,
                e.document,
                e.detail.clone(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        vec![
            (
                "document.created",
                Some(ann.id),
                None,
                Some(id),
                json!({"name": "Notes", "editor": "text"})
            ),
            (
                "inheritance.broken",
                Some(ann.id),
                None,
                Some(id),
                json!({"from": site.0, "copied": 1})
            ),
            (
                "access.granted",
                Some(ann.id),
                Some(bob.id),
                Some(id),
                json!({"role": "viewer"})
            ),
            (
                "access.changed",
                Some(ann.id),
                Some(bob.id),
                Some(id),
                json!({"from": "viewer", "to": "editor"})
            ),
            (
                "access.revoked",
                Some(ann.id),
                Some(bob.id),
                Some(id),
                json!({"role": "editor"})
            ),
        ]
    );
    let about_bob = test.db.events_about(bob.id).await.unwrap();
    assert!(about_bob.iter().any(|e| e.kind == "access.revoked"));
}

#[tokio::test]
async fn existing_documents_are_text() {
    let test = TestDb::create().await;
    let id = doc_id();
    sqlx::query("insert into documents (id, name) values ($1, 'Old notes')")
        .bind(id.0)
        .execute(test.db.test_pool())
        .await
        .unwrap();
    let read = test.db.document(id).await.unwrap().unwrap();
    assert_eq!(read.editor, Editor::Text);
}

#[test]
fn editor_is_chosen_by_extension() {
    assert_eq!(
        Editor::for_file_name("Budget.XLSX"),
        Some(Editor::GridCraft)
    );
    assert_eq!(Editor::for_file_name("a.docx"), Some(Editor::WordCraft));
    assert_eq!(Editor::for_file_name("a.pdf"), Some(Editor::PdfCraft));
    assert_eq!(
        Editor::for_file_name("a.solidcraft.json"),
        Some(Editor::SolidCraft)
    );
    assert_eq!(Editor::for_file_name("notes.txt"), None);
    assert_eq!(Editor::for_file_name("a.json"), None);
    assert_eq!(Editor::for_file_name("é"), None);
    for editor in Editor::ALL {
        assert_eq!(Editor::from_id(editor.id()), Some(editor));
        assert_eq!(editor.is_file(), editor != Editor::Text);
    }
    assert_eq!(Editor::from_id("excel"), None);
}

#[tokio::test]
async fn file_name_must_match_its_editor() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    for name in [".xlsx", "Budget.docx", "Budget", "xlsx"] {
        let result = test
            .db
            .create_document(ann.id, doc_id(), name, Editor::GridCraft)
            .await;
        assert_eq!(result, Err(AccessError::WrongExtension), "{name:?}");
    }
    let made = test
        .db
        .create_document(ann.id, doc_id(), "Budget.XLSX", Editor::GridCraft)
        .await
        .unwrap();
    assert_eq!(made.editor, Editor::GridCraft);
    assert_eq!(test.db.document(made.id).await.unwrap(), Some(made));
    // A file's name obeys the ordinary name rule too.
    let result = test
        .db
        .create_document(ann.id, doc_id(), "a/b.xlsx", Editor::GridCraft)
        .await;
    assert_eq!(result, Err(AccessError::InvalidName));
    // A text document may have any extension.
    assert!(
        test.db
            .create_document(ann.id, doc_id(), "Budget.docx", Editor::Text)
            .await
            .is_ok()
    );
}
