//! Site quotas: how many bytes each site holds and may hold.
//!
//! A site's usage (`used`) is the sum of the sizes of every version of every item in it, live
//! and in its bins, inline old versions included; a purged item's versions are given back, and
//! a file moved to another site takes its versions' sizes there. A save, upload or copy first
//! reserves its bytes ([`Db::reserve_bytes`]) with the site's row locked `FOR UPDATE`, refused
//! (`QuotaError::Exceeded`) when the site would go over its quota; once its commit is in, the
//! reservation becomes usage, and a failure releases it. An open upload session holds its
//! declared size reserved. Checkpoints of the live text room are counted without being
//! reserved ([`Db::add_usage`]): they are never refused.
//!
//! Locking order (extending the tree's): node rows, then link rows, then `site_usage` rows by
//! site id, then groups, then roles. The rows here can always be rebuilt: `used` from the sizes
//! of the recorded revisions ([`Db::rebuild_usage`], when a flag asks for it), `reserved` from
//! the open upload sessions at each start ([`Db::reset_reservations`]).

use std::collections::{BTreeMap, BTreeSet};

use collab_core::ids::{DocId, NodeId, UserId};
use serde_json::json;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::events::record;
use crate::{AccessError, Db, DbError, NewEvent, NodeRow};

/// The share of its quota from which a site's owners are warned, in percent.
pub const QUOTA_WARNING_PERCENT: u64 = 90;

/// The flag set by the upgrade to quotas, asking for the first rebuild of usage.
const USAGE_UPGRADE: &str = "upgrade";
/// The flag set when a change of usage could not be written.
const USAGE_DIRTY: &str = "dirty";

/// A site's storage as its row holds it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SiteUsage {
    /// Bytes held by every version of every item in the site, live and in its bins.
    pub used: u64,
    /// Bytes set aside by saves, uploads and copies under way, and open upload sessions.
    pub reserved: u64,
    /// The most the site may hold; `None` for no limit.
    pub quota: Option<u64>,
}

impl SiteUsage {
    /// Whether the site has used [`QUOTA_WARNING_PERCENT`] of its quota or more.
    #[must_use]
    pub fn near_quota(&self) -> bool {
        self.quota.is_some_and(|quota| {
            u128::from(self.used) * 100 >= u128::from(quota) * u128::from(QUOTA_WARNING_PERCENT)
        })
    }
}

/// Bytes set aside against a site's quota. It must end in [`Db::commit_reservation`] (the
/// bytes were stored) or [`Db::release_reservation`] (they were not); a server that stops
/// first drops it at its next start.
#[derive(Debug, PartialEq, Eq)]
#[must_use]
pub struct Reservation {
    site: NodeId,
    bytes: u64,
}

impl Reservation {
    /// The reservation of `bytes` in `site`, already made.
    pub(crate) fn new(site: NodeId, bytes: u64) -> Self {
        Self { site, bytes }
    }

    /// The site the bytes are set aside in.
    #[must_use]
    pub fn site(&self) -> NodeId {
        self.site
    }

