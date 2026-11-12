//! Adopting documents recorded before the content tree: placing them in the tree and moving
//! their grants onto it.

use collab_core::ids::NodeId;
use serde_json::json;
use sqlx::Row;
use time::OffsetDateTime;

use crate::documents::document_from_row;
use crate::events::{NewEvent, record};
use crate::groups::ensure_site_groups;
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
    /// Grants moved from `document_access` to `assignments`.
    pub grants: u64,
    /// Site groups made for sites that did not have all three.
    pub groups: u64,
}

impl Db {
    /// Whether anything is left to adopt: a document with no node, a grant still in
    /// `document_access`, or a site, deleted or not, without its three groups.
    pub async fn adoption_pending(&self) -> Result<bool, DbError> {
        let pending: bool = sqlx::query_scalar(
            "select exists (select 1 from documents d \
                            where not exists (select 1 from nodes n where n.id = d.id)) \
                 or exists (select 1 from document_access) \
                 or exists (select 1 from nodes s \
                            where s.parent_id is null \
                              and (select count(*) from groups g where g.site_id = s.id) < 3)",
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
    /// first; their names are copied into `documents.name`); gives every site, deleted or not,
    /// the groups it does not have yet, with no members (administrators fill them); makes every
    /// placed document with grants in `document_access` a scope,
    /// copies those grants to `assignments` (keeping any already there) and deletes them; and
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
        let lacking: Vec<uuid::Uuid> = sqlx::query_scalar(
            "select s.id from nodes s where s.parent_id is null \
               and (select count(*) from groups g where g.site_id = s.id) < 3 order by s.id",
        )
        .fetch_all(&mut *tx)
        .await?;
        let mut groups = 0u64;
        for id in lacking {
            groups = groups.saturating_add(ensure_site_groups(&mut tx, NodeId(id)).await?);
        }
        let scopes = sqlx::query(
            "update nodes set unique_scope = true \
             where kind = 'document' and not unique_scope \
               and id in (select document_id from document_access)",
        )
        .execute(&mut *tx)
        .await?
        .rows_affected();
        let grants = sqlx::query(
            "insert into assignments \
                 (node_id, principal_kind, principal_id, role_id, granted_by, granted_at) \
             select da.document_id, 'user', da.user_id, da.role, da.granted_by, da.granted_at \
             from document_access da join nodes n on n.id = da.document_id \
             on conflict (node_id, principal_kind, principal_id) do nothing",
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
                    "groups": groups,
                }),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(Adoption {
            scopes,
            grants,
            groups,
        })
    }
}
