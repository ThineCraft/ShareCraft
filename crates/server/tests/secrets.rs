//! Sealed secrets: encryption under a key file in the data folder.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use server::secrets::{Secret, SecretBox, SecretError};

#[test]
fn seal_then_unseal_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let secrets = SecretBox::open_or_create(dir.path()).unwrap();
    let sealed = secrets.seal(b"ghp_token", b"source-token:one");
    assert_eq!(sealed.key_id, 1);
    assert_ne!(sealed.ciphertext, b"ghp_token");
    assert_eq!(
        secrets.unseal(&sealed, b"source-token:one").unwrap(),
        b"ghp_token"
    );
}

#[test]
fn another_context_fails() {
    let dir = tempfile::tempdir().unwrap();
    let secrets = SecretBox::open_or_create(dir.path()).unwrap();
    let sealed = secrets.seal(b"ghp_token", b"source-token:one");
    assert_eq!(
        secrets.unseal(&sealed, b"source-token:two"),
        Err(SecretError::WrongKey)
    );
}

#[test]
fn a_flipped_byte_fails() {
    let dir = tempfile::tempdir().unwrap();
    let secrets = SecretBox::open_or_create(dir.path()).unwrap();
    let mut sealed = secrets.seal(b"ghp_token", b"ctx");
    sealed.ciphertext[0] ^= 1;
    assert!(secrets.unseal(&sealed, b"ctx").is_err());
}

#[test]
fn two_seals_of_one_value_differ() {
    let dir = tempfile::tempdir().unwrap();
    let secrets = SecretBox::open_or_create(dir.path()).unwrap();
    let a = secrets.seal(b"same", b"ctx");
    let b = secrets.seal(b"same", b"ctx");
    assert_ne!(a.nonce, b.nonce);
    assert_ne!(a.ciphertext, b.ciphertext);
}

#[test]
fn key_file_is_created_once_and_reused() {
    let dir = tempfile::tempdir().unwrap();
    let first = SecretBox::open_or_create(dir.path()).unwrap();
    let sealed = first.seal(b"value", b"ctx");
    let second = SecretBox::open_or_create(dir.path()).unwrap();
    assert_eq!(second.unseal(&sealed, b"ctx").unwrap(), b"value");
    let key = std::fs::read(dir.path().join("secret.key")).unwrap();
    assert_eq!(key.len(), 32);
}

#[test]
fn debug_and_display_hide_the_value() {
    let secret = Secret::new("ghp_x");
    assert!(!format!("{secret:?}").contains("ghp_x"));
    assert!(!format!("{secret}").contains("ghp_x"));
    assert_eq!(secret.expose(), "ghp_x");
}

#[test]
fn a_short_key_file_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("secret.key");
    std::fs::write(&path, b"12345").unwrap();
    assert!(matches!(
        SecretBox::open_or_create(dir.path()),
        Err(SecretError::Unreadable(_))
    ));
    assert_eq!(std::fs::read(&path).unwrap(), b"12345");
}
