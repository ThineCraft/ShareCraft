//! The event log: who did what, when. Events never hold document text, passwords or tokens.

use collab_core::ids::{DocId, NodeId, UserId};
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
#[derive(Clone, Debug, PartialEq)]
pub struct NewEvent {
    /// What happened, such as `editing.joined`.
    pub kind: &'static str,
    /// Who did it; `None` for the server or the command line.
    pub actor: Option<UserId>,
    /// The account acted on, if any.
    pub subject: Option<UserId>,
    pub document: Option<DocId>,
    /// The tree node the event is about, if any; a document's node is its document id.
    pub node: Option<NodeId>,
    /// Details of the event. Never document text, passwords or tokens.
    pub detail: serde_json::Value,
}

/// Writes `event` on `conn`, normally a transaction, so the event commits with the change.
pub(crate) async fn record(conn: &mut PgConnection, event: NewEvent) -> Result<(), sqlx::Error> {
    insert(conn, &event).await.map(drop)
}

/// As [`record`], answering the event's id.
pub(crate) async fn record_with_id(
    conn: &mut PgConnection,
    event: NewEvent,
) -> Result<i64, sqlx::Error> {
    insert(conn, &event).await
}

async fn insert(conn: &mut PgConnection, event: &NewEvent) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "insert into events (kind, actor, subject, document_id, node_id, detail) \
         values ($1, $2, $3, $4, $5, $6) returning id",
    )
    .bind(event.kind)
    .bind(event.actor.map(|u| u.0))
    .bind(event.subject.map(|u| u.0))
    .bind(event.document.map(|d| d.0))
    .bind(event.node.map(|n| n.0))
    .bind(&event.detail)
    .fetch_one(conn)
    .await
}

impl Db {
    /// Writes `event` on its own, outside any other change.
    pub async fn append_event(&self, event: &NewEvent) -> Result<(), DbError> {
        let mut conn = self.pool().acquire().await?;
        insert(&mut conn, event).await?;
        Ok(())
    }

    /// Adds `key` with `value` to the detail of the event `id`, recorded earlier: for a figure
    /// only known once the change has committed, such as the bytes a purge freed.
    pub async fn add_event_detail(
        &self,
        id: i64,
        key: &str,
        value: serde_json::Value,
    ) -> Result<(), DbError> {
        sqlx::query(
            "update events set detail = detail || jsonb_build_object($2::text, $3::jsonb) \
             where id = $1",
        )
        .bind(id)
        .bind(key)
        .bind(value)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// The latest `limit` events about document `doc`, oldest first.
    pub async fn recent_events_for(&self, doc: DocId, limit: i64) -> Result<Vec<Event>, DbError> {
        let rows = sqlx::query(
            "select id, at, kind, actor, subject, document_id, detail from ( \
                 select id, at, kind, actor, subject, document_id, detail \
                 from events where document_id = $1 order by id desc limit $2 \
             ) latest order by id",
        )
        .bind(doc.0)
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        rows.iter()
            .map(event_from_row)
            .collect::<Result<_, sqlx::Error>>()
            .map_err(DbError::from)
    }

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
