//! Where every secret is put, read and forgotten.
//!
//! A secret is sealed under the key file (see [`crate::secrets`]) and its ciphertext kept in
//! the database. Callers never touch the sealed rows themselves, so where the bytes live can
//! change in one place.

use records::{Db, SealedRow, SecretId};

use crate::secrets::{Sealed, SecretBox, SecretError};

/// The sentence for a store that could not be used. It never names a value.
const UNAVAILABLE: &str = "The secret store could not be reached.";

/// The one door to the secrets.
#[derive(Debug)]
pub struct SecretStore {
    sealer: SecretBox,
}

impl SecretStore {
    /// A store that seals under `sealer` and keeps the ciphertext in the database.
    #[must_use]
    pub fn local(sealer: SecretBox) -> Self {
        SecretStore { sealer }
    }

    /// Seals `plain` for `context` and stores it; its id.
    ///
    /// # Errors
    /// `Failed` when it could not be sealed; `Unavailable` when it could not be stored.
    pub async fn put(
        &self,
        db: &Db,
        plain: &[u8],
        context: &[u8],
    ) -> Result<SecretId, SecretError> {
        let sealed = self.sealer.seal(plain, context)?;
        db.put_secret(&SealedRow {
            key_id: sealed.key_id,
            nonce: sealed.nonce,
            ciphertext: sealed.ciphertext,
        })
        .await
        .map_err(|e| {
            eprintln!("sharecraft-server: a sealed secret could not be stored: {e}");
            SecretError::Unavailable(UNAVAILABLE.to_owned())
        })
    }

    /// The secret `id`, opened for `context`; `None` when there is no such secret.
    ///
    /// # Errors
    /// `WrongKey` or `Corrupt` when it does not open; `Unavailable` when it could not be read.
    pub async fn get(
        &self,
        db: &Db,
        id: SecretId,
        context: &[u8],
    ) -> Result<Option<Vec<u8>>, SecretError> {
        let row = db.get_secret(id).await.map_err(|e| {
            eprintln!("sharecraft-server: a sealed secret could not be read: {e}");
            SecretError::Unavailable(UNAVAILABLE.to_owned())
        })?;
        let Some(row) = row else {
            return Ok(None);
        };
        let sealed = Sealed {
            key_id: row.key_id,
            nonce: row.nonce,
            ciphertext: row.ciphertext,
        };
        self.sealer.unseal(&sealed, context).map(Some)
    }

    /// Deletes the secret `id`. A failure is logged and never fails the caller (the row holds
    /// only ciphertext).
    pub async fn forget(&self, db: &Db, id: SecretId) {
        if let Err(e) = db.delete_secret(id).await {
            eprintln!("sharecraft-server: a sealed secret could not be deleted: {e}");
        }
    }
}
