//! Stored share logins: the server of a UNC root, and one login per server.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use common::user;
use records::test_support::TestDb;
use records::{SHARE_ROOTS, SealedRow, SecretId, SharePath};
use serde_json::json;

fn sealed(bytes: &[u8]) -> SealedRow {
    SealedRow {
        key_id: 1,
        nonce: [7; 12],
        ciphertext: bytes.to_vec(),
    }
}

async fn secret(test: &TestDb, bytes: &[u8]) -> SecretId {
    test.db.put_secret(&sealed(bytes)).await.unwrap()
}

#[test]
fn server_of_unc_root_is_lower_case_host() {
    let path = SharePath::parse(r"\\FileServer.Example.COM\Docs\Sub").unwrap();
    assert_eq!(path.server().as_deref(), Some("fileserver.example.com"));
}

#[test]
fn server_of_drive_or_linux_path_is_none() {
    assert_eq!(SharePath::parse(r"C:\data").unwrap().server(), None);
    assert_eq!(SharePath::parse("/mnt/share").unwrap().server(), None);
}

#[tokio::test]
async fn a_login_is_set_replaced_and_removed() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", true).await;
    let first = secret(&test, b"one").await;
    let second = secret(&test, b"two").await;

    assert!(test.db.share_login("nas").await.unwrap().is_none());
    assert_eq!(
        test.db
            .set_share_login(ann.id, "nas", "DOM\\ann", first)
            .await
            .unwrap(),
        None
    );
    let login = test.db.share_login("nas").await.unwrap().unwrap();
    assert_eq!(login.server, "nas");
    assert_eq!(login.username, "DOM\\ann");
    assert_eq!(login.password_secret, first);

    // Replacing answers the old secret, to be forgotten.
    assert_eq!(
        test.db
            .set_share_login(ann.id, "nas", "bob", second)
            .await
            .unwrap(),
        Some(first)
    );
    let logins = test.db.share_logins().await.unwrap();
    assert_eq!(logins.len(), 1);
    assert_eq!(logins[0].username, "bob");
    assert_eq!(logins[0].password_secret, second);

    assert_eq!(
        test.db.remove_share_login("nas").await.unwrap(),
        Some(second)
    );
    assert_eq!(test.db.remove_share_login("nas").await.unwrap(), None);
    assert!(test.db.share_logins().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_server_name_must_be_lower_case() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", true).await;
    let id = secret(&test, b"one").await;
    assert!(
        test.db
            .set_share_login(ann.id, "NAS", "ann", id)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn login_events_hold_the_server_and_user_name_only() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", true).await;
    let id = secret(&test, b"one").await;
    test.db
        .set_share_login(ann.id, "nas", "ann", id)
        .await
        .unwrap();
    test.db.remove_share_login_as(ann.id, "nas").await.unwrap();
    let set = test.db.events_of_kind("share_login_set").await.unwrap();
    let removed = test.db.events_of_kind("share_login_removed").await.unwrap();
    assert_eq!(set.len(), 1);
    assert_eq!(removed.len(), 1);
    assert_eq!(set[0].actor, Some(ann.id));
    assert_eq!(set[0].detail, json!({"server": "nas", "username": "ann"}));
    assert_eq!(
        removed[0].detail,
        json!({"server": "nas", "username": "ann"})
    );
}

#[tokio::test]
async fn changing_roots_removes_logins_of_servers_left_without_a_root() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", true).await;
    let (a, b) = (secret(&test, b"a").await, secret(&test, b"b").await);
    test.db
        .set_setting(ann.id, SHARE_ROOTS, json!([r"\\nas\one", r"\\other\two"]))
        .await
        .unwrap();
    test.db
        .set_share_login(ann.id, "nas", "ann", a)
        .await
        .unwrap();
    test.db
        .set_share_login(ann.id, "other", "ann", b)
        .await
        .unwrap();

    // `nas` keeps another root; `other` loses its last.
    let gone = test
        .db
        .set_settings_pruning_logins(
            ann.id,
            vec![(SHARE_ROOTS, json!([r"\\NAS\three", "/mnt/x"]))],
        )
        .await
        .unwrap();
    assert_eq!(gone, vec![b]);
    let left = test.db.share_logins().await.unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].server, "nas");
    let removed = test.db.events_of_kind("share_login_removed").await.unwrap();
    assert_eq!(removed.len(), 1);
    assert_eq!(removed[0].detail["server"], "other");
}
