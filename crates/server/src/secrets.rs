//! Secrets kept at rest: AES-256-GCM under a key file in the data folder.
//!
//! The key file is `secret.key`, 32 random bytes, created on first use. Nothing needs
//! administrator rights. A sealed value is bound to its `context` (the owner of the secret,
//! for example `source-token:<library>`), so a ciphertext copied to another row fails to open.

use std::fmt;
use std::fs::OpenOptions;
use std::io::{ErrorKind, Write};
use std::path::Path;

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, KeyInit};

/// The name of the key file in the data folder.
const KEY_FILE: &str = "secret.key";
/// The key length in bytes.
const KEY_LEN: usize = 32;
/// The nonce length in bytes.
const NONCE_LEN: usize = 12;
/// The only key id so far; stored with each value so a later rotation can tell old from new.
const CURRENT_KEY_ID: u32 = 1;

/// An encrypted value, as stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sealed {
    /// Which key sealed it.
    pub key_id: u32,
    /// The nonce, fresh for each seal.
    pub nonce: [u8; NONCE_LEN],
    /// The ciphertext with its authentication tag.
    pub ciphertext: Vec<u8>,
}

/// Why a secret could not be sealed or opened. Never carries a secret or the key.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum SecretError {
    /// The key file could not be read, created or is not 32 bytes.
    #[error("the secret key file is unusable: {0}")]
    Unreadable(String),
    /// The value did not open: wrong key, wrong context or altered ciphertext.
    #[error("the secret did not open with this key and context")]
    WrongKey,
    /// The stored value is malformed.
    #[error("the stored secret is malformed")]
    Corrupt,
}

/// A secret string. `Debug` and `Display` never show it.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Wraps `value`.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Secret(value.into())
    }

    /// The value itself, for use at the point of need. Never print it.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[hidden]")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[hidden]")
    }
}

/// Seals and unseals values under the key from the data folder.
pub struct SecretBox {
    key: [u8; KEY_LEN],
    key_id: u32,
}

impl fmt::Debug for SecretBox {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretBox([hidden])")
    }
}

impl SecretBox {
    /// Opens `secret.key` in `data_dir`, creating it with random bytes when absent.
    ///
    /// # Errors
    /// `Unreadable` when the file cannot be read or created, or is not 32 bytes; an existing
    /// file is never overwritten.
    pub fn open_or_create(data_dir: &Path) -> Result<SecretBox, SecretError> {
        let path = data_dir.join(KEY_FILE);
        let unreadable = |e: std::io::Error| SecretError::Unreadable(e.kind().to_string());
        let mut key = [0u8; KEY_LEN];
        getrandom::fill(&mut key).map_err(|_| SecretError::Unreadable("no randomness".into()))?;

        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(mut file) => {
                file.write_all(&key).map_err(unreadable)?;
                file.sync_all().map_err(unreadable)?;
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                let bytes = std::fs::read(&path).map_err(unreadable)?;
                key = <[u8; KEY_LEN]>::try_from(bytes.as_slice())
                    .map_err(|_| SecretError::Unreadable("wrong length".into()))?;
            }
            Err(e) => return Err(unreadable(e)),
        }
        Ok(SecretBox {
            key,
            key_id: CURRENT_KEY_ID,
        })
    }

    fn cipher(&self) -> Result<Aes256Gcm, SecretError> {
        Aes256Gcm::new_from_slice(&self.key).map_err(|_| SecretError::Corrupt)
    }

    /// Encrypts `plain` bound to `context`, under a fresh nonce.
    ///
    /// Encryption cannot fail for any realistic input; if it ever did, the empty ciphertext
    /// returned would fail to open rather than store the value in the clear.
    #[must_use]
    pub fn seal(&self, plain: &[u8], context: &[u8]) -> Sealed {
        let mut nonce = [0u8; NONCE_LEN];
        let ciphertext = match (getrandom::fill(&mut nonce), self.cipher()) {
            (Ok(()), Ok(cipher)) => cipher
                .encrypt(
                    (&nonce).into(),
                    Payload {
                        msg: plain,
                        aad: context,
                    },
                )
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        Sealed {
            key_id: self.key_id,
            nonce,
            ciphertext,
        }
    }

    /// Decrypts `sealed`, which must have been sealed with the same `context`.
    ///
    /// # Errors
    /// `WrongKey` when the key, context or ciphertext does not match; `Corrupt` when the
    /// value is empty or made by an unknown key.
    pub fn unseal(&self, sealed: &Sealed, context: &[u8]) -> Result<Vec<u8>, SecretError> {
        if sealed.key_id != self.key_id || sealed.ciphertext.is_empty() {
            return Err(SecretError::Corrupt);
        }
        self.cipher()?
            .decrypt(
                (&sealed.nonce).into(),
                Payload {
                    msg: &sealed.ciphertext,
                    aad: context,
                },
            )
            .map_err(|_| SecretError::WrongKey)
    }
}
