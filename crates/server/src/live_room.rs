//! The opaque-entry live room.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::future::Future;
use std::time::Duration;

use axum::extract::ws::Utf8Bytes;
use collab_core::ids::{ClientId, DocId, Seq, SessionId, UserId};
use protocol::RemovedReason;
use protocol::live::{
    EntryBody, HEARTBEAT_MS, LIVE_PROTOCOL, LiveBuild, LiveEntry, LivePerson, LiveResume,
    LiveServerMsg, LiveSessionId, LocalNo, MAX_PAYLOAD_BYTES, ParticipantNo, Place, RefusedReason,
    TicketPurpose, valid_fingerprint, valid_format,
};
use records::{
    Access, DbError, GoneReason, LiveEntryRow, LiveLoaded, LiveLocal, NewEvent, NewLiveCancel,
    NewLiveChange, NewLiveCheckpoint,
};
use revisions::{Author, BlobRef, RevisionId, RevisionInfo};
use serde_json::{Map, Value};
use time::OffsetDateTime;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use uuid::Uuid;

use crate::events::EventSink;
use crate::live_cancel::{CancelBook, Verdict};
use crate::live_store::{ForensicWritten, LiveStore, LiveWrite, LiveWriteError};
use crate::members::{self, AccessRules, Admission, Frames, Member, Members};
use crate::room::{
    AccessChange, ConnId, JoinReply, Outbound, Place as RegistryPlace, credited_name,
    refuse_waiting, sleep_until,
};
use records::LiveCheckpointWriter;

mod notices;

pub(crate) use notices::build_refused;

#[derive(Clone, Copy)]
pub(crate) struct LiveTiming {
    pub heartbeat: Duration,
    pub misses: u32,
    /// Ask for a checkpoint once the log has been quiet this long after a change.
    pub checkpoint_quiet: Duration,
    /// Ask for a checkpoint once the first unsaved change is this old, quiet or not.
    pub checkpoint_wait: Duration,
    /// How long a checkpoint ticket can be used.
    pub ticket_lifetime: Duration,
}

/// The adopted checkpoint cadence (plan G4): 15 seconds without a new change, or 120 seconds
/// after the first unsaved change, whichever comes first.
pub(crate) const CHECKPOINT_QUIET: Duration = Duration::from_secs(15);
pub(crate) const CHECKPOINT_WAIT: Duration = Duration::from_secs(120);
/// A checkpoint ticket is one-use and expires after two minutes.
pub(crate) const TICKET_LIFETIME: Duration = Duration::from_secs(120);
/// The largest editor identity table stored with a checkpoint.
pub(crate) const MAX_SESSION_DATA_BYTES: usize = 8 * 1024 * 1024;
/// Editors report their entered digest at every position that is a multiple of this.
pub(crate) const AGREEMENT_EVERY: u64 = 64;
/// Agreement positions compared at once; the oldest goes first.
const MAX_AGREEMENT_BUCKETS: usize = 128;
/// Reports kept for one agreement position.
const MAX_AGREEMENT_REPORTS: usize = 256;
/// How long a checkpoint whose database record is unknown first waits before reconciling
/// again; each failed try doubles the wait, up to [`RECONCILE_RETRY_MAX`].
const RECONCILE_RETRY: Duration = Duration::from_secs(5);
/// The longest wait between reconcile tries.
pub(crate) const RECONCILE_RETRY_MAX: Duration = Duration::from_secs(300);
/// How long agreement reports at one position are collected before they are compared, when
/// not every editing person has reported by then.
pub(crate) const AGREEMENT_WAIT: Duration = Duration::from_secs(30);
/// The longest outstanding verify tickets may hold back a due write ticket.
pub(crate) const VERIFY_HOLD_MAX: Duration = Duration::from_secs(120);
/// One forensic upload per person in this long.
pub(crate) const FORENSIC_EVERY: Duration = Duration::from_secs(600);

pub(crate) const RATE_PER_SECOND: u32 = 30;
pub(crate) const RATE_BURST: u32 = 120;

const COMMAND_CAPACITY: usize = 1024;
const LIVE_SWEEP_EVERY: Duration = Duration::from_secs(3_600);

/// Sweeps retained live-log detail once, logging a database failure for the next hourly pass.
/// Answers how many entries were removed.
pub async fn sweep_live_once(state: &crate::AppState, now: OffsetDateTime) -> u64 {
    match state.db().sweep_live(now).await {
        Ok(swept) => swept.entries,
        Err(error) => {
            eprintln!("sharecraft-server: database failure: {error}");
            0
        }
    }
}

/// The running live-log retention sweeper. Dropping it stops the task.
pub(crate) struct LiveSweeper(tokio::task::JoinHandle<()>);

impl Drop for LiveSweeper {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Starts the live-log retention sweep now and every hour.
pub(crate) fn start_sweeper(state: crate::AppState) -> LiveSweeper {
    start_sweeper_task(move || {
        let state = state.clone();
        async move {
            sweep_live_once(&state, OffsetDateTime::now_utc()).await;
        }
    })
}

fn start_sweeper_task<F, Fut>(mut sweep: F) -> LiveSweeper
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    LiveSweeper(tokio::spawn(async move {
        let mut every = tokio::time::interval(LIVE_SWEEP_EVERY);
        every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            every.tick().await;
            sweep().await;
        }
    }))
}

/// The two channels into a running live room.
#[derive(Clone, Debug)]
pub(crate) struct LiveLink {
    pub commands: mpsc::Sender<LiveCommand>,
    pub access: mpsc::UnboundedSender<AccessChange>,
}

impl LiveLink {
    /// A link whose room has ended: every send to it fails.
    pub(crate) fn closed() -> Self {
        let (commands, _) = mpsc::channel(1);
        let (access, _) = mpsc::unbounded_channel();
        Self { commands, access }
    }
}

/// Starts one live-room task and returns its channels.
pub(crate) fn start_live(
    place: RegistryPlace,
    store: crate::live_store::LiveDb,
    events: EventSink,
    timing: LiveTiming,
) -> LiveLink {
    let (commands, command_rx) = mpsc::channel(COMMAND_CAPACITY);
    let (access, access_rx) = mpsc::unbounded_channel();
    tokio::spawn(run(place, store, events, timing, command_rx, access_rx));
    LiveLink { commands, access }
}

pub(crate) enum LiveCommand {
    Join {
        conn: ConnId,
        client: ClientId,
        user: UserId,
        session: SessionId,
        name: String,
        role: Access,
        protocol: u32,
        format: String,
        resume: Option<LiveResume>,
        seen: Option<Seq>,
        can_checkpoint: bool,
        build: LiveBuild,
        tx: mpsc::Sender<Outbound>,
        accepted: oneshot::Sender<JoinReply>,
    },
    Entry {
        conn: ConnId,
        client: ClientId,
        local: LocalNo,
        seen: Seq,
        format: String,
        payload: String,
    },
    Heartbeat {
        conn: ConnId,
        client: ClientId,
        seen: Seq,
    },
    Report {
        conn: ConnId,
        client: ClientId,
        seq: Seq,
    },
    Fingerprint {
        conn: ConnId,
        client: ClientId,
        at: Seq,
        entered: String,
    },
    Resync {
        conn: ConnId,
        client: ClientId,
    },
    Leave {
        conn: ConnId,
        client: ClientId,
    },
    Disconnected {
        conn: ConnId,
        client: ClientId,
    },
    /// The ticket's holder could not honour it.
    CheckpointFailed {
        conn: ConnId,
        client: ClientId,
        ticket: Uuid,
    },
    /// Uses the write ticket `ticket` before its upload body is read; answers whether it may.
    CheckpointClaim {
        user: UserId,
        ticket: Uuid,
        at: Seq,
        base: Option<RevisionId>,
        reply: oneshot::Sender<bool>,
    },
    /// Stores the uploaded checkpoint `blob` (in the content store) with its session data.
    CheckpointUpload {
        user: UserId,
        ticket: Uuid,
        at: Seq,
        base: Option<RevisionId>,
        blob: BlobRef,
        session_data: Vec<u8>,
        reply: oneshot::Sender<LiveCheckpointReply>,
    },
    /// A verify ticket's independently written hash.
    CheckpointHash {
        user: UserId,
        ticket: Uuid,
        at: Seq,
        sha256: [u8; 32],
        reply: oneshot::Sender<LiveCheckpointReply>,
    },
    /// A checkpoint write, run off the room task, has finished.
    CheckpointWritten(Box<WriteDone>),
    /// A presence note, relayed in memory only (see `notices`).
    Presence {
        conn: ConnId,
        client: ClientId,
        payload: String,
    },
}

/// The live room's answer to a checkpoint upload or hash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LiveCheckpointReply {
    /// Stored as the document's current, unverified revision.
    Saved { revision: String },
    /// A second hash was received.
    Accepted,
    /// No such usable ticket for this person, position and base, or hostile data.
    Refused,
    /// The document has a revision the checkpoint was not based on.
    Stale { current: Option<String> },
    /// The checkpoint could not be stored; a later ticket asks again.
    Failed,
}

/// A one-use checkpoint ticket held by one participant.
struct Ticket {
    id: Uuid,
    purpose: TicketPurpose,
    participant: ParticipantNo,
    client: ClientId,
    user: UserId,
    at: Seq,
    base: Option<String>,
    /// For a verify ticket, the stored revision it verifies.
    verifies: Option<String>,
    /// For a forensic write ticket, the stored revision its holder disagreed with.
    forensic_of: Option<String>,
    expires: Instant,
    claimed: bool,
}

/// A checkpoint Git committed whose database record is not known to be stored.
struct PendingReconcile {
    parent: Option<String>,
    revision: String,
    at: Seq,
    sha256: [u8; 32],
    session: Uuid,
    session_data: Vec<u8>,
    created_at: OffsetDateTime,
    /// Git's description of the commit, once it could be read.
    described: Option<RevisionInfo>,
    fields: Map<String, Value>,
    user: UserId,
    participant: ParticipantNo,
    client: ClientId,
    retry_at: Instant,
    /// The wait before the next try after this one fails.
    delay: Duration,
}

/// Who uploaded an unverified checkpoint; the build is unknown for one loaded at a start.
#[derive(Clone, Copy)]
struct CheckpointWriter {
    participant: ParticipantNo,
    client: ClientId,
    build: Option<LiveBuild>,
}

/// What a checkpoint write produced.
enum WriteResult {
    Checkpoint(Result<String, LiveWriteError>),
    Forensic(Result<ForensicWritten, LiveWriteError>),
}

/// A finished checkpoint write, fed back to the room as a command.
pub(crate) struct WriteDone {
    result: WriteResult,
    ticket: Ticket,
    user: UserId,
    at: Seq,
    sha256: [u8; 32],
    session: Uuid,
    session_data: Vec<u8>,
    created_at: OffsetDateTime,
    reply: oneshot::Sender<LiveCheckpointReply>,
}

/// A verifier that disagreed, to be asked for its own bytes once no write is outstanding.
struct ForensicDue {
    revision: String,
    at: Seq,
    participant: ParticipantNo,
    client: ClientId,
    user: UserId,
}

/// Entered digests reported at one agreement position.
#[derive(Default)]
struct AgreementBucket {
    /// Each report: the reporting place, its person and the digest.
    reports: Vec<(ParticipantNo, UserId, String)>,
    resynced: BTreeSet<ParticipantNo>,
    /// When the first report arrived; the position is compared at the latest
    /// [`AGREEMENT_WAIT`] later.
    first: Option<Instant>,
    /// Once compared: the agreed digest, or `None` when no majority could be trusted.
    decided: Option<Option<String>>,
}

#[derive(Debug, thiserror::Error)]
#[error("live room store failed: {0}")]
pub(crate) struct RoomFailed(pub DbError);

impl From<DbError> for RoomFailed {
    fn from(error: DbError) -> Self {
        Self(error)
    }
}

struct Session {
    id: LiveSessionId,
    format: String,
    first_seq: Seq,
    places: HashMap<ParticipantNo, LivePlace>,
}

struct LivePlace {
    client: ClientId,
    user: UserId,
    sign_in: Option<SessionId>,
    name: String,
    can_edit: bool,
    can_checkpoint: bool,
    build: LiveBuild,
    held: bool,
    seen: Seq,
    last_heard: Instant,
}

struct RateBucket {
    tokens: u32,
    at: Instant,
    remainder: u128,
}

impl RateBucket {
    fn full() -> Self {
        Self {
            tokens: RATE_BURST,
            at: Instant::now(),
            remainder: 0,
        }
    }

    fn take(&mut self) -> bool {
        const NANOS_PER_SECOND: u128 = 1_000_000_000;
        let elapsed = Instant::now().saturating_duration_since(self.at);
        self.at = Instant::now();
        let credit = elapsed.as_nanos() * u128::from(RATE_PER_SECOND) + self.remainder;
        let added = credit / NANOS_PER_SECOND;
        self.remainder = credit % NANOS_PER_SECOND;
        let added = u32::try_from(added).unwrap_or(u32::MAX);
        self.tokens = self.tokens.saturating_add(added).min(RATE_BURST);
        if self.tokens == 0 {
            false
        } else {
            self.tokens -= 1;
            true
        }
    }
}

pub(crate) struct LiveRoom<S: LiveStore> {
    doc: DocId,
    store: S,
    events: EventSink,
    timing: LiveTiming,
    members: Members<LiveFrames>,
    head: Seq,
    verified_through: Seq,
    base_revision: Option<String>,
    _base_session_data: Vec<u8>,
    /// Stored checkpoints after the verified base, without their session data.
    unverified_checkpoints: Vec<records::LiveCheckpointRow>,
    /// The revision the next checkpoint is written on top of.
    checkpoint_base: Option<String>,
    /// The newest position a stored checkpoint covers, verified or not.
    saved_through: Seq,
    first_unsaved: Option<Instant>,
    last_change: Option<Instant>,
    tickets: Vec<Ticket>,
    last_writer: Option<ParticipantNo>,
    ticket_misses: HashMap<ParticipantNo, u32>,
    /// A stale checkpoint base waits for the returning-client recovery path (Task 11); no
    /// write ticket is issued against an outside revision meanwhile.
    checkpoint_blocked: bool,
    pending_reconcile: Option<PendingReconcile>,
    /// A checkpoint write is running off the room task; no other write may start.
    writing: bool,
    written_tx: mpsc::UnboundedSender<LiveCommand>,
    written_rx: mpsc::UnboundedReceiver<LiveCommand>,
    /// Who wrote each unverified checkpoint this room stored, and with which build.
    checkpoint_writers: HashMap<String, CheckpointWriter>,
    /// Verifiers that let a verify or forensic ticket lapse or sent a hash that disagreed;
    /// not asked to verify again until they join again.
    lapsed: BTreeSet<ParticipantNo>,
    /// Since when a due write ticket has been held back by outstanding verify tickets.
    verify_hold: Option<Instant>,
    /// When each person was last given a forensic ticket.
    forensic_last: HashMap<UserId, Instant>,
    /// Checkpoints a forensic upload was already asked for.
    forensic_asked: BTreeSet<String>,
    /// Verifiers whose hash disagreed with an unverified checkpoint, or who let a verify ticket
    /// for it lapse; never asked to verify it again, so a silent verifier cannot hold back
    /// checkpoint work.
    not_asked: HashMap<String, BTreeSet<ParticipantNo>>,
    forensic_due: Option<ForensicDue>,
    agreement: BTreeMap<Seq, AgreementBucket>,
    /// Changes stored in the open session, for its end event.
    session_changes: u64,
    names: HashMap<UserId, String>,
    _unsaved_entries: u64,
    _unsaved_bytes: u64,
    _retained_session_data: Vec<records::LiveSessionDataRow>,
    format: Option<String>,
    entries: Vec<LiveEntry>,
    witnesses: HashMap<Seq, Vec<u32>>,
    cancel_book: CancelBook,
    viewer_reports: HashMap<Seq, BTreeSet<u32>>,
    resolved_reports: BTreeSet<(Seq, u32)>,
    session: Option<Session>,
    known_places: HashMap<ParticipantNo, ClientId>,
    clients: HashMap<ClientId, (UserId, u64)>,
    rates: HashMap<ClientId, RateBucket>,
    pending_ends: Vec<(LiveSessionId, ParticipantNo, GoneReason)>,
    /// The latest presence note of each place; memory only.
    presence: crate::live_presence::PresenceBook,
}

impl<S: LiveStore> LiveRoom<S> {
    pub(crate) async fn load(
        doc: DocId,
        store: S,
        events: EventSink,
        timing: LiveTiming,
    ) -> Result<Self, RoomFailed> {
        let loaded = store.load_live(doc).await?;
        let current = store.current_revision(doc).await?;
        Self::from_loaded(doc, store, events, timing, loaded, current)
    }

    fn from_loaded(
        doc: DocId,
        store: S,
        events: EventSink,
        timing: LiveTiming,
        loaded: LiveLoaded,
        current_revision: Option<String>,
    ) -> Result<Self, RoomFailed> {
        let base_revision = loaded
            .verified_checkpoint
            .as_ref()
            .map(|checkpoint| checkpoint.revision.clone());
        let base_session_data = loaded
            .verified_checkpoint
            .as_ref()
            .map_or_else(Vec::new, |checkpoint| checkpoint.session_data.clone());
        let mut entries = Vec::with_capacity(loaded.entries.len());
        let mut witnesses = HashMap::new();
        for row in loaded.entries {
            let seq = match &row {
                LiveEntryRow::Change { seq, .. } | LiveEntryRow::Cancel { seq, .. } => *seq,
            };
            if seq <= loaded.verified_through {
                continue;
            }
            if let LiveEntryRow::Change {
                seq,
                witnesses: row_witnesses,
                cancelled_by: None,
                ..
            } = &row
            {
                witnesses.insert(*seq, row_witnesses.clone());
            }
            entries.push(wire_entry(row)?);
        }
        let mut known_places = HashMap::new();
        let mut cancel_book = CancelBook::default();
        let mut clients = loaded
            .clients
            .iter()
            .map(|(client, user, next)| (*client, (*user, *next)))
            .collect::<HashMap<_, _>>();
        let mut members = Members::new(doc, events.clone());
        for place in &loaded.participants {
            known_places.insert(ParticipantNo(place.number), place.client);
            if place.gone.is_some() {
                let _ = cancel_book.gone(place.number);
            }
            clients.entry(place.client).or_insert((place.user, 1));
            let _ = members.claim(place.client, place.user);
        }
        let now = Instant::now();
        let session = loaded.open_session.map(|open| {
            let places = open
                .participants
                .into_iter()
                .map(|place| {
                    let participant = ParticipantNo(place.number);
                    (
                        participant,
                        LivePlace {
                            client: place.client,
                            user: place.user,
                            sign_in: None,
                            name: String::new(),
                            can_edit: false,
                            can_checkpoint: false,
                            build: LiveBuild::Browser,
                            held: place.gone.is_none(),
                            seen: Seq(0),
                            last_heard: now,
                        },
                    )
                })
                .collect();
            Session {
                id: LiveSessionId(open.id),
                format: open.format,
                first_seq: open.first_seq,
                places,
            }
        });
        let newest_unverified = loaded
            .unverified_checkpoints
            .iter()
            .max_by_key(|checkpoint| checkpoint.through);
        let saved_through = newest_unverified.map_or(loaded.verified_through, |checkpoint| {
            checkpoint.through.max(loaded.verified_through)
        });
        // A live checkpoint is the base of the next; only a document with none starts from its
        // current revision.
        let checkpoint_base = newest_unverified
            .map(|checkpoint| checkpoint.revision.clone())
            .or_else(|| base_revision.clone())
            .or(current_revision);
        let checkpoint_writers = loaded
            .unverified_checkpoints
            .iter()
            .filter_map(|checkpoint| {
                checkpoint.writer.map(|writer| {
                    (
                        checkpoint.revision.clone(),
                        CheckpointWriter {
                            participant: ParticipantNo(writer.participant),
                            client: writer.client,
                            build: None,
                        },
                    )
                })
            })
            .collect::<HashMap<_, _>>();
        let unverified_checkpoints = loaded
            .unverified_checkpoints
            .into_iter()
            .map(|mut checkpoint| {
                checkpoint.session_data = Vec::new();
                checkpoint
            })
            .collect();
        let unsaved = (loaded.head > saved_through).then_some(now);
        let session_changes = session.as_ref().map_or(0, |session| {
            entries
                .iter()
                .filter(|entry| {
                    entry.seq >= session.first_seq && matches!(entry.body, EntryBody::Change { .. })
                })
                .count()
        });
        let session_changes = u64::try_from(session_changes).unwrap_or(u64::MAX);
        let (written_tx, written_rx) = mpsc::unbounded_channel();
        Ok(Self {
            doc,
            store,
            events: events.clone(),
            timing,
            members,
            head: loaded.head,
            verified_through: loaded.verified_through,
            base_revision,
            _base_session_data: base_session_data,
            unverified_checkpoints,
            checkpoint_base,
            saved_through,
            first_unsaved: unsaved,
            last_change: unsaved,
            tickets: Vec::new(),
            last_writer: None,
            ticket_misses: HashMap::new(),
            checkpoint_blocked: false,
            pending_reconcile: None,
            writing: false,
            written_tx,
            written_rx,
            checkpoint_writers,
            lapsed: BTreeSet::new(),
            verify_hold: None,
            forensic_last: HashMap::new(),
            forensic_asked: BTreeSet::new(),
            not_asked: HashMap::new(),
            forensic_due: None,
            agreement: BTreeMap::new(),
            session_changes,
            names: HashMap::new(),
            _unsaved_entries: loaded.unsaved_entries,
            _unsaved_bytes: loaded.unsaved_bytes,
            _retained_session_data: loaded.retained_session_data,
            format: loaded.format,
            entries,
            witnesses,
            cancel_book,
            viewer_reports: HashMap::new(),
            resolved_reports: BTreeSet::new(),
            session,
            known_places,
            clients,
            rates: HashMap::new(),
            pending_ends: Vec::new(),
            presence: crate::live_presence::PresenceBook::default(),
        })
    }

    /// Records a committed checkpoint's metadata, reporting a fault event when it fails.
    #[cfg(test)]
    async fn reconcile_live_checkpoint(
        &self,
        parent: Option<&str>,
        checkpoint: &NewLiveCheckpoint<'_>,
        described: &RevisionInfo,
        expected_fields: &Map<String, Value>,
    ) -> Result<(), RoomFailed> {
        if let Err(error) = self
            .store
            .reconcile_live_checkpoint(self.doc, parent, checkpoint, described, expected_fields)
            .await
        {
            self.reconcile_fault(checkpoint.revision, checkpoint.through);
            return Err(error.into());
        }
        Ok(())
    }

    /// The one fault event of a checkpoint whose database record is not known to be stored.
    fn reconcile_fault(&self, revision: &str, through: Seq) {
        self.events.emit(NewEvent {
            kind: "live.checkpoint_reconcile_fault",
            actor: None,
            subject: None,
            document: Some(self.doc),
            node: Some(self.doc.into()),
            detail: serde_json::json!({ "revision": revision, "through": through.0 }),
        });
    }

    pub(crate) async fn handle(&mut self, cmd: LiveCommand) -> Result<(), RoomFailed> {
        let handled = self.handle_command(cmd).await;
        if handled.is_ok() {
            self.issue_due_tickets();
        }
        handled
    }

    async fn handle_command(&mut self, cmd: LiveCommand) -> Result<(), RoomFailed> {
        match cmd {
            LiveCommand::Join {
                conn,
                client,
                user,
                session,
                name,
                role,
                protocol,
                format,
                resume,
                seen,
                can_checkpoint,
                build,
                tx,
                accepted,
            } => {
                self.join(JoinRequest {
                    conn,
                    client,
                    user,
                    sign_in: session,
                    name,
                    role,
                    protocol,
                    format,
                    resume,
                    seen,
                    can_checkpoint,
                    build,
                    tx,
                    accepted,
                })
                .await
            }
            LiveCommand::Entry {
                conn,
                client,
                local,
                seen,
                format,
                payload,
            } => self.entry(conn, client, local, seen, format, payload).await,
            LiveCommand::Heartbeat { conn, client, seen } => {
                self.note_life(conn, client);
                let verdicts = self.note_seen(conn, client, seen);
                self.apply_verdicts(verdicts).await
            }
            LiveCommand::Report { conn, client, seq } => self.report(conn, client, seq).await,
            LiveCommand::Fingerprint {
                conn,
                client,
                at,
                entered,
            } => {
                self.note_life(conn, client);
                self.fingerprint(conn, client, at, entered);
                Ok(())
            }
            LiveCommand::Resync { conn, client } => {
                self.note_life(conn, client);
                self.resync(conn, client);
                Ok(())
            }
            LiveCommand::Leave { conn, client } => {
                self.note_life(conn, client);
                self.leave(conn, client).await
            }
            LiveCommand::Disconnected { conn, client } => {
                self.disconnect(conn, client);
                Ok(())
            }
            LiveCommand::CheckpointFailed {
                conn,
                client,
                ticket,
            } => {
                self.note_life(conn, client);
                if let Some(participant) = self.current_participant(conn, client) {
                    self.cancel_tickets(
                        |held| held.id == ticket && held.participant == participant,
                        true,
                    );
                }
                Ok(())
            }
            LiveCommand::CheckpointClaim {
                user,
                ticket,
                at,
                base,
                reply,
            } => {
                let claimed = self
                    .usable_ticket(user, ticket, at, base.as_ref(), TicketPurpose::Write)
                    .and_then(|index| self.tickets.get_mut(index))
                    .filter(|held| !held.claimed)
                    .map(|held| held.claimed = true)
                    .is_some();
                let _ = reply.send(claimed);
                Ok(())
            }
            LiveCommand::CheckpointUpload {
                user,
                ticket,
                at,
                base,
                blob,
                session_data,
                reply,
            } => {
                self.checkpoint_upload(user, ticket, at, base, blob, session_data, reply);
                Ok(())
            }
            LiveCommand::CheckpointHash {
                user,
                ticket,
                at,
                sha256,
                reply,
            } => match self.checkpoint_hash(user, ticket, at, sha256).await {
                Ok(answer) => {
                    let _ = reply.send(answer);
                    Ok(())
                }
                Err(error) => {
                    let _ = reply.send(LiveCheckpointReply::Failed);
                    Err(error)
                }
            },
            LiveCommand::CheckpointWritten(done) => {
                self.checkpoint_written(*done);
                Ok(())
            }
            LiveCommand::Presence {
                conn,
                client,
                payload,
            } => {
                self.note_life(conn, client);
                self.relay_presence(conn, client, payload);
                Ok(())
            }
        }
    }

