//! Where every secret is put, read and forgotten.
//!
//! A secret is either sealed under the key file (see [`crate::secrets`]) with its ciphertext
//! kept in the database, or, when an outside provider is configured (see
//! [`crate::secret_provider`]), kept by the provider, the database holding only a row that gives
//! it an id. Callers never touch the rows themselves, so where the bytes live can change in one
//! place. A row says by itself where its value lives, so rows sealed before a provider was
//! configured still open.
//!
//! Values read from the provider are kept in memory for sixty seconds, so a login read for
//! every sync is not fetched every time. A change made by this process drops its entry; another
//! front end sees it within the minute.

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use records::{Db, SealedRow, SecretId, StoredSecret};

use crate::secret_provider::{SecretProvider, provider_name};
use crate::secrets::{Sealed, SecretBox, SecretError};

/// The sentence for a store that could not be used. It never names a value.
const UNAVAILABLE: &str = "The secret store could not be reached.";
/// The sentence for a value kept by a provider this server has not been given.
const NO_PROVIDER: &str = "this server has no secret provider configured";
/// How long a value read from the provider is kept.
const CACHE_TTL: Duration = Duration::from_secs(60);
/// The most forgotten secrets one sweep deletes at the provider.
const SWEEP_BATCH: i64 = 500;

/// The one door to the secrets.
pub struct SecretStore {
    sealer: SecretBox,
    provider: Option<SecretProvider>,
    /// Values opened from the provider: when each was read, and the value.
    cache: Mutex<HashMap<SecretId, (Instant, Vec<u8>)>>,
    /// How long an entry of `cache` is good for, in milliseconds.
    cache_ttl_ms: AtomicU64,
}

impl fmt::Debug for SecretStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretStore")
            .field("sealer", &self.sealer)
            .field("provider", &self.provider)
            .field("cache", &"[hidden]")
            .finish_non_exhaustive()
    }
}

fn unavailable() -> SecretError {
    SecretError::Unavailable(UNAVAILABLE.to_owned())
}

impl SecretStore {
    /// A store that seals under `sealer` and keeps the ciphertext in the database.
    #[must_use]
    pub fn local(sealer: SecretBox) -> Self {
        SecretStore {
            sealer,
            provider: None,
            cache: Mutex::new(HashMap::new()),
            cache_ttl_ms: AtomicU64::new(duration_ms(CACHE_TTL)),
        }
    }

    /// This store, keeping new secrets with `provider` instead.
    #[must_use]
    pub fn with_provider(mut self, provider: SecretProvider) -> Self {
        self.provider = Some(provider);
        self
    }

    /// Changes how long a value read from the provider is kept, so a test need not wait a
    /// minute for it to expire.
    #[doc(hidden)]
    pub fn set_cache_ttl_for_tests(&self, ttl: Duration) {
        self.cache_ttl_ms.store(duration_ms(ttl), Ordering::Relaxed);
    }

    fn cached(&self, id: SecretId) -> Option<Vec<u8>> {
        let ttl = Duration::from_millis(self.cache_ttl_ms.load(Ordering::Relaxed));
        let cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        cache
            .get(&id)
            .filter(|(read, _)| read.elapsed() < ttl)
            .map(|(_, value)| value.clone())
    }

    fn keep(&self, id: SecretId, value: &[u8]) {
        let ttl = Duration::from_millis(self.cache_ttl_ms.load(Ordering::Relaxed));
        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        // Entries that expired are never read again: drop them as new ones come in.
        cache.retain(|_, (read, _)| read.elapsed() < ttl);
        cache.insert(id, (Instant::now(), value.to_vec()));
    }

    fn drop_cached(&self, id: SecretId) {
        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        cache.remove(&id);
    }

