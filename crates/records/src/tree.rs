//! The content tree: sites, libraries, folders and files, mirrored from Git into PostgreSQL.

use collab_core::ids::{NodeId, UserId};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use sqlx::postgres::PgRow;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::documents::UNIQUE_VIOLATION;
use crate::events::{NewEvent, record};
use crate::{AccessError, Db, DbError};

const MAX_NAME_CHARS: usize = 120;
const REFUSED_CHARS: [char; 9] = ['"', '*', ':', '<', '>', '?', '/', '\\', '|'];

/// What a node of the tree is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeKind {
    Site,
    Library,
    Folder,
    Document,
}

impl NodeKind {
    fn as_str(self) -> &'static str {
        match self {
            NodeKind::Site => "site",
            NodeKind::Library => "library",
            NodeKind::Folder => "folder",
            NodeKind::Document => "document",
        }
    }

    fn parse(text: &str) -> Option<NodeKind> {
        match text {
            "site" => Some(NodeKind::Site),
            "library" => Some(NodeKind::Library),
            "folder" => Some(NodeKind::Folder),
            "document" => Some(NodeKind::Document),
            _ => None,
        }
    }
}

/// A node as PostgreSQL holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    pub id: NodeId,
    pub kind: NodeKind,
    /// `None` exactly for a site.
    pub parent: Option<NodeId>,
    /// The site the node is in; a site's own id for the site itself.
    pub site: NodeId,
    pub name: String,
    /// Whether the node is a permission scope: it has its own assignments rather than
    /// inheriting. Sites always are.
    pub unique_scope: bool,
    pub deleted: bool,
    pub created_by: Option<UserId>,
    pub created_at: OffsetDateTime,
}

/// A node as Git holds it, which the projection writes into PostgreSQL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeRow {
    pub id: NodeId,
    pub kind: NodeKind,
    pub parent: Option<NodeId>,
    pub site: NodeId,
    pub name: String,
    pub deleted_at: Option<OffsetDateTime>,
    pub deleted_by: Option<UserId>,
    pub created_by: Option<UserId>,
    pub created_at: OffsetDateTime,
}

/// Checks a name for any node: 1 to 120 characters after trimming, no control characters,
/// none of `" * : < > ? / \ |`, and no trailing dot. Returns the trimmed name.
pub fn validate_node_name(name: &str) -> Result<&str, AccessError> {
    let name = name.trim();
    let length = name.chars().count();
    let well_formed = (1..=MAX_NAME_CHARS).contains(&length)
        && !name.ends_with('.')
        && !name
            .chars()
            .any(|c| c.is_control() || REFUSED_CHARS.contains(&c));
    if well_formed {
        Ok(name)
    } else {
        Err(AccessError::InvalidName)
    }
}

pub(crate) fn node_from_row(row: &PgRow) -> Result<Node, sqlx::Error> {
    let kind: String = row.try_get("kind")?;
    Ok(Node {
        id: NodeId(row.try_get("id")?),
        kind: NodeKind::parse(&kind)
            .ok_or_else(|| sqlx::Error::Decode(format!("unknown node kind {kind:?}").into()))?,
        parent: row.try_get::<Option<Uuid>, _>("parent_id")?.map(NodeId),
        site: NodeId(row.try_get("site_id")?),
        name: row.try_get("name")?,
        unique_scope: row.try_get("unique_scope")?,
        deleted: row.try_get("deleted")?,
        created_by: row.try_get::<Option<Uuid>, _>("created_by")?.map(UserId),
        created_at: row.try_get("created_at")?,
    })
}

