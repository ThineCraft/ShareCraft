//! Adopting documents recorded before the content tree: placing them in the tree and moving
//! their grants onto it.

use collab_core::ids::NodeId;
use serde_json::json;
use sqlx::Row;
use time::OffsetDateTime;

use crate::documents::document_from_row;
use crate::events::{NewEvent, record};
use crate::tree::upsert_node;
use crate::{AccessError, Db, DbError, Document, NodeRow};

/// A document recorded before the content tree that has no node yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unplaced {
    pub document: Document,
    pub created_at: OffsetDateTime,
}

/// What finishing an adoption did in PostgreSQL.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Adoption {
    /// Documents that became scopes because they had grants of their own.
    pub scopes: u64,
    /// Grants moved from `document_access` to `scope_access`.
    pub grants: u64,
    /// Owner assignments given to administrators on sites that had no owner.
    pub owners: u64,
}

impl Db {
    /// Whether anything is left to adopt: a document with no node, a grant still in
    /// `document_access`, or an undeleted site without an owner assignment.
    pub async fn adoption_pending(&self) -> Result<bool, DbError> {
        let pending: bool = sqlx::query_scalar(
            "select exists (select 1 from documents d \
                            where not exists (select 1 from nodes n where n.id = d.id)) \
                 or exists (select 1 from document_access) \
                 or exists (select 1 from nodes s \
                            where s.parent_id is null and s.deleted_at is null \
                              and not exists (select 1 from scope_access o \
                                              where o.node_id = s.id and o.role = 'owner'))",
        )
        .fetch_one(self.pool())
        .await?;
        Ok(pending)
    }

    /// The documents that have no node, oldest first.
    pub async fn unplaced_documents(&self) -> Result<Vec<Unplaced>, DbError> {
        let rows = sqlx::query(
            "select d.id, d.name, d.editor, d.created_by, d.revision, d.revised_at, d.created_at \
             from documents d where not exists (select 1 from nodes n where n.id = d.id) \
             order by d.created_at, d.id",
        )
        .fetch_all(self.pool())
        .await?;
        rows.iter()
            .map(|row| {
                Ok(Unplaced {
                    document: document_from_row(row)?,
                    created_at: row.try_get("created_at")?,
                })
            })
            .collect::<Result<_, sqlx::Error>>()
            .map_err(DbError::from)
    }

    /// Finishes an adoption in one transaction: applies `rows` (as Git holds them, parents
    /// first; their names are copied into `documents.name`); makes every enabled
    /// administrator owner of `site`, if given, and of every undeleted site with no owner
    /// assignment; makes every placed document with grants in `document_access` a scope,
    /// copies those grants to `scope_access` (keeping any already there) and deletes them; and
    /// records one `tree.adopted` event with the counts.
    pub async fn finish_adoption(
        &self,
        rows: &[NodeRow],
        site: Option<NodeId>,
        documents: usize,
        renamed: usize,
    ) -> Result<Adoption, AccessError> {
        let mut tx = self.pool().begin().await?;
        for row in rows {
            upsert_node(&mut tx, row).await?;
        }
        let owners = sqlx::query(
            "insert into scope_access (node_id, user_id, role) \
             select s.id, u.id, 'owner' from nodes s cross join users u \
             where s.parent_id is null and s.deleted_at is null \
               and u.is_admin and u.disabled_at is null \
               and (s.id = $1 or not exists (select 1 from scope_access o \
                                             where o.node_id = s.id and o.role = 'owner')) \
             on conflict (node_id, user_id) do update set role = 'owner'",
        )
        .bind(site.map(|s| s.0))
        .execute(&mut *tx)
        .await?
        .rows_affected();
        let scopes = sqlx::query(
            "update nodes set unique_scope = true \
             where kind = 'document' and not unique_scope \
               and id in (select document_id from document_access)",
        )
        .execute(&mut *tx)
        .await?
        .rows_affected();
        let grants = sqlx::query(
            "insert into scope_access (node_id, user_id, role, granted_by, granted_at) \
             select da.document_id, da.user_id, da.role, da.granted_by, da.granted_at \
             from document_access da join nodes n on n.id = da.document_id \
             on conflict (node_id, user_id) do nothing",
        )
        .execute(&mut *tx)
        .await?
        .rows_affected();
        sqlx::query(
            "delete from document_access da \
             where exists (select 1 from nodes n where n.id = da.document_id)",
        )
        .execute(&mut *tx)
        .await?;
        record(
            &mut tx,
            NewEvent {
                kind: "tree.adopted",
                actor: None,
                subject: None,
                document: None,
                node: site,
                detail: json!({
                    "site": site.map(|s| s.0),
                    "documents": documents,
                    "renamed": renamed,
                    "scopes": scopes,
                    "grants": grants,
                    "owners": owners,
                }),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(Adoption {
            scopes,
            grants,
            owners,
        })
    }
}
