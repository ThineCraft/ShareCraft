//! The mirror of the organisation's brand centre (`org/brand.json` in Git) and the pins its
//! images hold in the content store.
//!
//! The centre is one row of `org_brand`, which always exists. Each brand image in use pins its
//! content with a `brand_refs` row naming its owner (`org` for the centre, a site's id for a
//! site's look); erasure counts these rows as it counts `blob_refs` (see
//! [`Db::release_blob_if_unreferenced`]). A change is judged and written in one transaction:
//! the actor's account row `FOR SHARE`, then the `org_brand` row `FOR UPDATE`, then the
//! `blobs` rows of newly pinned content `FOR SHARE`, by hash, then the `brand_refs` rows. Git
//! stays the authority: [`BrandChange`] with no actor rewrites the row and its pins from Git.

use std::collections::{BTreeMap, BTreeSet};

use collab_core::ids::{NodeId, UserId};
use revisions::BlobRef;
use revisions::brand::{ImageRef, OrgBrand};
use sqlx::PgConnection;

use crate::blobs::upsert_blob;
use crate::events::{NewEvent, record};
use crate::{AccessError, Db, DbError};

/// The owner of the brand centre's `brand_refs` rows.
pub const BRAND_CENTRE_OWNER: &str = "org";

/// Who may make a change of the brand centre.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrandNeeds {
    /// An enabled administrator: naming the brand managers.
    Admin,
    /// An enabled administrator or brand manager: themes and images.
    Manager,
}

/// A committed change of the brand centre to mirror.
#[derive(Clone, Debug)]
pub struct BrandChange<'a> {
    /// The centre as Git now holds it.
    pub record: &'a OrgBrand,
    /// Who made it; `None` for a repair from Git, judged by nobody, which also creates the
    /// `blobs` rows of pinned content that has none (held for the scanner when one is
    /// configured, as a version recorded from Git is).
    pub actor: Option<UserId>,
    /// What `actor` must be. Brand managers are judged by the centre as it was before.
    pub needs: BrandNeeds,
    /// The events recorded with the change.
    pub events: Vec<NewEvent>,
}

/// The content of `image` as the content store names it; `None` for a malformed hash, which
/// [`ImageRef::problem`] refuses before anything is stored.
fn blob_of(image: &ImageRef) -> Option<BlobRef> {
    Some(BlobRef {
        sha256: revisions::parse_hex(&image.sha256)?,
        size: image.bytes,
    })
}

/// Makes the pins of `owner` exactly `images`, on `conn` (a transaction holding whatever the
/// owner's change locks). Content newly pinned has its `blobs` row locked `FOR SHARE`, by hash,
/// before its row is written; with `create_missing` (a repair from Git) a missing `blobs` row
/// is created first, otherwise it means the content was erased meanwhile and nothing is
/// written: answers `false`. Pins no longer wanted are deleted; the caller then erases content
/// nothing names any more.
pub(crate) async fn pin_images(
    conn: &mut PgConnection,
    owner: &str,
    images: &[&ImageRef],
    create_missing: bool,
) -> Result<bool, sqlx::Error> {
    let wanted: BTreeMap<String, &ImageRef> = images
        .iter()
        .map(|image| (image.sha256.clone(), *image))
        .collect();
    let existing: BTreeSet<String> =
        sqlx::query_scalar::<_, String>("select sha256 from brand_refs where owner = $1")
            .bind(owner)
            .fetch_all(&mut *conn)
            .await?
            .into_iter()
            .collect();
    let new: Vec<&ImageRef> = wanted
        .iter()
        .filter(|(sha, _)| !existing.contains(*sha))
        .map(|(_, image)| *image)
        .collect();
    if !new.is_empty() {
        if create_missing {
            for image in &new {
                if let Some(blob) = blob_of(image) {
                    upsert_blob(conn, &blob).await?;
                }
            }
        }
        let hashes: Vec<String> = new.iter().map(|image| image.sha256.clone()).collect();
        let locked: Vec<String> = sqlx::query_scalar(
            "select sha256 from blobs where sha256 = any($1) order by sha256 for share",
        )
        .bind(&hashes)
        .fetch_all(&mut *conn)
        .await?;
        if locked.len() != hashes.len() {
            return Ok(false);
        }
        for image in &new {
            sqlx::query("insert into brand_refs (sha256, owner, media_type) values ($1, $2, $3)")
                .bind(&image.sha256)
                .bind(owner)
                .bind(&image.media_type)
                .execute(&mut *conn)
                .await?;
        }
    }
    let keep: Vec<String> = wanted.into_keys().collect();
    sqlx::query("delete from brand_refs where owner = $1 and not (sha256 = any($2))")
        .bind(owner)
        .bind(&keep)
        .execute(&mut *conn)
        .await?;
    Ok(true)
}