    /// How many bytes are set aside.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}

/// Why bytes could not be reserved.
#[derive(Debug, thiserror::Error)]
pub enum QuotaError {
    /// The site would go over its quota.
    #[error("the site's storage quota would be exceeded")]
    Exceeded,
    #[error(transparent)]
    Db(#[from] DbError),
}

impl From<sqlx::Error> for QuotaError {
    fn from(e: sqlx::Error) -> Self {
        QuotaError::Db(e.into())
    }
}

/// A byte count as the database stores it.
fn column(bytes: u64) -> i64 {
    i64::try_from(bytes).unwrap_or(i64::MAX)
}

/// A byte count read from the database, where the checks keep it from being negative.
fn bytes_of(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

/// Whether `extra` more bytes fit in a site holding `used` and `reserved` under `quota`.
pub(crate) fn fits(used: u64, reserved: u64, quota: Option<u64>, extra: u64) -> bool {
    quota.is_none_or(|quota| {
        u128::from(used) + u128::from(reserved) + u128::from(extra) <= u128::from(quota)
    })
}

/// Creates the usage rows of `sites` that are missing, then locks them all `FOR UPDATE`, by
/// site id.
pub(crate) async fn lock_usage(
    conn: &mut PgConnection,
    sites: &[NodeId],
) -> Result<(), sqlx::Error> {
    if sites.is_empty() {
        return Ok(());
    }
    let mut ids: Vec<Uuid> = sites.iter().map(|s| s.0).collect();
    ids.sort_unstable();
    ids.dedup();
    sqlx::query(
        "insert into site_usage (site_id) select unnest($1::uuid[]) order by 1 \
         on conflict do nothing",
    )
    .bind(&ids)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "select site_id from site_usage where site_id = any($1) order by site_id for update",
    )
    .bind(&ids)
    .fetch_all(&mut *conn)
    .await?;
    Ok(())
}

/// The usage row of `site`, locked `FOR UPDATE` (created if missing).
pub(crate) async fn locked_usage(
    conn: &mut PgConnection,
    site: NodeId,
) -> Result<SiteUsage, sqlx::Error> {
    lock_usage(&mut *conn, &[site]).await?;
    let (used, reserved, quota): (i64, i64, Option<i64>) =
        sqlx::query_as("select used, reserved, quota from site_usage where site_id = $1")
            .bind(site.0)
            .fetch_one(&mut *conn)
            .await?;
    Ok(SiteUsage {
        used: bytes_of(used),
        reserved: bytes_of(reserved),
        quota: quota.map(bytes_of),
    })
}

/// Adds `delta` (which may be negative) to the reserved bytes of `site`, never below zero.
pub(crate) async fn change_reserved(
    conn: &mut PgConnection,
    site: NodeId,
    delta: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query("update site_usage set reserved = greatest(reserved + $2, 0) where site_id = $1")
        .bind(site.0)
        .bind(delta)
        .execute(conn)
        .await?;
    Ok(())
}

/// What a tree change does to sites' usage: the documents it purges (whose versions are given
/// back to their site) and the documents it moves to another site (whose versions' sizes go
/// with them). Worked out by [`usage_changes`] once the rows are locked, applied by
/// [`apply_usage_changes`] with the change.
#[derive(Clone, Debug, Default)]
pub(crate) struct UsageChanges {
    /// By site, the nodes it purges that were not purged before.
    freed: BTreeMap<NodeId, Vec<Uuid>>,
    /// By (site before, site after), the nodes it moves between sites.
    moved: BTreeMap<(NodeId, NodeId), Vec<Uuid>>,
}

/// Works out the usage `rows` change (see [`UsageChanges`]) and locks the usage rows of every
/// site involved, by site id. The caller holds the node rows `FOR UPDATE`.
pub(crate) async fn usage_changes(
    conn: &mut PgConnection,
    rows: &[NodeRow],
) -> Result<UsageChanges, sqlx::Error> {
    let ids: Vec<Uuid> = rows.iter().map(|row| row.id.0).collect();
    let before: Vec<(Uuid, Uuid, bool)> =
        sqlx::query_as("select id, site_id, purged_at is not null from nodes where id = any($1)")
            .bind(&ids)
            .fetch_all(&mut *conn)
            .await?;
    let before: BTreeMap<Uuid, (NodeId, bool)> = before
        .into_iter()
        .map(|(id, site, purged)| (id, (NodeId(site), purged)))
        .collect();
    let mut changes = UsageChanges::default();
    let mut sites = BTreeSet::new();
    for row in rows {
        let Some((site, was_purged)) = before.get(&row.id.0).copied() else {
            continue;
        };
        if row.purged_at.is_some() {
            if !was_purged {
                changes.freed.entry(site).or_default().push(row.id.0);
                sites.insert(site);
            }
        } else if !was_purged && row.site != site {
            changes
                .moved
                .entry((site, row.site))
                .or_default()
                .push(row.id.0);
            sites.insert(site);
            sites.insert(row.site);
        }
    }
    let sites: Vec<NodeId> = sites.into_iter().collect();
    lock_usage(&mut *conn, &sites).await?;
    Ok(changes)
}

/// The sum of the sizes of every recorded version of the documents `ids`.
async fn versions_size(conn: &mut PgConnection, ids: &[Uuid]) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "select coalesce(sum(size), 0)::bigint from revisions where document_id = any($1)",
    )
    .bind(ids)
    .fetch_one(conn)
    .await
}