    /// The index of `user`'s unexpired ticket `ticket` for `purpose` at exactly `at` on
    /// exactly `base` (any base for a verify ticket), held by a place that may still edit.
    fn usable_ticket(
        &self,
        user: UserId,
        ticket: Uuid,
        at: Seq,
        base: Option<&RevisionId>,
        purpose: TicketPurpose,
    ) -> Option<usize> {
        let now = Instant::now();
        let base = base.map(ToString::to_string);
        self.tickets.iter().position(|held| {
            held.id == ticket
                && held.purpose == purpose
                && held.user == user
                && held.at == at
                && (purpose == TicketPurpose::Verify || held.base == base)
                && held.expires > now
                && self.place_can_checkpoint(held.participant, held.client)
        })
    }

    /// Whether `participant` holds its place for `client`, may edit and is connected.
    fn place_can_checkpoint(&self, participant: ParticipantNo, client: ClientId) -> bool {
        self.session
            .as_ref()
            .and_then(|session| session.places.get(&participant))
            .is_some_and(|place| {
                place.held && place.can_edit && place.can_checkpoint && place.client == client
            })
            && self
                .members
                .iter()
                .any(|member| member.client == client && member.role.can_edit())
    }

    /// Removes the tickets `which` picks; `missed` counts them against their holders, so the
    /// next ticket prefers someone else.
    fn cancel_tickets(&mut self, which: impl Fn(&Ticket) -> bool, missed: bool) {
        let mut kept = Vec::with_capacity(self.tickets.len());
        for held in std::mem::take(&mut self.tickets) {
            if which(&held) {
                if missed {
                    let misses = self.ticket_misses.entry(held.participant).or_insert(0);
                    *misses = misses.saturating_add(1);
                    if held.purpose == TicketPurpose::Verify || held.forensic_of.is_some() {
                        self.lapsed.insert(held.participant);
                    }
                    if let Some(revision) = held.verifies.clone() {
                        self.not_asked
                            .entry(revision)
                            .or_default()
                            .insert(held.participant);
                    }
                }
            } else {
                kept.push(held);
            }
        }
        self.tickets = kept;
    }

    /// When a write checkpoint is due: unsaved changes, no write ticket out, nothing waiting
    /// to reconcile, and the quiet or the longest wait reached.
    fn write_due_at(&self) -> Option<Instant> {
        let due = self.plain_write_due_at()?;
        // An outstanding verify ticket goes first, so its checkpoint is re-derived before
        // newer work is asked for, but holds a due write back for at most VERIFY_HOLD_MAX.
        if self.verify_outstanding() {
            return Some(match self.verify_hold {
                Some(since) => due.max(since + VERIFY_HOLD_MAX),
                None => due.max(Instant::now() + VERIFY_HOLD_MAX),
            });
        }
        Some(due)
    }

    fn verify_outstanding(&self) -> bool {
        self.tickets
            .iter()
            .any(|held| held.purpose == TicketPurpose::Verify)
    }

    /// When a write checkpoint is due, outstanding verify tickets aside.
    fn plain_write_due_at(&self) -> Option<Instant> {
        if self.checkpoint_blocked
            || self.pending_reconcile.is_some()
            || self.writing
            || self.forensic_due.is_some()
            || self.head <= self.saved_through
            || self
                .tickets
                .iter()
                .any(|held| held.purpose == TicketPurpose::Write)
        {
            return None;
        }
        let quiet = self.last_change.map(|at| at + self.timing.checkpoint_quiet);
        let wait = self
            .first_unsaved
            .map(|at| at + self.timing.checkpoint_wait);
        match (quiet, wait) {
            (Some(quiet), Some(wait)) => Some(quiet.min(wait)),
            (quiet, wait) => quiet.or(wait).or_else(|| Some(Instant::now())),
        }
    }

    /// Eligible ticket holders, best first: connected editors whose host can checkpoint and
    /// who hold no ticket, fewest missed tickets first, then (for a write) not the last
    /// writer, (for a verify) the other build kind than `writer_build`, then by number.
    fn ticket_candidates(
        &self,
        exclude: Option<ParticipantNo>,
        writer_build: Option<LiveBuild>,
    ) -> Vec<(ParticipantNo, ClientId, UserId)> {
        let Some(session) = &self.session else {
            return Vec::new();
        };
        let mut candidates = session
            .places
            .iter()
            .filter(|(participant, place)| {
                Some(**participant) != exclude
                    && self.place_can_checkpoint(**participant, place.client)
                    && !self
                        .tickets
                        .iter()
                        .any(|held| held.participant == **participant)
            })
            .map(|(participant, place)| {
                (
                    (
                        self.ticket_misses.get(participant).copied().unwrap_or(0),
                        Some(*participant) == self.last_writer,
                        writer_build.is_some_and(|build| build == place.build),
                        *participant,
                    ),
                    (*participant, place.client, place.user),
                )
            })
            .collect::<Vec<_>>();
        candidates.sort_by_key(|(key, _)| *key);
        candidates
            .into_iter()
            .map(|(_, candidate)| candidate)
            .collect()
    }

    /// Issues the tickets now due, in order: a disagreeing verifier's forensic write, a verify
    /// ticket for the newest unverified checkpoint, then an ordinary write ticket.
    fn issue_due_tickets(&mut self) {
        self.issue_forensic_ticket();
        self.issue_verify_ticket();
        let now = Instant::now();
        if !self.verify_outstanding() {
            self.verify_hold = None;
        } else if self.verify_hold.is_none()
            && self.plain_write_due_at().is_some_and(|due| due <= now)
        {
            self.verify_hold = Some(now);
        }
        if !self.write_due_at().is_some_and(|due| due <= now) {
            return;
        }
        self.verify_hold = None;
        if let Some((participant, client, user)) =
            self.ticket_candidates(None, None).first().copied()
        {
            let at = self.head;
            let base = self.checkpoint_base.clone();
            self.issue_ticket(
                TicketPurpose::Write,
                (participant, client, user),
                at,
                base,
                None,
                None,
            );
        }
    }

    /// Asks a verifier that disagreed for its own bytes, once no write is outstanding; drops
    /// the request when that verifier can no longer checkpoint.
    fn issue_forensic_ticket(&mut self) {
        if self.writing
            || self.checkpoint_blocked
            || self.pending_reconcile.is_some()
            || self
                .tickets
                .iter()
                .any(|held| held.purpose == TicketPurpose::Write)
        {
            return;
        }
        let Some(due) = self.forensic_due.take() else {
            return;
        };
        if due.at <= self.verified_through
            || !self.place_can_checkpoint(due.participant, due.client)
            || self
                .tickets
                .iter()
                .any(|held| held.participant == due.participant)
        {
            return;
        }
        let base = self.checkpoint_base.clone();
        self.forensic_last.insert(due.user, Instant::now());
        self.forensic_asked.insert(due.revision.clone());
        self.issue_ticket(
            TicketPurpose::Write,
            (due.participant, due.client, due.user),
            due.at,
            base,
            None,
            Some(due.revision),
        );
    }

    /// Asks one eligible editor, never the writer or a verifier that disagreed, to re-derive
    /// the newest unverified checkpoint.
    fn issue_verify_ticket(&mut self) {
        if self
            .tickets
            .iter()
            .any(|held| held.purpose == TicketPurpose::Verify)
        {
            return;
        }
        let Some(target) = self
            .unverified_checkpoints
            .iter()
            .filter(|checkpoint| checkpoint.through > self.verified_through)
            .max_by_key(|checkpoint| checkpoint.through)
        else {
            return;
        };
        let revision = target.revision.clone();
        let at = target.through;
        let writer = self.checkpoint_writers.get(&revision).copied();
        let not_asked = self.not_asked.get(&revision);
        let Some(verifier) = self
            .ticket_candidates(
                writer.map(|writer| writer.participant),
                writer.and_then(|writer| writer.build),
            )
            .into_iter()
            .find(|(participant, client, _)| {
                writer.is_none_or(|writer| writer.client != *client)
                    && !self.lapsed.contains(participant)
                    && not_asked.is_none_or(|set| !set.contains(participant))
            })
        else {
            return;
        };
        self.issue_ticket(
            TicketPurpose::Verify,
            verifier,
            at,
            Some(revision.clone()),
            Some(revision),
            None,
        );
    }

    fn issue_ticket(
        &mut self,
        purpose: TicketPurpose,
        (participant, client, user): (ParticipantNo, ClientId, UserId),
        at: Seq,
        base: Option<String>,
        verifies: Option<String>,
        forensic_of: Option<String>,
    ) {
        let id = Uuid::new_v4();
        let frame = encode(&LiveServerMsg::CheckpointTicket {
            ticket: id,
            purpose,
            at,
            base: base.clone(),
            major: None,
        });
        self.tickets.push(Ticket {
            id,
            purpose,
            participant,
            client,
            user,
            at,
            base,
            verifies,
            forensic_of,
            expires: Instant::now() + self.timing.ticket_lifetime,
            claimed: false,
        });
        let _ = self.members.send(Some(client), &Outbound::One(frame));
    }

    /// Everyone credited on a checkpoint through `at`: the people whose standing changes lie
    /// after the last stored checkpoint and at or before `at`, in first-contribution order;
    /// the uploader alone when there are none.
    fn checkpoint_authors(&self, at: Seq, uploader: UserId) -> Vec<Author> {
        let mut users = Vec::new();
        for entry in &self.entries {
            if entry.seq <= self.saved_through || entry.seq > at {
                continue;
            }
            if let EntryBody::Change {
                client,
                cancelled: false,
                ..
            } = &entry.body
                && let Some((user, _)) = self.clients.get(client)
                && !users.contains(user)
            {
                users.push(*user);
            }
        }
        if users.is_empty() {
            users.push(uploader);
        }
        users
            .into_iter()
            .map(|user| Author {
                id: user.0.to_string(),
                name: credited_name(self.names.get(&user).map_or("", String::as_str)),
            })
            .collect()
    }

    /// Starts storing an upload for the write ticket `ticket`. The Git write runs off the room
    /// task, which keeps ordering entries meanwhile; its result comes back as
    /// [`LiveCommand::CheckpointWritten`]. No other write ticket is issued until then.
    #[allow(clippy::too_many_arguments)]
    fn checkpoint_upload(
        &mut self,
        user: UserId,
        ticket: Uuid,
        at: Seq,
        base: Option<RevisionId>,
        blob: BlobRef,
        session_data: Vec<u8>,
        reply: oneshot::Sender<LiveCheckpointReply>,
    ) {
        let Some(index) = self.usable_ticket(user, ticket, at, base.as_ref(), TicketPurpose::Write)
        else {
            let _ = reply.send(LiveCheckpointReply::Refused);
            return;
        };
        // One use: whatever happens now, the ticket is spent.
        let held = self.tickets.remove(index);
        if session_data.len() > MAX_SESSION_DATA_BYTES || at > self.head {
            let _ = reply.send(LiveCheckpointReply::Refused);
            return;
        }
        let Some(session) = self.session.as_ref().map(|session| session.id) else {
            let _ = reply.send(LiveCheckpointReply::Failed);
            return;
        };
        if self.writing {
            // Cannot happen: one write ticket at a time, and none while a write runs.
            let _ = reply.send(LiveCheckpointReply::Failed);
            return;
        }
        let authors = if held.forensic_of.is_some() {
            vec![Author {
                id: user.0.to_string(),
                name: credited_name(self.names.get(&user).map_or("", String::as_str)),
            }]
        } else {
            self.checkpoint_authors(at, user)
        };
        let now = OffsetDateTime::now_utc();
        // PostgreSQL keeps microseconds; a reconcile compares this exact time.
        let created_at = now
            .replace_nanosecond(now.nanosecond() / 1_000 * 1_000)
            .unwrap_or(now);
        self.writing = true;
        let store = self.store.clone();
        let doc = self.doc;
        let written = self.written_tx.clone();
        let write_base = held.base.clone();
        let forensic_of = held.forensic_of.clone();
        let writer = Some(LiveCheckpointWriter {
            participant: held.participant.0,
            client: held.client,
        });
        tokio::spawn(async move {
            let job = tokio::spawn(async move {
                let write = LiveWrite {
                    base: write_base.as_deref(),
                    blob,
                    authors: &authors,
                    at,
                    session: session.0,
                    session_data: &session_data,
                    created_at,
                    writer,
                };
                let result = match forensic_of.as_deref() {
                    None => WriteResult::Checkpoint(store.write_live_checkpoint(doc, &write).await),
                    Some(agreed) => {
                        WriteResult::Forensic(store.write_live_forensic(doc, &write, agreed).await)
                    }
                };
                (result, session_data)
            });
            let (result, session_data) = match job.await {
                Ok(done) => done,
                Err(error) => (
                    WriteResult::Checkpoint(Err(LiveWriteError::Failed(format!(
                        "checkpoint write task failed: {error}"
                    )))),
                    Vec::new(),
                ),
            };
            let done = WriteDone {
                result,
                ticket: held,
                user,
                at,
                sha256: blob.sha256,
                session: session.0,
                session_data,
                created_at,
                reply,
            };
            // A room that has ended drops the reply, which the caller reads as a failure; what
            // was written is durable and the next load reads it.
            let _ = written.send(LiveCommand::CheckpointWritten(Box::new(done)));
        });
    }

    /// Applies a finished checkpoint write and answers its upload.
    fn checkpoint_written(&mut self, done: WriteDone) {
        self.writing = false;
        let WriteDone {
            result,
            ticket: held,
            user,
            at,
            sha256,
            session,
            session_data,
            created_at,
            reply,
        } = done;
        let answer = match result {
            WriteResult::Forensic(Ok(written)) => {
                let of = held.forensic_of.clone().unwrap_or_default();
                self.checkpoint_base = Some(written.current.clone());
                self.events.emit(NewEvent {
                    kind: "live.checkpoint_forensic",
                    actor: Some(user),
                    subject: None,
                    document: Some(self.doc),
                    node: Some(self.doc.into()),
                    detail: serde_json::json!({
                        "revision": written.forensic,
                        "of": of,
                        "through": at.0,
                    }),
                });
                LiveCheckpointReply::Saved {
                    revision: written.forensic,
                }
            }
            WriteResult::Checkpoint(Ok(revision)) => {
                self.checkpoint_stored(
                    revision.clone(),
                    at,
                    sha256,
                    session,
                    user,
                    (held.participant, held.client),
                );
                LiveCheckpointReply::Saved { revision }
            }
            WriteResult::Checkpoint(Err(LiveWriteError::Stale { current }))
            | WriteResult::Forensic(Err(LiveWriteError::Stale { current })) => {
                self.checkpoint_blocked = true;
                self.events.emit(NewEvent {
                    kind: "live.checkpoint_stale",
                    actor: Some(user),
                    subject: None,
                    document: Some(self.doc),
                    node: Some(self.doc.into()),
                    detail: serde_json::json!({
                        "base": held.base,
                        "current": current,
                        "through": at.0,
                    }),
                });
                LiveCheckpointReply::Stale { current }
            }
            WriteResult::Checkpoint(Err(LiveWriteError::Failed(error)))
            | WriteResult::Forensic(Err(LiveWriteError::Failed(error))) => {
                eprintln!(
                    "sharecraft-server: live checkpoint of document {} failed: {error}",
                    self.doc.0
                );
                LiveCheckpointReply::Failed
            }
            WriteResult::Forensic(Err(LiveWriteError::Unrecorded {
                revision, error, ..
            })) => {
                // A forensic revision records no checkpoint, so this is not expected; Git holds
                // it and the next start reconciles the index.
                eprintln!(
                    "sharecraft-server: forensic revision {revision} of document {} is not \
                     recorded: {error}",
                    self.doc.0
                );
                LiveCheckpointReply::Failed
            }
            WriteResult::Checkpoint(Err(LiveWriteError::Unrecorded {
                revision,
                fields,
                described,
                error,
            })) => {
                eprintln!(
                    "sharecraft-server: live checkpoint {revision} of document {} is in Git but \
                     its record is not stored yet: {error}",
                    self.doc.0
                );
                // One fault event for the whole episode; the retries back off quietly.
                self.reconcile_fault(&revision, at);
                self.pending_reconcile = Some(PendingReconcile {
                    parent: held.base,
                    revision,
                    at,
                    sha256,
                    session,
                    session_data,
                    created_at,
                    described: described.map(|described| *described),
                    fields,
                    user,
                    participant: held.participant,
                    client: held.client,
                    retry_at: Instant::now() + RECONCILE_RETRY,
                    delay: RECONCILE_RETRY.saturating_mul(2).min(RECONCILE_RETRY_MAX),
                });
                LiveCheckpointReply::Failed
            }
        };
        let _ = reply.send(answer);
    }

    /// The effects of a stored, unverified checkpoint: the next checkpoint builds on it, its
    /// position is saved, and (through [`Self::issue_due_tickets`]) a different editor is asked
    /// to verify it.
    fn checkpoint_stored(
        &mut self,
        revision: String,
        at: Seq,
        sha256: [u8; 32],
        session: Uuid,
        user: UserId,
        (participant, client): (ParticipantNo, ClientId),
    ) {
        self.saved_through = self.saved_through.max(at);
        self.checkpoint_base = Some(revision.clone());
        self.last_writer = Some(participant);
        self.ticket_misses.remove(&participant);
        self.unverified_checkpoints
            .push(records::LiveCheckpointRow {
                revision: revision.clone(),
                through: at,
                sha256,
                verified: false,
                session,
                session_data: Vec::new(),
                major_reason: None,
                major_person: None,
                writer: Some(LiveCheckpointWriter {
                    participant: participant.0,
                    client,
                }),
            });
        if self.head <= at {
            self.first_unsaved = None;
            self.last_change = None;
        } else {
            self.first_unsaved = Some(Instant::now());
        }
        for (kind, detail) in [
            (
                "revision.saved",
                serde_json::json!({
                    "revision": revision,
                    "live_through": at.0,
                    "verified": false,
                }),
            ),
            (
                "live.checkpoint_unverified",
                serde_json::json!({ "revision": revision, "through": at.0 }),
            ),
        ] {
            self.events.emit(NewEvent {
                kind,
                actor: Some(user),
                subject: None,
                document: Some(self.doc),
                node: Some(self.doc.into()),
                detail,
            });
        }
        let build = self
            .session
            .as_ref()
            .and_then(|session| session.places.get(&participant))
            .map(|place| place.build);
        self.checkpoint_writers.insert(
            revision.clone(),
            CheckpointWriter {
                participant,
                client,
                build,
            },
        );
        self.notify_saved(revision, at, false, false);
    }

    /// Compares a verify ticket's independently written hash with the stored checkpoint. A
    /// match from another editing participant verifies it and advances the room's base; a
    /// mismatch keeps it unverified with its log and asks the verifier for its own bytes.
    async fn checkpoint_hash(
        &mut self,
        user: UserId,
        ticket: Uuid,
        at: Seq,
        sha256: [u8; 32],
    ) -> Result<LiveCheckpointReply, RoomFailed> {
        let Some(index) = self.usable_ticket(user, ticket, at, None, TicketPurpose::Verify) else {
            return Ok(LiveCheckpointReply::Refused);
        };
        let held = self.tickets.remove(index);
        let Some(revision) = held.verifies.clone() else {
            return Ok(LiveCheckpointReply::Refused);
        };
        if self
            .checkpoint_writers
            .get(&revision)
            .is_some_and(|writer| {
                writer.participant == held.participant || writer.client == held.client
            })
        {
            return Ok(LiveCheckpointReply::Refused);
        }
        let Some(stored) = self
            .unverified_checkpoints
            .iter()
            .find(|checkpoint| checkpoint.revision == revision && checkpoint.through == at)
            .map(|checkpoint| checkpoint.sha256)
        else {
            // Covered by a later verified checkpoint meanwhile: nothing left to verify.
            return Ok(LiveCheckpointReply::Accepted);
        };
        if at <= self.verified_through {
            return Ok(LiveCheckpointReply::Accepted);
        }
        if stored == sha256 {
            self.store
                .verify_live_checkpoint(self.doc, &revision, at, OffsetDateTime::now_utc())
                .await?;
            self.checkpoint_verified(revision, at, user);
        } else {
            self.not_asked
                .entry(revision.clone())
                .or_default()
                .insert(held.participant);
            self.lapsed.insert(held.participant);
            self.events.emit(NewEvent {
                kind: "live.checkpoint_mismatch",
                actor: Some(user),
                subject: None,
                document: Some(self.doc),
                node: Some(self.doc.into()),
                detail: serde_json::json!({ "revision": revision, "through": at.0 }),
            });
            // At most one forensic upload per checkpoint, and per person in FORENSIC_EVERY.
            let recent = self
                .forensic_last
                .get(&held.user)
                .is_some_and(|last| last.elapsed() < FORENSIC_EVERY);
            if !recent && !self.forensic_asked.contains(&revision) {
                self.forensic_due = Some(ForensicDue {
                    revision,
                    at,
                    participant: held.participant,
                    client: held.client,
                    user: held.user,
                });
            }
        }
        Ok(LiveCheckpointReply::Accepted)
    }

    /// The room's base is now the verified checkpoint `revision` at `at`: earlier unverified
    /// checkpoints, entries and agreement positions are no longer needed in memory.
    fn checkpoint_verified(&mut self, revision: String, at: Seq, user: UserId) {
        let major = self
            .unverified_checkpoints
            .iter()
            .any(|checkpoint| checkpoint.revision == revision && checkpoint.major_reason.is_some());
        self.verified_through = at;
        self.base_revision = Some(revision.clone());
        // The verified base's identity data is in the store; this room does not read it.
        self._base_session_data = Vec::new();
        self.unverified_checkpoints
            .retain(|checkpoint| checkpoint.through > at);
        let kept = self
            .unverified_checkpoints
            .iter()
            .map(|checkpoint| checkpoint.revision.clone())
            .collect::<BTreeSet<_>>();
        self.checkpoint_writers
            .retain(|revision, _| kept.contains(revision));
        self.not_asked.retain(|revision, _| kept.contains(revision));
        self.forensic_asked
            .retain(|revision| kept.contains(revision));
        self.forensic_last
            .retain(|_, last| last.elapsed() < FORENSIC_EVERY);
        if self.forensic_due.as_ref().is_some_and(|due| due.at <= at) {
            self.forensic_due = None;
        }
        self.cancel_tickets(
            |held| held.purpose == TicketPurpose::Verify && held.at <= at,
            false,
        );
        self.entries.retain(|entry| entry.seq > at);
        self.witnesses.retain(|seq, _| *seq > at);
        self.viewer_reports.retain(|seq, _| *seq > at);
        self.agreement.retain(|seq, _| *seq > at);
        self.events.emit(NewEvent {
            kind: "live.checkpoint_verified",
            actor: Some(user),
            subject: None,
            document: Some(self.doc),
            node: Some(self.doc.into()),
            detail: serde_json::json!({ "revision": revision, "through": at.0 }),
        });
        self.notify_saved(revision, at, true, major);
    }

