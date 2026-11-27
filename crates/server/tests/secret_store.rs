//! The secret store: every secret is put, read and forgotten through one seam.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use records::SecretId;
use records::test_support::TestDb;
use server::secret_store::SecretStore;
use server::secrets::{SecretBox, SecretError};

async fn store() -> (SecretStore, TestDb, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = SecretStore::local(SecretBox::open_or_create(dir.path()).unwrap());
    (store, TestDb::create().await, dir)
}

#[tokio::test]
async fn put_then_get_round_trips() {
    let (store, db, _dir) = store().await;
    let id = store
        .put(&db.db, b"ghp_token", b"source-token:one")
        .await
        .unwrap();
    let got = store.get(&db.db, id, b"source-token:one").await;
    assert!(
        matches!(&got, Ok(Some(value)) if value.as_slice() == b"ghp_token"),
        "the token was not read back"
    );
}

#[tokio::test]
async fn get_with_another_context_is_wrong_key() {
    let (store, db, _dir) = store().await;
    let id = store
        .put(&db.db, b"ghp_token", b"source-token:one")
        .await
        .unwrap();
    let got = store.get(&db.db, id, b"source-token:two").await;
    assert!(matches!(got, Err(SecretError::WrongKey)));
}

#[tokio::test]
async fn forget_then_get_is_none() {
    let (store, db, _dir) = store().await;
    let id = store.put(&db.db, b"ghp_token", b"ctx").await.unwrap();
    store.forget(&db.db, id).await;
    assert!(matches!(store.get(&db.db, id, b"ctx").await, Ok(None)));
    // Forgetting what is already gone does not fail.
    store.forget(&db.db, id).await;
}

#[tokio::test]
async fn an_unknown_id_is_none() {
    let (store, db, _dir) = store().await;
    let id = SecretId(uuid::Uuid::new_v4());
    assert!(matches!(store.get(&db.db, id, b"ctx").await, Ok(None)));
}
