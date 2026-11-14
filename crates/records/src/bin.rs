//! The two-stage recycle bin, read from the nodes PostgreSQL mirrors from Git.
//!
//! Deleting an item marks it and every live item below it deleted, each naming the item that
//! was deleted (`deleted_with`); that item is the bin's entry and carries its stage. An entry in
//! the first stage is seen, restored and moved on to the second stage by whoever could delete
//! it were it live ([`Db::access_as_if_live`]); the second stage is seen, restored from and
//! purged by people with `manage_site` on the site. An entry is purged [`BIN_DAYS`] days after
//! it was deleted, or when it is removed from the second stage; a purged node stays as a
//! tombstone, never listed and never reachable.

use collab_core::ids::{NodeId, UserId};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use crate::documents::parse_editor;
use crate::explorer::UserRef;
use crate::scopes::access_with;
use crate::tree::NodeKind;
use crate::{Access, AccessError, Db, DbError, Editor, Perms};

/// How many days an item stays in the recycle bin, both stages together, before it is purged.
pub const BIN_DAYS: i64 = 93;

/// A stage of the recycle bin.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BinStage {
    /// Where deleted items go; seen by whoever could restore them.
    First,
    /// Where items removed from the first stage go; seen only by the site's managers.
    Second,
}

impl BinStage {
    /// The stage as `nodes.bin_stage` stores it: 1 or 2.
    #[must_use]
    pub fn number(self) -> i16 {
        match self {
            BinStage::First => 1,
            BinStage::Second => 2,
        }
    }

    /// The stage stored as `number`, if it is one.
    #[must_use]
    pub fn from_number(number: i16) -> Option<BinStage> {
        match number {
            1 => Some(BinStage::First),
            2 => Some(BinStage::Second),
            _ => None,
        }
    }
}

/// One entry of a site's recycle bin: an item a person deleted, with everything deleted with
/// it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BinEntry {
    pub id: NodeId,
    pub kind: NodeKind,
    pub name: String,
    /// For a file, the editor that opens it.
    pub editor: Option<Editor>,
    /// The names of the nodes it was in, from the site down to the folder that held it.
    pub path: Vec<String>,
    /// The sum of the sizes of the stored versions of every file deleted with it, as the
    /// content store's rows know them (versions stored inside Git before the content store
    /// are not counted).
    pub size: i64,
    /// Who deleted it; `None` if that person is not known.
    pub deleted_by: Option<UserRef>,
    pub deleted_at: OffsetDateTime,
    pub stage: BinStage,
    /// When it will be purged: [`BIN_DAYS`] days after it was deleted.
    pub purge_at: OffsetDateTime,
}

/// Where a page of a bin ended: the listing, newest deletion first, continues after it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BinCursor {
    /// When the last entry listed was deleted, in nanoseconds since 1970.
    pub deleted_at: i128,
    pub id: NodeId,
}

/// A recycle bin entry's state, as PostgreSQL mirrors it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BinState {
    pub site: NodeId,
    pub stage: BinStage,
    pub deleted_at: OffsetDateTime,
}

/// How many candidate entries a listing reads at a time while filling a page.
const BATCH: i64 = 200;

impl Db {
    /// What `user` could do with `node` if it were live: the one access decision (see
    /// [`Db::effective_access`]) with the deleted flags of `node`, of what was deleted with it and
    /// of every folder above it ignored, and a purged folder above it passed over (its items are
    /// restored to the top of their library, so they are judged there; when what is passed over
    /// had permissions of its own, only people with `manage_site` on the site have any). The
    /// same as
    /// [`Db::effective_access`] for a live node. `None` for a purged node, a missing one, or a
    /// disabled or unknown account.
    pub async fn access_as_if_live(
        &self,
        user: UserId,
        node: NodeId,
    ) -> Result<Option<Access>, DbError> {
        let mut conn = self.pool().acquire().await?;
        Ok(access_with(&mut conn, user, node, true).await?)
    }

    /// The entry `node` of the recycle bin: `None` unless it is the item a person deleted, not
    /// purged.
    pub async fn bin_state(&self, node: NodeId) -> Result<Option<BinState>, DbError> {
        let row: Option<(Uuid, Option<i16>, OffsetDateTime)> = sqlx::query_as(
            "select site_id, bin_stage, deleted_at from nodes \
             where id = $1 and deleted_with = id and purged_at is null",
        )
        .bind(node.0)
        .fetch_optional(self.pool())
        .await?;
        Ok(row.map(|(site, stage, deleted_at)| BinState {
            site: NodeId(site),
            stage: stage
                .and_then(BinStage::from_number)
                .unwrap_or(BinStage::First),
            deleted_at,
        }))
    }

