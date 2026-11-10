//! The event log: who did what, when. Events never hold document text, passwords or tokens.

use collab_core::ids::{DocId, UserId};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{Db, DbError};

/// One entry in the event log.
#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    pub id: i64,
    pub at: OffsetDateTime,
    /// What happened, such as `user.created`.
    pub kind: String,
    /// Who did it; `None` for the server or the command line.
    pub actor: Option<UserId>,
    /// The account acted on, if any.
    pub subject: Option<UserId>,
    pub document: Option<DocId>,
    pub detail: serde_json::Value,
}

/// An event to write.
pub(crate) struct NewEvent<'a> {
    pub kind: &'a str,
    pub actor: Option<UserId>,
    pub subject: Option<UserId>,
    pub document: Option<DocId>,
    pub detail: serde_json::Value,
}

/// Writes `event` on `conn`, normally a transaction, so the event commits with the change.
pub(crate) async fn record(
    conn: &mut PgConnection,
    event: NewEvent<'_>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "insert into events (kind, actor, subject, document_id, detail) \
         values ($1, $2, $3, $4, $5)",
    )
    .bind(event.kind)
    .bind(event.actor.map(|u| u.0))
    .bind(event.subject.map(|u| u.0))
    .bind(event.document.map(|d| d.0))
    .bind(event.detail)
    .execute(conn)
    .await?;
    Ok(())
}

impl Db {
    /// The events whose subject is `subject`, oldest first.
    pub async fn events_about(&self, subject: UserId) -> Result<Vec<Event>, DbError> {
        let rows = sqlx::query(
            "select id, at, kind, actor, subject, document_id, detail \
             from events where subject = $1 order by id",
        )
        .bind(subject.0)
        .fetch_all(self.pool())
        .await?;
        rows.iter()
            .map(event_from_row)
            .collect::<Result<_, sqlx::Error>>()
            .map_err(DbError::from)
    }

    /// The events about document `doc`, oldest first.
    pub async fn events_for(&self, doc: DocId) -> Result<Vec<Event>, DbError> {
        let rows = sqlx::query(
            "select id, at, kind, actor, subject, document_id, detail \
             from events where document_id = $1 order by id",
        )
        .bind(doc.0)
        .fetch_all(self.pool())
        .await?;
        rows.iter()
            .map(event_from_row)
            .collect::<Result<_, sqlx::Error>>()
            .map_err(DbError::from)
    }
}

fn event_from_row(row: &PgRow) -> Result<Event, sqlx::Error> {
    Ok(Event {
        id: row.try_get("id")?,
        at: row.try_get("at")?,
        kind: row.try_get("kind")?,
        actor: row.try_get::<Option<Uuid>, _>("actor")?.map(UserId),
        subject: row.try_get::<Option<Uuid>, _>("subject")?.map(UserId),
        document: row.try_get::<Option<Uuid>, _>("document_id")?.map(DocId),
        detail: row.try_get("detail")?,
    })
}