impl Db {
    /// Applies one node as Git holds it, in one transaction: inserts or updates the row (kind,
    /// parent, site, name, deleted), writes `event` if given, and for a document node updates
    /// the name of the `documents` row with that id, if there is one, so rooms, downloads and
    /// saves use the new name. A new site row is a scope; an update never changes
    /// `unique_scope`. A name already used by an undeleted sibling gives `NameTaken`.
    pub async fn apply_node(
        &self,
        row: &NodeRow,
        event: Option<NewEvent>,
    ) -> Result<(), AccessError> {
        let mut tx = self.pool().begin().await?;
        let result = sqlx::query(
            "insert into nodes \
                 (id, kind, parent_id, site_id, name, unique_scope, deleted_at, deleted_by, \
                  created_by, created_at) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
             on conflict (id) do update set \
                 kind = excluded.kind, parent_id = excluded.parent_id, \
                 site_id = excluded.site_id, name = excluded.name, \
                 deleted_at = excluded.deleted_at, deleted_by = excluded.deleted_by",
        )
        .bind(row.id.0)
        .bind(row.kind.as_str())
        .bind(row.parent.map(|p| p.0))
        .bind(row.site.0)
        .bind(&row.name)
        .bind(row.kind == NodeKind::Site)
        .bind(row.deleted_at)
        .bind(row.deleted_by.map(|u| u.0))
        .bind(row.created_by.map(|u| u.0))
        .bind(row.created_at)
        .execute(&mut *tx)
        .await;
        match result {
            Ok(_) => {}
            Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some(UNIQUE_VIOLATION) => {
                return Err(AccessError::NameTaken);
            }
            Err(e) => return Err(e.into()),
        }
        if row.kind == NodeKind::Document {
            sqlx::query("update documents set name = $2 where id = $1")
                .bind(row.id.0)
                .bind(&row.name)
                .execute(&mut *tx)
                .await?;
        }
        if let Some(event) = event {
            record(&mut tx, event).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// The node with this id, deleted or not, whoever may see it.
    pub async fn node(&self, id: NodeId) -> Result<Option<Node>, DbError> {
        let row = sqlx::query(
            "select id, kind, parent_id, site_id, name, unique_scope, \
                    (deleted_at is not null) as deleted, created_by, created_at \
             from nodes where id = $1",
        )
        .bind(id.0)
        .fetch_optional(self.pool())
        .await?;
        Ok(row.as_ref().map(node_from_row).transpose()?)
    }

    /// Whether `node` is `ancestor` or lies anywhere below it. False if `node` does not exist.
    /// The walk ends even if the data held a cycle.
    pub async fn is_at_or_below(&self, node: NodeId, ancestor: NodeId) -> Result<bool, DbError> {
        let found: bool = sqlx::query_scalar(
            "with recursive up(id, parent_id) as ( \
                 select id, parent_id from nodes where id = $1 \
                 union \
                 select n.id, n.parent_id from nodes n join up on n.id = up.parent_id \
             ) select exists (select 1 from up where id = $2)",
        )
        .bind(node.0)
        .bind(ancestor.0)
        .fetch_one(self.pool())
        .await?;
        Ok(found)
    }

    /// Whether `id` has any undeleted child.
    pub async fn has_live_children(&self, id: NodeId) -> Result<bool, DbError> {
        let found: bool = sqlx::query_scalar(
            "select exists (select 1 from nodes where parent_id = $1 and deleted_at is null)",
        )
        .bind(id.0)
        .fetch_one(self.pool())
        .await?;
        Ok(found)
    }

    /// Whether no undeleted child of `parent`, other than `except`, is named `name`, ignoring
    /// case.
    pub async fn name_free(
        &self,
        parent: NodeId,
        name: &str,
        except: Option<NodeId>,
    ) -> Result<bool, DbError> {
        let taken: bool = sqlx::query_scalar(
            "select exists (select 1 from nodes \
                 where parent_id = $1 and lower(name) = lower($2) and deleted_at is null \
                   and ($3::uuid is null or id <> $3))",
        )
        .bind(parent.0)
        .bind(name)
        .bind(except.map(|e| e.0))
        .fetch_one(self.pool())
        .await?;
        Ok(!taken)
    }
}