impl Db {
    /// The brand centre as PostgreSQL mirrors it.
    pub async fn org_brand(&self) -> Result<OrgBrand, DbError> {
        let value: Option<serde_json::Value> =
            sqlx::query_scalar("select record from org_brand where id")
                .fetch_optional(self.pool())
                .await?;
        match value {
            Some(value) => serde_json::from_value(value).map_err(DbError::new),
            None => Ok(OrgBrand::default()),
        }
    }

    /// The contents `owner` pins, each with the type it is served as, by hash.
    pub async fn brand_pins(&self, owner: &str) -> Result<Vec<(String, String)>, DbError> {
        Ok(sqlx::query_as(
            "select sha256, media_type from brand_refs where owner = $1 order by sha256",
        )
        .bind(owner)
        .fetch_all(self.pool())
        .await?)
    }

    /// The type the content `sha256` is served as, if the brand centre or a site's look names
    /// it; `None` if nothing does.
    pub async fn brand_image_type(&self, sha256: &[u8; 32]) -> Result<Option<String>, DbError> {
        Ok(sqlx::query_scalar(
            "select media_type from brand_refs where sha256 = $1 order by owner limit 1",
        )
        .bind(revisions::hex(sha256))
        .fetch_optional(self.pool())
        .await?)
    }

    /// Whether `user` may change the brand centre's themes and images: an enabled account that
    /// is an administrator or one of the brand managers.
    pub async fn can_manage_brand(&self, user: UserId) -> Result<bool, DbError> {
        let found: Option<bool> = sqlx::query_scalar(
            "select u.is_admin or coalesce(o.record -> 'managers' @> jsonb_build_array($1::text), \
                                           false) \
             from users u left join org_brand o on o.id \
             where u.id = $1 and u.disabled_at is null",
        )
        .bind(user.0)
        .fetch_optional(self.pool())
        .await?;
        Ok(found == Some(true))
    }