    /// One page of the stage `stage` of `site`'s recycle bin as `user` sees it, newest deletion
    /// first, after `after`, at most `limit` entries, and where the next page starts (`None` on
    /// the last). The first stage lists the entries `user` could restore: those they may delete
    /// as if live (see [`Db::access_as_if_live`]), so nobody sees an entry whose name they could
    /// not see when it was live. The second stage lists every entry, to people with
    /// `manage_site` on the site only.
    ///
    /// # Errors
    /// `NotFound` if `site` is not a live site; `Forbidden` for the second stage without
    /// `manage_site` on it; otherwise the database error. Someone whose only access was in
    /// what was deleted still sees what they could restore, though they no longer reach the
    /// site.
    pub async fn bin(
        &self,
        user: UserId,
        site: NodeId,
        stage: BinStage,
        after: Option<&BinCursor>,
        limit: u32,
    ) -> Result<(Vec<BinEntry>, Option<BinCursor>), AccessError> {
        let found = self.node(site).await?;
        if !found.is_some_and(|n| n.kind == NodeKind::Site && !n.deleted) {
            return Err(AccessError::NotFound);
        }
        if stage == BinStage::Second {
            let access = self.effective_access(user, site).await?;
            if !access.is_some_and(|a| a.perms.contains(Perms::MANAGE_SITE)) {
                return Err(AccessError::Forbidden);
            }
        }
        let limit = usize::try_from(limit).unwrap_or(usize::MAX);
        let mut entries = Vec::new();
        let mut from = after.cloned();
        loop {
            let batch = self.bin_candidates(site, stage, from.as_ref()).await?;
            let exhausted = batch.len() < usize::try_from(BATCH).unwrap_or(usize::MAX);
            for entry in batch {
                from = Some(BinCursor {
                    deleted_at: entry.deleted_at.unix_timestamp_nanos(),
                    id: entry.id,
                });
                let shown = match stage {
                    BinStage::Second => true,
                    BinStage::First => self
                        .access_as_if_live(user, entry.id)
                        .await?
                        .is_some_and(|a| a.perms.contains(Perms::DELETE)),
                };
                if !shown {
                    continue;
                }
                if entries.len() == limit {
                    // One more exists: the page ends at the entry before it.
                    let last: &BinEntry = entries.last().unwrap_or(&entry);
                    let next = BinCursor {
                        deleted_at: last.deleted_at.unix_timestamp_nanos(),
                        id: last.id,
                    };
                    return Ok((entries, Some(next)));
                }
                entries.push(entry);
            }
            if exhausted {
                return Ok((entries, None));
            }
        }
    }

    /// The next [`BATCH`] entries of a stage of `site`'s bin after `after`, newest first, with
    /// their paths and sizes, whoever may see them.
    async fn bin_candidates(
        &self,
        site: NodeId,
        stage: BinStage,
        after: Option<&BinCursor>,
    ) -> Result<Vec<BinEntry>, DbError> {
        let (at, id) = match after {
            Some(cursor) => (
                Some(
                    OffsetDateTime::from_unix_timestamp_nanos(cursor.deleted_at)
                        .map_err(DbError::new)?,
                ),
                Some(cursor.id.0),
            ),
            None => (None, None),
        };
        let rows = sqlx::query(
            "select n.id, n.kind, n.name, n.deleted_at, n.deleted_by, u.name as deleted_by_name, \
                    d.editor, \
                    (select coalesce(array_agg(a.name order by a.depth desc), '{}') from ( \
                         with recursive up(id, parent_id, name, depth) as ( \
                             select p.id, p.parent_id, p.name, 1 from nodes p \
                             where p.id = n.parent_id \
                             union all \
                             select q.id, q.parent_id, q.name, up.depth + 1 \
                             from nodes q join up on q.id = up.parent_id \
                             where up.depth < 10000) \
                         select name, depth from up) a) as path, \
                    (select coalesce(sum(b.size), 0)::bigint from nodes m \
                         join blob_refs r on r.doc_id = m.id and not r.pending \
                         join blobs b on b.sha256 = r.sha256 \
                     where m.deleted_with = n.id and m.purged_at is null) as size \
             from nodes n \
             left join users u on u.id = n.deleted_by \
             left join documents d on d.id = n.id and n.kind = 'document' \
             where n.site_id = $1 and n.deleted_with = n.id and n.purged_at is null \
               and coalesce(n.bin_stage, 1) = $2 \
               and ($3::timestamptz is null or (n.deleted_at, n.id) < ($3, $4)) \
             order by n.deleted_at desc, n.id desc \
             limit $5",
        )
        .bind(site.0)
        .bind(stage.number())
        .bind(at)
        .bind(id)
        .bind(BATCH)
        .fetch_all(self.pool())
        .await?;
        rows.iter()
            .map(|row| {
                let kind: String = row.try_get("kind")?;
                let deleted_at: OffsetDateTime = row.try_get("deleted_at")?;
                let deleted_by = match (
                    row.try_get::<Option<Uuid>, _>("deleted_by")?,
                    row.try_get::<Option<String>, _>("deleted_by_name")?,
                ) {
                    (Some(id), Some(name)) => Some(UserRef {
                        id: UserId(id),
                        name,
                    }),
                    _ => None,
                };
                Ok(BinEntry {
                    id: NodeId(row.try_get("id")?),
                    kind: NodeKind::parse(&kind).ok_or_else(|| {
                        sqlx::Error::Decode(format!("unknown node kind {kind:?}").into())
                    })?,
                    name: row.try_get("name")?,
                    editor: row
                        .try_get::<Option<String>, _>("editor")?
                        .as_deref()
                        .map(parse_editor)
                        .transpose()?,
                    path: row.try_get("path")?,
                    size: row.try_get("size")?,
                    deleted_by,
                    deleted_at,
                    stage,
                    purge_at: deleted_at + Duration::days(BIN_DAYS),
                })
            })
            .collect::<Result<_, sqlx::Error>>()
            .map_err(DbError::from)
    }

    /// The entries of every recycle bin due to be purged at `now`: deleted [`BIN_DAYS`] days or
    /// more before it, oldest first.
    pub async fn due_for_purge(&self, now: OffsetDateTime) -> Result<Vec<NodeId>, DbError> {
        let ids: Vec<Uuid> = sqlx::query_scalar(
            "select id from nodes \
             where deleted_with = id and purged_at is null and deleted_at <= $1 \
             order by deleted_at, id",
        )
        .bind(now - Duration::days(BIN_DAYS))
        .fetch_all(self.pool())
        .await?;
        Ok(ids.into_iter().map(NodeId).collect())
    }
}
