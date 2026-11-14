//! The content store's safety rows: one row per stored content in `blobs`, and one per version
//! that names it, or is about to, in `blob_refs`.
//!
//! Before a Git commit names a hash, [`Db::claim_blob`] writes a pending row with the blob's row
//! locked `FOR SHARE`; after the commit, [`Db::settle_claim`] completes it with the revision, or
//! deletes it if the commit failed. [`Db::release_blob_if_unreferenced`] locks the blob's row
//! `FOR UPDATE`, counts every row naming it (pending ones included) and deletes it only at zero;
//! the caller then erases the file. `blobs` rows are locked in short transactions of their own,
//! never while holding any other lock. Git stays the authority: the completed rows can be
//! rebuilt from it ([`Db::rebuild_blob_refs`]), which the next start does when a flag says so.

use collab_core::ids::DocId;
use revisions::{BlobRef, hex, parse_hex};
use sqlx::PgConnection;
use time::OffsetDateTime;

use crate::{Db, DbError};

/// The flag set once by the upgrade to the content store.
pub const BLOB_REFS_UPGRADE: &str = "blob_refs_upgrade";
/// The flag set whenever a completed row could not be written.
pub const BLOB_REFS_DIRTY: &str = "blob_refs_dirty";

/// One pending row, written by [`Db::claim_blob`] and settled by [`Db::settle_claim`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ClaimId(i64);

/// A pending row left behind: its claim, the document and the content it names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StaleClaim {
    pub claim: ClaimId,
    pub doc: DocId,
    pub blob: BlobRef,
}

/// The size column's value for `blob`; sizes beyond `i64::MAX` cannot occur in a file system.
fn size_of(blob: &BlobRef) -> i64 {
    i64::try_from(blob.size).unwrap_or(i64::MAX)
}