    /// Records an editing participant's entered digest at an agreement position. The position
    /// is compared once every editing person has reported, or [`AGREEMENT_WAIT`] after its
    /// first report; a later report is compared with the agreed digest.
    fn fingerprint(&mut self, conn: ConnId, client: ClientId, at: Seq, entered: String) {
        let Some(participant) = self.current_participant(conn, client) else {
            return;
        };
        let Some(user) = self
            .members
            .iter()
            .find(|member| member.conn == conn && member.client == client && member.role.can_edit())
            .map(|member| member.user)
        else {
            return;
        };
        if !valid_fingerprint(&entered)
            || at.0 == 0
            || !at.0.is_multiple_of(AGREEMENT_EVERY)
            || at <= self.verified_through
            || at > self.head
        {
            return;
        }
        if !self.agreement.contains_key(&at) && self.agreement.len() >= MAX_AGREEMENT_BUCKETS {
            self.agreement.pop_first();
        }
        let bucket = self.agreement.entry(at).or_default();
        if bucket.reports.len() >= MAX_AGREEMENT_REPORTS
            || bucket
                .reports
                .iter()
                .any(|(reporter, _, _)| *reporter == participant)
        {
            return;
        }
        bucket.reports.push((participant, user, entered.clone()));
        bucket.first.get_or_insert_with(Instant::now);
        if let Some(decided) = bucket.decided.clone() {
            let disagrees = decided.is_some_and(|agreed| agreed != entered);
            if disagrees && bucket.resynced.insert(participant) {
                let reports = bucket.reports.len();
                self.resync_reporters(at, reports, &[participant]);
            }
            return;
        }
        let expected = self
            .session
            .as_ref()
            .into_iter()
            .flat_map(|session| session.places.values())
            .filter(|place| place.held && place.can_edit)
            .map(|place| place.user)
            .collect::<BTreeSet<_>>();
        let reported = self
            .agreement
            .get(&at)
            .map_or_else(BTreeSet::new, |bucket| {
                bucket
                    .reports
                    .iter()
                    .map(|(_, user, _)| *user)
                    .collect::<BTreeSet<_>>()
            });
        if expected.is_subset(&reported) {
            self.decide_agreement(at);
        }
    }

    /// Compares the reports at `at`, counting each person once (their first report), and
    /// resyncs the clients that disagree: the minority when at least three people reported
    /// and a strict majority agrees, otherwise everyone when they disagree.
    fn decide_agreement(&mut self, at: Seq) {
        let Some(bucket) = self.agreement.get_mut(&at) else {
            return;
        };
        if bucket.decided.is_some() {
            return;
        }
        let mut voices = Vec::<(UserId, &str)>::new();
        for (_, user, digest) in &bucket.reports {
            if !voices.iter().any(|(voter, _)| voter == user) {
                voices.push((*user, digest.as_str()));
            }
        }
        let mut counts = BTreeMap::<&str, usize>::new();
        for (_, digest) in &voices {
            *counts.entry(digest).or_default() += 1;
        }
        let total = voices.len();
        let agreed = if counts.len() == 1 {
            counts.keys().next().map(|digest| (*digest).to_owned())
        } else if total >= 3 {
            counts
                .iter()
                .find(|(_, count)| **count * 2 > total)
                .map(|(digest, _)| (*digest).to_owned())
        } else {
            None
        };
        let odd = bucket
            .reports
            .iter()
            .filter(|(_, _, digest)| agreed.as_ref().is_none_or(|agreed| agreed != digest))
            .map(|(reporter, _, _)| *reporter)
            .filter(|reporter| !bucket.resynced.contains(reporter))
            .collect::<Vec<_>>();
        bucket.decided = Some(agreed);
        bucket.resynced.extend(odd.iter().copied());
        let reports = bucket.reports.len();
        if !odd.is_empty() {
            self.resync_reporters(at, reports, &odd);
        }
    }

    /// Compares every agreement position whose wait is over.
    fn decide_due_agreements(&mut self) {
        let now = Instant::now();
        let due = self
            .agreement
            .iter()
            .filter(|(_, bucket)| {
                bucket.decided.is_none()
                    && bucket
                        .first
                        .is_some_and(|first| first + AGREEMENT_WAIT <= now)
            })
            .map(|(at, _)| *at)
            .collect::<Vec<_>>();
        for at in due {
            self.decide_agreement(at);
        }
    }

    /// Resyncs the reporters `odd` at agreement position `at` and records it.
    fn resync_reporters(&mut self, at: Seq, reports: usize, odd: &[ParticipantNo]) {
        for reporter in odd {
            let reporter_client = self
                .session
                .as_ref()
                .and_then(|session| session.places.get(reporter))
                .map(|place| place.client);
            if let Some(reporter_client) = reporter_client {
                self.send_resync(reporter_client);
            }
        }
        self.events.emit(NewEvent {
            kind: "live.agreement_mismatch",
            actor: None,
            subject: None,
            document: Some(self.doc),
            node: Some(self.doc.into()),
            detail: serde_json::json!({
                "at": at.0,
                "reports": reports,
                "resynced": odd.iter().map(|reporter| reporter.0).collect::<Vec<_>>(),
            }),
        });
    }

    /// Tries again to record a checkpoint Git committed, once its retry time has come: reads
    /// Git's description first when the write could not, then records the metadata. A failed
    /// try waits twice as long as the last, up to [`RECONCILE_RETRY_MAX`].
    async fn retry_reconcile(&mut self) {
        let Some(mut pending) = self
            .pending_reconcile
            .take_if(|pending| pending.retry_at <= Instant::now())
        else {
            return;
        };
        if pending.described.is_none()
            && let Ok(Some(described)) = self
                .store
                .describe_live_revision(self.doc, &pending.revision)
                .await
        {
            pending.described = Some(described);
        }
        let reconciled = match &pending.described {
            Some(described) => {
                let checkpoint = NewLiveCheckpoint {
                    revision: &pending.revision,
                    through: pending.at,
                    sha256: pending.sha256,
                    session: pending.session,
                    session_data: &pending.session_data,
                    major_reason: None,
                    major_person: None,
                    writer: Some(LiveCheckpointWriter {
                        participant: pending.participant.0,
                        client: pending.client,
                    }),
                    created_at: pending.created_at,
                };
                self.store
                    .reconcile_live_checkpoint(
                        self.doc,
                        pending.parent.as_deref(),
                        &checkpoint,
                        described,
                        &pending.fields,
                    )
                    .await
                    .is_ok()
            }
            None => false,
        };
        if reconciled {
            self.checkpoint_stored(
                pending.revision.clone(),
                pending.at,
                pending.sha256,
                pending.session,
                pending.user,
                (pending.participant, pending.client),
            );
            self.events.emit(NewEvent {
                kind: "live.checkpoint_reconciled",
                actor: Some(pending.user),
                subject: None,
                document: Some(self.doc),
                node: Some(self.doc.into()),
                detail: serde_json::json!({
                    "revision": pending.revision,
                    "through": pending.at.0,
                }),
            });
        } else {
            let delay = pending.delay;
            self.pending_reconcile = Some(PendingReconcile {
                retry_at: Instant::now() + delay,
                delay: delay.saturating_mul(2).min(RECONCILE_RETRY_MAX),
                ..pending
            });
        }
    }

    /// The next time the room must act on its own: a place or ticket expiring, a reconcile
    /// to retry, or a checkpoint falling due that someone can take.
    pub(crate) fn next_wake(&self) -> Option<Instant> {
        let due = self
            .write_due_at()
            .filter(|_| !self.ticket_candidates(None, None).is_empty());
        [
            self.next_expiry(),
            self.tickets.iter().map(|held| held.expires).min(),
            self.pending_reconcile
                .as_ref()
                .map(|pending| pending.retry_at),
            due,
            self.agreement
                .values()
                .filter(|bucket| bucket.decided.is_none())
                .filter_map(|bucket| bucket.first)
                .min()
                .map(|first| first + AGREEMENT_WAIT),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    pub(crate) async fn settle(&mut self) -> Result<(), RoomFailed> {
        let at = OffsetDateTime::now_utc();
        while let Some((session, participant, reason)) = self.pending_ends.first().copied() {
            self.store
                .end_live_place(session.0, participant.0, reason, at)
                .await?;
            self.pending_ends.remove(0);
            self.apply_place_end(session, participant, reason);
            let verdicts = self.gone_verdicts(participant);
            self.apply_verdicts(verdicts).await?;
        }
        let should_end = self
            .session
            .as_ref()
            .is_some_and(|session| session.places.values().all(|place| !place.held));
        if should_end && let Some(session) = &self.session {
            let session_id = session.id;
            let entries = self.session_changes;
            self.store.end_live_session(session_id.0, at).await?;
            self.events.emit(NewEvent {
                kind: "live.session_ended",
                actor: None,
                subject: None,
                document: Some(self.doc),
                node: Some(self.doc.into()),
                detail: serde_json::json!({ "head": self.head.0, "entries": entries }),
            });
            self.session = None;
        }
        Ok(())
    }

    pub(crate) fn next_expiry(&self) -> Option<Instant> {
        let lifetime = self.timing.heartbeat.saturating_mul(self.timing.misses);
        self.session
            .as_ref()
            .into_iter()
            .flat_map(|session| session.places.values())
            .filter(|place| place.held)
            .map(|place| place.last_heard + lifetime)
            .min()
    }

    pub(crate) async fn expire_due(&mut self) -> Result<(), RoomFailed> {
        let now = Instant::now();
        let lifetime = self.timing.heartbeat.saturating_mul(self.timing.misses);
        let due = self
            .session
            .as_ref()
            .into_iter()
            .flat_map(|session| &session.places)
            .filter_map(|(participant, place)| {
                (place.held && place.last_heard + lifetime <= now).then_some(*participant)
            })
            .collect::<Vec<_>>();
        for participant in due {
            self.end_place(participant, GoneReason::Expired).await?;
        }
        self.cancel_tickets(|held| held.expires <= now, true);
        self.retry_reconcile().await;
        self.decide_due_agreements();
        self.issue_due_tickets();
        Ok(())
    }

    /// Waits for every checkpoint write in progress and applies its result.
    #[cfg(test)]
    pub(crate) async fn finish_writes(&mut self) -> Result<(), RoomFailed> {
        while self.writing {
            match self.written_rx.recv().await {
                Some(command) => self.handle(command).await?,
                None => break,
            }
        }
        Ok(())
    }

    /// Whether a checkpoint write is running off the room task.
    pub(crate) fn writing(&self) -> bool {
        self.writing
    }

    pub(crate) fn has_session(&self) -> bool {
        self.session.is_some()
    }

    async fn join(&mut self, request: JoinRequest) -> Result<(), RoomFailed> {
        let expected = self
            .session
            .as_ref()
            .map(|session| session.format.as_str())
            .or(self.format.as_deref());
        if request.protocol != LIVE_PROTOCOL
            || !valid_format(&request.format)
            || expected.is_some_and(|format| format != request.format)
        {
            let message = LiveServerMsg::Incompatible {
                protocol: LIVE_PROTOCOL,
                format: expected.unwrap_or(&request.format).to_owned(),
            };
            send_direct(&request.tx, Outbound::One(encode(&message)));
            let _ = request.accepted.send(JoinReply::Refused);
            return Ok(());
        }
        if self
            .clients
            .get(&request.client)
            .is_some_and(|(owner, _)| *owner != request.user)
            || !self.members.claim(request.client, request.user)
        {
            let _ = request.accepted.send(JoinReply::Refused);
            return Ok(());
        }

        if self.session.is_none() {
            let id = LiveSessionId(Uuid::new_v4());
            let at = OffsetDateTime::now_utc();
            self.store
                .start_live_session(self.doc, id.0, &request.format, at)
                .await?;
            self.events.emit(NewEvent {
                kind: "live.session_started",
                actor: Some(request.user),
                subject: Some(request.user),
                document: Some(self.doc),
                node: Some(self.doc.into()),
                detail: serde_json::json!({ "format": request.format }),
            });
            self.session = Some(Session {
                id,
                format: request.format.clone(),
                first_seq: Seq(self.head.0.saturating_add(1)),
                places: HashMap::new(),
            });
            self.session_changes = 0;
        }

        let kept = request.resume.and_then(|resume| {
            self.session.as_ref().and_then(|session| {
                (resume.session == session.id)
                    .then(|| session.places.get(&resume.participant))
                    .flatten()
                    .filter(|place| place.held && place.client == request.client)
                    .map(|_| resume.participant)
            })
        });
        let returning = request.resume.filter(|resume| {
            kept.is_none() && self.known_places.get(&resume.participant) == Some(&request.client)
        });
        let (participant, place) = if let Some(participant) = kept {
            (participant, Place::Kept)
        } else {
            let session = self.session.as_ref().ok_or_else(no_session)?;
            let number = self
                .store
                .add_live_participant(
                    self.doc,
                    session.id.0,
                    request.client,
                    request.user,
                    OffsetDateTime::now_utc(),
                )
                .await?;
            if number == 0 {
                return Err(RoomFailed(DbError::new(
                    "live store returned participant zero",
                )));
            }
            let participant = ParticipantNo(number);
            self.known_places.insert(participant, request.client);
            let place = returning.map_or(Place::New, |resume| Place::Returning {
                previous: resume.participant,
            });
            if let Place::Returning { previous } = place {
                self.events.emit(NewEvent {
                    kind: "live.client_returned",
                    actor: Some(request.user),
                    subject: Some(request.user),
                    document: Some(self.doc),
                    node: Some(self.doc.into()),
                    detail: serde_json::json!({
                        "participant": participant.0,
                        "previous": previous.0,
                    }),
                });
            }
            (participant, place)
        };

        if let Some(session) = &mut self.session {
            let stated = request.seen.unwrap_or(Seq(0)).min(self.head);
            if let Some(existing) = session.places.get_mut(&participant) {
                existing.user = request.user;
                existing.sign_in = Some(request.sign_in);
                existing.name = request.name.clone();
                existing.can_edit = request.role.can_edit();
                existing.can_checkpoint = request.can_checkpoint;
                existing.build = request.build;
                existing.held = true;
                existing.seen = existing.seen.max(stated);
                existing.last_heard = Instant::now();
            } else {
                session.places.insert(
                    participant,
                    LivePlace {
                        client: request.client,
                        user: request.user,
                        sign_in: Some(request.sign_in),
                        name: request.name.clone(),
                        can_edit: request.role.can_edit(),
                        can_checkpoint: request.can_checkpoint,
                        build: request.build,
                        held: true,
                        seen: stated,
                        last_heard: Instant::now(),
                    },
                );
            }
        }
        self.names.insert(request.user, request.name.clone());
        // Joining again makes a verifier that lapsed eligible again.
        self.lapsed.remove(&participant);
        let member = Member {
            conn: request.conn,
            client: request.client,
            user: request.user,
            session: request.sign_in,
            name: request.name,
            role: request.role,
            cursor: None,
            tx: request.tx,
        };
        if !matches!(self.members.admit(member), Admission::Joined { .. }) {
            let _ = request.accepted.send(JoinReply::Refused);
            return Ok(());
        }
        self.rates
            .entry(request.client)
            .or_insert_with(RateBucket::full);
        let _ = request.accepted.send(JoinReply::Accepted);

        let from = request
            .seen
            .filter(|seen| *seen >= self.verified_through && *seen <= self.head)
            .unwrap_or(self.verified_through);
        let mut frames = vec![
            encode(&LiveServerMsg::LiveJoined {
                session: self.session.as_ref().ok_or_else(no_session)?.id,
                participant,
                place,
                can_edit: self
                    .members
                    .iter()
                    .find(|member| member.client == request.client)
                    .is_some_and(|member| member.role.can_edit()),
                heartbeat_ms: u64::try_from(self.timing.heartbeat.as_millis())
                    .unwrap_or(HEARTBEAT_MS),
                people: self.people(),
            }),
            encode(&LiveServerMsg::Catchup {
                base: self.verified_through,
                revision: self.base_revision.clone(),
                from,
                head: self.head,
            }),
        ];
        frames.extend(
            self.entries
                .iter()
                .filter(|entry| entry.seq > from)
                .map(|entry| {
                    encode(&LiveServerMsg::Entry {
                        entry: entry.clone(),
                    })
                }),
        );
        let _ = self
            .members
            .send(Some(request.client), &Outbound::Many(frames));
        self.after_join(request.client, participant, place);
        self.broadcast_people();
        let stated = self
            .session
            .as_ref()
            .and_then(|session| session.places.get(&participant))
            .map_or(Seq(0), |place| place.seen);
        let verdicts = self.seen_verdicts(participant, stated);
        self.apply_verdicts(verdicts).await
    }

    async fn entry(
        &mut self,
        conn: ConnId,
        client: ClientId,
        local: LocalNo,
        seen: Seq,
        format: String,
        payload: String,
    ) -> Result<(), RoomFailed> {
        if self.current_participant(conn, client).is_none() {
            return Ok(());
        }
        self.note_life(conn, client);
        if !store_local(local.0) || !store_number(seen.0) {
            self.refuse(client, local, RefusedReason::OutOfOrder);
            return Ok(());
        }
        let verdicts = self.note_seen(conn, client, seen);
        self.apply_verdicts(verdicts).await?;
        let Some(participant) = self.current_participant(conn, client) else {
            return Ok(());
        };
        let (user, can_edit) = match self
            .members
            .iter()
            .find(|member| member.conn == conn && member.client == client)
        {
            Some(member) => (member.user, member.role.can_edit()),
            None => return Ok(()),
        };
        match self.store.live_local(self.doc, client, local.0).await? {
            LiveLocal::Stored { .. } => return Ok(()),
            LiveLocal::Refused { reason } => {
                self.refuse(client, local, reason_from_store(&reason));
                return Ok(());
            }
            LiveLocal::Unseen => {}
        }
        let next_local = self.clients.get(&client).map_or(1, |(_, next)| *next);
        if local.0 < next_local {
            self.refuse(client, local, RefusedReason::OutOfOrder);
            return Ok(());
        }

        let reason = if !can_edit {
            Some(RefusedReason::ReadOnly)
        } else if self
            .session
            .as_ref()
            .is_none_or(|session| session.format != format)
        {
            Some(RefusedReason::WrongFormat)
        } else if payload.len() > MAX_PAYLOAD_BYTES {
            Some(RefusedReason::TooLarge)
        } else if seen > self.head {
            Some(RefusedReason::BaseAhead)
        } else if self.head.0 >= i64::MAX as u64 {
            Some(RefusedReason::OutOfOrder)
        } else if !self
            .rates
            .entry(client)
            .or_insert_with(RateBucket::full)
            .take()
        {
            Some(RefusedReason::RateLimited)
        } else {
            None
        };
        if let Some(reason) = reason {
            let at = OffsetDateTime::now_utc();
            self.store
                .consume_live_local(self.doc, client, user, local.0, reason_name(reason), at)
                .await?;
            self.advance_local(client, user, local);
            self.refuse(client, local, reason);
            return Ok(());
        }

        let seq = Seq(self
            .head
            .0
            .checked_add(1)
            .ok_or_else(|| RoomFailed(DbError::new("live sequence exhausted")))?);
        let at = OffsetDateTime::now_utc();
        let seed = random_seed()?;
        let witnesses = self.witnesses();
        let session = self.session.as_ref().ok_or_else(no_session)?.id;
        self.store
            .append_live_change(
                self.doc,
                &NewLiveChange {
                    seq,
                    session: session.0,
                    client,
                    local: local.0,
                    participant: participant.0,
                    user,
                    seen,
                    at,
                    seed,
                    format: &format,
                    payload: payload.as_bytes(),
                    witnesses: &witnesses,
                },
            )
            .await?;
        let entry = LiveEntry {
            seq,
            at_ms: millis(at)?,
            seed,
            body: EntryBody::Change {
                participant,
                client,
                local,
                seen,
                payload,
                cancelled: false,
            },
        };
        self.head = seq;
        self.format = Some(format);
        self.entries.push(entry.clone());
        self.witnesses.insert(seq, witnesses);
        self.advance_local(client, user, local);
        self.note_unsaved_change();
        self.session_changes = self.session_changes.saturating_add(1);
        let _ = self.members.send(
            None,
            &Outbound::One(encode(&LiveServerMsg::Entry { entry })),
        );
        Ok(())
    }

    fn note_unsaved_change(&mut self) {
        let now = Instant::now();
        self.first_unsaved.get_or_insert(now);
        self.last_change = Some(now);
    }

    fn current_participant(&self, conn: ConnId, client: ClientId) -> Option<ParticipantNo> {
        self.members
            .iter()
            .any(|member| member.conn == conn && member.client == client)
            .then(|| {
                self.session.as_ref().and_then(|session| {
                    session.places.iter().find_map(|(number, place)| {
                        (place.held && place.client == client).then_some(*number)
                    })
                })
            })
            .flatten()
    }

    fn note_seen(&mut self, conn: ConnId, client: ClientId, seen: Seq) -> Vec<Verdict> {
        let Some(participant) = self.current_participant(conn, client) else {
            return Vec::new();
        };
        let Some(place) = self
            .session
            .as_mut()
            .and_then(|session| session.places.get_mut(&participant))
        else {
            return Vec::new();
        };
        place.seen = place.seen.max(seen.min(self.head));
        let position = place.seen;
        self.seen_verdicts(participant, position)
    }

    fn seen_verdicts(&mut self, participant: ParticipantNo, position: Seq) -> Vec<Verdict> {
        let mut verdicts = self.cancel_book.seen(participant.0, position);
        let already_standing = verdicts
            .iter()
            .filter_map(|verdict| match verdict {
                Verdict::Stands { seq, .. } => Some(*seq),
                Verdict::Cancel { .. } => None,
            })
            .collect::<BTreeSet<_>>();
        verdicts.extend(
            self.viewer_reports
                .keys()
                .filter(|seq| !already_standing.contains(seq))
                .filter(|seq| {
                    position >= **seq
                        && self
                            .witnesses
                            .get(seq)
                            .is_some_and(|witnesses| witnesses.contains(&participant.0))
                })
                .map(|seq| Verdict::Stands {
                    seq: *seq,
                    reporters: Vec::new(),
                }),
        );
        verdicts
    }

    fn note_life(&mut self, conn: ConnId, client: ClientId) {
        let Some(participant) = self.current_participant(conn, client) else {
            return;
        };
        if let Some(place) = self
            .session
            .as_mut()
            .and_then(|session| session.places.get_mut(&participant))
        {
            place.last_heard = Instant::now();
        }
    }

    async fn leave(&mut self, conn: ConnId, client: ClientId) -> Result<(), RoomFailed> {
        let Some(participant) = self.current_participant(conn, client) else {
            return Ok(());
        };
        self.end_place(participant, GoneReason::Left).await
    }

    fn disconnect(&mut self, conn: ConnId, client: ClientId) {
        if self.members.leave(conn, client).is_none() {
            return;
        }
        // Not connected, so it cannot be asked: the next eligible editor is.
        self.cancel_tickets(|held| held.client == client, true);
        self.broadcast_people();
    }

    async fn end_place(
        &mut self,
        participant: ParticipantNo,
        reason: GoneReason,
    ) -> Result<(), RoomFailed> {
        let Some(session) = self.session.as_ref() else {
            return Ok(());
        };
        if session
            .places
            .get(&participant)
            .is_none_or(|place| !place.held)
        {
            return Ok(());
        }
        let session_id = session.id;
        if let Err(error) = self
            .store
            .end_live_place(
                session_id.0,
                participant.0,
                reason,
                OffsetDateTime::now_utc(),
            )
            .await
        {
            let pending = (session_id, participant, reason);
            if !self.pending_ends.contains(&pending) {
                self.pending_ends.push(pending);
            }
            return Err(error.into());
        }
        self.pending_ends
            .retain(|pending| *pending != (session_id, participant, reason));
        self.apply_place_end(session_id, participant, reason);
        let verdicts = self.gone_verdicts(participant);
        self.apply_verdicts(verdicts).await
    }

    fn gone_verdicts(&mut self, participant: ParticipantNo) -> Vec<Verdict> {
        let mut verdicts = self.cancel_book.gone(participant.0);
        let resolved = verdicts
            .iter()
            .map(|verdict| match verdict {
                Verdict::Cancel { seq, .. } | Verdict::Stands { seq, .. } => *seq,
            })
            .collect::<BTreeSet<_>>();
        let held = self.seen_positions();
        verdicts.extend(
            self.viewer_reports
                .keys()
                .filter(|seq| !resolved.contains(seq))
                .filter(|seq| {
                    self.witnesses.get(seq).is_some_and(|witnesses| {
                        witnesses.iter().all(|witness| !held.contains_key(witness))
                    })
                })
                .map(|seq| Verdict::Stands {
                    seq: *seq,
                    reporters: Vec::new(),
                }),
        );
        verdicts
    }

    fn apply_place_end(
        &mut self,
        session_id: LiveSessionId,
        participant: ParticipantNo,
        reason: GoneReason,
    ) {
        let Some(place) = self
            .session
            .as_mut()
            .filter(|session| session.id == session_id)
            .and_then(|session| session.places.get_mut(&participant))
        else {
            return;
        };
        if !place.held {
            return;
        }
        place.held = false;
        let client = place.client;
        let user = place.user;
        self.cancel_tickets(|held| held.participant == participant, true);
        if reason == GoneReason::Expired {
            let _ = self.members.send(
                Some(client),
                &Outbound::One(encode(&LiveServerMsg::Expired)),
            );
            self.events.emit(NewEvent {
                kind: "live.client_expired",
                actor: None,
                subject: Some(user),
                document: Some(self.doc),
                node: Some(self.doc.into()),
                detail: serde_json::json!({ "participant": participant.0 }),
            });
        }
        if let Some(conn) = self
            .members
            .iter()
            .find(|member| member.client == client)
            .map(|member| member.conn)
        {
            let _ = self.members.leave(conn, client);
        }
        self.broadcast_people();
    }

    fn advance_local(&mut self, client: ClientId, user: UserId, local: LocalNo) {
        let next = local.0.saturating_add(1);
        self.clients
            .entry(client)
            .and_modify(|(_, current)| *current = (*current).max(next))
            .or_insert((user, next));
    }

    fn refuse(&mut self, client: ClientId, local: LocalNo, reason: RefusedReason) {
        let _ = self.members.send(
            Some(client),
            &Outbound::One(encode(&LiveServerMsg::Refused { local, reason })),
        );
    }

    async fn report(&mut self, conn: ConnId, client: ClientId, seq: Seq) -> Result<(), RoomFailed> {
        let Some(participant) = self.current_participant(conn, client) else {
            return Ok(());
        };
        self.note_life(conn, client);
        let can_edit = self
            .members
            .iter()
            .find(|member| member.conn == conn && member.client == client)
            .is_some_and(|member| member.role.can_edit());
        let Some(witnesses) = self.witnesses.get(&seq).cloned() else {
            return Ok(());
        };
        if !can_edit {
            let positions = self.seen_positions();
            let stands = self.cancel_book.is_standing(seq)
                || witnesses
                    .iter()
                    .any(|witness| positions.get(witness).is_some_and(|seen| *seen >= seq));
            if stands {
                return self
                    .apply_verdicts(vec![Verdict::Stands {
                        seq,
                        reporters: vec![participant.0],
                    }])
                    .await;
            }
            self.viewer_reports
                .entry(seq)
                .or_default()
                .insert(participant.0);
            return Ok(());
        }
        let positions = self.seen_positions();
        let verdict = self
            .cancel_book
            .report(seq, &witnesses, participant.0, &|number| {
                positions.get(&number).copied().unwrap_or(Seq(0))
            });
        self.apply_verdicts(verdict.into_iter().collect()).await
    }

    fn resync(&mut self, conn: ConnId, client: ClientId) {
        if self.current_participant(conn, client).is_none() {
            return;
        }
        self.send_resync(client);
    }

    /// Sends `client` the log again from the verified base.
    fn send_resync(&mut self, client: ClientId) {
        let mut frames = vec![encode(&LiveServerMsg::Catchup {
            base: self.verified_through,
            revision: self.base_revision.clone(),
            from: self.verified_through,
            head: self.head,
        })];
        frames.extend(
            self.entries
                .iter()
                .filter(|entry| entry.seq > self.verified_through)
                .map(|entry| {
                    encode(&LiveServerMsg::Entry {
                        entry: entry.clone(),
                    })
                }),
        );
        let _ = self.members.send(Some(client), &Outbound::Many(frames));
    }

    fn seen_positions(&self) -> HashMap<u32, Seq> {
        self.session
            .as_ref()
            .into_iter()
            .flat_map(|session| &session.places)
            .filter_map(|(participant, place)| place.held.then_some((participant.0, place.seen)))
            .collect()
    }

    async fn apply_verdicts(&mut self, verdicts: Vec<Verdict>) -> Result<(), RoomFailed> {
        let mut pending = VecDeque::from(verdicts);
        while let Some(verdict) = pending.pop_front() {
            match verdict {
                Verdict::Cancel { seq, reporters } => {
                    self.cancel_entry(seq, reporters).await?;
                }
                Verdict::Stands { seq, reporters } => {
                    let mut all = reporters.into_iter().collect::<BTreeSet<_>>();
                    if let Some(viewers) = self.viewer_reports.remove(&seq) {
                        all.extend(viewers);
                    }
                    for reporter in all {
                        pending.extend(self.cannot_open(ParticipantNo(reporter), seq).await?);
                    }
                }
            }
        }
        Ok(())
    }

    async fn cancel_entry(&mut self, cancels: Seq, reporters: Vec<u32>) -> Result<(), RoomFailed> {
        let Some(index) = self.entries.iter().position(|entry| {
            entry.seq == cancels
                && matches!(
                    entry.body,
                    EntryBody::Change {
                        cancelled: false,
                        ..
                    }
                )
        }) else {
            return Ok(());
        };
        let seq = Seq(self
            .head
            .0
            .checked_add(1)
            .ok_or_else(|| RoomFailed(DbError::new("live sequence exhausted")))?);
        let reports = u32::try_from(reporters.len())
            .map_err(|_| RoomFailed(DbError::new("too many live crash reports")))?;
        let session = self.session.as_ref().ok_or_else(no_session)?.id;
        let at = OffsetDateTime::now_utc();
        let seed = random_seed()?;
        self.store
            .cancel_live_entry(
                self.doc,
                &NewLiveCancel {
                    seq,
                    session: session.0,
                    cancels,
                    reports,
                    at,
                    seed,
                },
            )
            .await?;
        let Some(entry) = self.entries.get_mut(index) else {
            return Err(RoomFailed(DbError::new("live cancel target disappeared")));
        };
        let EntryBody::Change {
            payload, cancelled, ..
        } = &mut entry.body
        else {
            return Err(RoomFailed(DbError::new("live cancel target changed kind")));
        };
        payload.clear();
        *cancelled = true;
        let cancel = LiveEntry {
            seq,
            at_ms: millis(at)?,
            seed,
            body: EntryBody::Cancel { cancels, reports },
        };
        self.head = seq;
        self.note_unsaved_change();
        self.entries.push(cancel.clone());
        self.witnesses.remove(&cancels);
        self.viewer_reports.remove(&cancels);
        let _ = self.members.send(
            None,
            &Outbound::One(encode(&LiveServerMsg::Entry { entry: cancel })),
        );
        self.events.emit(NewEvent {
            kind: "live.entry_cancelled",
            actor: None,
            subject: None,
            document: Some(self.doc),
            node: Some(self.doc.into()),
            detail: serde_json::json!({
                "seq": cancels.0,
                "at": seq.0,
                "reports": reports,
            }),
        });
        Ok(())
    }

    async fn cannot_open(
        &mut self,
        participant: ParticipantNo,
        seq: Seq,
    ) -> Result<Vec<Verdict>, RoomFailed> {
        if self.resolved_reports.contains(&(seq, participant.0)) {
            return Ok(Vec::new());
        }
        let Some((session, client, user, held)) = self.session.as_ref().and_then(|session| {
            session
                .places
                .get(&participant)
                .map(|place| (session.id, place.client, place.user, place.held))
        }) else {
            return Ok(Vec::new());
        };
        if held {
            self.store
                .end_live_place(
                    session.0,
                    participant.0,
                    GoneReason::CannotOpen,
                    OffsetDateTime::now_utc(),
                )
                .await?;
        }
        self.resolved_reports.insert((seq, participant.0));
        if held {
            let _ = self.members.send(
                Some(client),
                &Outbound::One(encode(&LiveServerMsg::CannotOpen { seq })),
            );
        }
        self.events.emit(NewEvent {
            kind: "live.entry_failed_on_device",
            actor: Some(user),
            subject: Some(user),
            document: Some(self.doc),
            node: Some(self.doc.into()),
            detail: serde_json::json!({ "seq": seq.0 }),
        });
        if held {
            self.apply_place_end(session, participant, GoneReason::CannotOpen);
            Ok(self.gone_verdicts(participant))
        } else {
            Ok(Vec::new())
        }
    }

    fn witnesses(&self) -> Vec<u32> {
        self.session
            .as_ref()
            .into_iter()
            .flat_map(|session| &session.places)
            .filter_map(|(number, place)| (place.held && place.can_edit).then_some(number.0))
            .collect()
    }

    fn people(&self) -> Vec<LivePerson> {
        let mut people = self
            .session
            .as_ref()
            .into_iter()
            .flat_map(|session| &session.places)
            .filter(|(_, place)| place.held)
            .map(|(participant, place)| LivePerson {
                participant: *participant,
                client: place.client,
                user: place.user,
                name: place.name.clone(),
                can_edit: place.can_edit,
                connected: self
                    .members
                    .iter()
                    .any(|member| member.client == place.client),
            })
            .collect::<Vec<_>>();
        people.sort_by_key(|person| person.participant);
        people
    }

    fn broadcast_people(&mut self) {
        let frame = encode(&LiveServerMsg::People {
            people: self.people(),
        });
        let _ = self.members.send(None, &Outbound::One(frame));
        self.drop_absent_presence();
    }

    fn prepare_access(&mut self, change: &AccessChange) {
        let Some(session) = &self.session else {
            return;
        };
        let ending = session
            .places
            .iter()
            .filter(|(_, place)| place.held)
            .filter(|(_, place)| match change {
                AccessChange::SetAccess { user, role: None }
                | AccessChange::RemoveUser { user } => place.user == *user,
                AccessChange::EndSession { session } => place.sign_in == Some(*session),
                AccessChange::Deleted | AccessChange::Ended { .. } => true,
                AccessChange::SetAccess { role: Some(_), .. }
                | AccessChange::Renamed { .. }
                | AccessChange::ConnectedUsers { .. } => false,
            })
            .map(|(participant, _)| (session.id, *participant, GoneReason::Removed))
            .collect::<Vec<_>>();
        for pending in ending {
            if !self.pending_ends.contains(&pending) {
                self.pending_ends.push(pending);
            }
        }
    }
}

impl<S: LiveStore> AccessRules for LiveRoom<S> {
    fn change_role(&mut self, user: UserId, role: Access) {
        let can_edit = role.can_edit();
        if let Some(session) = &mut self.session {
            for place in session
                .places
                .values_mut()
                .filter(|place| place.user == user)
            {
                place.can_edit = can_edit;
            }
        }
        let change = self.members.change_role(user, role);
        if !can_edit {
            self.cancel_tickets(|held| held.user == user, true);
        }
        if change.changed || change.departed.any() {
            self.broadcast_people();
        }
    }

    fn remove(&mut self, which: &dyn Fn(&Member) -> bool, reason: RemovedReason) {
        let ending = self
            .members
            .iter()
            .filter(|member| which(member))
            .filter_map(|member| {
                let session = self.session.as_ref()?;
                let participant = session.places.iter().find_map(|(number, place)| {
                    (place.client == member.client && place.held).then_some(*number)
                })?;
                Some((session.id, participant, GoneReason::Removed))
            })
            .collect::<Vec<_>>();
        for pending in ending {
            if !self.pending_ends.contains(&pending) {
                self.pending_ends.push(pending);
            }
        }
        let _ = self.members.remove(which, reason);
    }

    fn rename(&mut self, _name: String) {}

    fn connected_users(&self) -> Vec<UserId> {
        let mut users = members::users_of(self.members.iter());
        for user in self
            .session
            .as_ref()
            .into_iter()
            .flat_map(|session| session.places.values())
            .filter(|place| place.held)
            .map(|place| place.user)
        {
            if !users.contains(&user) {
                users.push(user);
            }
        }
        users
    }
}

/// Loads and runs one live room. Access changes have priority over commands, and expiry is
/// driven by the earliest held place. A store failure ends the task and drops every connection.
async fn run(
    place: RegistryPlace,
    db: crate::live_store::LiveDb,
    events: EventSink,
    timing: LiveTiming,
    mut rx: mpsc::Receiver<LiveCommand>,
    mut access_rx: mpsc::UnboundedReceiver<AccessChange>,
) {
    let doc = place.doc();
    let mut room = match LiveRoom::load(doc, db, events, timing).await {
        Ok(room) => room,
        Err(error) => {
            eprintln!(
                "sharecraft-server: cannot open live document {}: {error}",
                doc.0
            );
            place.close();
            access_rx.close();
            refuse_waiting(&mut rx, |command| match command {
                LiveCommand::Join { accepted, .. } => Some(accepted),
                _ => None,
            });
            return;
        }
    };
    let mut access_open = true;
    loop {
        let wake = room.next_wake();
        let handled = tokio::select! {
            biased;
            change = access_rx.recv(), if access_open => match change {
                Some(change) => {
                    room.prepare_access(&change);
                    members::apply_access(&mut room, change);
                    true
                }
                None => {
                    access_open = false;
                    true
                }
            },
            written = room.written_rx.recv() => match written {
                Some(command) => room.handle(command).await.is_ok(),
                None => true,
            },
            command = rx.recv() => match command {
                Some(command) => room.handle(command).await.is_ok(),
                None => false,
            },
            () = sleep_until(wake) => room.expire_due().await.is_ok(),
        };
        if !handled || room.settle().await.is_err() {
            break;
        }
        if !room.has_session() && !room.writing() && rx.is_empty() && access_rx.is_empty() {
            break;
        }
    }
    place.close();
    access_rx.close();
    rx.close();
    while rx.try_recv().is_ok() {}
}

struct JoinRequest {
    conn: ConnId,
    client: ClientId,
    user: UserId,
    sign_in: SessionId,
    name: String,
    role: Access,
    protocol: u32,
    format: String,
    resume: Option<LiveResume>,
    seen: Option<Seq>,
    can_checkpoint: bool,
    build: LiveBuild,
    tx: mpsc::Sender<Outbound>,
    accepted: oneshot::Sender<JoinReply>,
}

struct LiveFrames;

impl Frames for LiveFrames {
    fn people(participants: Vec<protocol::Participant>) -> Option<Utf8Bytes> {
        let mut people = Vec::with_capacity(participants.len());
        for (index, person) in participants.into_iter().enumerate() {
            people.push(LivePerson {
                participant: ParticipantNo(u32::try_from(index).ok()?.checked_add(1)?),
                client: person.client,
                user: person.user,
                name: person.name,
                can_edit: person.can_edit,
                connected: true,
            });
        }
        Some(encode(&LiveServerMsg::People { people }))
    }

    fn access(can_edit: bool) -> Option<Utf8Bytes> {
        Some(encode(&LiveServerMsg::Access { can_edit }))
    }

    fn removed(reason: RemovedReason) -> Option<Utf8Bytes> {
        Some(encode(&LiveServerMsg::Removed { reason }))
    }
}

fn wire_entry(row: LiveEntryRow) -> Result<LiveEntry, DbError> {
    match row {
        LiveEntryRow::Change {
            seq,
            client,
            local,
            participant,
            seen,
            at,
            seed,
            payload,
            cancelled_by,
            ..
        } => Ok(LiveEntry {
            seq,
            at_ms: millis(at)?,
            seed,
            body: EntryBody::Change {
                participant: ParticipantNo(participant),
                client,
                local: LocalNo(local),
                seen,
                payload: if cancelled_by.is_some() {
                    String::new()
                } else {
                    String::from_utf8(payload).map_err(DbError::new)?
                },
                cancelled: cancelled_by.is_some(),
            },
        }),
        LiveEntryRow::Cancel {
            seq,
            at,
            seed,
            cancels,
            reports,
            ..
        } => Ok(LiveEntry {
            seq,
            at_ms: millis(at)?,
            seed,
            body: EntryBody::Cancel { cancels, reports },
        }),
    }
}

fn millis(at: OffsetDateTime) -> Result<i64, DbError> {
    i64::try_from(at.unix_timestamp_nanos() / 1_000_000)
        .map_err(|_| DbError::new("live timestamp is out of range"))
}

fn random_seed() -> Result<u32, RoomFailed> {
    let mut bytes = [0; 4];
    getrandom::fill(&mut bytes).map_err(|error| RoomFailed(DbError::new(error)))?;
    Ok(u32::from_le_bytes(bytes))
}

fn encode(message: &LiveServerMsg) -> Utf8Bytes {
    Utf8Bytes::from(
        serde_json::to_string(message).unwrap_or_else(|_| {
            "{\"type\":\"incompatible\",\"protocol\":1,\"format\":\"\"}".into()
        }),
    )
}

fn send_direct(tx: &mpsc::Sender<Outbound>, outbound: Outbound) {
    let _ = tx.try_send(outbound);
}

fn no_session() -> RoomFailed {
    RoomFailed(DbError::new("live room has no open session"))
}

fn reason_name(reason: RefusedReason) -> &'static str {
    match reason {
        RefusedReason::ReadOnly => "read_only",
        RefusedReason::WrongFormat => "wrong_format",
        RefusedReason::TooLarge => "too_large",
        RefusedReason::RateLimited => "rate_limited",
        RefusedReason::BaseAhead => "base_ahead",
        RefusedReason::OutOfOrder => "out_of_order",
        RefusedReason::Unknown => "unknown",
    }
}

fn reason_from_store(reason: &str) -> RefusedReason {
    match reason {
        "read_only" => RefusedReason::ReadOnly,
        "wrong_format" => RefusedReason::WrongFormat,
        "too_large" => RefusedReason::TooLarge,
        "rate_limited" => RefusedReason::RateLimited,
        "base_ahead" => RefusedReason::BaseAhead,
        "out_of_order" => RefusedReason::OutOfOrder,
        _ => RefusedReason::Unknown,
    }
}

/// PostgreSQL stores live positions as signed 64-bit integers.
fn store_number(value: u64) -> bool {
    i64::try_from(value).is_ok()
}

/// A stored local also advances `next_local`, so it must leave room for `local + 1`.
fn store_local(value: u64) -> bool {
    value < i64::MAX as u64
}

#[cfg(test)]
#[allow(
    clippy::too_many_arguments,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use collab_core::ids::{ClientId, DocId, Seq, SessionId, UserId};
    use protocol::live::{
        EntryBody, LiveBuild, LiveResume, LiveServerMsg, LocalNo, ParticipantNo, Place,
        RefusedReason, TicketPurpose,
    };
    use records::NewLiveCheckpoint;
    use revisions::{BlobRef, RevisionInfo};
    use serde_json::{Map, Value, json};
    use tokio::sync::{mpsc, oneshot};
    use uuid::Uuid;

