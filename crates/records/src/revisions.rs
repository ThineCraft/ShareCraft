//! The revision index: one row per Git revision of each document, in order. Git holds the
//! revisions themselves; these rows can always be rebuilt from it.

use collab_core::ids::DocId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::types::Json;
use sqlx::{PgConnection, Row};
use time::OffsetDateTime;

use crate::{Db, DbError};

/// One revision as Git describes it, to be recorded.
#[derive(Clone, Debug, PartialEq)]
pub struct RevisionRecord {
    pub doc: DocId,
    /// The Git commit id.
    pub revision: String,
    pub committed_at: OffsetDateTime,
    /// `(id, name)` of everyone credited, the commit author first.
    pub authors: Vec<(String, String)>,
    /// The document's file name at this revision.
    pub name: String,
    /// The document's fields at this revision.
    pub fields: Value,
    /// The size of the document's content at this revision, in bytes.
    pub size: u64,
}

/// One recorded revision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevisionRow {
    pub revision: String,
    /// 1 for the document's first revision, then counting up without gaps.
    pub position: i32,
    /// The document's previous revision.
    pub parent: Option<String>,
    pub committed_at: OffsetDateTime,
    /// `(id, name)` of everyone credited, the commit author first.
    pub authors: Vec<(String, String)>,
}

/// How an author is stored in the `authors` column.
#[derive(Serialize, Deserialize)]
struct StoredAuthor {
    id: String,
    name: String,
}

fn stored_authors(authors: &[(String, String)]) -> Json<Vec<StoredAuthor>> {
    Json(
        authors
            .iter()
            .map(|(id, name)| StoredAuthor {
                id: id.clone(),
                name: name.clone(),
            })
            .collect(),
    )
}

/// Creates `doc`'s record if it has none, with no creator and no grants, then locks it, so
/// writers of one document's revisions take turns. The record is named after the document's
/// node if it has one, otherwise `name`.
async fn lock_document(conn: &mut PgConnection, doc: DocId, name: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "insert into documents (id, name) \
         values ($1, coalesce((select name from nodes where id = $1 and kind = 'document'), $2)) \
         on conflict (id) do nothing",
    )
    .bind(doc.0)
    .bind(name)
    .execute(&mut *conn)
    .await?;
    sqlx::query("select id from documents where id = $1 for update")
        .bind(doc.0)
        .fetch_one(&mut *conn)
        .await?;
    Ok(())
}

async fn insert(
    conn: &mut PgConnection,
    r: &RevisionRecord,
    position: i32,
    parent: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "insert into revisions \
             (revision, document_id, position, parent, committed_at, authors, size) \
         values ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(&r.revision)
    .bind(r.doc.0)
    .bind(position)
    .bind(parent)
    .bind(r.committed_at)
    .bind(stored_authors(&r.authors))
    .bind(i64::try_from(r.size).unwrap_or(i64::MAX))
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Makes the document's record describe `newest`, its latest revision. Its name is the name of
/// the document's node if it has one (the tree is where a document is renamed, and a rename
/// writes no revision), otherwise the name in `newest`.
async fn set_current(conn: &mut PgConnection, newest: &RevisionRecord) -> Result<(), sqlx::Error> {
    sqlx::query(
        "update documents set revision = $2, revised_at = $3, \
             name = coalesce((select name from nodes where id = $1 and kind = 'document'), $4), \
             fields = $5 \
         where id = $1",
    )
    .bind(newest.doc.0)
    .bind(&newest.revision)
    .bind(newest.committed_at)
    .bind(&newest.name)
    .bind(&newest.fields)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

impl Db {
    /// Records `r` as the revision after `parent`, the document's previous one (`None` for its
    /// first). Returns `false`, changing nothing, if `parent` is not the latest revision
    /// recorded for the document. Creates the document's record, with no creator and no
    /// grants, if it has none; then sets its revision, time, name and fields from `r`.
    pub async fn append_revision(
        &self,
        parent: Option<&str>,
        r: &RevisionRecord,
    ) -> Result<bool, DbError> {
        let mut tx = self.pool().begin().await?;
        lock_document(&mut tx, r.doc, &r.name).await?;
        let head = sqlx::query(
            "select revision, position from revisions where document_id = $1 \
             order by position desc limit 1",
        )
        .bind(r.doc.0)
        .fetch_optional(&mut *tx)
        .await?;
        let (head, position) = match head {
            Some(row) => (
                Some(row.try_get::<String, _>("revision")?),
                row.try_get::<i32, _>("position")?,
            ),
            None => (None, 0),
        };
        if head.as_deref() != parent {
            // Dropping the transaction rolls back the record it may have created.
            return Ok(false);
        }
        let position = position
            .checked_add(1)
            .ok_or_else(|| DbError::new("too many revisions"))?;
        insert(&mut tx, r, position, parent).await?;
        set_current(&mut tx, r).await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Replaces `doc`'s recorded revisions with `history`, oldest first, in one transaction.
    /// Creates the document's record, with no creator and no grants, if it has none; then sets
    /// its revision, time, name and fields from the newest revision. An empty `history` clears
    /// the list and the document's current revision.
    ///
    /// Fails, changing nothing, if a revision belongs to another document or cannot be stored.
    pub async fn replace_revisions(
        &self,
        doc: DocId,
        history: &[RevisionRecord],
    ) -> Result<(), DbError> {
        if history.iter().any(|r| r.doc != doc) {
            return Err(DbError::new("a revision of another document was given"));
        }
        let mut tx = self.pool().begin().await?;
        match history.last() {
            Some(newest) => lock_document(&mut tx, doc, &newest.name).await?,
            None => {
                sqlx::query("select id from documents where id = $1 for update")
                    .bind(doc.0)
                    .fetch_optional(&mut *tx)
                    .await?;
            }
        }
        sqlx::query("delete from revisions where document_id = $1")
            .bind(doc.0)
            .execute(&mut *tx)
            .await?;
        let mut parent: Option<&str> = None;
        for (position, r) in (1..).zip(history) {
            insert(&mut tx, r, position, parent).await?;
            parent = Some(&r.revision);
        }
        match history.last() {
            Some(newest) => set_current(&mut tx, newest).await?,
            None => {
                sqlx::query(
                    "update documents set revision = null, revised_at = null where id = $1",
                )
                .bind(doc.0)
                .execute(&mut *tx)
                .await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    /// `doc`'s recorded revisions, oldest first.
    pub async fn revisions(&self, doc: DocId) -> Result<Vec<RevisionRow>, DbError> {
        let rows = sqlx::query(
            "select revision, position, parent, committed_at, authors from revisions \
             where document_id = $1 order by position",
        )
        .bind(doc.0)
        .fetch_all(self.pool())
        .await?;
        rows.iter()
            .map(|row| {
                let Json(authors): Json<Vec<StoredAuthor>> = row.try_get("authors")?;
                Ok(RevisionRow {
                    revision: row.try_get("revision")?,
                    position: row.try_get("position")?,
                    parent: row.try_get("parent")?,
                    committed_at: row.try_get("committed_at")?,
                    authors: authors.into_iter().map(|a| (a.id, a.name)).collect(),
                })
            })
            .collect::<Result<_, sqlx::Error>>()
            .map_err(DbError::from)
    }
}