/// Creates the row of `blob` if there is none.
async fn upsert_blob(conn: &mut PgConnection, blob: &BlobRef) -> Result<(), sqlx::Error> {
    sqlx::query("insert into blobs (sha256, size) values ($1, $2) on conflict (sha256) do nothing")
        .bind(blob.hex())
        .bind(size_of(blob))
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Writes the completed row naming `blob` from `revision` of `doc`, unless it exists.
async fn insert_completed(
    conn: &mut PgConnection,
    doc: DocId,
    sha256: &str,
    revision: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "insert into blob_refs (sha256, doc_id, revision, pending) values ($1, $2, $3, false) \
         on conflict (doc_id, sha256, revision) do nothing",
    )
    .bind(sha256)
    .bind(doc.0)
    .bind(revision)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

impl Db {
    /// Claims `blob` for a version of `doc` about to be committed: creates its `blobs` row if
    /// absent, locks it `FOR SHARE` (so no erasure can run meanwhile) and writes a pending
    /// `blob_refs` row, in one transaction. Once this returns, nothing erases `blob` until the
    /// claim is settled; the caller checks that the file is still there (an erasure that ran
    /// just before may have removed it).
    pub async fn claim_blob(&self, blob: BlobRef, doc: DocId) -> Result<ClaimId, DbError> {
        let mut tx = self.pool().begin().await?;
        upsert_blob(&mut tx, &blob).await?;
        sqlx::query("select sha256 from blobs where sha256 = $1 for share")
            .bind(blob.hex())
            .fetch_one(&mut *tx)
            .await?;
        let id: i64 = sqlx::query_scalar(
            "insert into blob_refs (sha256, doc_id, revision, pending) \
             values ($1, $2, null, true) returning id",
        )
        .bind(blob.hex())
        .bind(doc.0)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(ClaimId(id))
    }

    /// Claims several contents at once, as [`Db::claim_blob`] claims one, in one transaction:
    /// for each `(content, document)` a pending row, with every content's `blobs` row created
    /// if absent and locked `FOR SHARE`, by hash. Answers the claims in the order given. For a
    /// copy, whose versions are committed together.
    pub async fn claim_blobs(&self, claims: &[(BlobRef, DocId)]) -> Result<Vec<ClaimId>, DbError> {
        let mut tx = self.pool().begin().await?;
        // By hash, so two claims of the same contents never wait on each other in turn.
        let blobs: std::collections::BTreeMap<String, BlobRef> =
            claims.iter().map(|(blob, _)| (blob.hex(), *blob)).collect();
        for blob in blobs.values() {
            upsert_blob(&mut tx, blob).await?;
        }
        let hashes: Vec<String> = blobs.keys().cloned().collect();
        sqlx::query("select sha256 from blobs where sha256 = any($1) order by sha256 for share")
            .bind(&hashes)
            .fetch_all(&mut *tx)
            .await?;
        let mut ids = Vec::with_capacity(claims.len());
        for (blob, doc) in claims {
            let id: i64 = sqlx::query_scalar(
                "insert into blob_refs (sha256, doc_id, revision, pending) \
                 values ($1, $2, null, true) returning id",
            )
            .bind(blob.hex())
            .bind(doc.0)
            .fetch_one(&mut *tx)
            .await?;
            ids.push(ClaimId(id));
        }
        tx.commit().await?;
        Ok(ids)
    }

    /// Settles `claim` after its Git commit: with `Some(revision)` the version was committed and
    /// a completed row names it; with `None` the commit failed or wrote nothing, and the pending
    /// row is deleted. Settling a claim already settled does nothing.
    pub async fn settle_claim(
        &self,
        claim: ClaimId,
        revision: Option<&str>,
    ) -> Result<(), DbError> {
        let mut tx = self.pool().begin().await?;
        let pending: Option<(String, uuid::Uuid)> = sqlx::query_as(
            "delete from blob_refs where id = $1 and pending returning sha256, doc_id",
        )
        .bind(claim.0)
        .fetch_optional(&mut *tx)
        .await?;
        if let (Some((sha256, doc)), Some(revision)) = (pending, revision) {
            insert_completed(&mut tx, DocId(doc), &sha256, revision).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Settles every claim of `claims` as [`Db::settle_claim`] settles one, with the same
    /// `revision`, in one transaction.
    pub async fn settle_claims(
        &self,
        claims: &[ClaimId],
        revision: Option<&str>,
    ) -> Result<(), DbError> {
        let ids: Vec<i64> = claims.iter().map(|c| c.0).collect();
        let mut tx = self.pool().begin().await?;
        let pending: Vec<(String, uuid::Uuid)> = sqlx::query_as(
            "delete from blob_refs where id = any($1) and pending returning sha256, doc_id",
        )
        .bind(&ids)
        .fetch_all(&mut *tx)
        .await?;
        if let Some(revision) = revision {
            for (sha256, doc) in pending {
                insert_completed(&mut tx, DocId(doc), &sha256, revision).await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    /// Records that `revision` of `doc` names `blob`: creates the blob's row if absent and the
    /// completed row unless it exists. Used when a revision is recorded from Git.
    pub async fn record_blob_ref(
        &self,
        doc: DocId,
        blob: BlobRef,
        revision: &str,
    ) -> Result<(), DbError> {
        let mut tx = self.pool().begin().await?;
        upsert_blob(&mut tx, &blob).await?;
        insert_completed(&mut tx, doc, &blob.hex(), revision).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Deletes the row of the content `sha256` if no `blob_refs` row names it, pending ones
    /// included, with the row locked `FOR UPDATE` while counting. Answers whether nothing names
    /// it now (also when it had no row), in which case the caller erases the file.
    pub async fn release_blob_if_unreferenced(&self, sha256: &[u8; 32]) -> Result<bool, DbError> {
        let hash = hex(sha256);
        let mut tx = self.pool().begin().await?;
        let found: Option<String> =
            sqlx::query_scalar("select sha256 from blobs where sha256 = $1 for update")
                .bind(&hash)
                .fetch_optional(&mut *tx)
                .await?;
        if found.is_none() {
            return Ok(true);
        }
        let named: i64 = sqlx::query_scalar("select count(*) from blob_refs where sha256 = $1")
            .bind(&hash)
            .fetch_one(&mut *tx)
            .await?;
        if named > 0 {
            return Ok(false);
        }
        sqlx::query("delete from blobs where sha256 = $1")
            .bind(&hash)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// When the row of the content `sha256` was created; `None` if it has none.
    pub async fn blob_created_at(
        &self,
        sha256: &[u8; 32],
    ) -> Result<Option<OffsetDateTime>, DbError> {
        Ok(
            sqlx::query_scalar("select created_at from blobs where sha256 = $1")
                .bind(hex(sha256))
                .fetch_optional(self.pool())
                .await?,
        )
    }

    /// Which of the contents `sha256s` have a row, asked a few thousand at a time.
    pub async fn blobs_with_rows(
        &self,
        sha256s: &[[u8; 32]],
    ) -> Result<std::collections::HashSet<[u8; 32]>, DbError> {
        const BATCH: usize = 2000;
        let mut found = std::collections::HashSet::new();
        for batch in sha256s.chunks(BATCH) {
            let hashes: Vec<String> = batch.iter().map(|h| hex(h)).collect();
            let rows: Vec<String> =
                sqlx::query_scalar("select sha256 from blobs where sha256 = any($1)")
                    .bind(&hashes)
                    .fetch_all(self.pool())
                    .await?;
            found.extend(rows.iter().filter_map(|h| parse_hex(h)));
        }
        Ok(found)
    }

    /// The contents with a row created before `before` that no `blob_refs` row names.
    pub async fn unreferenced_blobs(
        &self,
        before: OffsetDateTime,
    ) -> Result<Vec<[u8; 32]>, DbError> {
        let rows: Vec<String> = sqlx::query_scalar(
            "select b.sha256 from blobs b \
             where b.created_at < $1 \
               and not exists (select 1 from blob_refs r where r.sha256 = b.sha256) \
             order by b.sha256",
        )
        .bind(before)
        .fetch_all(self.pool())
        .await?;
        Ok(rows.iter().filter_map(|h| parse_hex(h)).collect())
    }

    /// The `(document, revision)` rows naming the content `sha256`, completed ones only, in no
    /// particular order.
    pub async fn blob_refs_of(&self, sha256: &[u8; 32]) -> Result<Vec<(DocId, String)>, DbError> {
        let rows: Vec<(uuid::Uuid, String)> = sqlx::query_as(
            "select doc_id, revision from blob_refs where sha256 = $1 and not pending \
             order by doc_id, revision",
        )
        .bind(hex(sha256))
        .fetch_all(self.pool())
        .await?;
        Ok(rows.into_iter().map(|(d, r)| (DocId(d), r)).collect())
    }

    /// The pending rows written before `before`, oldest first.
    pub async fn stale_claims(&self, before: OffsetDateTime) -> Result<Vec<StaleClaim>, DbError> {
        let rows: Vec<(i64, uuid::Uuid, String, i64)> = sqlx::query_as(
            "select r.id, r.doc_id, r.sha256, b.size from blob_refs r \
             join blobs b on b.sha256 = r.sha256 \
             where r.pending and r.created_at < $1 order by r.created_at, r.id",
        )
        .bind(before)
        .fetch_all(self.pool())
        .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(id, doc, sha256, size)| {
                Some(StaleClaim {
                    claim: ClaimId(id),
                    doc: DocId(doc),
                    blob: BlobRef {
                        sha256: parse_hex(&sha256)?,
                        size: u64::try_from(size).ok()?,
                    },
                })
            })
            .collect())
    }

    /// Whether a flag asks for `blob_refs` to be rebuilt from Git.
    pub async fn blob_refs_flagged(&self) -> Result<bool, DbError> {
        Ok(
            sqlx::query_scalar("select exists (select 1 from maintenance_flags)")
                .fetch_one(self.pool())
                .await?,
        )
    }

    /// Sets the flag `name` ([`BLOB_REFS_UPGRADE`] or [`BLOB_REFS_DIRTY`]).
    pub async fn set_blob_refs_flag(&self, name: &str) -> Result<(), DbError> {
        sqlx::query("insert into maintenance_flags (name) values ($1) on conflict do nothing")
            .bind(name)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// Writes the completed rows `refs` (document, content, revision), as Git describes every
    /// version, in one transaction. With `complete` (every document in Git was read), the
    /// completed rows not among them are deleted first and the flags are cleared; pending rows
    /// are kept either way.
    pub async fn rebuild_blob_refs(
        &self,
        refs: &[(DocId, BlobRef, String)],
        complete: bool,
    ) -> Result<(), DbError> {
        let mut tx = self.pool().begin().await?;
        if complete {
            sqlx::query("delete from blob_refs where not pending")
                .execute(&mut *tx)
                .await?;
        }
        for (doc, blob, revision) in refs {
            upsert_blob(&mut tx, blob).await?;
            insert_completed(&mut tx, *doc, &blob.hex(), revision).await?;
        }
        if complete {
            sqlx::query("delete from maintenance_flags")
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}