/// Applies `changes` to the usage rows [`usage_changes`] locked.
pub(crate) async fn apply_usage_changes(
    conn: &mut PgConnection,
    changes: &UsageChanges,
) -> Result<(), sqlx::Error> {
    let add = |site: NodeId, delta: i64| {
        sqlx::query("update site_usage set used = greatest(used + $2, 0) where site_id = $1")
            .bind(site.0)
            .bind(delta)
    };
    for (site, ids) in &changes.freed {
        let bytes = versions_size(&mut *conn, ids).await?;
        add(*site, -bytes).execute(&mut *conn).await?;
    }
    for ((from, to), ids) in &changes.moved {
        let bytes = versions_size(&mut *conn, ids).await?;
        add(*from, -bytes).execute(&mut *conn).await?;
        add(*to, bytes).execute(&mut *conn).await?;
    }
    Ok(())
}

impl Db {
    /// The storage of `site`: nothing used, reserved or limited if it has no row yet.
    pub async fn site_usage(&self, site: NodeId) -> Result<SiteUsage, DbError> {
        let row: Option<(i64, i64, Option<i64>)> =
            sqlx::query_as("select used, reserved, quota from site_usage where site_id = $1")
                .bind(site.0)
                .fetch_optional(self.pool())
                .await?;
        Ok(
            row.map_or_else(SiteUsage::default, |(used, reserved, quota)| SiteUsage {
                used: bytes_of(used),
                reserved: bytes_of(reserved),
                quota: quota.map(bytes_of),
            }),
        )
    }

