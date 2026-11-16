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
    let sealed = secrets.seal(b"ghp_token", b"source-token:one").unwrap();
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
    let sealed = secrets.seal(b"ghp_token", b"source-token:one").unwrap();
    assert_eq!(
        secrets.unseal(&sealed, b"source-token:two"),
        Err(SecretError::WrongKey)
    );
}

#[test]
fn a_flipped_byte_fails() {
    let dir = tempfile::tempdir().unwrap();
    let secrets = SecretBox::open_or_create(dir.path()).unwrap();
    let mut sealed = secrets.seal(b"ghp_token", b"ctx").unwrap();
    sealed.ciphertext[0] ^= 1;
    assert!(secrets.unseal(&sealed, b"ctx").is_err());
}

#[test]
fn two_seals_of_one_value_differ() {
    let dir = tempfile::tempdir().unwrap();
    let secrets = SecretBox::open_or_create(dir.path()).unwrap();
    let a = secrets.seal(b"same", b"ctx").unwrap();
    let b = secrets.seal(b"same", b"ctx").unwrap();
    assert_ne!(a.nonce, b.nonce);
    assert_ne!(a.ciphertext, b.ciphertext);
}

#[test]
fn key_file_is_created_once_and_reused() {
    let dir = tempfile::tempdir().unwrap();
    let first = SecretBox::open_or_create(dir.path()).unwrap();
    let sealed = first.seal(b"value", b"ctx").unwrap();
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

#[test]
fn racing_starts_all_share_one_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let path = path.clone();
            std::thread::spawn(move || {
                let secrets = SecretBox::open_or_create(&path).unwrap();
                let sealed = secrets.seal(b"v", b"ctx").unwrap();
                (secrets, sealed)
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    for (secrets, _) in &results {
        for (_, sealed) in &results {
            assert_eq!(secrets.unseal(sealed, b"ctx").unwrap(), b"v");
        }
    }
    let names: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
    assert_eq!(names.len(), 1, "no temporary files are left behind");
}

#[test]
fn a_failed_creation_leaves_no_key_file() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("not-there");
    assert!(matches!(
        SecretBox::open_or_create(&missing),
        Err(SecretError::Unreadable(_))
    ));
    assert!(!missing.join("secret.key").exists());
}
