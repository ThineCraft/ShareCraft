//! The live session log (plan G2): the live room's sessions, places and entries, and each
//! client's local numbers, so a restarted room carries on exactly where the last one stopped.
//!
//! Everything here is plain numbers and ids: `records` does not know the wire messages. A
//! change's content is stored and loaded byte for byte and never read.

use collab_core::ids::{ClientId, DocId, Seq, UserId};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::revisions::{insert as insert_revision, lock_document, recorded_head, set_current};
use crate::{Db, DbError, RevisionRecord};

pub(crate) async fn delete_live_logs(
    conn: &mut PgConnection,
    documents: &[DocId],
) -> Result<(), sqlx::Error> {
    if documents.is_empty() {
        return Ok(());
    }
    let ids: Vec<Uuid> = documents.iter().map(|document| document.0).collect();
    sqlx::query("delete from live_entries where document_id = any($1)")
        .bind(&ids)
        .execute(&mut *conn)
        .await?;
    sqlx::query("delete from live_refusals where document_id = any($1)")
        .bind(&ids)
        .execute(&mut *conn)
        .await?;
    sqlx::query("delete from live_checkpoints where document_id = any($1)")
        .bind(&ids)
        .execute(&mut *conn)
        .await?;
    sqlx::query("delete from live_return_tombstones where document_id = any($1)")
        .bind(&ids)
        .execute(&mut *conn)
        .await?;
    sqlx::query("delete from live_sessions where document_id = any($1)")
        .bind(&ids)
        .execute(&mut *conn)
        .await?;
    sqlx::query("delete from live_clients where document_id = any($1)")
        .bind(&ids)
        .execute(&mut *conn)
        .await?;
    sqlx::query("delete from live_logs where document_id = any($1)")
        .bind(&ids)
        .execute(conn)
        .await?;
    Ok(())
}

/// How long entries covered by a verified revision and final refusals are retained.
pub const LIVE_KEEP_DAYS: i64 = 30;

/// Rows removed by one live-log retention sweep.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LiveSwept {
    pub entries: u64,
    pub sessions: u64,
    pub clients: u64,
}

/// The most unverified checkpoints a document keeps: storing another removes the oldest, so
/// one editor cannot grow checkpoint storage without bound while nobody verifies.
pub const MAX_UNVERIFIED_CHECKPOINTS: usize = 4;

/// The size a cancel record counts against the unsaved log: its encoded size, about the bytes
/// of its fixed fields on the wire.
pub const LIVE_CANCEL_BYTES: u64 = 64;

/// One durable checkpoint of a live log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveCheckpointRow {
    pub revision: String,
    pub through: Seq,
    pub sha256: [u8; 32],
    pub verified: bool,
    pub session: Uuid,
    /// Opaque editor identity data associated with the checkpoint bytes.
    pub session_data: Vec<u8>,
    pub major_reason: Option<String>,
    pub major_person: Option<UserId>,
    /// Who uploaded the bytes, when a participant did.
    pub writer: Option<LiveCheckpointWriter>,
    /// Accepted as the base with no second capable editor to verify it, and not verified by
    /// one since.
    pub sole_editor: bool,
}

/// The place and client that uploaded a checkpoint's bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LiveCheckpointWriter {
    pub participant: u32,
    pub client: ClientId,
}

/// A checkpoint to associate with an existing revision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NewLiveCheckpoint<'a> {
    pub revision: &'a str,
    pub through: Seq,
    pub sha256: [u8; 32],
    pub session: Uuid,
    pub session_data: &'a [u8],
    pub major_reason: Option<&'a str>,
    pub major_person: Option<UserId>,
    /// Who uploaded the bytes, when a participant did.
    pub writer: Option<LiveCheckpointWriter>,
    pub created_at: OffsetDateTime,
}

/// The minimal durable identity of one place in an ended session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveReturnTombstoneRow {
    pub session: Uuid,
    pub participant: u32,
    pub client: ClientId,
    pub user: UserId,
    pub ended_at: OffsetDateTime,
    pub final_seq: Seq,
}

/// The newest retained identity table for one live session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveSessionDataRow {
    pub session: Uuid,
    pub through: Seq,
    /// Opaque editor identity data.
    pub session_data: Vec<u8>,
}

/// Why a place in a live session ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GoneReason {
    /// The person left.
    Left,
    /// Three heartbeats were missed.
    Expired,
    /// The person lost the right to open the document.
    Removed,
    /// Their editor could not open a change that stands.
    CannotOpen,
}

impl GoneReason {
    fn as_str(self) -> &'static str {
        match self {
            GoneReason::Left => "left",
            GoneReason::Expired => "expired",
            GoneReason::Removed => "removed",
            GoneReason::CannotOpen => "cannot_open",
        }
    }

    fn parse(s: &str) -> Result<GoneReason, DbError> {
        match s {
            "left" => Ok(GoneReason::Left),
            "expired" => Ok(GoneReason::Expired),
            "removed" => Ok(GoneReason::Removed),
            "cannot_open" => Ok(GoneReason::CannotOpen),
            other => Err(DbError::new(format!("unknown place end {other:?}"))),
        }
    }
}

/// A document's live log as stored, for a room starting up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveLoaded {
    /// The newest position ever given; 0 when none. The next entry is stored at `head + 1`.
    pub head: Seq,
    /// The newest participant number ever given; 0 when none.
    pub last_participant: u32,
    /// The format of the kept entries; `None` when none are kept.
    pub format: Option<String>,
    /// Entries at or below this position are covered by a verified revision.
    pub verified_through: Seq,
    /// The newest verified checkpoint, which is the catch-up base.
    pub verified_checkpoint: Option<LiveCheckpointRow>,
    /// Unverified checkpoints later than the catch-up base, oldest first.
    pub unverified_checkpoints: Vec<LiveCheckpointRow>,
    /// Every kept entry after `verified_through`, oldest first.
    pub entries: Vec<LiveEntryRow>,
    /// Kept entries after the verified base.
    pub unsaved_entries: u64,
    /// Payload bytes kept after the verified base.
    pub unsaved_bytes: u64,
    /// The open session, if any.
    pub open_session: Option<LiveSessionRow>,
    /// Every participant ever given a place in this document, including ended sessions.
    pub participants: Vec<LiveParticipantRow>,
    /// Every client of the document: its id, its person and its next local number (one past
    /// the highest number stored or finally refused).
    pub clients: Vec<(ClientId, UserId, u64)>,
    /// Retained identity data for current and ended sessions, oldest first.
    pub retained_session_data: Vec<LiveSessionDataRow>,
}

/// A live session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveSessionRow {
    pub id: Uuid,
    pub format: String,
    /// The first position the session could give: the head when it started, plus one.
    pub first_seq: Seq,
    pub started_at: OffsetDateTime,
    /// Every place in the session, held or ended, by number.
    pub participants: Vec<LiveParticipantRow>,
}