    /// Sets the quota of `site` to `quota` bytes (`None`: no limit), for `actor`, who must be an
    /// enabled administrator (`Forbidden`), recording `quota.changed`. `NotFound` unless `site`
    /// is a live site. Answers the quota before. A quota below what the site holds is allowed:
    /// the site then takes nothing more until space is freed.
    pub async fn set_site_quota(
        &self,
        actor: UserId,
        site: NodeId,
        quota: Option<u64>,
    ) -> Result<Option<u64>, AccessError> {
        let mut tx = self.pool().begin().await?;
        let is_site: Option<bool> = sqlx::query_scalar(
            "select kind = 'site' and deleted_at is null from nodes where id = $1 for share",
        )
        .bind(site.0)
        .fetch_optional(&mut *tx)
        .await?;
        if is_site != Some(true) {
            return Err(AccessError::NotFound);
        }
        let admin: Option<bool> = sqlx::query_scalar(
            "select is_admin and disabled_at is null from users where id = $1 for share",
        )
        .bind(actor.0)
        .fetch_optional(&mut *tx)
        .await?;
        if admin != Some(true) {
            return Err(AccessError::Forbidden);
        }
        let before = locked_usage(&mut tx, site).await?;
        sqlx::query("update site_usage set quota = $2 where site_id = $1")
            .bind(site.0)
            .bind(quota.map(column))
            .execute(&mut *tx)
            .await?;
        record(
            &mut tx,
            NewEvent {
                kind: "quota.changed",
                actor: Some(actor),
                subject: None,
                document: None,
                node: Some(site),
                detail: json!({ "quota": quota, "previous": before.quota }),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(before.quota)
    }

    /// Reserves `bytes` in `site`: refused (`Exceeded`), changing nothing, if what the site
    /// uses and has reserved, with them, would go over its quota. The check and the
    /// reservation are one transaction with the site's row locked, so reservations made at
    /// once never go over together.
    pub async fn reserve_bytes(&self, site: NodeId, bytes: u64) -> Result<Reservation, QuotaError> {
        let mut tx = self.pool().begin().await?;
        let usage = locked_usage(&mut tx, site).await?;
        if !fits(usage.used, usage.reserved, usage.quota, bytes) {
            return Err(QuotaError::Exceeded);
        }
        change_reserved(&mut tx, site, column(bytes)).await?;
        tx.commit().await?;
        Ok(Reservation { site, bytes })
    }

    /// Gives back `reservation`: what it set aside was not stored.
    pub async fn release_reservation(&self, reservation: Reservation) -> Result<(), DbError> {
        let mut conn = self.pool().acquire().await?;
        change_reserved(&mut conn, reservation.site, -column(reservation.bytes)).await?;
        Ok(())
    }

    /// Turns `reservation` into usage, in one statement: what it set aside was stored.
    pub async fn commit_reservation(&self, reservation: Reservation) -> Result<(), DbError> {
        let bytes = column(reservation.bytes);
        sqlx::query(
            "update site_usage set reserved = greatest(reserved - $2, 0), used = used + $2 \
             where site_id = $1",
        )
        .bind(reservation.site.0)
        .bind(bytes)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Adds `bytes` to the usage of the site holding `doc`, whatever its quota: a version of
    /// `doc` was just committed. Nothing for a document with no node.
    pub async fn add_usage(&self, doc: DocId, bytes: u64) -> Result<(), DbError> {
        sqlx::query(
            "insert into site_usage (site_id, used) \
             select site_id, $2 from nodes where id = $1 \
             on conflict (site_id) do update set used = site_usage.used + excluded.used",
        )
        .bind(doc.0)
        .bind(column(bytes))
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Drops every reservation but those of the open upload sessions, which are set aside again:
    /// at a start, no save, upload or copy is under way.
    pub async fn reset_reservations(&self) -> Result<(), DbError> {
        sqlx::query(
            "update site_usage u set reserved = coalesce( \
                 (select sum(s.size) from upload_sessions s where s.site_id = u.site_id), 0)",
        )
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Whether a flag asks for every site's usage to be rebuilt.
    pub async fn usage_rebuild_flagged(&self) -> Result<bool, DbError> {
        Ok(
            sqlx::query_scalar("select exists (select 1 from usage_flags)")
                .fetch_one(self.pool())
                .await?,
        )
    }

    /// Asks the next start to rebuild every site's usage: a change of usage could not be
    /// written.
    pub async fn flag_usage_rebuild(&self) -> Result<(), DbError> {
        sqlx::query("insert into usage_flags (name) values ($1) on conflict do nothing")
            .bind(USAGE_DIRTY)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// The documents with a recorded version whose size is not known (recorded before sizes
    /// were kept), for the rebuild to read from Git.
    pub async fn revision_sizes_missing(&self) -> Result<Vec<DocId>, DbError> {
        let ids: Vec<Uuid> = sqlx::query_scalar(
            "select distinct document_id from revisions where size is null order by 1",
        )
        .fetch_all(self.pool())
        .await?;
        Ok(ids.into_iter().map(DocId).collect())
    }

    /// Records the sizes of `doc`'s versions, `(revision, size)`, as Git describes them.
    pub async fn set_revision_sizes(
        &self,
        doc: DocId,
        sizes: &[(String, u64)],
    ) -> Result<(), DbError> {
        let mut tx = self.pool().begin().await?;
        for (revision, size) in sizes {
            sqlx::query("update revisions set size = $3 where document_id = $1 and revision = $2")
                .bind(doc.0)
                .bind(revision)
                .bind(column(*size))
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Sets every site's usage to the sum of the recorded sizes of every version of every
    /// item in it that is not purged (live or in its bins), in one transaction; a version
    /// whose size is not known counts nothing. With `complete` (every size was known), the
    /// flags asking for this are cleared.
    pub async fn rebuild_usage(&self, complete: bool) -> Result<(), DbError> {
        let mut tx = self.pool().begin().await?;
        sqlx::query(
            "insert into site_usage (site_id) select id from nodes where kind = 'site' \
             order by id on conflict do nothing",
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query("select site_id from site_usage order by site_id for update")
            .fetch_all(&mut *tx)
            .await?;
        sqlx::query(
            "update site_usage u set used = coalesce( \
                 (select sum(r.size) from revisions r join nodes n on n.id = r.document_id \
                  where n.site_id = u.site_id and n.purged_at is null), 0)",
        )
        .execute(&mut *tx)
        .await?;
        if complete {
            sqlx::query("delete from usage_flags where name in ($1, $2)")
                .bind(USAGE_UPGRADE)
                .bind(USAGE_DIRTY)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}