    /// Stores `plain` for `context`; its id.
    ///
    /// With a provider the value goes there, under a name made from the id, and `context` is
    /// not used (the vault protects the value). Without, it is sealed for `context`.
    ///
    /// # Errors
    /// `Failed` when it could not be sealed; `Unavailable` when it could not be stored.
    pub async fn put(
        &self,
        db: &Db,
        plain: &[u8],
        context: &[u8],
    ) -> Result<SecretId, SecretError> {
        let Some(provider) = &self.provider else {
            return self.put_local(db, plain, context).await;
        };
        // The row first, for the id the provider's name is made from.
        let id = db.put_external_secret().await.map_err(|e| {
            eprintln!("sharecraft-server: a secret could not be recorded: {e}");
            unavailable()
        })?;
        if let Err(e) = provider.put(&provider_name(id), plain).await {
            // The provider may have stored the value before its answer was lost: mark the row
            // forgotten, so the sweep deletes the value there (a 404 counts as gone) and the row.
            if let Err(e) = db.forget_secret(id).await {
                eprintln!("sharecraft-server: a secret row could not be marked: {e}");
            }
            return Err(e);
        }
        self.drop_cached(id);
        Ok(id)
    }

    async fn put_local(
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
            unavailable()
        })
    }

    /// The secret `id`, opened for `context`; `None` when there is no such secret.
    ///
    /// # Errors
    /// `WrongKey` or `Corrupt` when it does not open; `Unavailable` when it could not be read,
    /// or it is kept by a provider this server has not been given.
    pub async fn get(
        &self,
        db: &Db,
        id: SecretId,
        context: &[u8],
    ) -> Result<Option<Vec<u8>>, SecretError> {
        if let Some(value) = self.cached(id) {
            return Ok(Some(value));
        }
        let row = db.get_stored_secret(id).await.map_err(|e| {
            eprintln!("sharecraft-server: a sealed secret could not be read: {e}");
            unavailable()
        })?;
        match row {
            None => Ok(None),
            Some(StoredSecret::Local(row)) => {
                let sealed = Sealed {
                    key_id: row.key_id,
                    nonce: row.nonce,
                    ciphertext: row.ciphertext,
                };
                self.sealer.unseal(&sealed, context).map(Some)
            }
            Some(StoredSecret::External) => {
                let Some(provider) = &self.provider else {
                    return Err(SecretError::Unavailable(NO_PROVIDER.to_owned()));
                };
                let value = provider.get(&provider_name(id)).await?;
                if let Some(value) = &value {
                    self.keep(id, value);
                }
                Ok(value)
            }
        }
    }

    /// Forgets the secret `id`. A sealed row is deleted at once; a value kept by the provider
    /// is marked, and [`SecretStore::sweep_forgotten`] deletes it there. A failure is logged and
    /// never fails the caller.
    pub async fn forget(&self, db: &Db, id: SecretId) {
        self.drop_cached(id);
        if let Err(e) = db.forget_secret(id).await {
            eprintln!("sharecraft-server: a secret could not be forgotten: {e}");
        }
    }

    /// Deletes at the provider each value that was forgotten, and then its row; how many went.
    /// A value that could not be deleted stays marked and is tried again at the next sweep.
    pub async fn sweep_forgotten(&self, db: &Db) -> usize {
        let forgotten = match db.forgotten_secrets(SWEEP_BATCH).await {
            Ok(forgotten) => forgotten,
            Err(e) => {
                eprintln!("sharecraft-server: forgotten secrets could not be listed: {e}");
                return 0;
            }
        };
        let Some(provider) = &self.provider else {
            // Nothing can be deleted without the provider: the marks wait for one.
            if !forgotten.is_empty() {
                eprintln!(
                    "sharecraft-server: {} forgotten secret(s) wait for a secret provider",
                    forgotten.len()
                );
            }
            return 0;
        };
        let mut deleted = 0;
        for id in forgotten {
            self.drop_cached(id);
            if provider.delete(&provider_name(id)).await.is_err() {
                continue;
            }
            match db.delete_secret(id).await {
                Ok(()) => deleted += 1,
                Err(e) => eprintln!("sharecraft-server: a secret row could not be removed: {e}"),
            }
        }
        deleted
    }
}

fn duration_ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}