/// A place in a live session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveParticipantRow {
    /// From 1; never reused within a document.
    pub number: u32,
    pub client: ClientId,
    pub user: UserId,
    pub joined_at: OffsetDateTime,
    /// Why the place ended; `None` while it is held.
    pub gone: Option<GoneReason>,
}

/// One entry of the log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LiveEntryRow {
    /// A change from a client.
    Change {
        seq: Seq,
        session: Uuid,
        client: ClientId,
        local: u64,
        participant: u32,
        user: UserId,
        seen: Seq,
        at: OffsetDateTime,
        seed: u32,
        format: String,
        /// The content byte for byte; empty once cancelled.
        payload: Vec<u8>,
        witnesses: Vec<u32>,
        cancelled_by: Option<Seq>,
    },
    /// A cancel record for the change at `cancels`.
    Cancel {
        seq: Seq,
        session: Uuid,
        at: OffsetDateTime,
        seed: u32,
        cancels: Seq,
        reports: u32,
    },
}

/// A change to store at the head of the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NewLiveChange<'a> {
    /// Must be the head plus one.
    pub seq: Seq,
    pub session: Uuid,
    pub client: ClientId,
    pub local: u64,
    /// From 1.
    pub participant: u32,
    pub user: UserId,
    /// As the sender stated it.
    pub seen: Seq,
    pub at: OffsetDateTime,
    pub seed: u32,
    pub format: &'a str,
    pub payload: &'a [u8],
    pub witnesses: &'a [u32],
}

/// A cancel record to store at the head of the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NewLiveCancel {
    /// Must be the head plus one.
    pub seq: Seq,
    pub session: Uuid,
    pub cancels: Seq,
    pub reports: u32,
    pub at: OffsetDateTime,
    pub seed: u32,
}

/// What became of one of a client's local numbers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LiveLocal {
    /// Its change is stored at `seq`.
    Stored { seq: Seq },
    /// Its change was finally refused, for `reason` (as the wire names it).
    Refused { reason: String },
    /// Neither stored nor refused: never seen, or skipped by the client, or swept.
    Unseen,
}

fn signed(value: u64) -> Result<i64, DbError> {
    i64::try_from(value).map_err(|_| DbError::new(format!("{value} is out of range")))
}

fn unsigned(value: i64) -> Result<u64, DbError> {
    u64::try_from(value).map_err(|_| DbError::new(format!("{value} is out of range")))
}

fn number(value: u32) -> Result<i32, DbError> {
    i32::try_from(value).map_err(|_| DbError::new(format!("{value} is out of range")))
}

fn unnumber(value: i32) -> Result<u32, DbError> {
    u32::try_from(value).map_err(|_| DbError::new(format!("{value} is out of range")))
}

fn seed_of(value: i64) -> Result<u32, DbError> {
    u32::try_from(value).map_err(|_| DbError::new(format!("seed {value} is out of range")))
}