    use super::{LiveCheckpointReply, LiveCommand, LiveRoom, LiveTiming, start_sweeper_task};
    use crate::events::EventSink;
    use crate::live_store::MemStore;
    use crate::members::{AccessRules, builtin_access};
    use crate::room::{JoinReply, Outbound};

    // Not a `gridcraft/` format: desktop builds, which several tests use, are refused there.
    const FORMAT: &str = "stand-in/1";

    struct Joined {
        conn: u64,
        client: ClientId,
        rx: mpsc::Receiver<Outbound>,
        session: protocol::live::LiveSessionId,
        participant: ParticipantNo,
        place: Place,
        initial: Vec<LiveServerMsg>,
        /// Frames sent right after the join's own, such as a checkpoint ticket.
        after_join: Vec<LiveServerMsg>,
    }

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn timing() -> LiveTiming {
        LiveTiming {
            heartbeat: Duration::from_secs(60),
            misses: 3,
            checkpoint_quiet: super::CHECKPOINT_QUIET,
            checkpoint_wait: super::CHECKPOINT_WAIT,
            ticket_lifetime: super::TICKET_LIFETIME,
        }
    }

    fn checkpoint<'a>(
        revision: &'a str,
        through: Seq,
        session: protocol::live::LiveSessionId,
        session_data: &'a [u8],
    ) -> NewLiveCheckpoint<'a> {
        NewLiveCheckpoint {
            revision,
            through,
            sha256: [7; 32],
            session: session.0,
            session_data,
            major_reason: None,
            major_person: None,
            writer: None,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
        }
    }

    async fn room() -> (LiveRoom<MemStore>, MemStore) {
        let store = MemStore::new();
        let room = LiveRoom::load(
            DocId(id(100)),
            store.clone(),
            EventSink::discard(),
            timing(),
        )
        .await
        .unwrap();
        (room, store)
    }

    #[tokio::test(start_paused = true)]
    async fn live_sweeper_runs_now_hourly_and_stops_when_dropped() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&calls);
        let sweeper = start_sweeper_task(move || {
            counted.fetch_add(1, Ordering::SeqCst);
            std::future::ready(())
        });
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1, "the first sweep runs now");

        tokio::time::advance(Duration::from_secs(3_599)).await;
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "the boundary starts a sweep"
        );

        drop(sweeper);
        tokio::time::advance(Duration::from_secs(3_600)).await;
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 2, "drop stops later sweeps");
    }

    fn messages(outbound: Outbound) -> Vec<LiveServerMsg> {
        let frames = match outbound {
            Outbound::One(frame) => vec![frame],
            Outbound::Many(frames) => frames,
        };
        frames
            .into_iter()
            .map(|frame| serde_json::from_str(frame.as_str()).unwrap())
            .collect()
    }

    async fn recv_messages(rx: &mut mpsc::Receiver<Outbound>) -> Vec<LiveServerMsg> {
        messages(rx.recv().await.expect("room should send a frame"))
    }

    fn drain(rx: &mut mpsc::Receiver<Outbound>) -> Vec<LiveServerMsg> {
        let mut all = Vec::new();
        while let Ok(outbound) = rx.try_recv() {
            all.extend(messages(outbound));
        }
        all
    }

    #[allow(clippy::too_many_arguments)]
    async fn join_as<S: crate::live_store::LiveStore>(
        room: &mut LiveRoom<S>,
        conn: u64,
        client_n: u128,
        user_n: u128,
        role: &str,
        format: &str,
        resume: Option<LiveResume>,
        seen: Option<Seq>,
    ) -> Joined {
        join_user(
            room,
            conn,
            client_n,
            UserId(id(user_n)),
            role,
            format,
            resume,
            seen,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn join_user<S: crate::live_store::LiveStore>(
        room: &mut LiveRoom<S>,
        conn: u64,
        client_n: u128,
        user: UserId,
        role: &str,
        format: &str,
        resume: Option<LiveResume>,
        seen: Option<Seq>,
    ) -> Joined {
        join_full(
            room,
            conn,
            client_n,
            user,
            role,
            format,
            resume,
            seen,
            false,
            LiveBuild::Browser,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn join_full<S: crate::live_store::LiveStore>(
        room: &mut LiveRoom<S>,
        conn: u64,
        client_n: u128,
        user: UserId,
        role: &str,
        format: &str,
        resume: Option<LiveResume>,
        seen: Option<Seq>,
        can_checkpoint: bool,
        build: LiveBuild,
    ) -> Joined {
        let client = ClientId(id(client_n));
        let (tx, rx) = mpsc::channel(512);
        let (accepted, answer) = oneshot::channel();
        room.handle(LiveCommand::Join {
            conn,
            client,
            user,
            session: SessionId(id(user.0.as_u128().wrapping_add(1000))),
            name: format!("User {}", user.0.as_u128()),
            role: builtin_access(role),
            protocol: 1,
            format: format.to_owned(),
            resume,
            seen,
            can_checkpoint,
            build,
            tx,
            accepted,
        })
        .await
        .unwrap();
        assert_eq!(answer.await.unwrap(), JoinReply::Accepted);
        let mut joined = Joined {
            conn,
            client,
            rx,
            session: protocol::live::LiveSessionId(Uuid::nil()),
            participant: ParticipantNo(0),
            place: Place::New,
            initial: Vec::new(),
            after_join: Vec::new(),
        };
        let first = recv_messages(&mut joined.rx).await;
        match first.as_slice() {
            [
                LiveServerMsg::LiveJoined {
                    session,
                    participant,
                    place,
                    ..
                },
                LiveServerMsg::Catchup { .. },
                ..,
            ] => {
                joined.session = *session;
                joined.participant = *participant;
                joined.place = *place;
            }
            other => panic!("unexpected join frames: {other:?}"),
        }
        joined.initial = first;
        joined.after_join = drain(&mut joined.rx);
        joined
    }

    async fn entry<S: crate::live_store::LiveStore>(
        room: &mut LiveRoom<S>,
        joined: &Joined,
        local: u64,
        seen: u64,
        format: &str,
        payload: String,
    ) -> Result<(), super::RoomFailed> {
        room.handle(LiveCommand::Entry {
            conn: joined.conn,
            client: joined.client,
            local: LocalNo(local),
            seen: Seq(seen),
            format: format.to_owned(),
            payload,
        })
        .await
    }

    fn entries(messages: Vec<LiveServerMsg>) -> Vec<protocol::live::LiveEntry> {
        messages
            .into_iter()
            .filter_map(|message| match message {
                LiveServerMsg::Entry { entry } => Some(entry),
                _ => None,
            })
            .collect()
    }

    #[tokio::test(start_paused = true)]
    async fn first_join_starts_a_session() {
        let store = MemStore::new();
        let (events, mut event_rx) = EventSink::channel();
        let mut room = LiveRoom::load(DocId(id(100)), store, events, timing())
            .await
            .unwrap();
        let ann = join_as(&mut room, 1, 1, 11, "editor", FORMAT, None, None).await;
        assert_eq!(ann.participant, ParticipantNo(1));
        assert_eq!(ann.place, Place::New);
        assert!(matches!(
            ann.initial.as_slice(),
            [
                LiveServerMsg::LiveJoined {
                    participant: ParticipantNo(1),
                    place: Place::New,
                    ..
                },
                LiveServerMsg::Catchup {
                    base: Seq(0),
                    from: Seq(0),
                    head: Seq(0),
                    revision: None
                }
            ]
        ));
        let started = event_rx.recv().await.unwrap();
        assert_eq!(started.kind, "live.session_started");
        assert_eq!(started.detail, serde_json::json!({ "format": FORMAT }));
        assert!(room.has_session());
    }

    #[tokio::test(start_paused = true)]
    async fn entries_are_numbered_stamped_and_sent_to_all() {
        let (mut room, _) = room().await;
        let mut ann = join_as(&mut room, 1, 1, 11, "editor", FORMAT, None, None).await;
        let mut bob = join_as(&mut room, 2, 2, 12, "editor", FORMAT, None, None).await;
        let _ = drain(&mut ann.rx);
        let before = time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000;
        entry(&mut room, &ann, 1, 0, FORMAT, "{\"raw\":1}".into())
            .await
            .unwrap();
        for received in [&mut ann.rx, &mut bob.rx] {
            let sent = entries(drain(received));
            assert_eq!(sent.len(), 1);
            assert_eq!(sent[0].seq, Seq(1));
            assert!(i128::from(sent[0].at_ms) >= before);
            assert!(matches!(
                &sent[0].body,
                EntryBody::Change { participant, payload, .. }
                    if *participant == ann.participant && payload == "{\"raw\":1}"
            ));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_is_logged_and_sent() {
        let store = MemStore::new();
        let (events, mut event_rx) = EventSink::channel();
        let mut room = LiveRoom::load(DocId(id(100)), store.clone(), events, timing())
            .await
            .unwrap();
        let mut ann = join_as(&mut room, 1, 1, 11, "editor", FORMAT, None, None).await;
        let mut bob = join_as(&mut room, 2, 2, 12, "editor", FORMAT, None, None).await;
        let _ = drain(&mut ann.rx);
        entry(&mut room, &ann, 1, 0, FORMAT, "crashes".into())
            .await
            .unwrap();
        let _ = drain(&mut ann.rx);
        let _ = drain(&mut bob.rx);

        room.handle(LiveCommand::Report {
            conn: ann.conn,
            client: ann.client,
            seq: Seq(1),
        })
        .await
        .unwrap();
        assert!(drain(&mut ann.rx).is_empty());
        room.handle(LiveCommand::Report {
            conn: bob.conn,
            client: bob.client,
            seq: Seq(1),
        })
        .await
        .unwrap();

        for received in [&mut ann.rx, &mut bob.rx] {
            assert!(matches!(
                entries(drain(received)).as_slice(),
                [protocol::live::LiveEntry {
                    seq: Seq(2),
                    body: EntryBody::Cancel {
                        cancels: Seq(1),
                        reports: 2
                    },
                    ..
                }]
            ));
        }
        assert_eq!(store.entry_count(), 2);
        let cancelled = loop {
            let event = event_rx.recv().await.unwrap();
            if event.kind == "live.entry_cancelled" {
                break event;
            }
        };
        assert_eq!(
            cancelled.detail,
            serde_json::json!({ "seq": 1, "at": 2, "reports": 2 })
        );

        let later = join_as(&mut room, 3, 3, 13, "editor", FORMAT, None, Some(Seq(0))).await;
        let caught_up = entries(later.initial);
        assert!(matches!(
            &caught_up[0].body,
            EntryBody::Change { cancelled: true, payload, .. } if payload.is_empty()
        ));
        assert!(matches!(
            caught_up[1].body,
            EntryBody::Cancel {
                cancels: Seq(1),
                reports: 2
            }
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn witness_leaving_causes_cancellation() {
        let (mut room, store) = room().await;
        let mut ann = join_as(&mut room, 1, 1, 11, "editor", FORMAT, None, None).await;
        let bob = join_as(&mut room, 2, 2, 12, "editor", FORMAT, None, None).await;
        let _ = drain(&mut ann.rx);
        entry(&mut room, &ann, 1, 0, FORMAT, "crashes-ann".into())
            .await
            .unwrap();
        let _ = drain(&mut ann.rx);

        room.handle(LiveCommand::Report {
            conn: ann.conn,
            client: ann.client,
            seq: Seq(1),
        })
        .await
        .unwrap();
        room.handle(LiveCommand::Leave {
            conn: bob.conn,
            client: bob.client,
        })
        .await
        .unwrap();

        assert!(matches!(
            entries(drain(&mut ann.rx)).as_slice(),
            [protocol::live::LiveEntry {
                seq: Seq(2),
                body: EntryBody::Cancel {
                    cancels: Seq(1),
                    reports: 1
                },
                ..
            }]
        ));
        assert_eq!(store.entry_count(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn viewer_report_is_ignored() {
        let (mut room, _) = room().await;
        let mut ann = join_as(&mut room, 1, 1, 11, "editor", FORMAT, None, None).await;
        let mut bob = join_as(&mut room, 2, 2, 12, "editor", FORMAT, None, None).await;
        let mut viewer = join_as(&mut room, 3, 3, 13, "viewer", FORMAT, None, None).await;
        let _ = drain(&mut ann.rx);
        let _ = drain(&mut bob.rx);
        entry(&mut room, &ann, 1, 0, FORMAT, "crashes-viewer".into())
            .await
            .unwrap();
        let _ = drain(&mut ann.rx);
        let _ = drain(&mut bob.rx);
        let _ = drain(&mut viewer.rx);

        room.handle(LiveCommand::Report {
            conn: viewer.conn,
            client: viewer.client,
            seq: Seq(1),
        })
        .await
        .unwrap();
        room.handle(LiveCommand::Report {
            conn: ann.conn,
            client: ann.client,
            seq: Seq(1),
        })
        .await
        .unwrap();
        assert!(drain(&mut ann.rx).is_empty());
        assert!(drain(&mut bob.rx).is_empty());
        assert!(drain(&mut viewer.rx).is_empty());

        room.handle(LiveCommand::Report {
            conn: bob.conn,
            client: bob.client,
            seq: Seq(1),
        })
        .await
        .unwrap();
        assert!(matches!(
            entries(drain(&mut viewer.rx)).as_slice(),
            [protocol::live::LiveEntry {
                body: EntryBody::Cancel { reports: 2, .. },
                ..
            }]
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn viewer_stalled_on_standing_entry_gets_cannot_open() {
        let (mut room, _) = room().await;
        let mut ann = join_as(&mut room, 1, 1, 11, "editor", FORMAT, None, None).await;
        let mut bob = join_as(&mut room, 2, 2, 12, "editor", FORMAT, None, None).await;
        let mut viewer = join_as(&mut room, 3, 3, 13, "viewer", FORMAT, None, None).await;
        let _ = drain(&mut ann.rx);
        let _ = drain(&mut bob.rx);
        entry(&mut room, &ann, 1, 0, FORMAT, "stands-for-viewer".into())
            .await
            .unwrap();
        let _ = drain(&mut ann.rx);
        let _ = drain(&mut bob.rx);
        let _ = drain(&mut viewer.rx);
        room.handle(LiveCommand::Heartbeat {
            conn: bob.conn,
            client: bob.client,
            seen: Seq(1),
        })
        .await
        .unwrap();

        room.handle(LiveCommand::Report {
            conn: viewer.conn,
            client: viewer.client,
            seq: Seq(1),
        })
        .await
        .unwrap();

        assert!(matches!(
            drain(&mut viewer.rx).as_slice(),
            [LiveServerMsg::CannotOpen { seq: Seq(1) }]
        ));
        assert!(drain(&mut ann.rx).iter().all(|message| !matches!(
            message,
            LiveServerMsg::Entry {
                entry: protocol::live::LiveEntry {
                    body: EntryBody::Cancel { .. },
                    ..
                }
            }
        )));
    }

    #[tokio::test(start_paused = true)]
    async fn lone_viewer_reporter_gets_cannot_open_when_witnesses_leave() {
        let store = MemStore::new();
        let (events, mut event_rx) = EventSink::channel();
        let mut room = LiveRoom::load(DocId(id(100)), store.clone(), events, timing())
            .await
            .unwrap();
        let ann = join_as(&mut room, 1, 1, 11, "editor", FORMAT, None, None).await;
        let bob = join_as(&mut room, 2, 2, 12, "editor", FORMAT, None, None).await;
        let mut viewer = join_as(&mut room, 3, 3, 13, "viewer", FORMAT, None, None).await;
        entry(&mut room, &ann, 1, 0, FORMAT, "only-viewer-stalls".into())
            .await
            .unwrap();
        let _ = drain(&mut viewer.rx);
        room.handle(LiveCommand::Report {
            conn: viewer.conn,
            client: viewer.client,
            seq: Seq(1),
        })
        .await
        .unwrap();

        room.handle(LiveCommand::Leave {
            conn: ann.conn,
            client: ann.client,
        })
        .await
        .unwrap();
        assert!(
            drain(&mut viewer.rx)
                .iter()
                .all(|message| !matches!(message, LiveServerMsg::CannotOpen { .. }))
        );
        room.handle(LiveCommand::Leave {
            conn: bob.conn,
            client: bob.client,
        })
        .await
        .unwrap();

        let messages = drain(&mut viewer.rx);
        assert!(
            messages
                .iter()
                .any(|message| matches!(message, LiveServerMsg::CannotOpen { seq: Seq(1) }))
        );
        assert!(entries(messages).is_empty());
        assert_eq!(store.entry_count(), 1);
        let failures = std::iter::from_fn(|| event_rx.try_recv().ok())
            .filter(|event| event.kind == "live.entry_failed_on_device")
            .collect::<Vec<_>>();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].actor, Some(UserId(id(13))));
        assert_eq!(failures[0].detail, serde_json::json!({ "seq": 1 }));
    }

    #[tokio::test(start_paused = true)]
    async fn viewer_reporter_is_removed_when_a_witness_moves_past() {
        let (mut room, _) = room().await;
        let mut ann = join_as(&mut room, 1, 1, 11, "editor", FORMAT, None, None).await;
        let mut bob = join_as(&mut room, 2, 2, 12, "editor", FORMAT, None, None).await;
        let mut viewer = join_as(&mut room, 3, 3, 13, "viewer", FORMAT, None, None).await;
        let _ = drain(&mut ann.rx);
        let _ = drain(&mut bob.rx);
        entry(&mut room, &ann, 1, 0, FORMAT, "later-stands".into())
            .await
            .unwrap();
        let _ = drain(&mut ann.rx);
        let _ = drain(&mut bob.rx);
        let _ = drain(&mut viewer.rx);
        room.handle(LiveCommand::Report {
            conn: viewer.conn,
            client: viewer.client,
            seq: Seq(1),
        })
        .await
        .unwrap();
        assert!(drain(&mut viewer.rx).is_empty());

        room.handle(LiveCommand::Heartbeat {
            conn: bob.conn,
            client: bob.client,
            seen: Seq(1),
        })
        .await
        .unwrap();

        assert!(matches!(
            drain(&mut viewer.rx).as_slice(),
            [LiveServerMsg::CannotOpen { seq: Seq(1) }]
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn viewer_reporter_is_removed_when_a_witness_rejoins_past() {
        let (mut room, _) = room().await;
        let mut ann = join_as(&mut room, 1, 1, 11, "editor", FORMAT, None, None).await;
        let bob = join_as(&mut room, 2, 2, 12, "editor", FORMAT, None, None).await;
        let mut viewer = join_as(&mut room, 3, 3, 13, "viewer", FORMAT, None, None).await;
        let _ = drain(&mut ann.rx);
        entry(&mut room, &ann, 1, 0, FORMAT, "join-stands".into())
            .await
            .unwrap();
        let _ = drain(&mut ann.rx);
        let _ = drain(&mut viewer.rx);
        room.handle(LiveCommand::Report {
            conn: viewer.conn,
            client: viewer.client,
            seq: Seq(1),
        })
        .await
        .unwrap();
        let resume = Some(LiveResume {
            session: bob.session,
            participant: bob.participant,
        });
        room.handle(LiveCommand::Disconnected {
            conn: bob.conn,
            client: bob.client,
        })
        .await
        .unwrap();
        let _ = drain(&mut viewer.rx);

        let _bob = join_as(&mut room, 4, 2, 12, "editor", FORMAT, resume, Some(Seq(1))).await;

        assert!(matches!(
            drain(&mut viewer.rx).last(),
            Some(LiveServerMsg::CannotOpen { seq: Seq(1) })
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn standing_entry_removes_the_reporter() {
        let store = MemStore::new();
        let (events, mut event_rx) = EventSink::channel();
        let mut room = LiveRoom::load(DocId(id(100)), store, events, timing())
            .await
            .unwrap();
        let mut ann = join_as(&mut room, 1, 1, 11, "editor", FORMAT, None, None).await;
        let mut bob = join_as(&mut room, 2, 2, 12, "editor", FORMAT, None, None).await;
        let _ = drain(&mut ann.rx);
        entry(&mut room, &ann, 1, 0, FORMAT, "stands".into())
            .await
            .unwrap();
        let _ = drain(&mut ann.rx);
        let _ = drain(&mut bob.rx);
        room.handle(LiveCommand::Heartbeat {
            conn: bob.conn,
            client: bob.client,
            seen: Seq(1),
        })
        .await
        .unwrap();
        let resume = Some(LiveResume {
            session: bob.session,
            participant: bob.participant,
        });
        room.handle(LiveCommand::Disconnected {
            conn: bob.conn,
            client: bob.client,
        })
        .await
        .unwrap();
        bob = join_as(&mut room, 3, 2, 12, "editor", FORMAT, resume, Some(Seq(0))).await;
        assert_eq!(bob.place, Place::Kept);
        let _ = drain(&mut ann.rx);

        room.handle(LiveCommand::Report {
            conn: ann.conn,
            client: ann.client,
            seq: Seq(1),
        })
        .await
        .unwrap();

        assert!(matches!(
            drain(&mut ann.rx).as_slice(),
            [LiveServerMsg::CannotOpen { seq: Seq(1) }]
        ));
        assert!(
            drain(&mut bob.rx)
                .iter()
                .all(|message| !matches!(message, LiveServerMsg::CannotOpen { .. }))
        );
        let failed = loop {
            let event = event_rx.recv().await.unwrap();
            if event.kind == "live.entry_failed_on_device" {
                break event;
            }
        };
        assert_eq!(failed.detail, serde_json::json!({ "seq": 1 }));
        assert!(
            room.people()
                .iter()
                .all(|person| person.participant != ann.participant)
        );
        assert!(
            room.people()
                .iter()
                .any(|person| person.participant == bob.participant)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn reporter_leaving_before_entry_stands_still_records_failure_once() {
        let store = MemStore::new();
        let (events, mut event_rx) = EventSink::channel();
        let mut room = LiveRoom::load(DocId(id(100)), store, events, timing())
            .await
            .unwrap();
        let ann = join_as(&mut room, 1, 1, 11, "editor", FORMAT, None, None).await;
        let bob = join_as(&mut room, 2, 2, 12, "editor", FORMAT, None, None).await;
        entry(&mut room, &ann, 1, 0, FORMAT, "stands-after-leave".into())
            .await
            .unwrap();
        room.handle(LiveCommand::Report {
            conn: ann.conn,
            client: ann.client,
            seq: Seq(1),
        })
        .await
        .unwrap();
        room.handle(LiveCommand::Leave {
            conn: ann.conn,
            client: ann.client,
        })
        .await
        .unwrap();

        for _ in 0..2 {
            room.handle(LiveCommand::Heartbeat {
                conn: bob.conn,
                client: bob.client,
                seen: Seq(1),
            })
            .await
            .unwrap();
        }

        let failures = std::iter::from_fn(|| event_rx.try_recv().ok())
            .filter(|event| event.kind == "live.entry_failed_on_device")
            .collect::<Vec<_>>();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].actor, Some(UserId(id(11))));
        assert_eq!(failures[0].detail, serde_json::json!({ "seq": 1 }));
    }

    #[tokio::test(start_paused = true)]
    async fn resync_resends_from_base() {
        let (mut room, _) = room().await;
        let mut ann = join_as(&mut room, 1, 1, 11, "editor", FORMAT, None, None).await;
        entry(&mut room, &ann, 1, 0, FORMAT, "one".into())
            .await
            .unwrap();
        entry(&mut room, &ann, 2, 1, FORMAT, "two".into())
            .await
            .unwrap();
        let _ = drain(&mut ann.rx);

        room.handle(LiveCommand::Resync {
            conn: ann.conn,
            client: ann.client,
        })
        .await
        .unwrap();

        let resent = drain(&mut ann.rx);
        assert!(matches!(
            resent.first(),
            Some(LiveServerMsg::Catchup {
                base: Seq(0),
                from: Seq(0),
                head: Seq(2),
                ..
            })
        ));
        assert_eq!(
            entries(resent)
                .iter()
                .map(|entry| entry.seq)
                .collect::<Vec<_>>(),
            [Seq(1), Seq(2)]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn racing_entries_are_ordered_by_arrival() {
        let (mut room, _) = room().await;
        let mut ann = join_as(&mut room, 1, 1, 11, "editor", FORMAT, None, None).await;
        let mut bob = join_as(&mut room, 2, 2, 12, "editor", FORMAT, None, None).await;
        let _ = drain(&mut ann.rx);
        entry(&mut room, &ann, 1, 0, FORMAT, "a".into())
            .await
            .unwrap();
        entry(&mut room, &bob, 1, 0, FORMAT, "b".into())
            .await
            .unwrap();
        for received in [&mut ann.rx, &mut bob.rx] {
            let sent = entries(drain(received));
            assert_eq!(
                sent.iter().map(|e| e.seq).collect::<Vec<_>>(),
                [Seq(1), Seq(2)]
            );
            let payloads = sent
                .iter()
                .map(|e| match &e.body {
                    EntryBody::Change { payload, .. } => payload.as_str(),
                    EntryBody::Cancel { .. } => "cancel",
                })
                .collect::<Vec<_>>();
            assert_eq!(payloads, ["a", "b"]);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn repeat_is_dropped_silently() {
        let (mut room, store) = room().await;
        let mut ann = join_as(&mut room, 1, 1, 11, "editor", FORMAT, None, None).await;
        entry(&mut room, &ann, 1, 0, FORMAT, "once".into())
            .await
            .unwrap();
        let _ = drain(&mut ann.rx);
        entry(&mut room, &ann, 1, 0, FORMAT, "twice".into())
            .await
            .unwrap();
        assert!(drain(&mut ann.rx).is_empty());
        assert_eq!(store.entry_count(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn hostile_entries_are_refused_and_room_carries_on() {
        let (mut room, _) = room().await;
        let mut ann = join_as(&mut room, 1, 1, 11, "editor", FORMAT, None, None).await;
        let mut viewer = join_as(&mut room, 2, 2, 12, "viewer", FORMAT, None, None).await;
        let mut bob = join_as(&mut room, 3, 3, 13, "editor", FORMAT, None, None).await;
        let _ = drain(&mut ann.rx);
        let _ = drain(&mut viewer.rx);

        entry(&mut room, &ann, 3, 0, FORMAT, "skip".into())
            .await
            .unwrap();
        assert_eq!(entries(drain(&mut ann.rx)).len(), 1);
        let _ = drain(&mut bob.rx);
        let _ = drain(&mut viewer.rx);
        entry(&mut room, &ann, 1, 1, FORMAT, "late".into())
            .await
            .unwrap();
        assert!(matches!(
            drain(&mut ann.rx).as_slice(),
            [LiveServerMsg::Refused {
                local: LocalNo(1),
                reason: RefusedReason::OutOfOrder
            }]
        ));

        let cases = [
            (true, 1, FORMAT, "x".to_owned(), RefusedReason::ReadOnly),
            (
                false,
                4,
                "other/1",
                "x".to_owned(),
                RefusedReason::WrongFormat,
            ),
            (
                false,
                5,
                FORMAT,
                "x".repeat(262_145),
                RefusedReason::TooLarge,
            ),
            (false, 6, FORMAT, "x".to_owned(), RefusedReason::BaseAhead),
        ];
        for (is_viewer, local, format, payload, reason) in cases {
            let seen = if reason == RefusedReason::BaseAhead {
                99
            } else {
                1
            };
            if is_viewer {
                entry(&mut room, &viewer, local, seen, format, payload)
                    .await
                    .unwrap();
            } else {
                entry(&mut room, &ann, local, seen, format, payload)
                    .await
                    .unwrap();
            }
            let received = if is_viewer {
                &mut viewer.rx
            } else {
                &mut ann.rx
            };
            let refused = drain(received);
            assert!(
                matches!(refused.as_slice(), [LiveServerMsg::Refused { reason: got, .. }] if *got == reason),
                "unexpected refusal frames: {refused:?}"
            );
            if reason == RefusedReason::WrongFormat {
                entry(&mut room, &ann, local, seen, FORMAT, "now-valid".into())
                    .await
                    .unwrap();
                assert!(matches!(
                    drain(&mut ann.rx).as_slice(),
                    [LiveServerMsg::Refused {
                        local: LocalNo(4),
                        reason: RefusedReason::WrongFormat
                    }]
                ));
            }
            entry(&mut room, &bob, local, 1, FORMAT, format!("ok-{local}"))
                .await
                .unwrap();
            assert_eq!(entries(drain(&mut bob.rx)).len(), 1);
            let _ = drain(&mut ann.rx);
            let _ = drain(&mut viewer.rx);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn rate_limit_refuses_after_the_burst() {
        let (mut room, _) = room().await;
        let mut ann = join_as(&mut room, 1, 1, 11, "editor", FORMAT, None, None).await;
        for local in 1..=121 {
            entry(&mut room, &ann, local, local - 1, FORMAT, local.to_string())
                .await
                .unwrap();
        }
        let all = drain(&mut ann.rx);
        assert_eq!(entries(all.clone()).len(), 120);
        assert!(all.iter().any(|m| matches!(
            m,
            LiveServerMsg::Refused {
                local: LocalNo(121),
                reason: RefusedReason::RateLimited
            }
        )));
        tokio::time::advance(Duration::from_secs(1)).await;
        for local in 122..=151 {
            entry(&mut room, &ann, local, 120, FORMAT, local.to_string())
                .await
                .unwrap();
        }
        assert_eq!(entries(drain(&mut ann.rx)).len(), 30);
    }

    #[tokio::test(start_paused = true)]
    async fn store_failure_ends_room_without_broadcast() {
        let (mut room, store) = room().await;
        let mut ann = join_as(&mut room, 1, 1, 11, "editor", FORMAT, None, None).await;
        let mut bob = join_as(&mut room, 2, 2, 12, "editor", FORMAT, None, None).await;
        let _ = drain(&mut ann.rx);
        store.fail_next();
        assert!(
            entry(&mut room, &ann, 1, 0, FORMAT, "lost".into())
                .await
                .is_err()
        );
        assert!(entries(drain(&mut ann.rx)).is_empty());
        assert!(entries(drain(&mut bob.rx)).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn incompatible_join_is_told_and_not_admitted() {
        let (mut room, _) = room().await;
        let mut ann = join_as(&mut room, 1, 1, 11, "editor", FORMAT, None, None).await;
        for (conn, protocol, format) in [(2, 2, FORMAT), (3, 1, "gridcraft/2")] {
            let (tx, mut rx) = mpsc::channel(8);
            let (accepted, answer) = oneshot::channel();
            room.handle(LiveCommand::Join {
                conn,
                client: ClientId(id(u128::from(conn))),
                user: UserId(id(u128::from(conn + 20))),
                session: SessionId(id(u128::from(conn + 30))),
                name: "No".into(),
                role: builtin_access("editor"),
                protocol,
                format: format.into(),
                resume: None,
                seen: None,
                can_checkpoint: false,
                build: LiveBuild::Browser,
                tx,
                accepted,
            })
            .await
            .unwrap();
            assert_eq!(answer.await.unwrap(), JoinReply::Refused);
            assert!(matches!(
                recv_messages(&mut rx).await.as_slice(),
                [LiveServerMsg::Incompatible { .. }]
            ));
        }
        assert!(drain(&mut ann.rx).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn kept_and_returning_places() {
        let (mut room, _) = room().await;
        let ann = join_as(&mut room, 1, 1, 11, "editor", FORMAT, None, None).await;
        let resume = Some(LiveResume {
            session: ann.session,
            participant: ann.participant,
        });
        let kept = join_as(&mut room, 2, 1, 11, "editor", FORMAT, resume, Some(Seq(0))).await;
        assert_eq!(kept.participant, ann.participant);
        assert_eq!(kept.place, Place::Kept);
        room.handle(LiveCommand::Leave {
            conn: kept.conn,
            client: kept.client,
        })
        .await
        .unwrap();
        room.settle().await.unwrap();
        let returned = join_as(&mut room, 3, 1, 11, "editor", FORMAT, resume, Some(Seq(0))).await;
        assert_eq!(returned.participant, ParticipantNo(2));
        assert_eq!(
            returned.place,
            Place::Returning {
                previous: ParticipantNo(1)
            }
        );
        let impostor = join_as(&mut room, 4, 9, 19, "editor", FORMAT, resume, Some(Seq(0))).await;
        assert_eq!(impostor.participant, ParticipantNo(3));
        assert_eq!(impostor.place, Place::New);
    }

    #[tokio::test(start_paused = true)]
    async fn catchup_honours_seen() {
        let (mut room, _) = room().await;
        let mut ann = join_as(&mut room, 1, 1, 11, "editor", FORMAT, None, None).await;
        for local in 1..=5 {
            entry(&mut room, &ann, local, local - 1, FORMAT, local.to_string())
                .await
                .unwrap();
        }
        let _ = drain(&mut ann.rx);
        for (conn, client_n, seen, from, count) in [(2, 2, 3, 3, 2), (3, 3, 9, 0, 5)] {
            let joined = join_as(
                &mut room,
                conn,
                client_n,
                client_n + 10,
                "editor",
                FORMAT,
                None,
                Some(Seq(seen)),
            )
            .await;
            assert!(
                matches!(&joined.initial[1], LiveServerMsg::Catchup { base: Seq(0), from: got, head: Seq(5), .. } if *got == Seq(from))
            );
            assert_eq!(entries(joined.initial).len(), count);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn join_catchup_uses_the_last_verified_revision_and_position() {
        let doc = DocId(id(100));
        let store = MemStore::new();
        let mut first = LiveRoom::load(doc, store.clone(), EventSink::discard(), timing())
            .await
            .unwrap();
        let mut ann = join_as(&mut first, 1, 1, 11, "editor", FORMAT, None, None).await;
        entry(&mut first, &ann, 1, 0, FORMAT, "before".into())
            .await
            .unwrap();
        entry(&mut first, &ann, 2, 1, FORMAT, "after".into())
            .await
            .unwrap();
        let _ = drain(&mut ann.rx);

        crate::live_store::LiveStore::record_live_checkpoint(
            &store,
            doc,
            &checkpoint("verified-revision", Seq(1), ann.session, b"identities"),
        )
        .await
        .unwrap();
        crate::live_store::LiveStore::verify_live_checkpoint(
            &store,
            doc,
            "verified-revision",
            Seq(1),
            time::OffsetDateTime::UNIX_EPOCH,
        )
        .await
        .unwrap();
        let mut restarted = LiveRoom::load(doc, store, EventSink::discard(), timing())
            .await
            .unwrap();

        let joined = join_as(&mut restarted, 2, 2, 12, "editor", FORMAT, None, None).await;
        assert!(matches!(
            joined.initial.as_slice(),
            [
                LiveServerMsg::LiveJoined { .. },
                LiveServerMsg::Catchup {
                    base: Seq(1),
                    revision: Some(revision),
                    from: Seq(1),
                    head: Seq(2),
                },
                LiveServerMsg::Entry { entry },
            ] if revision == "verified-revision" && entry.seq == Seq(2)
        ));
    }

    async fn join_capable<S: crate::live_store::LiveStore>(
        room: &mut LiveRoom<S>,
        conn: u64,
        n: u128,
        role: &str,
        build: LiveBuild,
    ) -> Joined {
        join_full(
            room,
            conn,
            n,
            UserId(id(n + 10)),
            role,
            FORMAT,
            None,
            None,
            true,
            build,
        )
        .await
    }

    fn checkpoint_ticket(messages: Vec<LiveServerMsg>) -> Option<(Uuid, TicketPurpose, Seq)> {
        messages.into_iter().find_map(|message| match message {
            LiveServerMsg::CheckpointTicket {
                ticket,
                purpose,
                at,
                ..
            } => Some((ticket, purpose, at)),
            _ => None,
        })
    }

    async fn claim<S: crate::live_store::LiveStore>(
        room: &mut LiveRoom<S>,
        user: UserId,
        ticket: Uuid,
        at: Seq,
    ) -> bool {
        let (reply, answer) = oneshot::channel();
        room.handle(LiveCommand::CheckpointClaim {
            user,
            ticket,
            at,
            base: None,
            reply,
        })
        .await
        .unwrap();
        answer.await.unwrap()
    }

    async fn upload<S: crate::live_store::LiveStore>(
        room: &mut LiveRoom<S>,
        user: UserId,
        ticket: Uuid,
        at: Seq,
        sha: u8,
    ) -> LiveCheckpointReply {
        upload_on(room, user, ticket, at, None, sha).await
    }

    async fn upload_on<S: crate::live_store::LiveStore>(
        room: &mut LiveRoom<S>,
        user: UserId,
        ticket: Uuid,
        at: Seq,
        base: Option<&str>,
        sha: u8,
    ) -> LiveCheckpointReply {
        let (reply, answer) = oneshot::channel();
        room.handle(LiveCommand::CheckpointUpload {
            user,
            ticket,
            at,
            base: base.map(|base| base.parse().unwrap()),
            blob: BlobRef {
                sha256: [sha; 32],
                size: 3,
            },
            session_data: b"identities".to_vec(),
            reply,
        })
        .await
        .unwrap();
        room.finish_writes().await.unwrap();
        answer.await.unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn due_checkpoint_tickets_one_capable_editor() {
        let (mut room, _) = room().await;
        let mut viewer = join_capable(&mut room, 1, 1, "viewer", LiveBuild::Browser).await;
        let mut incapable = join_as(&mut room, 2, 2, 22, "editor", FORMAT, None, None).await;
        let mut capable = join_capable(&mut room, 3, 3, "editor", LiveBuild::Desktop).await;
        entry(&mut room, &incapable, 1, 0, FORMAT, "one".into())
            .await
            .unwrap();
        let _ = drain(&mut viewer.rx);
        let _ = drain(&mut incapable.rx);
        let _ = drain(&mut capable.rx);

        tokio::time::advance(Duration::from_secs(14)).await;
        room.expire_due().await.unwrap();
        assert!(
            checkpoint_ticket(drain(&mut capable.rx)).is_none(),
            "not due before 15 quiet seconds"
        );
        tokio::time::advance(Duration::from_secs(1)).await;
        room.expire_due().await.unwrap();

        assert!(checkpoint_ticket(drain(&mut viewer.rx)).is_none());
        assert!(checkpoint_ticket(drain(&mut incapable.rx)).is_none());
        let (_, purpose, at) = checkpoint_ticket(drain(&mut capable.rx)).unwrap();
        assert_eq!(purpose, TicketPurpose::Write);
        assert_eq!(at, Seq(1));
        tokio::time::advance(Duration::from_secs(1)).await;
        room.expire_due().await.unwrap();
        assert!(
            checkpoint_ticket(drain(&mut capable.rx)).is_none(),
            "one ticket at a time"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn steady_changes_are_ticketed_after_two_minutes() {
        let (mut room, _) = room().await;
        let mut ann = join_capable(&mut room, 1, 1, "editor", LiveBuild::Browser).await;
        for local in 1..=13 {
            entry(&mut room, &ann, local, local - 1, FORMAT, "x".into())
                .await
                .unwrap();
            tokio::time::advance(Duration::from_secs(10)).await;
            room.expire_due().await.unwrap();
        }
        let tickets = drain(&mut ann.rx)
            .into_iter()
            .filter(|message| matches!(message, LiveServerMsg::CheckpointTicket { .. }))
            .count();
        assert_eq!(
            tickets, 1,
            "asked once the first unsaved change is 120s old"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn ticket_is_cancelled_on_expiry_and_role_loss() {
        let (mut room, _) = room().await;
        let mut ann = join_capable(&mut room, 1, 1, "editor", LiveBuild::Browser).await;
        let mut bob = join_capable(&mut room, 2, 2, "editor", LiveBuild::Desktop).await;
        entry(&mut room, &ann, 1, 0, FORMAT, "one".into())
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(15)).await;
        room.expire_due().await.unwrap();
        let first = checkpoint_ticket(drain(&mut ann.rx)).unwrap().0;
        assert!(checkpoint_ticket(drain(&mut bob.rx)).is_none());

        tokio::time::advance(Duration::from_secs(120)).await;
        room.expire_due().await.unwrap();
        let second = checkpoint_ticket(drain(&mut bob.rx)).unwrap().0;
        assert_ne!(first, second, "expiry hands the ticket to the next editor");
        assert!(!claim(&mut room, UserId(id(11)), first, Seq(1)).await);

        room.change_role(UserId(id(12)), builtin_access("viewer"));
        room.expire_due().await.unwrap();
        let third = checkpoint_ticket(drain(&mut ann.rx)).unwrap().0;
        assert_ne!(second, third, "role loss hands the ticket on at once");
        assert!(!claim(&mut room, UserId(id(12)), second, Seq(1)).await);

        room.handle(LiveCommand::Leave {
            conn: ann.conn,
            client: ann.client,
        })
        .await
        .unwrap();
        assert!(
            !claim(&mut room, UserId(id(11)), third, Seq(1)).await,
            "leaving cancels the ticket"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn ticket_is_one_use_and_expires() {
        let (mut room, store) = room().await;
        let mut ann = join_capable(&mut room, 1, 1, "editor", LiveBuild::Browser).await;
        let mut viewer = join_capable(&mut room, 2, 2, "viewer", LiveBuild::Browser).await;
        entry(&mut room, &ann, 1, 0, FORMAT, "one".into())
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(15)).await;
        room.expire_due().await.unwrap();
        let ticket = checkpoint_ticket(drain(&mut ann.rx)).unwrap().0;
        let _ = drain(&mut viewer.rx);

        assert!(
            !claim(&mut room, UserId(id(12)), ticket, Seq(1)).await,
            "another person cannot use the ticket"
        );
        assert!(
            !claim(&mut room, UserId(id(11)), ticket, Seq(0)).await,
            "the ticket names one position"
        );
        assert!(claim(&mut room, UserId(id(11)), ticket, Seq(1)).await);
        assert!(
            !claim(&mut room, UserId(id(11)), ticket, Seq(1)).await,
            "a claimed ticket is used"
        );
        assert!(matches!(
            upload(&mut room, UserId(id(11)), ticket, Seq(1), 9).await,
            LiveCheckpointReply::Saved { .. }
        ));
        assert_eq!(
            upload(&mut room, UserId(id(11)), ticket, Seq(1), 9).await,
            LiveCheckpointReply::Refused
        );

        entry(&mut room, &ann, 2, 1, FORMAT, "two".into())
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(15)).await;
        room.expire_due().await.unwrap();
        let next = checkpoint_ticket(drain(&mut ann.rx)).unwrap().0;
        tokio::time::advance(Duration::from_secs(120)).await;
        assert_eq!(
            upload(&mut room, UserId(id(11)), next, Seq(2), 8).await,
            LiveCheckpointReply::Refused,
            "an expired ticket is refused"
        );
        assert_eq!(
            crate::live_store::LiveStore::load_live(&store, DocId(id(100)))
                .await
                .unwrap()
                .unverified_checkpoints
                .len(),
            1
        );
    }

    #[tokio::test(start_paused = true)]
    async fn checkpoint_position_cannot_cover_a_later_entry() {
        let (mut room, store) = room().await;
        let mut ann = join_capable(&mut room, 1, 1, "editor", LiveBuild::Browser).await;
        entry(&mut room, &ann, 1, 0, FORMAT, "one".into())
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(15)).await;
        room.expire_due().await.unwrap();
        let ticket = checkpoint_ticket(drain(&mut ann.rx)).unwrap().0;
        entry(&mut room, &ann, 2, 1, FORMAT, "two".into())
            .await
            .unwrap();

        assert_eq!(
            upload(&mut room, UserId(id(11)), ticket, Seq(2), 8).await,
            LiveCheckpointReply::Refused
        );
        assert!(
            crate::live_store::LiveStore::load_live(&store, DocId(id(100)))
                .await
                .unwrap()
                .unverified_checkpoints
                .is_empty()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn checkpoint_credits_every_contributor_through_position() {
        let (mut room, store) = room().await;
        let mut ann = join_capable(&mut room, 1, 1, "editor", LiveBuild::Browser).await;
        let bob = join_as(&mut room, 2, 2, 12, "editor", FORMAT, None, None).await;
        let cat = join_as(&mut room, 3, 3, 13, "editor", FORMAT, None, None).await;
        entry(&mut room, &bob, 1, 0, FORMAT, "one".into())
            .await
            .unwrap();
        entry(&mut room, &ann, 1, 1, FORMAT, "two".into())
            .await
            .unwrap();
        entry(&mut room, &bob, 2, 2, FORMAT, "three".into())
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(15)).await;
        room.expire_due().await.unwrap();
        let ticket = checkpoint_ticket(drain(&mut ann.rx)).unwrap().0;
        entry(&mut room, &cat, 1, 3, FORMAT, "after the ticket".into())
            .await
            .unwrap();

        assert!(matches!(
            upload(&mut room, UserId(id(11)), ticket, Seq(3), 9).await,
            LiveCheckpointReply::Saved { .. }
        ));
        let authors = store.last_authors();
        assert_eq!(
            authors
                .iter()
                .map(|author| author.id.clone())
                .collect::<Vec<_>>(),
            [id(12).to_string(), id(11).to_string()],
            "contributors through the position, in first-contribution order"
        );
        assert_eq!(authors[0].name, "User 12");
    }

    #[tokio::test(start_paused = true)]
    async fn upload_is_current_but_unverified_and_asks_another_editor_to_verify() {
        let doc = DocId(id(100));
        let store = MemStore::new();
        let (events, mut emitted) = EventSink::channel();
        let mut room = LiveRoom::load(doc, store.clone(), events, timing())
            .await
            .unwrap();
        let mut ann = join_capable(&mut room, 1, 1, "editor", LiveBuild::Browser).await;
        let mut bob = join_capable(&mut room, 2, 2, "editor", LiveBuild::Desktop).await;
        entry(&mut room, &ann, 1, 0, FORMAT, "one".into())
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(15)).await;
        room.expire_due().await.unwrap();
        let ticket = checkpoint_ticket(drain(&mut ann.rx)).unwrap().0;
        let _ = drain(&mut bob.rx);

        let LiveCheckpointReply::Saved { revision } =
            upload(&mut room, UserId(id(11)), ticket, Seq(1), 9).await
        else {
            panic!("the upload must be stored");
        };

        let loaded = crate::live_store::LiveStore::load_live(&store, doc)
            .await
            .unwrap();
        assert_eq!(
            loaded.verified_through,
            Seq(0),
            "never verified by its writer"
        );
        assert!(loaded.verified_checkpoint.is_none());
        assert_eq!(loaded.unverified_checkpoints.len(), 1);
        assert_eq!(loaded.unverified_checkpoints[0].revision, revision);
        assert_eq!(loaded.unverified_checkpoints[0].through, Seq(1));
        assert_eq!(loaded.unverified_checkpoints[0].session_data, b"identities");
        assert!(matches!(
            checkpoint_ticket(drain(&mut bob.rx)),
            Some((_, TicketPurpose::Verify, Seq(1)))
        ));
        let kinds = std::iter::from_fn(|| emitted.try_recv().ok())
            .map(|event| (event.kind, event.detail))
            .filter(|(kind, _)| {
                kind.starts_with("revision.") || kind.starts_with("live.checkpoint")
            })
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            [
                (
                    "revision.saved",
                    json!({ "revision": revision, "live_through": 1, "verified": false })
                ),
                (
                    "live.checkpoint_unverified",
                    json!({ "revision": revision, "through": 1 })
                ),
            ]
        );

        // Nothing unsaved: no further write ticket until a new change.
        tokio::time::advance(Duration::from_secs(200)).await;
        room.expire_due().await.unwrap();
        assert!(
            drain(&mut ann.rx)
                .iter()
                .all(|message| !matches!(message, LiveServerMsg::CheckpointTicket { .. }))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn stale_upload_is_refused_and_stops_further_write_tickets() {
        let (mut room, store) = room().await;
        let mut ann = join_capable(&mut room, 1, 1, "editor", LiveBuild::Browser).await;
        entry(&mut room, &ann, 1, 0, FORMAT, "one".into())
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(15)).await;
        room.expire_due().await.unwrap();
        let ticket = checkpoint_ticket(drain(&mut ann.rx)).unwrap().0;
        store.set_current_revision(DocId(id(100)), "f".repeat(40));

        assert_eq!(
            upload(&mut room, UserId(id(11)), ticket, Seq(1), 9).await,
            LiveCheckpointReply::Stale {
                current: Some("f".repeat(40))
            }
        );
        tokio::time::advance(Duration::from_secs(200)).await;
        room.expire_due().await.unwrap();
        assert!(
            checkpoint_ticket(drain(&mut ann.rx)).is_none(),
            "the outside revision is never silently adopted as a base"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn oversized_session_data_is_refused() {
        let (mut room, _) = room().await;
        let mut ann = join_capable(&mut room, 1, 1, "editor", LiveBuild::Browser).await;
        entry(&mut room, &ann, 1, 0, FORMAT, "one".into())
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(15)).await;
        room.expire_due().await.unwrap();
        let ticket = checkpoint_ticket(drain(&mut ann.rx)).unwrap().0;
        let (reply, answer) = oneshot::channel();
        room.handle(LiveCommand::CheckpointUpload {
            user: UserId(id(11)),
            ticket,
            at: Seq(1),
            base: None,
            blob: BlobRef {
                sha256: [1; 32],
                size: 3,
            },
            session_data: vec![0; super::MAX_SESSION_DATA_BYTES + 1],
            reply,
        })
        .await
        .unwrap();
        assert_eq!(answer.await.unwrap(), LiveCheckpointReply::Refused);
    }

    fn ticket_full(
        messages: Vec<LiveServerMsg>,
    ) -> Option<(Uuid, TicketPurpose, Seq, Option<String>)> {
        messages.into_iter().find_map(|message| match message {
            LiveServerMsg::CheckpointTicket {
                ticket,
                purpose,
                at,
                base,
                ..
            } => Some((ticket, purpose, at, base)),
            _ => None,
        })
    }

    async fn hash<S: crate::live_store::LiveStore>(
        room: &mut LiveRoom<S>,
        user: UserId,
        ticket: Uuid,
        at: Seq,
        sha: u8,
    ) -> LiveCheckpointReply {
        let (reply, answer) = oneshot::channel();
        room.handle(LiveCommand::CheckpointHash {
            user,
            ticket,
            at,
            sha256: [sha; 32],
            reply,
        })
        .await
        .unwrap();
        answer.await.unwrap()
    }

    async fn fingerprint<S: crate::live_store::LiveStore>(
        room: &mut LiveRoom<S>,
        joined: &Joined,
        at: u64,
        entered: &str,
    ) {
        room.handle(LiveCommand::Fingerprint {
            conn: joined.conn,
            client: joined.client,
            at: Seq(at),
            entered: entered.to_owned(),
        })
        .await
        .unwrap();
    }

    fn emitted_kinds(
        emitted: &mut mpsc::UnboundedReceiver<records::NewEvent>,
        prefix: &str,
    ) -> Vec<(&'static str, Value)> {
        std::iter::from_fn(|| emitted.try_recv().ok())
            .map(|event| (event.kind, event.detail))
            .filter(|(kind, _)| kind.starts_with(prefix))
            .collect()
    }

    fn resyncs(messages: &[LiveServerMsg]) -> usize {
        messages
            .iter()
            .filter(|message| matches!(message, LiveServerMsg::Catchup { .. }))
            .count()
    }

    /// Ann (Browser, user 11) and Bob (Desktop, user 12) are capable editors; Ann writes a
    /// checkpoint of `sha` at position 1 and Bob holds the verify ticket.
    async fn written_and_ticketed(
        events: EventSink,
        sha: u8,
    ) -> (LiveRoom<MemStore>, MemStore, Joined, Joined, String, Uuid) {
        let store = MemStore::new();
        let mut room = LiveRoom::load(DocId(id(100)), store.clone(), events, timing())
            .await
            .unwrap();
        let mut ann = join_capable(&mut room, 1, 1, "editor", LiveBuild::Browser).await;
        let mut bob = join_capable(&mut room, 2, 2, "editor", LiveBuild::Desktop).await;
        entry(&mut room, &ann, 1, 0, FORMAT, "one".into())
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(15)).await;
        room.expire_due().await.unwrap();
        let ticket = checkpoint_ticket(drain(&mut ann.rx)).unwrap().0;
        let _ = drain(&mut bob.rx);
        let LiveCheckpointReply::Saved { revision } =
            upload(&mut room, UserId(id(11)), ticket, Seq(1), sha).await
        else {
            panic!("the upload must be stored");
        };
        let (verify, purpose, at) = checkpoint_ticket(drain(&mut bob.rx)).unwrap();
        assert_eq!((purpose, at), (TicketPurpose::Verify, Seq(1)));
        (room, store, ann, bob, revision, verify)
    }

    #[tokio::test(start_paused = true)]
    async fn matching_second_hash_verifies_checkpoint() {
        let doc = DocId(id(100));
        let (events, mut emitted) = EventSink::channel();
        let (mut room, store, _ann, mut bob, revision, verify) =
            written_and_ticketed(events, 9).await;
        let _ = emitted_kinds(&mut emitted, "");

        assert_eq!(
            hash(&mut room, UserId(id(12)), verify, Seq(1), 9).await,
            LiveCheckpointReply::Accepted
        );
        let loaded = crate::live_store::LiveStore::load_live(&store, doc)
            .await
            .unwrap();
        assert_eq!(loaded.verified_through, Seq(1));
        assert_eq!(loaded.verified_checkpoint.unwrap().revision, revision);
        assert!(loaded.unverified_checkpoints.is_empty());
        assert_eq!(
            emitted_kinds(&mut emitted, "live.checkpoint"),
            [(
                "live.checkpoint_verified",
                json!({ "revision": revision, "through": 1 })
            )]
        );

        // The room's base moved: a resync starts from the verified checkpoint.
        let _ = drain(&mut bob.rx);
        room.handle(LiveCommand::Resync {
            conn: bob.conn,
            client: bob.client,
        })
        .await
        .unwrap();
        let resent = drain(&mut bob.rx);
        assert!(
            matches!(
                resent.as_slice(),
                [LiveServerMsg::Catchup { base: Seq(1), revision: Some(base), from: Seq(1), head: Seq(1) }]
                    if *base == revision
            ),
            "{resent:?}"
        );
        assert_eq!(
            hash(&mut room, UserId(id(12)), verify, Seq(1), 9).await,
            LiveCheckpointReply::Refused,
            "a verify ticket is one-use"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn same_participant_cannot_verify_its_upload() {
        let doc = DocId(id(100));
        let (mut room, store, mut ann, bob, _revision, verify) =
            written_and_ticketed(EventSink::discard(), 9).await;

        assert_eq!(
            hash(&mut room, UserId(id(11)), verify, Seq(1), 9).await,
            LiveCheckpointReply::Refused,
            "the writer cannot use another's verify ticket"
        );
        room.handle(LiveCommand::Leave {
            conn: bob.conn,
            client: bob.client,
        })
        .await
        .unwrap();
        tokio::time::advance(Duration::from_secs(300)).await;
        room.expire_due().await.unwrap();
        assert!(
            checkpoint_ticket(drain(&mut ann.rx)).is_none(),
            "the writer is never asked to verify its own upload"
        );
        assert_eq!(
            hash(&mut room, UserId(id(12)), verify, Seq(1), 9).await,
            LiveCheckpointReply::Refused,
            "leaving cancels the verify ticket"
        );
        let loaded = crate::live_store::LiveStore::load_live(&store, doc)
            .await
            .unwrap();
        assert_eq!(loaded.verified_through, Seq(0));
    }

    #[tokio::test(start_paused = true)]
    async fn silent_verifier_does_not_hold_back_checkpoints() {
        let (mut room, _store, mut ann, mut bob, revision, _verify) =
            written_and_ticketed(EventSink::discard(), 9).await;
        entry(&mut room, &ann, 2, 1, FORMAT, "two".into())
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(15)).await;
        room.expire_due().await.unwrap();
        assert!(
            checkpoint_ticket(drain(&mut ann.rx)).is_none(),
            "the outstanding verify ticket goes first"
        );

        tokio::time::advance(Duration::from_secs(120)).await;
        room.expire_due().await.unwrap();
        let mut asked = drain(&mut ann.rx);
        asked.extend(drain(&mut bob.rx));
        let tickets = asked
            .into_iter()
            .filter_map(|message| ticket_full(vec![message]))
            .collect::<Vec<_>>();
        assert_eq!(tickets.len(), 1, "{tickets:?}");
        assert_eq!(
            (tickets[0].1, tickets[0].2, tickets[0].3.clone()),
            (TicketPurpose::Write, Seq(2), Some(revision)),
            "the lapsed verifier is not asked again; checkpoint work goes on"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn restart_never_asks_the_writer_to_verify_its_checkpoint() {
        let doc = DocId(id(100));
        let store = MemStore::new();
        let (ann_resume, revision) = {
            let mut room = LiveRoom::load(doc, store.clone(), EventSink::discard(), timing())
                .await
                .unwrap();
            let mut ann = join_capable(&mut room, 1, 1, "editor", LiveBuild::Browser).await;
            entry(&mut room, &ann, 1, 0, FORMAT, "one".into())
                .await
                .unwrap();
            tokio::time::advance(Duration::from_secs(15)).await;
            room.expire_due().await.unwrap();
            let ticket = checkpoint_ticket(drain(&mut ann.rx)).unwrap().0;
            let LiveCheckpointReply::Saved { revision } =
                upload(&mut room, UserId(id(11)), ticket, Seq(1), 9).await
            else {
                panic!("the upload must be stored");
            };
            let resume = LiveResume {
                session: ann.session,
                participant: ann.participant,
            };
            (resume, revision)
        };
        let stored = crate::live_store::LiveStore::load_live(&store, doc)
            .await
            .unwrap();
        assert_eq!(
            stored.unverified_checkpoints[0].writer,
            Some(records::LiveCheckpointWriter {
                participant: ann_resume.participant.0,
                client: ClientId(id(1)),
            }),
            "the store records who uploaded the checkpoint"
        );

        // A restart: Ann keeps her place, and is the only capable editor.
        let mut room = LiveRoom::load(doc, store.clone(), EventSink::discard(), timing())
            .await
            .unwrap();
        let mut ann = join_full(
            &mut room,
            5,
            1,
            UserId(id(11)),
            "editor",
            FORMAT,
            Some(ann_resume),
            Some(Seq(1)),
            true,
            LiveBuild::Browser,
        )
        .await;
        assert_eq!(ann.place, Place::Kept);
        tokio::time::advance(Duration::from_secs(15)).await;
        room.expire_due().await.unwrap();
        let mut asked = ann.after_join.clone();
        asked.extend(drain(&mut ann.rx));
        assert!(
            checkpoint_ticket(asked).is_none(),
            "the writer is never asked to verify its own checkpoint"
        );
        assert_eq!(
            hash(&mut room, UserId(id(11)), Uuid::new_v4(), Seq(1), 9).await,
            LiveCheckpointReply::Refused
        );

        // Another editor is asked instead.
        let bob = join_capable(&mut room, 6, 2, "editor", LiveBuild::Desktop).await;
        let (verify, purpose, at, base) = ticket_full(bob.after_join.clone()).unwrap();
        assert_eq!(
            (purpose, at, base),
            (TicketPurpose::Verify, Seq(1), Some(revision))
        );
        assert_eq!(
            hash(&mut room, UserId(id(12)), verify, Seq(1), 9).await,
            LiveCheckpointReply::Accepted
        );
    }

    #[tokio::test(start_paused = true)]
    async fn unknown_checkpoint_outcome_backs_off_and_reconciles() {
        let doc = DocId(id(100));
        let store = MemStore::new();
        let (events, mut emitted) = EventSink::channel();
        let mut room = LiveRoom::load(doc, store.clone(), events, timing())
            .await
            .unwrap();
        let mut ann = join_capable(&mut room, 1, 1, "editor", LiveBuild::Browser).await;
        let mut bob = join_capable(&mut room, 2, 2, "editor", LiveBuild::Desktop).await;
        entry(&mut room, &ann, 1, 0, FORMAT, "one".into())
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(15)).await;
        room.expire_due().await.unwrap();
        let ticket = checkpoint_ticket(drain(&mut ann.rx)).unwrap().0;
        let _ = drain(&mut bob.rx);
        let _ = emitted_kinds(&mut emitted, "");

        // Git commits; the database outcome is unknown and Git cannot describe the commit for
        // a while.
        store.fail_after_next();
        store.fail_describes(4);
        assert_eq!(
            upload(&mut room, UserId(id(11)), ticket, Seq(1), 9).await,
            LiveCheckpointReply::Failed
        );
        entry(&mut room, &bob, 1, 1, FORMAT, "two".into())
            .await
            .unwrap();

        // Retries back off: 5, 10, 20 and 40 seconds, so four tries take 75 seconds.
        let mut tries = Vec::new();
        for second in 1..=75 {
            tokio::time::advance(Duration::from_secs(1)).await;
            room.expire_due().await.unwrap();
            let asked = drain(&mut ann.rx)
                .into_iter()
                .chain(drain(&mut bob.rx))
                .filter(|message| matches!(message, LiveServerMsg::CheckpointTicket { .. }))
                .count();
            assert_eq!(
                asked, 0,
                "no ticket while the outcome is unknown ({second}s)"
            );
            if store.describe_calls() > tries.len() {
                tries.push(second);
            }
        }
        assert_eq!(tries, [5, 15, 35, 75]);
        assert_eq!(
            emitted_kinds(&mut emitted, "live.checkpoint"),
            [(
                "live.checkpoint_reconcile_fault",
                json!({ "revision": "0000000000000000000000000000000000000001", "through": 1 })
            )],
            "one fault event for the whole episode"
        );

        // The capped next try succeeds: the checkpoint is stored and verified as usual.
        tokio::time::advance(Duration::from_secs(80)).await;
        room.expire_due().await.unwrap();
        assert_eq!(store.describe_calls(), 5);
        assert_eq!(
            emitted_kinds(&mut emitted, "live.checkpoint"),
            [
                (
                    "live.checkpoint_unverified",
                    json!({ "revision": "0000000000000000000000000000000000000001", "through": 1 })
                ),
                (
                    "live.checkpoint_reconciled",
                    json!({ "revision": "0000000000000000000000000000000000000001", "through": 1 })
                ),
            ]
        );
        let (verify, purpose, at) = checkpoint_ticket(drain(&mut bob.rx)).unwrap();
        assert_eq!((purpose, at), (TicketPurpose::Verify, Seq(1)));
        assert_eq!(
            hash(&mut room, UserId(id(12)), verify, Seq(1), 9).await,
            LiveCheckpointReply::Accepted
        );
        assert_eq!(room.verified_through, Seq(1));

        // Checkpointing is not blocked: Bob's later change is ticketed.
        tokio::time::advance(Duration::from_secs(15)).await;
        room.expire_due().await.unwrap();
        let mut asked = drain(&mut ann.rx);
        asked.extend(drain(&mut bob.rx));
        let (_, purpose, at, _) = ticket_full(asked).unwrap();
        assert_eq!((purpose, at), (TicketPurpose::Write, Seq(2)));
    }

    #[tokio::test(start_paused = true)]
    async fn reconcile_retries_are_capped() {
        let doc = DocId(id(100));
        let store = MemStore::new();
        let mut room = LiveRoom::load(doc, store.clone(), EventSink::discard(), timing())
            .await
            .unwrap();
        let mut ann = join_capable(&mut room, 1, 1, "editor", LiveBuild::Browser).await;
        entry(&mut room, &ann, 1, 0, FORMAT, "one".into())
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(15)).await;
        room.expire_due().await.unwrap();
        let ticket = checkpoint_ticket(drain(&mut ann.rx)).unwrap().0;
        store.fail_after_next();
        store.fail_describes(u32::MAX);
        assert_eq!(
            upload(&mut room, UserId(id(11)), ticket, Seq(1), 9).await,
            LiveCheckpointReply::Failed
        );
        // A day of failures is a bounded number of tries: the wait stops growing at the cap.
        for _ in 0..(24 * 60) {
            tokio::time::advance(Duration::from_secs(60)).await;
            room.expire_due().await.unwrap();
        }
        let tries = store.describe_calls();
        let capped = super::RECONCILE_RETRY_MAX.as_secs() as usize;
        assert!(
            (24 * 3_600 / capped..=24 * 3_600 / capped + 8).contains(&tries),
            "{tries} tries in a day"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn silent_verifiers_are_not_asked_again_on_later_checkpoints() {
        let (mut room, store) = room().await;
        let mut ann = join_capable(&mut room, 1, 1, "editor", LiveBuild::Browser).await;
        let mut bob = join_capable(&mut room, 2, 2, "editor", LiveBuild::Desktop).await;
        let mut cat = join_capable(&mut room, 3, 3, "editor", LiveBuild::Desktop).await;
        let users = [UserId(id(11)), UserId(id(12)), UserId(id(13))];
        let mut verifies = [0; 3];
        let mut writes = Vec::new();
        let mut local = 0;
        for second in 0..=900_u64 {
            // Ann keeps changing the workbook: one change at the start and one per checkpoint.
            if second == 0 || writes.len() > usize::try_from(local).unwrap() - 1 {
                local += 1;
                let seen = room.head.0;
                entry(&mut room, &ann, local, seen, FORMAT, "x".into())
                    .await
                    .unwrap();
            }
            tokio::time::advance(Duration::from_secs(1)).await;
            room.expire_due().await.unwrap();
            let received = [drain(&mut ann.rx), drain(&mut bob.rx), drain(&mut cat.rx)];
            for (index, messages) in received.into_iter().enumerate() {
                for message in messages {
                    let LiveServerMsg::CheckpointTicket {
                        ticket,
                        purpose,
                        at,
                        base,
                        ..
                    } = message
                    else {
                        continue;
                    };
                    if purpose == TicketPurpose::Verify {
                        // Bob and Cat never answer a verify ticket.
                        verifies[index] += 1;
                    } else {
                        let reply =
                            upload_on(&mut room, users[index], ticket, at, base.as_deref(), 9)
                                .await;
                        assert!(matches!(reply, LiveCheckpointReply::Saved { .. }));
                        writes.push(second);
                    }
                }
            }
        }
        assert_eq!(verifies[0], 0, "the writer is never asked");
        assert!(verifies[1] <= 1 && verifies[2] <= 1, "{verifies:?}");
        assert!(writes.len() >= 4, "writes keep coming: {writes:?}");
        for pair in writes.windows(2) {
            assert!(
                pair[1] - pair[0] <= 15 + super::VERIFY_HOLD_MAX.as_secs() + 5,
                "verify tickets hold a write back for a bounded time: {writes:?}"
            );
        }
        assert!(store.revisions(DocId(id(100))).len() >= 4);
    }

    #[tokio::test(start_paused = true)]
    async fn forensic_uploads_are_rate_limited_per_person() {
        let doc = DocId(id(100));
        let (mut room, store, ann, mut bob, revision, verify) =
            written_and_ticketed(EventSink::discard(), 9).await;
        assert_eq!(
            hash(&mut room, UserId(id(12)), verify, Seq(1), 8).await,
            LiveCheckpointReply::Accepted
        );
        let (forensic, purpose, _, _) = ticket_full(drain(&mut bob.rx)).unwrap();
        assert_eq!(purpose, TicketPurpose::Write);
        assert!(matches!(
            upload_on(
                &mut room,
                UserId(id(12)),
                forensic,
                Seq(1),
                Some(&revision),
                8
            )
            .await,
            LiveCheckpointReply::Saved { .. }
        ));
        let written = store.revisions(doc).len();

        // A second checkpoint; Bob joins again, is asked to verify it and disagrees again.
        entry(&mut room, &ann, 2, 1, FORMAT, "two".into())
            .await
            .unwrap();
        let mut ann = ann;
        let mut second = None;
        for _ in 0..200 {
            tokio::time::advance(Duration::from_secs(1)).await;
            room.expire_due().await.unwrap();
            if let Some((ticket, TicketPurpose::Write, at, base)) = ticket_full(drain(&mut ann.rx))
            {
                let LiveCheckpointReply::Saved { revision } =
                    upload_on(&mut room, UserId(id(11)), ticket, at, base.as_deref(), 7).await
                else {
                    panic!("the second checkpoint must be stored");
                };
                second = Some(revision);
                break;
            }
        }
        let second = second.expect("a second write ticket");
        let resume = LiveResume {
            session: bob.session,
            participant: bob.participant,
        };
        let bob = join_full(
            &mut room,
            9,
            2,
            UserId(id(12)),
            "editor",
            FORMAT,
            Some(resume),
            Some(Seq(2)),
            true,
            LiveBuild::Desktop,
        )
        .await;
        let mut bob = bob;
        let (verify, purpose, at, base) = ticket_full(bob.after_join.clone()).unwrap();
        assert_eq!(
            (purpose, at, base),
            (TicketPurpose::Verify, Seq(2), Some(second))
        );
        assert_eq!(
            hash(&mut room, UserId(id(12)), verify, Seq(2), 6).await,
            LiveCheckpointReply::Accepted
        );
        room.expire_due().await.unwrap();
        assert!(
            checkpoint_ticket(drain(&mut bob.rx)).is_none(),
            "one forensic upload per person in ten minutes"
        );
        assert_eq!(store.revisions(doc).len(), written + 1);
    }

    #[tokio::test(start_paused = true)]
    async fn panicking_checkpoint_write_is_a_failure() {
        let (mut room, store) = room().await;
        let mut ann = join_capable(&mut room, 1, 1, "editor", LiveBuild::Browser).await;
        entry(&mut room, &ann, 1, 0, FORMAT, "one".into())
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(15)).await;
        room.expire_due().await.unwrap();
        let ticket = checkpoint_ticket(drain(&mut ann.rx)).unwrap().0;
        store.panic_next_write();
        assert_eq!(
            upload(&mut room, UserId(id(11)), ticket, Seq(1), 9).await,
            LiveCheckpointReply::Failed
        );
        assert!(!room.writing);
        tokio::time::advance(Duration::from_secs(15)).await;
        room.expire_due().await.unwrap();
        let (_, purpose, at) = checkpoint_ticket(drain(&mut ann.rx)).unwrap();
        assert_eq!((purpose, at), (TicketPurpose::Write, Seq(1)));
    }

    #[tokio::test(start_paused = true)]
    async fn hostile_tabs_cannot_outvote_other_people() {
        let (events, mut emitted) = EventSink::channel();
        let mut room = LiveRoom::load(DocId(id(100)), MemStore::new(), events, timing())
            .await
            .unwrap();
        let mut ann = join_capable(&mut room, 1, 1, "editor", LiveBuild::Browser).await;
        let mut bob = join_capable(&mut room, 2, 2, "editor", LiveBuild::Desktop).await;
        let mut cat = join_capable(&mut room, 3, 3, "editor", LiveBuild::Browser).await;
        let mallory = UserId(id(99));
        let mut tabs = Vec::new();
        for (conn, client) in [(4, 4), (5, 5), (6, 6)] {
            tabs.push(
                join_full(
                    &mut room,
                    conn,
                    client,
                    mallory,
                    "editor",
                    FORMAT,
                    None,
                    None,
                    true,
                    LiveBuild::Browser,
                )
                .await,
            );
        }
        for local in 1..=64 {
            entry(&mut room, &ann, local, local - 1, FORMAT, "x".into())
                .await
                .unwrap();
        }
        let agreed = "a".repeat(64);
        let bogus = "b".repeat(64);
        // Mallory's three tabs report first; then the honest people one by one.
        for tab in &tabs {
            fingerprint(&mut room, tab, 64, &bogus).await;
        }
        fingerprint(&mut room, &ann, 64, &agreed).await;
        fingerprint(&mut room, &bob, 64, &agreed).await;
        let _ = (drain(&mut ann.rx), drain(&mut bob.rx), drain(&mut cat.rx));
        assert!(
            emitted_kinds(&mut emitted, "live.agreement").is_empty(),
            "nothing is decided before every editing person reported"
        );
        fingerprint(&mut room, &cat, 64, &agreed).await;

        assert_eq!(resyncs(&drain(&mut ann.rx)), 0);
        assert_eq!(resyncs(&drain(&mut bob.rx)), 0);
        assert_eq!(resyncs(&drain(&mut cat.rx)), 0);
        for tab in &mut tabs {
            assert_eq!(resyncs(&drain(&mut tab.rx)), 1);
        }
        let numbers = tabs.iter().map(|tab| tab.participant.0).collect::<Vec<_>>();
        assert_eq!(
            emitted_kinds(&mut emitted, "live.agreement"),
            [(
                "live.agreement_mismatch",
                json!({ "at": 64, "reports": 6, "resynced": numbers })
            )]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn agreement_is_decided_after_a_bounded_wait() {
        let mut room = LiveRoom::load(
            DocId(id(100)),
            MemStore::new(),
            EventSink::discard(),
            timing(),
        )
        .await
        .unwrap();
        let mut ann = join_capable(&mut room, 1, 1, "editor", LiveBuild::Browser).await;
        let mut bob = join_capable(&mut room, 2, 2, "editor", LiveBuild::Desktop).await;
        let mut cat = join_capable(&mut room, 3, 3, "editor", LiveBuild::Browser).await;
        for local in 1..=64 {
            entry(&mut room, &ann, local, local - 1, FORMAT, "x".into())
                .await
                .unwrap();
        }
        let _ = (drain(&mut ann.rx), drain(&mut bob.rx), drain(&mut cat.rx));
        fingerprint(&mut room, &ann, 64, &"a".repeat(64)).await;
        fingerprint(&mut room, &bob, 64, &"a".repeat(64)).await;
        fingerprint(&mut room, &cat, 64, &"a".repeat(64)).await;
        // A later position where Cat never reports: Ann and Bob disagree, decided at the wait.
        tokio::time::advance(Duration::from_secs(10)).await;
        for local in 65..=128 {
            entry(&mut room, &ann, local, local - 1, FORMAT, "x".into())
                .await
                .unwrap();
        }
        let _ = (drain(&mut ann.rx), drain(&mut bob.rx), drain(&mut cat.rx));
        fingerprint(&mut room, &ann, 128, &"a".repeat(64)).await;
        fingerprint(&mut room, &bob, 128, &"c".repeat(64)).await;
        tokio::time::advance(super::AGREEMENT_WAIT - Duration::from_secs(1)).await;
        room.expire_due().await.unwrap();
        assert_eq!(resyncs(&drain(&mut ann.rx)), 0);
        assert_eq!(resyncs(&drain(&mut bob.rx)), 0);
        assert!(room.next_wake().is_some());
        tokio::time::advance(Duration::from_secs(1)).await;
        room.expire_due().await.unwrap();
        assert_eq!(resyncs(&drain(&mut ann.rx)), 1);
        assert_eq!(resyncs(&drain(&mut bob.rx)), 1);
        assert_eq!(resyncs(&drain(&mut cat.rx)), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn lone_checkpoint_stays_unverified_until_second_hash() {
        let doc = DocId(id(100));
        let (mut room, store) = room().await;
        let mut ann = join_capable(&mut room, 1, 1, "editor", LiveBuild::Browser).await;
        entry(&mut room, &ann, 1, 0, FORMAT, "one".into())
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(15)).await;
        room.expire_due().await.unwrap();
        let ticket = checkpoint_ticket(drain(&mut ann.rx)).unwrap().0;
        let LiveCheckpointReply::Saved { revision } =
            upload(&mut room, UserId(id(11)), ticket, Seq(1), 9).await
        else {
            panic!("the upload must be stored");
        };
        tokio::time::advance(Duration::from_secs(300)).await;
        room.expire_due().await.unwrap();
        assert!(checkpoint_ticket(drain(&mut ann.rx)).is_none());
        let loaded = crate::live_store::LiveStore::load_live(&store, doc)
            .await
            .unwrap();
        assert_eq!(loaded.verified_through, Seq(0));
        assert_eq!(loaded.unverified_checkpoints.len(), 1);
        assert_eq!(
            crate::live_store::LiveStore::current_revision(&store, doc)
                .await
                .unwrap(),
            Some(revision.clone()),
            "the lone checkpoint is current while unverified"
        );

        let cat = join_capable(&mut room, 3, 3, "editor", LiveBuild::Desktop).await;
        let (verify, purpose, at) = checkpoint_ticket(cat.after_join.clone()).unwrap();
        assert_eq!((purpose, at), (TicketPurpose::Verify, Seq(1)));
        assert_eq!(
            hash(&mut room, UserId(id(13)), verify, Seq(1), 9).await,
            LiveCheckpointReply::Accepted
        );
        let loaded = crate::live_store::LiveStore::load_live(&store, doc)
            .await
            .unwrap();
        assert_eq!(loaded.verified_through, Seq(1));
        assert_eq!(loaded.verified_checkpoint.unwrap().revision, revision);
    }

    #[tokio::test(start_paused = true)]
    async fn later_joiner_rederives_unverified_checkpoint() {
        let doc = DocId(id(100));
        let store = MemStore::new();
        let revision = {
            let mut room = LiveRoom::load(doc, store.clone(), EventSink::discard(), timing())
                .await
                .unwrap();
            let mut ann = join_capable(&mut room, 1, 1, "editor", LiveBuild::Browser).await;
            for local in 1..=2 {
                entry(&mut room, &ann, local, local - 1, FORMAT, "x".into())
                    .await
                    .unwrap();
            }
            tokio::time::advance(Duration::from_secs(15)).await;
            room.expire_due().await.unwrap();
            let ticket = checkpoint_ticket(drain(&mut ann.rx)).unwrap().0;
            let LiveCheckpointReply::Saved { revision } =
                upload(&mut room, UserId(id(11)), ticket, Seq(2), 9).await
            else {
                panic!("the upload must be stored");
            };
            revision
        };

        // A restart: the unverified checkpoint is only in the store.
        let mut room = LiveRoom::load(doc, store.clone(), EventSink::discard(), timing())
            .await
            .unwrap();
        let viewer = join_capable(&mut room, 2, 2, "viewer", LiveBuild::Browser).await;
        assert!(
            checkpoint_ticket(viewer.after_join.clone()).is_none(),
            "a viewer is never asked"
        );
        let mut cat = join_capable(&mut room, 3, 3, "editor", LiveBuild::Desktop).await;
        assert!(matches!(
            &cat.initial[1],
            LiveServerMsg::Catchup {
                base: Seq(0),
                revision: None,
                from: Seq(0),
                head: Seq(2)
            }
        ));
        assert_eq!(entries(cat.initial.clone()).len(), 2);
        let (verify, purpose, at) = checkpoint_ticket(cat.after_join.clone()).unwrap();
        assert_eq!((purpose, at), (TicketPurpose::Verify, Seq(2)));

        // A newer change waits until the unverified checkpoint is verified.
        let mut dan = join_capable(&mut room, 4, 4, "editor", LiveBuild::Browser).await;
        entry(&mut room, &dan, 1, 2, FORMAT, "three".into())
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(15)).await;
        room.expire_due().await.unwrap();
        assert!(checkpoint_ticket(drain(&mut dan.rx)).is_none());
        assert!(checkpoint_ticket(drain(&mut cat.rx)).is_none());

        assert_eq!(
            hash(&mut room, UserId(id(13)), verify, Seq(2), 9).await,
            LiveCheckpointReply::Accepted
        );
        let loaded = crate::live_store::LiveStore::load_live(&store, doc)
            .await
            .unwrap();
        assert_eq!(loaded.verified_through, Seq(2));
        let mut asked = drain(&mut dan.rx);
        asked.extend(drain(&mut cat.rx));
        let (_, purpose, at, base) = ticket_full(asked).unwrap();
        assert_eq!(
            (purpose, at, base),
            (TicketPurpose::Write, Seq(3), Some(revision))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn second_hash_mismatch_keeps_both_files_and_log() {
        let doc = DocId(id(100));
        let (events, mut emitted) = EventSink::channel();
        let (mut room, store, mut ann, mut bob, revision, verify) =
            written_and_ticketed(events, 9).await;
        let _ = emitted_kinds(&mut emitted, "");

        assert_eq!(
            hash(&mut room, UserId(id(12)), verify, Seq(1), 8).await,
            LiveCheckpointReply::Accepted
        );
        let loaded = crate::live_store::LiveStore::load_live(&store, doc)
            .await
            .unwrap();
        assert_eq!(
            loaded.verified_through,
            Seq(0),
            "a mismatch verifies nothing"
        );
        assert_eq!(loaded.unverified_checkpoints.len(), 1);
        assert_eq!(loaded.unverified_checkpoints[0].session_data, b"identities");
        assert_eq!(loaded.entries.len(), 1, "the log is kept");
        assert_eq!(
            emitted_kinds(&mut emitted, "live.checkpoint"),
            [(
                "live.checkpoint_mismatch",
                json!({ "revision": revision, "through": 1 })
            )]
        );

        // The verifier writes its own bytes for the record.
        let (forensic, purpose, at, base) = ticket_full(drain(&mut bob.rx)).unwrap();
        assert_eq!(
            (purpose, at, base.as_deref()),
            (TicketPurpose::Write, Seq(1), Some(revision.as_str()))
        );
        assert!(checkpoint_ticket(drain(&mut ann.rx)).is_none());
        let LiveCheckpointReply::Saved { revision: kept } = upload_on(
            &mut room,
            UserId(id(12)),
            forensic,
            Seq(1),
            Some(&revision),
            8,
        )
        .await
        else {
            panic!("the verifier's bytes must be kept");
        };
        let revisions = store.revisions(doc);
        assert_eq!(revisions.len(), 3, "{revisions:?}");
        assert_eq!(revisions[0], (revision.clone(), [9; 32]));
        assert_eq!(revisions[1], (kept.clone(), [8; 32]));
        assert_eq!(revisions[2].1, [9; 32], "the agreed file is current again");
        let current = revisions[2].0.clone();
        assert_eq!(
            crate::live_store::LiveStore::current_revision(&store, doc)
                .await
                .unwrap(),
            Some(current.clone())
        );
        let loaded = crate::live_store::LiveStore::load_live(&store, doc)
            .await
            .unwrap();
        assert_eq!(loaded.verified_through, Seq(0));
        assert_eq!(loaded.unverified_checkpoints.len(), 1);
        assert_eq!(loaded.unverified_checkpoints[0].revision, revision);
        assert_eq!(
            emitted_kinds(&mut emitted, "live.checkpoint"),
            [(
                "live.checkpoint_forensic",
                json!({ "revision": kept, "of": revision, "through": 1 })
            )]
        );

        // The room continues from the log, on top of the current file.
        entry(&mut room, &ann, 2, 1, FORMAT, "two".into())
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(15)).await;
        room.expire_due().await.unwrap();
        let mut asked = drain(&mut ann.rx);
        asked.extend(drain(&mut bob.rx));
        let (_, purpose, at, base) = ticket_full(asked).unwrap();
        assert_eq!(
            (purpose, at, base),
            (TicketPurpose::Write, Seq(2), Some(current))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn agreement_uses_entered_digest_only() {
        let (events, mut emitted) = EventSink::channel();
        let mut room = LiveRoom::load(DocId(id(100)), MemStore::new(), events, timing())
            .await
            .unwrap();
        let mut ann = join_capable(&mut room, 1, 1, "editor", LiveBuild::Browser).await;
        let mut bob = join_capable(&mut room, 2, 2, "editor", LiveBuild::Desktop).await;
        let mut viewer = join_capable(&mut room, 3, 3, "viewer", LiveBuild::Browser).await;
        for local in 1..=64 {
            entry(&mut room, &ann, local, local - 1, FORMAT, "x".into())
                .await
                .unwrap();
        }
        let _ = (
            drain(&mut ann.rx),
            drain(&mut bob.rx),
            drain(&mut viewer.rx),
        );
        let entered = "a".repeat(64);
        fingerprint(&mut room, &ann, 64, &entered).await;
        fingerprint(&mut room, &bob, 64, &entered).await;
        // Off the agreement positions, beyond the head and from a viewer, reports are not
        // compared.
        fingerprint(&mut room, &ann, 63, &"b".repeat(64)).await;
        fingerprint(&mut room, &bob, 63, &"c".repeat(64)).await;
        fingerprint(&mut room, &viewer, 64, &"d".repeat(64)).await;
        fingerprint(&mut room, &ann, 128, &"e".repeat(64)).await;
        fingerprint(&mut room, &bob, 128, &"f".repeat(64)).await;

        assert_eq!(resyncs(&drain(&mut ann.rx)), 0);
        assert_eq!(resyncs(&drain(&mut bob.rx)), 0);
        assert_eq!(resyncs(&drain(&mut viewer.rx)), 0);
        assert!(emitted_kinds(&mut emitted, "live.agreement").is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn agreement_mismatch_resyncs_only_the_odd_client() {
        let (events, mut emitted) = EventSink::channel();
        let mut room = LiveRoom::load(DocId(id(100)), MemStore::new(), events, timing())
            .await
            .unwrap();
        let mut ann = join_capable(&mut room, 1, 1, "editor", LiveBuild::Browser).await;
        let mut bob = join_capable(&mut room, 2, 2, "editor", LiveBuild::Desktop).await;
        let mut cat = join_capable(&mut room, 3, 3, "editor", LiveBuild::Browser).await;
        for local in 1..=64 {
            entry(&mut room, &ann, local, local - 1, FORMAT, "x".into())
                .await
                .unwrap();
        }
        let _ = (drain(&mut ann.rx), drain(&mut bob.rx), drain(&mut cat.rx));
        let agreed = "a".repeat(64);
        let odd = "b".repeat(64);
        // The odd report comes first; nothing is decided until everyone has reported.
        fingerprint(&mut room, &cat, 64, &odd).await;
        fingerprint(&mut room, &ann, 64, &agreed).await;
        assert_eq!(resyncs(&drain(&mut ann.rx)), 0);
        assert_eq!(resyncs(&drain(&mut cat.rx)), 0);
        fingerprint(&mut room, &bob, 64, &agreed).await;

        assert_eq!(resyncs(&drain(&mut ann.rx)), 0);
        assert_eq!(resyncs(&drain(&mut bob.rx)), 0);
        let resent = drain(&mut cat.rx);
        assert!(matches!(
            resent.first(),
            Some(LiveServerMsg::Catchup {
                base: Seq(0),
                from: Seq(0),
                head: Seq(64),
                ..
            })
        ));
        assert_eq!(entries(resent).len(), 64);
        assert_eq!(
            emitted_kinds(&mut emitted, "live.agreement"),
            [(
                "live.agreement_mismatch",
                json!({ "at": 64, "reports": 3, "resynced": [3] })
            )]
        );

        // At a later position the odd report comes second: still only the odd client.
        tokio::time::advance(Duration::from_secs(10)).await;
        for local in 65..=128 {
            entry(&mut room, &ann, local, local - 1, FORMAT, "x".into())
                .await
                .unwrap();
        }
        let _ = (drain(&mut ann.rx), drain(&mut bob.rx), drain(&mut cat.rx));
        fingerprint(&mut room, &ann, 128, &agreed).await;
        fingerprint(&mut room, &bob, 128, &odd).await;
        assert_eq!(resyncs(&drain(&mut ann.rx)), 0);
        assert_eq!(resyncs(&drain(&mut bob.rx)), 0);
        fingerprint(&mut room, &cat, 128, &agreed).await;
        assert_eq!(resyncs(&drain(&mut ann.rx)), 0);
        assert_eq!(resyncs(&drain(&mut bob.rx)), 1);
        assert_eq!(resyncs(&drain(&mut cat.rx)), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn room_keeps_ordering_entries_while_a_checkpoint_is_written() {
        let (mut room, store) = room().await;
        let mut ann = join_capable(&mut room, 1, 1, "editor", LiveBuild::Browser).await;
        let mut bob = join_as(&mut room, 2, 2, 12, "editor", FORMAT, None, None).await;
        entry(&mut room, &ann, 1, 0, FORMAT, "one".into())
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(15)).await;
        room.expire_due().await.unwrap();
        let ticket = checkpoint_ticket(drain(&mut ann.rx)).unwrap().0;
        let _ = drain(&mut bob.rx);
        let gate = store.hold_checkpoint_writes();

        let (reply, mut answer) = oneshot::channel();
        tokio::time::timeout(
            Duration::from_secs(5),
            room.handle(LiveCommand::CheckpointUpload {
                user: UserId(id(11)),
                ticket,
                at: Seq(1),
                base: None,
                blob: BlobRef {
                    sha256: [9; 32],
                    size: 3,
                },
                session_data: b"identities".to_vec(),
                reply,
            }),
        )
        .await
        .expect("the room must not wait for the Git write")
        .unwrap();
        entry(&mut room, &bob, 1, 1, FORMAT, "during the write".into())
            .await
            .unwrap();
        let ordered = entries(drain(&mut ann.rx));
        assert_eq!(
            ordered.iter().map(|entry| entry.seq).collect::<Vec<_>>(),
            [Seq(2)]
        );
        assert!(
            answer.try_recv().is_err(),
            "not answered before Git is done"
        );
        tokio::time::advance(Duration::from_secs(15)).await;
        room.expire_due().await.unwrap();
        assert!(
            checkpoint_ticket(drain(&mut ann.rx)).is_none(),
            "no second write ticket while one is being written"
        );

        gate.add_permits(1);
        room.finish_writes().await.unwrap();
        let LiveCheckpointReply::Saved { revision } = answer.await.unwrap() else {
            panic!("the upload must be stored");
        };
        tokio::time::advance(Duration::from_secs(15)).await;
        room.expire_due().await.unwrap();
        let (_, purpose, at, base) = ticket_full(drain(&mut ann.rx)).unwrap();
        assert_eq!(
            (purpose, at, base),
            (TicketPurpose::Write, Seq(2), Some(revision)),
            "the entry ordered during the write is still unsaved"
        );
    }

    #[tokio::test]
    async fn database_restart_loads_the_verified_base_and_sends_its_suffix() {
        let test = records::test_support::TestDb::create().await;
        let db = test.db.clone();
        let mut users = Vec::new();
        for name in ["Ann", "Bob"] {
            users.push(
                db.create_user(
                    None,
                    records::NewUser {
                        email: &format!("{name}@test.invalid"),
                        name,
                        password: "a long enough password",
                        is_admin: false,
                    },
                )
                .await
                .unwrap()
                .id,
            );
        }
        let doc = DocId(Uuid::new_v4());
        db.create_document(users[0], doc, "book.xlsx", records::Editor::GridCraft)
            .await
            .unwrap();
        let mut first = LiveRoom::load(doc, db.clone(), EventSink::discard(), timing())
            .await
            .unwrap();
        let mut ann = join_user(&mut first, 1, 1, users[0], "editor", FORMAT, None, None).await;
        for local in 1..=3 {
            entry(
                &mut first,
                &ann,
                local,
                local - 1,
                FORMAT,
                format!("change {local}"),
            )
            .await
            .unwrap();
        }
        let _ = drain(&mut ann.rx);
        let revision = "a".repeat(40);
        let now = time::OffsetDateTime::now_utc()
            .replace_nanosecond(0)
            .unwrap();
        db.record_live_checkpoint_revision(
            None,
            &records::RevisionRecord {
                doc,
                revision: revision.clone(),
                committed_at: now,
                authors: vec![(users[0].0.to_string(), "Ann".to_owned())],
                name: "book.xlsx".to_owned(),
                fields: json!({ "verified": false, "live_through": 2 }),
                size: 10,
            },
            &NewLiveCheckpoint {
                created_at: now,
                ..checkpoint(&revision, Seq(2), ann.session, b"identities at two")
            },
        )
        .await
        .unwrap();
        db.verify_live_checkpoint(doc, &revision, Seq(2), now)
            .await
            .unwrap();
        drop(first);

        let mut restarted = LiveRoom::load(doc, db.clone(), EventSink::discard(), timing())
            .await
            .unwrap();
        let bob = join_user(&mut restarted, 2, 2, users[1], "editor", FORMAT, None, None).await;
        match bob.initial.as_slice() {
            [
                LiveServerMsg::LiveJoined { .. },
                LiveServerMsg::Catchup {
                    base: Seq(2),
                    revision: Some(base_revision),
                    from: Seq(2),
                    head: Seq(3),
                },
                LiveServerMsg::Entry { entry },
            ] => {
                assert_eq!(base_revision, &revision);
                assert_eq!(entry.seq, Seq(3));
                assert!(matches!(
                    &entry.body,
                    EntryBody::Change { payload, .. } if payload == "change 3"
                ));
            }
            other => panic!("unexpected catch-up from the database: {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn unverified_checkpoint_does_not_advance_catchup_base() {
        let doc = DocId(id(100));
        let store = MemStore::new();
        let mut first = LiveRoom::load(doc, store.clone(), EventSink::discard(), timing())
            .await
            .unwrap();
        let mut ann = join_as(&mut first, 1, 1, 11, "editor", FORMAT, None, None).await;
        entry(&mut first, &ann, 1, 0, FORMAT, "verified".into())
            .await
            .unwrap();
        entry(&mut first, &ann, 2, 1, FORMAT, "unverified".into())
            .await
            .unwrap();
        let _ = drain(&mut ann.rx);

        crate::live_store::LiveStore::record_live_checkpoint(
            &store,
            doc,
            &checkpoint(
                "verified-revision",
                Seq(1),
                ann.session,
                b"verified identities",
            ),
        )
        .await
        .unwrap();
        crate::live_store::LiveStore::verify_live_checkpoint(
            &store,
            doc,
            "verified-revision",
            Seq(1),
            time::OffsetDateTime::UNIX_EPOCH,
        )
        .await
        .unwrap();
        crate::live_store::LiveStore::record_live_checkpoint(
            &store,
            doc,
            &checkpoint(
                "unverified-revision",
                Seq(2),
                ann.session,
                b"unverified identities",
            ),
        )
        .await
        .unwrap();
        let mut restarted = LiveRoom::load(doc, store, EventSink::discard(), timing())
            .await
            .unwrap();

        let joined = join_as(&mut restarted, 2, 2, 12, "editor", FORMAT, None, None).await;
        assert!(matches!(
            joined.initial.as_slice(),
            [
                LiveServerMsg::LiveJoined { .. },
                LiveServerMsg::Catchup {
                    base: Seq(1),
                    revision: Some(revision),
                    from: Seq(1),
                    head: Seq(2),
                },
                LiveServerMsg::Entry { entry },
            ] if revision == "verified-revision" && entry.seq == Seq(2)
        ));
    }

    #[tokio::test]
    async fn checkpoint_reconcile_mismatch_faults_without_storing_metadata() {
        let doc = DocId(id(100));
        let store = MemStore::new();
        let (events, mut emitted) = EventSink::channel();
        let room = LiveRoom::load(doc, store.clone(), events, timing())
            .await
            .unwrap();
        let checkpoint = checkpoint(
            "1111111111111111111111111111111111111111",
            Seq(0),
            protocol::live::LiveSessionId(id(200)),
            b"identities",
        );
        let fields: Map<String, Value> = serde_json::from_value(json!({
            "title": "kept base field",
            "verified": false,
            "live_through": 0,
        }))
        .unwrap();
        let described = RevisionInfo {
            revision: checkpoint.revision.parse().unwrap(),
            doc: DocId(id(999)),
            committed_at: 0,
            authors: Vec::new(),
            file_name: "book.gridcraft".into(),
            fields: fields.clone(),
            blob: Some(BlobRef {
                sha256: checkpoint.sha256,
                size: 10,
            }),
            size: 10,
        };

        assert!(
            room.reconcile_live_checkpoint(None, &checkpoint, &described, &fields)
                .await
                .is_err()
        );
        let event = emitted.try_recv().unwrap();
        assert_eq!(event.kind, "live.checkpoint_reconcile_fault");
        assert_eq!(
            event.detail,
            json!({ "revision": checkpoint.revision, "through": 0 })
        );
        assert!(
            crate::live_store::LiveStore::load_live(&store, doc)
                .await
                .unwrap()
                .unverified_checkpoints
                .is_empty()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn three_missed_heartbeats_expire() {
        let (mut room, _) = room().await;
        let mut ann = join_as(&mut room, 1, 1, 11, "editor", FORMAT, None, None).await;
        let mut bob = join_as(&mut room, 2, 2, 12, "editor", FORMAT, None, None).await;
        let _ = drain(&mut ann.rx);

        tokio::time::advance(Duration::from_millis(179_999)).await;
        room.handle(LiveCommand::Heartbeat {
            conn: bob.conn,
            client: bob.client,
            seen: Seq(0),
        })
        .await
        .unwrap();
        room.expire_due().await.unwrap();
        assert!(drain(&mut ann.rx).is_empty());
        assert_eq!(
            room.next_expiry(),
            Some(tokio::time::Instant::now() + Duration::from_millis(1))
        );

        tokio::time::advance(Duration::from_millis(1)).await;
        room.expire_due().await.unwrap();
        assert!(matches!(
            drain(&mut ann.rx).as_slice(),
            [LiveServerMsg::Expired]
        ));
        assert!(drain(&mut bob.rx).iter().any(|message| matches!(
            message,
            LiveServerMsg::People { people }
                if people.iter().all(|person| person.participant != ann.participant)
        )));
    }

    #[tokio::test(start_paused = true)]
    async fn any_message_counts_as_life() {
        let (mut room, _) = room().await;
        let ann = join_as(&mut room, 1, 1, 11, "editor", FORMAT, None, None).await;
        for local in 1..=6 {
            tokio::time::advance(Duration::from_secs(100)).await;
            let format = if local == 2 { "wrong/1" } else { FORMAT };
            let seen = room.head.0;
            entry(&mut room, &ann, local, seen, format, local.to_string())
                .await
                .unwrap();
            room.expire_due().await.unwrap();
            assert!(
                room.people()
                    .iter()
                    .any(|person| person.participant == ann.participant)
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn disconnect_keeps_the_place() {
        let (mut room, _) = room().await;
        let ann = join_as(&mut room, 1, 1, 11, "editor", FORMAT, None, None).await;
        let resume = Some(LiveResume {
            session: ann.session,
            participant: ann.participant,
        });
        room.handle(LiveCommand::Disconnected {
            conn: ann.conn,
            client: ann.client,
        })
        .await
        .unwrap();
        tokio::time::advance(Duration::from_secs(179)).await;
        room.expire_due().await.unwrap();

        let kept = join_as(&mut room, 2, 1, 11, "editor", FORMAT, resume, Some(Seq(0))).await;
        assert_eq!(kept.participant, ann.participant);
        assert_eq!(kept.place, Place::Kept);
    }

    #[tokio::test(start_paused = true)]
    async fn entry_in_flight_at_expiry_is_stored_once() {
        {
            let (mut room, store) = room().await;
            let ann = join_as(&mut room, 1, 1, 11, "editor", FORMAT, None, None).await;
            tokio::time::advance(Duration::from_millis(179_900)).await;
            entry(&mut room, &ann, 1, 0, FORMAT, "just-in-time".into())
                .await
                .unwrap();
            tokio::time::advance(Duration::from_millis(100_000)).await;
            room.expire_due().await.unwrap();
            assert_eq!(store.entry_count(), 1);
            assert!(
                room.people()
                    .iter()
                    .any(|person| person.participant == ann.participant)
            );
        }

        {
            let (mut room, store) = room().await;
            let ann = join_as(&mut room, 1, 1, 11, "editor", FORMAT, None, None).await;
            let resume = Some(LiveResume {
                session: ann.session,
                participant: ann.participant,
            });
            tokio::time::advance(Duration::from_secs(180)).await;
            room.expire_due().await.unwrap();
            entry(&mut room, &ann, 1, 0, FORMAT, "ignored".into())
                .await
                .unwrap();
            assert_eq!(store.entry_count(), 0);

            let returned =
                join_as(&mut room, 2, 1, 11, "editor", FORMAT, resume, Some(Seq(0))).await;
            assert_eq!(
                returned.place,
                Place::Returning {
                    previous: ann.participant
                }
            );
            entry(&mut room, &returned, 1, 0, FORMAT, "resent".into())
                .await
                .unwrap();
            entry(&mut room, &returned, 1, 0, FORMAT, "resent-again".into())
                .await
                .unwrap();
            assert_eq!(store.entry_count(), 1);
        }

        {
            let (mut room, store) = room().await;
            let ann = join_as(&mut room, 1, 1, 11, "editor", FORMAT, None, None).await;
            let resume = Some(LiveResume {
                session: ann.session,
                participant: ann.participant,
            });
            tokio::time::advance(Duration::from_secs(100)).await;
            entry(&mut room, &ann, 1, 0, FORMAT, "stored".into())
                .await
                .unwrap();
            tokio::time::advance(Duration::from_secs(180)).await;
            room.expire_due().await.unwrap();
            let returned =
                join_as(&mut room, 2, 1, 11, "editor", FORMAT, resume, Some(Seq(1))).await;
            entry(&mut room, &returned, 1, 0, FORMAT, "repeat".into())
                .await
                .unwrap();
            assert_eq!(store.entry_count(), 1);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn last_place_gone_ends_session() {
        let store = MemStore::new();
        let (events, mut event_rx) = EventSink::channel();
        let mut room = LiveRoom::load(DocId(id(100)), store, events, timing())
            .await
            .unwrap();
        let ann = join_as(&mut room, 1, 1, 11, "editor", FORMAT, None, None).await;
        let started = event_rx.recv().await.unwrap();
        assert_eq!(started.kind, "live.session_started");
        entry(&mut room, &ann, 1, 0, FORMAT, "one".into())
            .await
            .unwrap();
        room.handle(LiveCommand::Leave {
            conn: ann.conn,
            client: ann.client,
        })
        .await
        .unwrap();
        room.settle().await.unwrap();
        assert!(!room.has_session());
        let mut ended = None;
        while let Ok(event) = event_rx.try_recv() {
            if event.kind == "live.session_ended" {
                ended = Some(event);
            }
        }
        let ended = ended.expect("session-ended event");
        assert_eq!(ended.detail, serde_json::json!({ "head": 1, "entries": 1 }));

        let bob = join_as(&mut room, 2, 2, 12, "editor", FORMAT, None, None).await;
        assert_eq!(bob.participant, ParticipantNo(2));
    }

    #[tokio::test(start_paused = true)]
    async fn session_end_counts_changes_not_cancel_records() {
        let store = MemStore::new();
        let (events, mut event_rx) = EventSink::channel();
        let mut room = LiveRoom::load(DocId(id(100)), store, events, timing())
            .await
            .unwrap();
        let ann = join_as(&mut room, 1, 1, 11, "editor", FORMAT, None, None).await;
        let bob = join_as(&mut room, 2, 2, 12, "editor", FORMAT, None, None).await;
        entry(&mut room, &ann, 1, 0, FORMAT, "cancelled".into())
            .await
            .unwrap();
        for member in [&ann, &bob] {
            room.handle(LiveCommand::Report {
                conn: member.conn,
                client: member.client,
                seq: Seq(1),
            })
            .await
            .unwrap();
        }
        for member in [&ann, &bob] {
            room.handle(LiveCommand::Leave {
                conn: member.conn,
                client: member.client,
            })
            .await
            .unwrap();
        }
        room.settle().await.unwrap();

        let ended = std::iter::from_fn(|| event_rx.try_recv().ok())
            .find(|event| event.kind == "live.session_ended")
            .expect("session-ended event");
        assert_eq!(ended.detail, serde_json::json!({ "head": 2, "entries": 1 }));
    }

    #[tokio::test(start_paused = true)]
    async fn loaded_session_continues_with_fresh_expiry() {
        let (mut original, store) = room().await;
        let ann = join_as(&mut original, 1, 1, 11, "editor", FORMAT, None, None).await;
        let _bob = join_as(&mut original, 2, 2, 12, "editor", FORMAT, None, None).await;
        let resume = Some(LiveResume {
            session: ann.session,
            participant: ann.participant,
        });
        tokio::time::advance(Duration::from_secs(1_000)).await;
        drop(original);

        let mut loaded = LiveRoom::load(DocId(id(100)), store, EventSink::discard(), timing())
            .await
            .unwrap();
        tokio::time::advance(Duration::from_millis(179_999)).await;
        loaded.expire_due().await.unwrap();
        assert_eq!(loaded.people().len(), 2);
        let kept = join_as(&mut loaded, 3, 1, 11, "editor", FORMAT, resume, None).await;
        assert_eq!(kept.place, Place::Kept);
        assert_eq!(kept.participant, ann.participant);
    }

    #[tokio::test(start_paused = true)]
    async fn viewers_hold_places_too() {
        let (mut room, _) = room().await;
        let viewer = join_as(&mut room, 1, 1, 11, "viewer", FORMAT, None, None).await;
        tokio::time::advance(Duration::from_secs(100)).await;
        room.handle(LiveCommand::Heartbeat {
            conn: viewer.conn,
            client: viewer.client,
            seen: Seq(0),
        })
        .await
        .unwrap();
        tokio::time::advance(Duration::from_secs(179)).await;
        room.expire_due().await.unwrap();
        assert!(room.has_session());
        tokio::time::advance(Duration::from_secs(1)).await;
        room.expire_due().await.unwrap();
        room.settle().await.unwrap();
        assert!(!room.has_session());
    }

    #[tokio::test(start_paused = true)]
    async fn reload_restores_client_ownership() {
        let (mut original, store) = room().await;
        let ann = join_as(&mut original, 1, 1, 11, "editor", FORMAT, None, None).await;
        let resume = Some(LiveResume {
            session: ann.session,
            participant: ann.participant,
        });
        drop(original);
        let mut loaded = LiveRoom::load(DocId(id(100)), store, EventSink::discard(), timing())
            .await
            .unwrap();
        let (tx, mut rx) = mpsc::channel(8);
        let (accepted, answer) = oneshot::channel();
        loaded
            .handle(LiveCommand::Join {
                conn: 2,
                client: ann.client,
                user: UserId(id(99)),
                session: SessionId(id(1099)),
                name: "Impostor".into(),
                role: builtin_access("editor"),
                protocol: 1,
                format: FORMAT.into(),
                resume,
                seen: Some(Seq(0)),
                can_checkpoint: false,
                build: LiveBuild::Browser,
                tx,
                accepted,
            })
            .await
            .unwrap();
        assert_eq!(answer.await.unwrap(), JoinReply::Refused);
        assert!(drain(&mut rx).is_empty());
        assert!(matches!(
            loaded.people().as_slice(),
            [person]
                if person.participant == ann.participant
                    && person.client == ann.client
                    && person.user == UserId(id(11))
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn ended_session_place_is_returning_after_reload() {
        let (mut original, store) = room().await;
        let ann = join_as(&mut original, 1, 1, 11, "editor", FORMAT, None, None).await;
        let resume = Some(LiveResume {
            session: ann.session,
            participant: ann.participant,
        });
        original
            .handle(LiveCommand::Leave {
                conn: ann.conn,
                client: ann.client,
            })
            .await
            .unwrap();
        original.settle().await.unwrap();
        drop(original);

        let mut loaded = LiveRoom::load(DocId(id(100)), store, EventSink::discard(), timing())
            .await
            .unwrap();
        let returned = join_as(&mut loaded, 2, 1, 11, "editor", FORMAT, resume, None).await;
        assert_eq!(returned.participant, ParticipantNo(2));
        assert_eq!(
            returned.place,
            Place::Returning {
                previous: ann.participant
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn failed_place_settlement_stays_pending_and_unpublished() {
        let (mut room, store) = room().await;
        let mut ann = join_as(&mut room, 1, 1, 11, "editor", FORMAT, None, None).await;
        let mut bob = join_as(&mut room, 2, 2, 12, "editor", FORMAT, None, None).await;
        let _ = drain(&mut ann.rx);
        store.fail_next();
        assert!(
            room.handle(LiveCommand::Leave {
                conn: ann.conn,
                client: ann.client,
            })
            .await
            .is_err()
        );
        assert!(drain(&mut ann.rx).is_empty());
        assert!(drain(&mut bob.rx).is_empty());
        assert!(
            room.people()
                .iter()
                .any(|person| person.participant == ann.participant)
        );

        room.settle().await.unwrap();
        assert!(drain(&mut bob.rx).iter().any(|message| matches!(
            message,
            LiveServerMsg::People { people }
                if people.iter().all(|person| person.participant != ann.participant)
        )));
    }

    #[tokio::test(start_paused = true)]
    async fn failed_session_settlement_keeps_session() {
        let (mut room, store) = room().await;
        let ann = join_as(&mut room, 1, 1, 11, "editor", FORMAT, None, None).await;
        room.handle(LiveCommand::Leave {
            conn: ann.conn,
            client: ann.client,
        })
        .await
        .unwrap();
        store.fail_next();
        assert!(room.settle().await.is_err());
        assert!(room.has_session());
        room.settle().await.unwrap();
        assert!(!room.has_session());
    }
}