    /// Mirrors a committed change of the brand centre: judges the actor (see
    /// [`BrandChange::needs`]), writes the row, makes the centre's pins its images (see the
    /// module documentation for the locks) and records the events, in one transaction.
    ///
    /// # Errors
    /// `Forbidden` when the actor is not what the change needs (a disabled account never is);
    /// `ImageGone` when an image's content was erased meanwhile; `Unjudged` when the database
    /// fails before the actor was judged; otherwise the database error.
    pub async fn apply_org_brand(&self, change: BrandChange<'_>) -> Result<(), AccessError> {
        let unjudged = |e: sqlx::Error| AccessError::Unjudged(e.into());
        let mut tx = self.pool().begin().await.map_err(unjudged)?;
        let account: Option<(bool, bool)> = match change.actor {
            Some(actor) => sqlx::query_as(
                "select is_admin, disabled_at is null from users where id = $1 for share",
            )
            .bind(actor.0)
            .fetch_optional(&mut *tx)
            .await
            .map_err(unjudged)?
            .or(Some((false, false))),
            None => None,
        };
        let before: Option<serde_json::Value> =
            sqlx::query_scalar("select record from org_brand where id for update")
                .fetch_optional(&mut *tx)
                .await
                .map_err(unjudged)?;
        if let (Some(actor), Some((admin, enabled))) = (change.actor, account) {
            let manager = || -> Result<bool, AccessError> {
                let Some(value) = &before else {
                    return Ok(false);
                };
                let brand: OrgBrand = serde_json::from_value(value.clone())
                    .map_err(|e| AccessError::Unjudged(DbError::new(e)))?;
                Ok(brand.managers.contains(&actor.0))
            };
            let allowed = enabled
                && (admin
                    || match change.needs {
                        BrandNeeds::Admin => false,
                        BrandNeeds::Manager => manager()?,
                    });
            if !allowed {
                return Err(AccessError::Forbidden);
            }
        }

        let value = serde_json::to_value(change.record).map_err(DbError::new)?;
        sqlx::query(
            "insert into org_brand (id, record) values (true, $1) \
             on conflict (id) do update set record = excluded.record",
        )
        .bind(&value)
        .execute(&mut *tx)
        .await?;
        let images: Vec<&ImageRef> = change.record.images.iter().map(|i| &i.image).collect();
        if !pin_images(&mut tx, BRAND_CENTRE_OWNER, &images, change.actor.is_none()).await? {
            return Err(AccessError::ImageGone);
        }
        for event in change.events {
            record(&mut tx, event).await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

/// One change of a site's look, as its `site.look_changed` event recorded it.
#[derive(Clone, Debug, PartialEq)]
pub struct LookChange {
    /// The event's id.
    pub event_id: i64,
    /// When the change was made.
    pub at: time::OffsetDateTime,
    /// Who made it, with their name; `None` for the server, or an account since removed.
    pub by: Option<(UserId, String)>,
    /// The look before the change: `null` for none (the defaults).
    pub before: serde_json::Value,
    /// The look after the change: `null` for none.
    pub after: serde_json::Value,
}

/// Reads one row of a `site.look_changed` query.
fn look_change_from_row(row: &sqlx::postgres::PgRow) -> Result<LookChange, sqlx::Error> {
    use sqlx::Row;
    let actor: Option<uuid::Uuid> = row.try_get("actor")?;
    let name: Option<String> = row.try_get("name")?;
    let detail: serde_json::Value = row.try_get("detail")?;
    let side = |key: &str| detail.get(key).cloned().unwrap_or(serde_json::Value::Null);
    Ok(LookChange {
        event_id: row.try_get("id")?,
        at: row.try_get("at")?,
        by: actor.zip(name).map(|(id, name)| (UserId(id), name)),
        before: side("before"),
        after: side("after"),
    })
}

/// The changes of a site's look listed: `site.look_changed` events of the site that record
/// what the look became (repairs from Git do not).
const LOOK_CHANGES: &str = "select e.id, e.at, e.actor, u.name, e.detail from events e \
     left join users u on u.id = e.actor \
     where e.kind = 'site.look_changed' and e.node_id = $1 and e.detail ? 'after'";

impl Db {
    /// The latest `limit` changes of the look of `site`, newest first.
    pub async fn look_changes(&self, site: NodeId, limit: i64) -> Result<Vec<LookChange>, DbError> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "{LOOK_CHANGES} order by e.id desc limit $2"
        )))
        .bind(site.0)
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        rows.iter()
            .map(look_change_from_row)
            .collect::<Result<_, sqlx::Error>>()
            .map_err(DbError::from)
    }

    /// The change of the look of `site` recorded by the event `event_id`; `None` if that event
    /// is not one of its look's changes.
    pub async fn look_change(
        &self,
        site: NodeId,
        event_id: i64,
    ) -> Result<Option<LookChange>, DbError> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!("{LOOK_CHANGES} and e.id = $2")))
            .bind(site.0)
            .bind(event_id)
            .fetch_optional(self.pool())
            .await?;
        Ok(row.as_ref().map(look_change_from_row).transpose()?)
    }

    /// Every brand image pin, `(owner, sha256, media_type)`, by owner and hash: for comparing
    /// with Git.
    pub async fn all_brand_pins(&self) -> Result<Vec<(String, String, String)>, DbError> {
        Ok(sqlx::query_as(
            "select owner, sha256, media_type from brand_refs order by owner, sha256",
        )
        .fetch_all(self.pool())
        .await?)
    }

    /// Deletes the pins of `owner`, a site that is no longer in the tree; answers how many.
    /// The sweeper erases content nothing names any more.
    pub async fn drop_brand_pins(&self, owner: &str) -> Result<u64, DbError> {
        Ok(sqlx::query("delete from brand_refs where owner = $1")
            .bind(owner)
            .execute(self.pool())
            .await?
            .rows_affected())
    }

    /// The sites that are deleted (purged or not).
    pub async fn deleted_sites(&self) -> Result<Vec<NodeId>, DbError> {
        let ids: Vec<uuid::Uuid> = sqlx::query_scalar(
            "select id from nodes where kind = 'site' and parent_id is null \
               and deleted_at is not null",
        )
        .fetch_all(self.pool())
        .await?;
        Ok(ids.into_iter().map(NodeId).collect())
    }

    /// Which contents of `hashes` the content store has no row for: erased, or never stored.
    pub async fn missing_blobs(&self, hashes: &[String]) -> Result<Vec<String>, DbError> {
        let found: Vec<String> =
            sqlx::query_scalar("select sha256 from blobs where sha256 = any($1)")
                .bind(hashes)
                .fetch_all(self.pool())
                .await?;
        Ok(hashes
            .iter()
            .filter(|h| !found.contains(h))
            .cloned()
            .collect())
    }
}