/// Creates `doc`'s log row if it has none.
async fn ensure_log(conn: &mut PgConnection, doc: DocId) -> Result<(), DbError> {
    sqlx::query("insert into live_logs (document_id) values ($1) on conflict do nothing")
        .bind(doc.0)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Locks `doc`'s log row, so that writers of one log take turns, and returns its head.
async fn lock_log(conn: &mut PgConnection, doc: DocId) -> Result<i64, DbError> {
    ensure_log(conn, doc).await?;
    Ok(
        sqlx::query_scalar("select head from live_logs where document_id = $1 for update")
            .bind(doc.0)
            .fetch_one(&mut *conn)
            .await?,
    )
}

/// The document of `session`, whose log row it creates if missing.
async fn session_doc(conn: &mut PgConnection, session: Uuid) -> Result<DocId, DbError> {
    let doc: Option<Uuid> =
        sqlx::query_scalar("select document_id from live_sessions where id = $1")
            .bind(session)
            .fetch_optional(&mut *conn)
            .await?;
    let doc = DocId(doc.ok_or_else(|| DbError::new(format!("no live session {session}")))?);
    ensure_log(conn, doc).await?;
    Ok(doc)
}

async fn local_disposition(
    conn: &mut PgConnection,
    doc: DocId,
    client: ClientId,
    local: i64,
) -> Result<LiveLocal, DbError> {
    let row = sqlx::query(
        "select (select seq from live_entries \
                 where document_id = $1 and client_id = $2 and local = $3 \
                   and kind = 'change') as seq, \
                (select reason from live_refusals \
                 where document_id = $1 and client_id = $2 and local = $3) as reason",
    )
    .bind(doc.0)
    .bind(client.0)
    .bind(local)
    .fetch_one(&mut *conn)
    .await?;
    let seq: Option<i64> = row.try_get("seq")?;
    let reason: Option<String> = row.try_get("reason")?;
    Ok(match (seq, reason) {
        (Some(seq), _) => LiveLocal::Stored {
            seq: Seq(unsigned(seq)?),
        },
        (None, Some(reason)) => LiveLocal::Refused { reason },
        (None, None) => LiveLocal::Unseen,
    })
}

/// Returns `client`'s next local number, checking that it belongs to `user`.
async fn next_local(
    conn: &mut PgConnection,
    doc: DocId,
    client: ClientId,
    user: UserId,
) -> Result<Option<i64>, DbError> {
    let row = sqlx::query(
        "select user_id, next_local from live_clients \
         where document_id = $1 and client_id = $2",
    )
    .bind(doc.0)
    .bind(client.0)
    .fetch_optional(&mut *conn)
    .await?;
    match row {
        Some(row) if row.try_get::<Uuid, _>("user_id")? != user.0 => Err(DbError::new(format!(
            "client {} belongs to another person",
            client.0
        ))),
        Some(row) => Ok(Some(row.try_get("next_local")?)),
        None => Ok(None),
    }
}

/// Moves `client`'s next local number to `local + 1`, recording it as `user`'s on first use.
/// The caller has established that the number is unseen and not behind `next_local`.
async fn use_local(
    conn: &mut PgConnection,
    doc: DocId,
    client: ClientId,
    user: UserId,
    local: u64,
) -> Result<(), DbError> {
    let next = signed(local)?
        .checked_add(1)
        .ok_or_else(|| DbError::new(format!("local {local} is out of range")))?;
    let done = sqlx::query(
        "insert into live_clients (document_id, client_id, user_id, next_local) \
         values ($1, $2, $3, $4) \
         on conflict (document_id, client_id) do update \
             set next_local = greatest(live_clients.next_local, excluded.next_local) \
             where live_clients.user_id = excluded.user_id",
    )
    .bind(doc.0)
    .bind(client.0)
    .bind(user.0)
    .bind(next)
    .execute(&mut *conn)
    .await?;
    if done.rows_affected() == 1 {
        Ok(())
    } else {
        Err(DbError::new(format!(
            "client {} belongs to another person",
            client.0
        )))
    }
}

fn entry_of(row: &PgRow) -> Result<LiveEntryRow, DbError> {
    let kind: String = row.try_get("kind")?;
    let seq = Seq(unsigned(row.try_get("seq")?)?);
    let session: Uuid = row.try_get("session_id")?;
    let at: OffsetDateTime = row.try_get("at")?;
    let seed = seed_of(row.try_get("seed")?)?;
    let missing = |field: &str| DbError::new(format!("entry {} has no {field}", seq.0));
    match kind.as_str() {
        "change" => {
            let client: Option<Uuid> = row.try_get("client_id")?;
            let local: Option<i64> = row.try_get("local")?;
            let participant: Option<i32> = row.try_get("participant")?;
            let user: Option<Uuid> = row.try_get("user_id")?;
            let witnesses: Vec<i32> = row.try_get("witnesses")?;
            let cancelled_by: Option<i64> = row.try_get("cancelled_by")?;
            Ok(LiveEntryRow::Change {
                seq,
                session,
                client: ClientId(client.ok_or_else(|| missing("client"))?),
                local: unsigned(local.ok_or_else(|| missing("local"))?)?,
                participant: unnumber(participant.ok_or_else(|| missing("participant"))?)?,
                user: UserId(user.ok_or_else(|| missing("user"))?),
                seen: Seq(unsigned(row.try_get("seen")?)?),
                at,
                seed,
                format: row.try_get("format")?,
                payload: row.try_get("payload")?,
                witnesses: witnesses
                    .into_iter()
                    .map(unnumber)
                    .collect::<Result<_, _>>()?,
                cancelled_by: cancelled_by.map(unsigned).transpose()?.map(Seq),
            })
        }
        "cancel" => {
            let cancels: Option<i64> = row.try_get("cancels")?;
            let reports: Option<i32> = row.try_get("reports")?;
            Ok(LiveEntryRow::Cancel {
                seq,
                session,
                at,
                seed,
                cancels: Seq(unsigned(cancels.ok_or_else(|| missing("cancels"))?)?),
                reports: unnumber(reports.ok_or_else(|| missing("reports"))?)?,
            })
        }
        other => Err(DbError::new(format!("unknown entry kind {other:?}"))),
    }
}

async fn advance_live_verified(
    conn: &mut PgConnection,
    doc: DocId,
    through: i64,
    at: OffsetDateTime,
) -> Result<(), DbError> {
    sqlx::query(
        "update live_logs set verified_through = $2, verified_at = $3 \
         where document_id = $1 and verified_through < $2",
    )
    .bind(doc.0)
    .bind(through)
    .bind(at)
    .execute(conn)
    .await?;
    Ok(())
}

/// The entries kept after `verified_through` and their bytes, cancel records at their encoded
/// size.
async fn unsaved_counts(
    conn: &mut PgConnection,
    doc: DocId,
    verified_through: i64,
) -> Result<(u64, u64), DbError> {
    let row = sqlx::query(
        "select count(*)::bigint as entries,                 coalesce(sum(case when kind = 'cancel' then $3                                   else octet_length(payload) end), 0)::bigint as bytes          from live_entries where document_id = $1 and seq > $2",
    )
    .bind(doc.0)
    .bind(verified_through)
    .bind(i64::try_from(LIVE_CANCEL_BYTES).unwrap_or(i64::MAX))
    .fetch_one(conn)
    .await?;
    Ok((
        unsigned(row.try_get("entries")?)?,
        unsigned(row.try_get("bytes")?)?,
    ))
}

fn checkpoint_of(row: &PgRow) -> Result<LiveCheckpointRow, DbError> {
    let bytes: Vec<u8> = row.try_get("sha256")?;
    let sha256 = bytes.try_into().map_err(|bytes: Vec<u8>| {
        DbError::new(format!("checkpoint hash has {} bytes", bytes.len()))
    })?;
    Ok(LiveCheckpointRow {
        revision: row.try_get("revision")?,
        through: Seq(unsigned(row.try_get("through")?)?),
        sha256,
        verified: row
            .try_get::<Option<OffsetDateTime>, _>("verified_at")?
            .is_some(),
        session: row.try_get("session_id")?,
        session_data: row.try_get("session_data")?,
        major_reason: row.try_get("major_reason")?,
        major_person: row.try_get::<Option<Uuid>, _>("major_person")?.map(UserId),
        writer: match (
            row.try_get::<Option<i32>, _>("writer_participant")?,
            row.try_get::<Option<Uuid>, _>("writer_client")?,
        ) {
            (Some(participant), Some(client)) => Some(LiveCheckpointWriter {
                participant: unnumber(participant)?,
                client: ClientId(client),
            }),
            _ => None,
        },
        sole_editor: row
            .try_get::<Option<OffsetDateTime>, _>("sole_editor_at")?
            .is_some(),
    })
}

fn return_tombstone_of(row: &PgRow) -> Result<LiveReturnTombstoneRow, DbError> {
    Ok(LiveReturnTombstoneRow {
        session: row.try_get("session_id")?,
        participant: unnumber(row.try_get("participant")?)?,
        client: ClientId(row.try_get("client_id")?),
        user: UserId(row.try_get("user_id")?),
        ended_at: row.try_get("ended_at")?,
        final_seq: Seq(unsigned(row.try_get("final_seq")?)?),
    })
}

fn session_data_of(row: &PgRow) -> Result<LiveSessionDataRow, DbError> {
    Ok(LiveSessionDataRow {
        session: row.try_get("session_id")?,
        through: Seq(unsigned(row.try_get("through")?)?),
        session_data: row.try_get("session_data")?,
    })
}

/// Associates an existing revision with one exact position and its opaque session data, under
/// the live-log lock. Repeating the same record is idempotent; a conflicting revision or
/// position fails.
async fn insert_live_checkpoint(
    tx: &mut PgConnection,
    doc: DocId,
    checkpoint: &NewLiveCheckpoint<'_>,
) -> Result<(), DbError> {
    let through = signed(checkpoint.through.0)?;
    let head = lock_log(&mut *tx, doc).await?;
    if through > head {
        return Err(DbError::new(format!(
            "checkpoint position {through} exceeds the head {head} of {}",
            doc.0
        )));
    }
    let session_exists: bool = sqlx::query_scalar(
        "select exists(select 1 from live_sessions where id = $1 and document_id = $2)",
    )
    .bind(checkpoint.session)
    .bind(doc.0)
    .fetch_one(&mut *tx)
    .await?;
    if !session_exists {
        return Err(DbError::new(format!(
            "{} is not a live session of {}",
            checkpoint.session, doc.0
        )));
    }
    let revision_exists: bool = sqlx::query_scalar(
        "select exists(select 1 from revisions where revision = $1 and document_id = $2)",
    )
    .bind(checkpoint.revision)
    .bind(doc.0)
    .fetch_one(&mut *tx)
    .await?;
    if !revision_exists {
        return Err(DbError::new(format!(
            "revision {:?} does not belong to {}",
            checkpoint.revision, doc.0
        )));
    }
    let writer_participant = checkpoint
        .writer
        .map(|writer| number(writer.participant))
        .transpose()?;
    let writer_client = checkpoint.writer.map(|writer| writer.client.0);
    let inserted = sqlx::query(
        "insert into live_checkpoints \
                 (document_id, revision, through, sha256, session_id, session_data, \
                  major_reason, major_person, created_at, writer_participant, writer_client) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) \
             on conflict do nothing",
    )
    .bind(doc.0)
    .bind(checkpoint.revision)
    .bind(through)
    .bind(checkpoint.sha256.as_slice())
    .bind(checkpoint.session)
    .bind(checkpoint.session_data)
    .bind(checkpoint.major_reason)
    .bind(checkpoint.major_person.map(|person| person.0))
    .bind(checkpoint.created_at)
    .bind(writer_participant)
    .bind(writer_client)
    .execute(&mut *tx)
    .await?;
    if inserted.rows_affected() == 1 {
        // Keep at most MAX_UNVERIFIED_CHECKPOINTS unverified checkpoints after the verified
        // base: the oldest go, with their identity data.
        let verified_through: i64 =
            sqlx::query_scalar("select verified_through from live_logs where document_id = $1")
                .bind(doc.0)
                .fetch_one(&mut *tx)
                .await?;
        let cap = i64::try_from(MAX_UNVERIFIED_CHECKPOINTS).unwrap_or(i64::MAX);
        sqlx::query(
            "delete from live_checkpoints \
             where document_id = $1 and verified_at is null \
               and revision in ( \
                 select revision from live_checkpoints \
                 where document_id = $1 and verified_at is null and major_reason is null \
                   and disputed_at is null \
                   and (through > $2 or ($2 = 0 and through = 0)) \
                 order by through desc offset $3)",
        )
        .bind(doc.0)
        .bind(verified_through)
        .bind(cap)
        .execute(&mut *tx)
        .await?;
    }
    if inserted.rows_affected() == 0 {
        let same: bool = sqlx::query_scalar(
            "select exists( \
                    select 1 from live_checkpoints \
                    where document_id = $1 and revision = $2 and through = $3 \
                      and disputed_at is null \
                      and sha256 = $4 and session_id = $5 and session_data = $6 \
                      and major_reason is not distinct from $7 \
                      and major_person is not distinct from $8 and created_at = $9 \
                      and writer_participant is not distinct from $10 \
                      and writer_client is not distinct from $11)",
        )
        .bind(doc.0)
        .bind(checkpoint.revision)
        .bind(through)
        .bind(checkpoint.sha256.as_slice())
        .bind(checkpoint.session)
        .bind(checkpoint.session_data)
        .bind(checkpoint.major_reason)
        .bind(checkpoint.major_person.map(|person| person.0))
        .bind(checkpoint.created_at)
        .bind(writer_participant)
        .bind(writer_client)
        .fetch_one(&mut *tx)
        .await?;
        if !same {
            return Err(DbError::new(format!(
                "checkpoint {:?} or position {through} conflicts with a stored checkpoint",
                checkpoint.revision
            )));
        }
    }
    Ok(())
}

impl Db {
    /// `doc`'s live log: its head, its kept entries after the verified position, its open
    /// session with every place in it, and its clients.
    pub async fn load_live(&self, doc: DocId) -> Result<LiveLoaded, DbError> {
        let mut tx = self.pool().begin().await?;
        // One snapshot for every read below.
        sqlx::query("set transaction isolation level repeatable read")
            .execute(&mut *tx)
            .await?;
        ensure_log(&mut tx, doc).await?;
        let log = sqlx::query(
            "select head, last_participant, format, verified_through from live_logs \
             where document_id = $1",
        )
        .bind(doc.0)
        .fetch_one(&mut *tx)
        .await?;
        let verified_through: i64 = log.try_get("verified_through")?;
        let verified_checkpoint = sqlx::query(
            "select revision, through, sha256, verified_at, session_id, session_data, \
                    major_reason, major_person, writer_participant, writer_client, \
                    sole_editor_at \
             from live_checkpoints where document_id = $1 and verified_at is not null \
             order by through desc limit 1",
        )
        .bind(doc.0)
        .fetch_optional(&mut *tx)
        .await?
        .as_ref()
        .map(checkpoint_of)
        .transpose()?;
        let unverified_checkpoints = sqlx::query(
            "select revision, through, sha256, verified_at, session_id, session_data, \
                    major_reason, major_person, writer_participant, writer_client, \
                    sole_editor_at \
             from live_checkpoints \
             where document_id = $1 and verified_at is null and disputed_at is null \
               and (through > $2 or ($2 = 0 and through = 0)) \
             order by through",
        )
        .bind(doc.0)
        .bind(verified_through)
        .fetch_all(&mut *tx)
        .await?
        .iter()
        .map(checkpoint_of)
        .collect::<Result<Vec<_>, _>>()?;
        let entries = sqlx::query(
            "select seq, session_id, kind, client_id, local, participant, user_id, seen, at, \
                    seed, format, payload, witnesses, cancels, reports, cancelled_by \
             from live_entries where document_id = $1 and seq > $2 order by seq",
        )
        .bind(doc.0)
        .bind(verified_through)
        .fetch_all(&mut *tx)
        .await?
        .iter()
        .map(entry_of)
        .collect::<Result<Vec<_>, _>>()?;
        let unsaved = unsaved_counts(&mut tx, doc, verified_through).await?;

        let session = sqlx::query(
            "select id, format, first_seq, started_at from live_sessions \
             where document_id = $1 and ended_at is null",
        )
        .bind(doc.0)
        .fetch_optional(&mut *tx)
        .await?;
        let open_session = match session {
            None => None,
            Some(row) => {
                let id: Uuid = row.try_get("id")?;
                let participants = sqlx::query(
                    "select number, client_id, user_id, joined_at, gone_reason \
                     from live_participants where session_id = $1 order by number",
                )
                .bind(id)
                .fetch_all(&mut *tx)
                .await?
                .iter()
                .map(|p| {
                    let gone: Option<String> = p.try_get("gone_reason")?;
                    Ok(LiveParticipantRow {
                        number: unnumber(p.try_get("number")?)?,
                        client: ClientId(p.try_get("client_id")?),
                        user: UserId(p.try_get("user_id")?),
                        joined_at: p.try_get("joined_at")?,
                        gone: gone.as_deref().map(GoneReason::parse).transpose()?,
                    })
                })
                .collect::<Result<Vec<_>, DbError>>()?;
                Some(LiveSessionRow {
                    id,
                    format: row.try_get("format")?,
                    first_seq: Seq(unsigned(row.try_get("first_seq")?)?),
                    started_at: row.try_get("started_at")?,
                    participants,
                })
            }
        };

        let participants = sqlx::query(
            "select p.number, p.client_id, p.user_id, p.joined_at, p.gone_reason \
             from live_participants p join live_sessions s on s.id = p.session_id \
             where s.document_id = $1 order by p.number",
        )
        .bind(doc.0)
        .fetch_all(&mut *tx)
        .await?
        .iter()
        .map(|p| {
            let gone: Option<String> = p.try_get("gone_reason")?;
            Ok(LiveParticipantRow {
                number: unnumber(p.try_get("number")?)?,
                client: ClientId(p.try_get("client_id")?),
                user: UserId(p.try_get("user_id")?),
                joined_at: p.try_get("joined_at")?,
                gone: gone.as_deref().map(GoneReason::parse).transpose()?,
            })
        })
        .collect::<Result<Vec<_>, DbError>>()?;

        let clients = sqlx::query(
            "select client_id, user_id, next_local from live_clients \
             where document_id = $1 order by client_id",
        )
        .bind(doc.0)
        .fetch_all(&mut *tx)
        .await?
        .iter()
        .map(|c| {
            Ok((
                ClientId(c.try_get("client_id")?),
                UserId(c.try_get("user_id")?),
                unsigned(c.try_get("next_local")?)?,
            ))
        })
        .collect::<Result<Vec<_>, DbError>>()?;
        let retained_session_data = sqlx::query(
            "select session_id, through, session_data from ( \
                 select distinct on (session_id) session_id, through, session_data \
                 from live_checkpoints \
                 where document_id = $1 and octet_length(session_data) > 0 \
                 order by session_id, through desc \
             ) retained order by through",
        )
        .bind(doc.0)
        .fetch_all(&mut *tx)
        .await?
        .iter()
        .map(session_data_of)
        .collect::<Result<Vec<_>, _>>()?;
        tx.commit().await?;

        Ok(LiveLoaded {
            head: Seq(unsigned(log.try_get("head")?)?),
            last_participant: unnumber(log.try_get("last_participant")?)?,
            format: log.try_get("format")?,
            verified_through: Seq(unsigned(verified_through)?),
            verified_checkpoint,
            unverified_checkpoints,
            entries,
            unsaved_entries: unsaved.0,
            unsaved_bytes: unsaved.1,
            open_session,
            participants,
            clients,
            retained_session_data,
        })
    }

    /// Starts the session `session` of `doc` in `format`. Fails when `doc` has an open
    /// session already.
    pub async fn start_live_session(
        &self,
        doc: DocId,
        session: Uuid,
        format: &str,
        at: OffsetDateTime,
    ) -> Result<(), DbError> {
        let mut tx = self.pool().begin().await?;
        let head = lock_log(&mut tx, doc).await?;
        sqlx::query(
            "insert into live_sessions (id, document_id, format, first_seq, started_at) \
             values ($1, $2, $3, $4, $5)",
        )
        .bind(session)
        .bind(doc.0)
        .bind(format)
        .bind(head + 1)
        .bind(at)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Gives `client` of `user` a place in `doc`'s open session `session`, under the next
    /// participant number of the document, and returns that number. Numbers start at 1 and
    /// are never reused. Fails when `session` is not `doc`'s open session.
    pub async fn add_live_participant(
        &self,
        doc: DocId,
        session: Uuid,
        client: ClientId,
        user: UserId,
        at: OffsetDateTime,
    ) -> Result<u32, DbError> {
        let mut tx = self.pool().begin().await?;
        lock_log(&mut tx, doc).await?;
        let open: Option<Uuid> = sqlx::query_scalar(
            "select id from live_sessions \
             where id = $1 and document_id = $2 and ended_at is null for update",
        )
        .bind(session)
        .bind(doc.0)
        .fetch_optional(&mut *tx)
        .await?;
        if open.is_none() {
            return Err(DbError::new(format!(
                "{session} is not the open live session of {}",
                doc.0
            )));
        }
        let number: i32 = sqlx::query_scalar(
            "update live_logs set last_participant = last_participant + 1 \
             where document_id = $1 returning last_participant",
        )
        .bind(doc.0)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query(
            "insert into live_participants (session_id, number, client_id, user_id, joined_at) \
             values ($1, $2, $3, $4, $5)",
        )
        .bind(session)
        .bind(number)
        .bind(client.0)
        .bind(user.0)
        .bind(at)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        unnumber(number)
    }

    /// Ends the place `number` in `session` for `reason`. A place already ended keeps how it
    /// ended. Fails when there is no such place.
    pub async fn end_live_place(
        &self,
        session: Uuid,
        number: u32,
        reason: GoneReason,
        at: OffsetDateTime,
    ) -> Result<(), DbError> {
        let mut tx = self.pool().begin().await?;
        session_doc(&mut tx, session).await?;
        let place: Option<i32> = sqlx::query_scalar(
            "update live_participants set gone_at = coalesce(gone_at, $3), \
                 gone_reason = coalesce(gone_reason, $4) \
             where session_id = $1 and number = $2 returning number",
        )
        .bind(session)
        .bind(self::number(number)?)
        .bind(at)
        .bind(reason.as_str())
        .fetch_optional(&mut *tx)
        .await?;
        if place.is_none() {
            return Err(DbError::new(format!("no place {number} in {session}")));
        }
        tx.commit().await?;
        Ok(())
    }

    /// Stores `change` at the head of `doc`'s log, in one transaction: the entry, the head
    /// moved to its position, the log's format, and its client's next local number. Fails,
    /// storing nothing, when its position is not the head plus one (a second writer, or a stale
    /// room), when its client's number is stored or refused already, when its client belongs to
    /// someone else, or when its session is not `doc`'s.
    pub async fn append_live_change(
        &self,
        doc: DocId,
        change: &NewLiveChange<'_>,
    ) -> Result<(), DbError> {
        let seq = signed(change.seq.0)?;
        let local = signed(change.local)?;
        let mut tx = self.pool().begin().await?;
        ensure_log(&mut tx, doc).await?;
        let moved = sqlx::query(
            "update live_logs set head = $2, format = $3 \
             where document_id = $1 and head = $2 - 1",
        )
        .bind(doc.0)
        .bind(seq)
        .bind(change.format)
        .execute(&mut *tx)
        .await?;
        if moved.rows_affected() != 1 {
            return Err(DbError::new(format!(
                "position {seq} does not follow the head of {}",
                doc.0
            )));
        }
        match local_disposition(&mut tx, doc, change.client, local).await? {
            LiveLocal::Stored { seq } => {
                return Err(DbError::new(format!(
                    "local {local} of client {} is stored at {}",
                    change.client.0, seq.0
                )));
            }
            LiveLocal::Refused { reason } => {
                return Err(DbError::new(format!(
                    "local {local} of client {} was refused ({reason})",
                    change.client.0
                )));
            }
            LiveLocal::Unseen => {}
        }
        if let Some(next) = next_local(&mut tx, doc, change.client, change.user).await?
            && local < next
        {
            return Err(DbError::new(format!(
                "local {local} of client {} is out of order (next local is {next})",
                change.client.0
            )));
        }
        use_local(&mut tx, doc, change.client, change.user, change.local).await?;
        let witnesses = change
            .witnesses
            .iter()
            .map(|w| number(*w))
            .collect::<Result<Vec<_>, _>>()?;
        let stored = sqlx::query(
            "insert into live_entries (document_id, seq, session_id, kind, client_id, local, \
                                       participant, user_id, seen, at, seed, format, payload, \
                                       witnesses) \
             select $1, $2, $3, 'change', $4, $5, $6, $7, $8, $9, $10, $11, $12, $13 \
             where exists (select 1 from live_sessions where id = $3 and document_id = $1)",
        )
        .bind(doc.0)
        .bind(seq)
        .bind(change.session)
        .bind(change.client.0)
        .bind(local)
        .bind(number(change.participant)?)
        .bind(change.user.0)
        .bind(signed(change.seen.0)?)
        .bind(change.at)
        .bind(i64::from(change.seed))
        .bind(change.format)
        .bind(change.payload)
        .bind(witnesses)
        .execute(&mut *tx)
        .await?;
        if stored.rows_affected() != 1 {
            return Err(DbError::new(format!(
                "{} is not a live session of {}",
                change.session, doc.0
            )));
        }
        tx.commit().await?;
        Ok(())
    }

    /// Stores a cancel record at the head of `doc`'s log, empties the cancelled change and
    /// points it at the new record, all in one transaction.
    pub async fn cancel_live_entry(
        &self,
        doc: DocId,
        cancel: &NewLiveCancel,
    ) -> Result<(), DbError> {
        let seq = signed(cancel.seq.0)?;
        let cancels = signed(cancel.cancels.0)?;
        let mut tx = self.pool().begin().await?;
        ensure_log(&mut tx, doc).await?;
        let moved =
            sqlx::query("update live_logs set head = $2 where document_id = $1 and head = $2 - 1")
                .bind(doc.0)
                .bind(seq)
                .execute(&mut *tx)
                .await?;
        if moved.rows_affected() != 1 {
            return Err(DbError::new(format!(
                "position {seq} does not follow the head of {}",
                doc.0
            )));
        }
        let format: Option<String> = sqlx::query_scalar(
            "update live_entries set cancelled_by = $3, payload = '' \
             where document_id = $1 and seq = $2 and kind = 'change' \
                   and cancelled_by is null returning format",
        )
        .bind(doc.0)
        .bind(cancels)
        .bind(seq)
        .fetch_optional(&mut *tx)
        .await?;
        let format = format.ok_or_else(|| {
            DbError::new(format!(
                "position {cancels} is not an uncancelled change of {}",
                doc.0
            ))
        })?;
        let stored = sqlx::query(
            "insert into live_entries (document_id, seq, session_id, kind, at, seed, format, \
                                       cancels, reports) \
             select $1, $2, $3, 'cancel', $4, $5, $6, $7, $8 \
             where exists (select 1 from live_sessions where id = $3 and document_id = $1)",
        )
        .bind(doc.0)
        .bind(seq)
        .bind(cancel.session)
        .bind(cancel.at)
        .bind(i64::from(cancel.seed))
        .bind(format)
        .bind(cancels)
        .bind(number(cancel.reports)?)
        .execute(&mut *tx)
        .await?;
        if stored.rows_affected() != 1 {
            return Err(DbError::new(format!(
                "{} is not a live session of {}",
                cancel.session, doc.0
            )));
        }
        tx.commit().await?;
        Ok(())
    }

    /// Records that `client`'s change numbered `local` was finally refused for `reason` (as
    /// the wire names it, such as `read_only`), using up its number: its next local number
    /// moves past it, and [`Db::live_local`] answers `Refused` for it from now on. Refusing a
    /// number again keeps the first reason. Fails when its change is stored, or when the
    /// client belongs to someone else.
    pub async fn consume_live_local(
        &self,
        doc: DocId,
        client: ClientId,
        user: UserId,
        local: u64,
        reason: &str,
        at: OffsetDateTime,
    ) -> Result<(), DbError> {
        let signed_local = signed(local)?;
        let mut tx = self.pool().begin().await?;
        lock_log(&mut tx, doc).await?;
        match local_disposition(&mut tx, doc, client, signed_local).await? {
            LiveLocal::Stored { seq } => {
                return Err(DbError::new(format!(
                    "local {local} of client {} is stored at {}",
                    client.0, seq.0
                )));
            }
            LiveLocal::Refused { .. } => {
                next_local(&mut tx, doc, client, user).await?;
                tx.commit().await?;
                return Ok(());
            }
            LiveLocal::Unseen => {}
        }
        if let Some(next) = next_local(&mut tx, doc, client, user).await?
            && signed_local < next
        {
            return Err(DbError::new(format!(
                "local {local} of client {} is out of order (next local is {next})",
                client.0
            )));
        }
        use_local(&mut tx, doc, client, user, local).await?;
        sqlx::query(
            "insert into live_refusals (document_id, client_id, local, reason, at) \
             values ($1, $2, $3, $4, $5) on conflict do nothing",
        )
        .bind(doc.0)
        .bind(client.0)
        .bind(signed_local)
        .bind(reason)
        .bind(at)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// What became of `client`'s change numbered `local` in `doc`: stored (and where),
    /// finally refused (and why), or neither.
    pub async fn live_local(
        &self,
        doc: DocId,
        client: ClientId,
        local: u64,
    ) -> Result<LiveLocal, DbError> {
        let mut conn = self.pool().acquire().await?;
        local_disposition(&mut conn, doc, client, signed(local)?).await
    }

    /// Associates an existing revision with one exact position and its opaque session data.
    /// Repeating the same record is idempotent; a conflicting revision or position fails.
    pub async fn record_live_checkpoint(
        &self,
        doc: DocId,
        checkpoint: &NewLiveCheckpoint<'_>,
    ) -> Result<(), DbError> {
        let mut tx = self.pool().begin().await?;
        insert_live_checkpoint(&mut tx, doc, checkpoint).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Records `revision`, as Git describes it, in the revision index after `parent` when it is
    /// not recorded yet, then associates it with `checkpoint`, in one transaction: a revision
    /// that Git committed is never left without its checkpoint record because the
    /// asynchronous revision projection has not caught up (or lost its row). Fails, storing
    /// nothing, when the revision is missing and `parent` is not the recorded head; a rebuild
    /// of the index from Git must run first.
    pub async fn record_live_checkpoint_revision(
        &self,
        parent: Option<&str>,
        revision: &RevisionRecord,
        checkpoint: &NewLiveCheckpoint<'_>,
    ) -> Result<(), DbError> {
        let doc = revision.doc;
        if revision.revision != checkpoint.revision {
            return Err(DbError::new(format!(
                "checkpoint names {:?} but the revision is {:?}",
                checkpoint.revision, revision.revision
            )));
        }
        let mut tx = self.pool().begin().await?;
        // The document lock first, then the live-log lock.
        lock_document(&mut tx, doc, &revision.name).await?;
        let recorded: bool = sqlx::query_scalar(
            "select exists(select 1 from revisions where revision = $1 and document_id = $2)",
        )
        .bind(&revision.revision)
        .bind(doc.0)
        .fetch_one(&mut *tx)
        .await?;
        if !recorded {
            let (head, position) = recorded_head(&mut tx, doc).await?;
            if head.as_deref() != parent {
                return Err(DbError::new(format!(
                    "revision {:?} of {} is not recorded and its parent is not the recorded head",
                    revision.revision, doc.0
                )));
            }
            let position = position
                .checked_add(1)
                .ok_or_else(|| DbError::new("too many revisions"))?;
            insert_revision(&mut tx, revision, position, parent).await?;
            set_current(&mut tx, revision).await?;
        }
        insert_live_checkpoint(&mut tx, doc, checkpoint).await?;
        tx.commit().await?;
        Ok(())
    }

    /// The tombstone of place `participant` of the ended session `session` of `doc`, by its
    /// key: one indexed row, however many sessions the document has had.
    pub async fn live_return_tombstone(
        &self,
        doc: DocId,
        session: Uuid,
        participant: u32,
    ) -> Result<Option<LiveReturnTombstoneRow>, DbError> {
        let mut conn = self.pool().acquire().await?;
        sqlx::query(
            "select session_id, participant, client_id, user_id, ended_at, final_seq \
             from live_return_tombstones \
             where session_id = $1 and participant = $2 and document_id = $3",
        )
        .bind(session)
        .bind(number(participant)?)
        .bind(doc.0)
        .fetch_optional(&mut *conn)
        .await?
        .as_ref()
        .map(return_tombstone_of)
        .transpose()
    }

    /// Marks exactly the named checkpoint verified and atomically advances the catch-up base.
    pub async fn verify_live_checkpoint(
        &self,
        doc: DocId,
        revision: &str,
        through: Seq,
        at: OffsetDateTime,
    ) -> Result<(), DbError> {
        self.settle_live_checkpoint(doc, revision, through, at, false)
            .await
    }

    /// Accepts the named checkpoint as the catch-up base although no second capable editor
    /// could verify it, marked sole-editor until one does.
    pub async fn accept_live_sole_editor(
        &self,
        doc: DocId,
        revision: &str,
        through: Seq,
        at: OffsetDateTime,
    ) -> Result<(), DbError> {
        self.settle_live_checkpoint(doc, revision, through, at, true)
            .await
    }

    /// A second editor's hash matched a sole-editor base: it is verified like any other.
    pub async fn confirm_live_sole_editor(
        &self,
        doc: DocId,
        revision: &str,
    ) -> Result<(), DbError> {
        let mut conn = self.pool().acquire().await?;
        sqlx::query(
            "update live_checkpoints set sole_editor_at = null \
             where document_id = $1 and revision = $2",
        )
        .bind(doc.0)
        .bind(revision)
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    /// Sets aside the unverified checkpoint `revision`, whose verifier disagreed: it stays for
    /// forensics without its identity data, is never a base, and frees its position.
    pub async fn dispute_live_checkpoint(
        &self,
        doc: DocId,
        revision: &str,
        at: OffsetDateTime,
    ) -> Result<(), DbError> {
        let mut tx = self.pool().begin().await?;
        lock_log(&mut tx, doc).await?;
        sqlx::query(
            "update live_checkpoints \
             set disputed_at = coalesce(disputed_at, $3), session_data = '' \
             where document_id = $1 and revision = $2 and verified_at is null",
        )
        .bind(doc.0)
        .bind(revision)
        .bind(at)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// The entries kept after the verified base and their bytes (cancel records at their
    /// encoded size), without loading them.
    pub async fn live_unsaved_counts(&self, doc: DocId) -> Result<(u64, u64), DbError> {
        let mut conn = self.pool().acquire().await?;
        let verified_through: Option<i64> =
            sqlx::query_scalar("select verified_through from live_logs where document_id = $1")
                .bind(doc.0)
                .fetch_optional(&mut *conn)
                .await?;
        unsaved_counts(&mut conn, doc, verified_through.unwrap_or(0)).await
    }

    async fn settle_live_checkpoint(
        &self,
        doc: DocId,
        revision: &str,
        through: Seq,
        at: OffsetDateTime,
        sole_editor: bool,
    ) -> Result<(), DbError> {
        let through = signed(through.0)?;
        let mut tx = self.pool().begin().await?;
        let head = lock_log(&mut tx, doc).await?;
        if through > head {
            return Err(DbError::new(format!(
                "checkpoint position {through} exceeds the head {head} of {}",
                doc.0
            )));
        }
        let checkpoint: Option<Vec<u8>> = sqlx::query_scalar(
            "select sha256 from live_checkpoints \
             where document_id = $1 and revision = $2 and through = $3 \
               and disputed_at is null for update",
        )
        .bind(doc.0)
        .bind(revision)
        .bind(through)
        .fetch_optional(&mut *tx)
        .await?;
        if checkpoint.is_none() {
            // A superseded checkpoint is removed once a later one is verified; verifying it
            // late changes nothing.
            let verified_through: i64 =
                sqlx::query_scalar("select verified_through from live_logs where document_id = $1")
                    .bind(doc.0)
                    .fetch_one(&mut *tx)
                    .await?;
            if through <= verified_through {
                return Ok(());
            }
            return Err(DbError::new(format!(
                "no checkpoint {:?} at position {through} for {}",
                revision, doc.0
            )));
        }
        sqlx::query(
            "update live_checkpoints set verified_at = coalesce(verified_at, $4), \
                    sole_editor_at = case when $5 then coalesce(sole_editor_at, $4) end \
             where document_id = $1 and revision = $2 and through = $3",
        )
        .bind(doc.0)
        .bind(revision)
        .bind(through)
        .bind(at)
        .bind(sole_editor)
        .execute(&mut *tx)
        .await?;
        advance_live_verified(&mut tx, doc, through, at).await?;
        // Earlier unverified checkpoints are superseded: they give up their identity data,
        // except the newest of another session, which a returning client of that session may
        // still need; those left with none are removed.
        sqlx::query(
            "update live_checkpoints c set session_data = '' \
             where c.document_id = $1 and c.verified_at is null and c.through < $2 \
               and octet_length(c.session_data) > 0 \
               and (c.session_id = (select session_id from live_checkpoints \
                                    where document_id = $1 and revision = $3) \
                    or exists (select 1 from live_checkpoints n \
                               where n.document_id = c.document_id \
                                 and n.session_id = c.session_id and n.through > c.through))",
        )
        .bind(doc.0)
        .bind(through)
        .bind(revision)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "delete from live_checkpoints \
             where document_id = $1 and verified_at is null and through < $2 \
               and octet_length(session_data) = 0 and disputed_at is null",
        )
        .bind(doc.0)
        .bind(through)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Makes `checkpoint`, a revision that replaced the live file from outside and was stored
    /// again on top of the log's last state, the verified catch-up base at its position (S4):
    /// the ordinary checkpoints it supersedes are removed (disputed and major rows stay) and the verified
    /// position advances to it. Its session data is normally empty: the base is a plain file.
    /// Kept entries stay for their retention period.
    pub async fn adopt_live_base(
        &self,
        doc: DocId,
        checkpoint: &NewLiveCheckpoint<'_>,
        at: OffsetDateTime,
    ) -> Result<(), DbError> {
        let through = signed(checkpoint.through.0)?;
        let mut tx = self.pool().begin().await?;
        let head = lock_log(&mut tx, doc).await?;
        if through > head {
            return Err(DbError::new(format!(
                "base position {through} exceeds the head {head} of {}",
                doc.0
            )));
        }
        sqlx::query(
            "delete from live_checkpoints \
             where document_id = $1 and disputed_at is null and major_reason is null \
               and (revision = $2 or through >= $3 or verified_at is null)",
        )
        .bind(doc.0)
        .bind(checkpoint.revision)
        .bind(through)
        .execute(&mut *tx)
        .await?;
        let writer_participant = checkpoint
            .writer
            .map(|writer| number(writer.participant))
            .transpose()?;
        sqlx::query(
            "insert into live_checkpoints \
                 (document_id, revision, through, sha256, verified_at, session_id, session_data, \
                  major_reason, major_person, created_at, writer_participant, writer_client) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
        )
        .bind(doc.0)
        .bind(checkpoint.revision)
        .bind(through)
        .bind(checkpoint.sha256.as_slice())
        .bind(at)
        .bind(checkpoint.session)
        .bind(checkpoint.session_data)
        .bind(checkpoint.major_reason)
        .bind(checkpoint.major_person.map(|person| person.0))
        .bind(checkpoint.created_at)
        .bind(writer_participant)
        .bind(checkpoint.writer.map(|writer| writer.client.0))
        .execute(&mut *tx)
        .await?;
        advance_live_verified(&mut tx, doc, through, at).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Marks entries through `through` as covered by a verified revision. A stale mark does
    /// not move the verified position backwards or restart its retention period.
    pub async fn mark_live_verified(
        &self,
        doc: DocId,
        through: Seq,
        at: OffsetDateTime,
    ) -> Result<(), DbError> {
        let mut tx = self.pool().begin().await?;
        lock_log(&mut tx, doc).await?;
        advance_live_verified(&mut tx, doc, signed(through.0)?, at).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Removes live-log detail past its retention period while keeping every document's log
    /// row and head position forever.
    pub async fn sweep_live(&self, now: OffsetDateTime) -> Result<LiveSwept, DbError> {
        let cutoff = now - time::Duration::days(LIVE_KEEP_DAYS);
        let mut tx = self.pool().begin().await?;
        let entries = sqlx::query(
            "delete from live_entries e using live_logs l \
             where e.document_id = l.document_id and e.seq <= l.verified_through \
               and l.verified_at < $1 \
               and not exists (select 1 from live_sessions s \
                               where s.document_id = e.document_id \
                                 and s.ended_at is null and e.seq >= s.first_seq)",
        )
        .bind(cutoff)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        // Only a verified checkpoint gives up its identity data: an unverified one (at
        // position 0 included) may still be verified and become the catch-up base.
        sqlx::query(
            "update live_checkpoints c set session_data = '' \
             where octet_length(c.session_data) > 0 \
               and c.verified_at is not null \
               and c.created_at < $1 \
               and not exists (select 1 from live_entries e where e.session_id = c.session_id) \
               and not exists (select 1 from live_sessions s \
                               where s.id = c.session_id and s.ended_at is null)",
        )
        .bind(cutoff)
        .execute(&mut *tx)
        .await?;
        sqlx::query("delete from live_refusals where at < $1")
            .bind(cutoff)
            .execute(&mut *tx)
            .await?;
        let sessions = sqlx::query(
            "delete from live_sessions s where s.ended_at < $1 \
               and not exists (select 1 from live_entries e where e.session_id = s.id)",
        )
        .bind(cutoff)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        let clients = sqlx::query(
            "delete from live_clients c \
             where not exists (select 1 from live_entries e \
                               where e.document_id = c.document_id) \
               and not exists (select 1 from live_sessions s \
                               where s.document_id = c.document_id and s.ended_at is null)",
        )
        .execute(&mut *tx)
        .await?
        .rows_affected();
        sqlx::query(
            "update live_logs l set format = null where format is not null \
               and not exists (select 1 from live_entries e \
                               where e.document_id = l.document_id) \
               and not exists (select 1 from live_sessions s \
                               where s.document_id = l.document_id and s.ended_at is null)",
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(LiveSwept {
            entries,
            sessions,
            clients,
        })
    }

    /// Deletes every part of `doc`'s live log when the document is permanently purged.
    pub async fn delete_live_log(&self, doc: DocId) -> Result<(), DbError> {
        let mut tx = self.pool().begin().await?;
        delete_live_logs(&mut tx, &[doc]).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Ends the session `session`. A session already ended keeps its end. Fails when there is
    /// no such session.
    pub async fn end_live_session(&self, session: Uuid, at: OffsetDateTime) -> Result<(), DbError> {
        let mut tx = self.pool().begin().await?;
        let doc = session_doc(&mut tx, session).await?;
        let head = lock_log(&mut tx, doc).await?;
        sqlx::query("update live_sessions set ended_at = $2 where id = $1 and ended_at is null")
            .bind(session)
            .bind(at)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "insert into live_return_tombstones \
                 (session_id, participant, document_id, client_id, user_id, ended_at, final_seq) \
             select s.id, p.number, s.document_id, p.client_id, p.user_id, s.ended_at, $2 \
             from live_sessions s join live_participants p on p.session_id = s.id \
             where s.id = $1 and s.ended_at is not null \
             on conflict do nothing",
        )
        .bind(session)
        .bind(head)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }
}
