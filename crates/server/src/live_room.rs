//! The opaque-entry live room.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::future::Future;
use std::time::Duration;

use axum::extract::ws::Utf8Bytes;
use collab_core::ids::{ClientId, DocId, Seq, SessionId, UserId};
use protocol::RemovedReason;
use protocol::live::{
    EntryBody, HEARTBEAT_MS, LIVE_PROTOCOL, LiveBuild, LiveEntry, LivePerson, LiveResume,
    LiveServerMsg, LiveSessionId, LocalNo, MAX_PAYLOAD_BYTES, MajorReason, ParticipantNo, Place,
    RefusedReason, TicketPurpose, UnavailableReason, valid_fingerprint, valid_format,
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
use crate::live_store::{
    LiveStore, LiveWrite, LiveWriteError, MajorKind, MajorWrite, MajorWritten,
};
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
    /// Unsaved-log and catch-up limits.
    pub limits: crate::config::LiveLimits,
}

/// The adopted checkpoint cadence (plan G4): 15 seconds without a new change, or 120 seconds
/// after the first unsaved change, whichever comes first.
pub(crate) const CHECKPOINT_QUIET: Duration = Duration::from_secs(15);
pub(crate) const CHECKPOINT_WAIT: Duration = Duration::from_secs(120);
/// A checkpoint ticket is one-use and expires after two minutes.
pub(crate) const TICKET_LIFETIME: Duration = Duration::from_secs(120);
/// A claimed write ticket's upload is given at least this rate: its deadline is the ticket's
/// lifetime plus the upload's most bytes at this rate (64 KiB/s).
pub(crate) const UPLOAD_FLOOR_BYTES_PER_SECOND: u64 = 64 * 1024;
/// The largest editor identity table stored with a checkpoint.
pub(crate) const MAX_SESSION_DATA_BYTES: usize = 8 * 1024 * 1024;
/// Editors report their entered digest at every position that is a multiple of this.
pub(crate) const AGREEMENT_EVERY: u64 = protocol::live::AGREEMENT_EVERY;
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
/// Failed reconcile tries before the outcome is treated as failed.
pub(crate) const RECONCILE_TRIES: u32 = 8;
/// How long a room that refused an over-limit join stays to answer reconnects.
const OVER_LIMIT_LINGER: Duration = Duration::from_secs(60);
/// One major-version mark for a person's returns in this long.
pub(crate) const RETURN_MARK_EVERY: Duration = Duration::from_secs(600);
/// How long a returning batch may take to reach the local named on the join before it is
/// closed and its client resynced.
pub(crate) const RETURN_BATCH_LIMIT: Duration = Duration::from_secs(120);
/// A person takes part in at most one dispute that stops a document's live session in this
/// time; taking part in another within it excludes them from its checkpoints instead.
const DISPUTE_STOP_EVERY: time::Duration = time::Duration::hours(1);
/// How long a person is excluded from a document's checkpoint work after a second dispute.
const DISPUTE_EXCLUSION: time::Duration = time::Duration::hours(24);
/// The most returning clients waiting for their major versions at once.
pub(crate) const MAX_RETURNS: usize = 16;
/// The most expired returning clients waiting to archive their copies at once.
pub(crate) const MAX_ARCHIVES: usize = 16;
/// How long a failed dispute stop waits before it is tried again.
const DISPUTE_RETRY: Duration = Duration::from_secs(5);

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
        /// The last local number of a returning client's saved batch.
        returning_through: Option<LocalNo>,
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
    /// `length` is the most bytes the upload can bring (its declared length, else the largest
    /// allowed): the claimed ticket's deadline gives it time at [`UPLOAD_FLOOR_BYTES_PER_SECOND`].
    CheckpointClaim {
        user: UserId,
        ticket: Uuid,
        at: Seq,
        base: Option<RevisionId>,
        length: u64,
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
    /// A verify ticket's independently written hashes: of the file bytes, and of the editor's
    /// session (identity) data that would go with them.
    CheckpointHash {
        user: UserId,
        ticket: Uuid,
        at: Seq,
        sha256: [u8; 32],
        session_sha256: [u8; 32],
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
    /// For a write ticket that makes a major version, which one.
    major: Option<MajorTicket>,
    expires: Instant,
    claimed: bool,
}

/// The major version a write ticket makes (C22, C23, S4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MajorTicket {
    /// The room's last state, kept below the outside revision that replaced the file.
    Outside,
    /// The current document before a returning person's late changes.
    Before(UserId),
    /// The result right after them.
    After(UserId),
    /// An expired returning person's own copy, kept below the current file.
    Archive(UserId),
    /// A verifier's own bytes in a dispute, kept beside the disputed checkpoint; neither
    /// becomes the base.
    DisputedCopy(UserId),
}

impl MajorTicket {
    fn reason(self) -> MajorReason {
        match self {
            MajorTicket::Outside => MajorReason::OutsideReplacement,
            MajorTicket::Before(_) => MajorReason::ReturningClientBefore,
            MajorTicket::After(_) => MajorReason::ReturningClientAfter,
            MajorTicket::Archive(_) => MajorReason::ReturningClientArchive,
            MajorTicket::DisputedCopy(_) => MajorReason::DisputedCopy,
        }
    }

    /// Whether changes wait while it is out, so its position stays the head.
    fn holds_changes(self) -> bool {
        !matches!(self, MajorTicket::Archive(_))
    }
}

/// The stored name of a major version's reason.
fn major_reason_name(reason: MajorReason) -> &'static str {
    match reason {
        MajorReason::ReturningClientBefore => "returning_client_before",
        MajorReason::ReturningClientAfter => "returning_client_after",
        MajorReason::OutsideReplacement => "outside_replacement",
        MajorReason::ReturningClientArchive => "returning_client_archive",
        MajorReason::DisputedCopy => "disputed_copy",
    }
}

/// Where a returning client's batch is (C22).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReturnStage {
    /// The current document is to be stored as a major version first.
    Before,
    /// `ReturnReady` was sent; the batch's entries are arriving.
    Batch,
    /// The batch ended: the result is to be stored as a major version.
    After,
}

/// A returning client whose saved batch goes between two major versions.
#[derive(Clone, Copy, Debug)]
struct ReturnBarrier {
    participant: ParticipantNo,
    client: ClientId,
    user: UserId,
    through: LocalNo,
    stage: ReturnStage,
    /// Since when the batch may be sent (`ReturnReady`).
    batch_since: Option<Instant>,
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
    /// The major version the checkpoint makes, if any.
    major: Option<MajorTicket>,
    retry_at: Instant,
    /// Failed tries so far.
    tries: u32,
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
    Major(Result<MajorWritten, LiveWriteError>),
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

/// Two editors disagree on the checkpoint `revision` at `at`: the verifier is asked for its own
/// bytes, then both are kept as major versions, the last verified base becomes current and the
/// live session stops.
struct OpenDispute {
    revision: String,
    at: Seq,
    verifier: (ParticipantNo, ClientId, UserId),
    /// The verifier was given its write ticket.
    asked: bool,
    /// When a failed stop may be tried again.
    retry_at: Option<Instant>,
    /// The revision stored again as current once both copies are kept: the last verified
    /// base (for a disputed sole-editor base, the last one a peer verified).
    restore: Option<String>,
    /// Both copies are kept already; only the restore is still to be written.
    kept: bool,
}

/// The file was replaced from outside: the room's last state is written on top of `head` and
/// `outside` is stored again on top of that (S4).
#[derive(Clone, Debug)]
struct Recovery {
    head: String,
    outside: String,
}

/// A major-version write the room runs off its task.
struct OwnedMajor {
    kind: MajorKind,
    reason: &'static str,
    person: UserId,
    current: String,
    restore: Option<String>,
    disputed: Option<String>,
    blob: Option<BlobRef>,
    authors: Vec<Author>,
    at: Seq,
    session: Uuid,
    created_at: OffsetDateTime,
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
    /// The outside revision that replaced the file under the room (S4): until the room's last
    /// state is kept below it, changes wait and only that write is asked for.
    recovery: Option<Recovery>,
    /// A checkpoint dispute being settled; no ordinary ticket is issued meanwhile.
    dispute: Option<OpenDispute>,
    /// A dispute was settled: everyone is told and the live session ends.
    stop_due: bool,
    /// A dispute ended the live session: the room ends too, so the next join loads afresh.
    dispute_ended: bool,
    /// Each person's part in this document's disputes (one stop per hour, then exclusion).
    dispute_marks: HashMap<UserId, records::LiveDisputeMark>,
    /// The revision the room first loaded as its base, other than a sole-editor base: what a
    /// disputed sole-editor base falls back to when no peer ever verified one.
    first_loaded: Option<String>,
    /// When each person's return was last marked with major versions.
    return_marks: HashMap<UserId, Instant>,
    /// Clients whose change was held back: the first held local number. Later numbers are
    /// dropped silently until it comes again, so held changes keep their order.
    held_from: HashMap<ClientId, u64>,
    /// An over-limit room loaded its whole log for a capable editor's checkpoint.
    over_limit_loaded: bool,
    /// Returning clients' barriers, the first in progress.
    returns: VecDeque<ReturnBarrier>,
    /// Expired returning clients still to be asked for their own copy (C22, after 30 days).
    archives: Vec<(ParticipantNo, ClientId, UserId)>,
    /// Whether everyone was told that changes wait for a major version.
    hold_announced: bool,
    /// The base accepted from a sole editor and not yet verified by a second one: its
    /// revision, position, hash and writer's client.
    sole_base: Option<(String, Seq, [u8; 32], Option<ClientId>)>,
    /// The build of the sole base's writer, when this room saw it; only a place on the same
    /// build verifies it.
    sole_base_build: Option<LiveBuild>,
    /// The SHA-256 of each unverified checkpoint's session (identity) data, and of a sole
    /// editor's base: a verifier's hash of its own session data must match it too.
    session_hashes: HashMap<String, [u8; 32]>,
    /// Past a warning threshold, the one urgent checkpoint was asked for.
    urgent_asked: bool,
    /// When an over-limit join was last refused; the event is recorded once per room.
    over_limit_refused: Option<Instant>,
    pending_reconcile: Option<PendingReconcile>,
    /// A checkpoint write is running off the room task; no other write may start.
    writing: bool,
    /// The running write makes a major version at the head, so changes wait for it.
    writing_hold: bool,
    /// When a failed major mark of the stored head may be tried again.
    mark_retry_at: Option<Instant>,
    written_tx: mpsc::UnboundedSender<LiveCommand>,
    written_rx: mpsc::UnboundedReceiver<LiveCommand>,
    /// Who wrote each unverified checkpoint this room stored, and with which build.
    checkpoint_writers: HashMap<String, CheckpointWriter>,
    /// Verifiers that let a verify or forensic ticket lapse or sent a hash that disagreed;
    /// not asked to verify again until they join again.
    lapsed: BTreeSet<ParticipantNo>,
    /// Since when a due write ticket has been held back by outstanding verify tickets.
    verify_hold: Option<Instant>,
    /// Verifiers whose hash disagreed with an unverified checkpoint, or who let a verify ticket
    /// for it lapse; never asked to verify it again, so a silent verifier cannot hold back
    /// checkpoint work.
    not_asked: HashMap<String, BTreeSet<ParticipantNo>>,
    agreement: BTreeMap<Seq, AgreementBucket>,
    /// Changes stored in the open session, for its end event.
    session_changes: u64,
    names: HashMap<UserId, String>,
    /// Entries kept after the verified base (the catch-up a joiner is sent), cancel records
    /// included.
    unsaved_entries: u64,
    /// Payload bytes kept after the verified base.
    unsaved_bytes: u64,
    /// New changes are refused until a verified checkpoint brings the log under its limits.
    paused: bool,
    /// The log is past a warning threshold: a checkpoint is asked for at once.
    warned: bool,
    /// The durable log is beyond what the limits allow (a limit was lowered): new joins are
    /// refused rather than sent an unbounded catch-up.
    over_limit: bool,
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
        let (entries, bytes) = store.live_unsaved_counts(doc).await?;
        if beyond_limits(timing.limits, entries, bytes) {
            // Beyond what may be sent to a joiner: refuse joins without loading the log.
            let mut room = Self::from_loaded(
                doc,
                store,
                events,
                timing,
                LiveLoaded {
                    head: Seq(0),
                    last_participant: 0,
                    format: None,
                    verified_through: Seq(0),
                    verified_checkpoint: None,
                    unverified_checkpoints: Vec::new(),
                    entries: Vec::new(),
                    unsaved_entries: entries,
                    unsaved_bytes: bytes,
                    open_session: None,
                    participants: Vec::new(),
                    clients: Vec::new(),
                    retained_session_data: Vec::new(),
                },
                None,
            )?;
            room.over_limit = true;
            return Ok(room);
        }
        Self::load_whole(doc, store, events, timing).await
    }

    /// Loads the whole log, checking the current revision against the newest checkpoint: the
    /// room's own revisions, outside replacements and half-done recoveries (S4).
    async fn load_whole(
        doc: DocId,
        store: S,
        events: EventSink,
        timing: LiveTiming,
    ) -> Result<Self, RoomFailed> {
        let mut loaded = store.load_live(doc).await?;
        let current = store.current_revision(doc).await?;
        let marks = store.live_dispute_marks(doc).await?;
        let mut recovery = None;
        let mut restored = None;
        let newest = loaded
            .unverified_checkpoints
            .iter()
            .max_by_key(|checkpoint| checkpoint.through)
            .or(loaded.verified_checkpoint.as_ref())
            .map(|checkpoint| {
                (
                    checkpoint.revision.clone(),
                    checkpoint.through,
                    checkpoint.session,
                )
            });
        if let Some((live, through, session)) = newest
            && let Some(now) = current.clone().filter(|now| *now != live)
        {
            // The document's current revision is newer than the room's newest checkpoint.
            match store.describe_live_revision(doc, &now).await {
                Ok(Some(info)) if crate::live_store::written_by_room(&info) => {
                    if info.fields.get("major_reason").and_then(Value::as_str)
                        == Some("outside_replacement")
                    {
                        // An outside recovery stopped after keeping the room's state: the
                        // outside file it names is stored again on top first.
                        let outside = info
                            .fields
                            .get("replaced_by")
                            .and_then(Value::as_str)
                            .map(str::to_owned);
                        recovery = outside.map(|outside| Recovery {
                            head: now.clone(),
                            outside,
                        });
                    }
                    // The room's own revision (a mark, a restore, or a checkpoint whose record
                    // was not stored): it is the base the next checkpoint goes on.
                    restored = Some(now);
                }
                Ok(Some(info)) if loaded.head <= through => {
                    // Replaced from outside with nothing unsaved: the outside file is the base.
                    let at = OffsetDateTime::now_utc();
                    store
                        .adopt_live_base(
                            doc,
                            &NewLiveCheckpoint {
                                revision: &now,
                                through: loaded.head,
                                sha256: info.blob.map_or([0; 32], |blob| blob.sha256),
                                session,
                                session_data: &[],
                                major_reason: Some("outside_current"),
                                major_person: None,
                                writer: None,
                                created_at: at,
                            },
                            at,
                        )
                        .await?;
                    loaded = store.load_live(doc).await?;
                }
                // Replaced from outside under unsaved changes: they are written once more and
                // kept below it first (S4).
                Ok(Some(_)) => {
                    recovery = Some(Recovery {
                        head: now.clone(),
                        outside: now,
                    });
                }
                // Unknown: the next checkpoint write finds out.
                Ok(None) | Err(_) => {}
            }
        }
        let sole = loaded
            .verified_checkpoint
            .as_ref()
            .filter(|checkpoint| checkpoint.sole_editor)
            .map(|checkpoint| checkpoint.revision.clone());
        let first_loaded = match &loaded.verified_checkpoint {
            Some(checkpoint) if !checkpoint.sole_editor => Some(checkpoint.revision.clone()),
            _ => current
                .clone()
                .filter(|current| Some(current) != sole.as_ref()),
        };
        let mut room = Self::from_loaded(doc, store, events, timing, loaded, current)?;
        if restored.is_some() {
            room.checkpoint_base = restored;
        }
        room.recovery = recovery;
        room.first_loaded = first_loaded;
        room.dispute_marks = marks.into_iter().map(|mark| (mark.user, mark)).collect();
        Ok(room)
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
        let mut session_hashes = HashMap::new();
        if let Some(checkpoint) = loaded
            .verified_checkpoint
            .as_ref()
            .filter(|checkpoint| checkpoint.sole_editor)
        {
            session_hashes.insert(
                checkpoint.revision.clone(),
                session_sha256(&checkpoint.session_data),
            );
        }
        let unverified_checkpoints = loaded
            .unverified_checkpoints
            .into_iter()
            .map(|mut checkpoint| {
                session_hashes.insert(
                    checkpoint.revision.clone(),
                    session_sha256(&checkpoint.session_data),
                );
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
        let limits = timing.limits;
        let at_hard = loaded.unsaved_entries >= limits.hard_entries
            || loaded.unsaved_bytes >= limits.hard_bytes;
        let at_warning = loaded.unsaved_entries >= limits.warn_entries
            || loaded.unsaved_bytes >= limits.warn_bytes;
        let over_limit = beyond_limits(limits, loaded.unsaved_entries, loaded.unsaved_bytes);
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
            recovery: None,
            dispute: None,
            stop_due: false,
            dispute_ended: false,
            dispute_marks: HashMap::new(),
            first_loaded: None,
            return_marks: HashMap::new(),
            held_from: HashMap::new(),
            over_limit_loaded: false,
            returns: VecDeque::new(),
            archives: Vec::new(),
            hold_announced: false,
            sole_base: loaded
                .verified_checkpoint
                .as_ref()
                .filter(|checkpoint| checkpoint.sole_editor)
                .map(|checkpoint| {
                    (
                        checkpoint.revision.clone(),
                        checkpoint.through,
                        checkpoint.sha256,
                        checkpoint.writer.map(|writer| writer.client),
                    )
                }),
            sole_base_build: None,
            session_hashes,
            urgent_asked: false,
            over_limit_refused: None,
            pending_reconcile: None,
            writing: false,
            writing_hold: false,
            mark_retry_at: None,
            written_tx,
            written_rx,
            checkpoint_writers,
            lapsed: BTreeSet::new(),
            verify_hold: None,
            not_asked: HashMap::new(),
            agreement: BTreeMap::new(),
            session_changes,
            names: HashMap::new(),
            unsaved_entries: loaded.unsaved_entries,
            unsaved_bytes: loaded.unsaved_bytes,
            paused: at_hard,
            warned: at_warning,
            over_limit,
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
        let handled = match handled {
            Ok(()) => self.accept_sole_editor().await,
            failed => failed,
        };
        if handled.is_ok() {
            self.advance_returns().await;
            self.issue_due_tickets();
        }
        let handled = match handled {
            Ok(()) => self.stop_session().await,
            failed => failed,
        };
        self.sync_hold();
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
                returning_through,
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
                    returning_through,
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
                length,
                reply,
            } => {
                // From now on the deadline is the upload's: the lifetime again, plus its most
                // bytes at the floor rate.
                let upload_time = self
                    .timing
                    .ticket_lifetime
                    .saturating_add(Duration::from_secs(
                        length.div_ceil(UPLOAD_FLOOR_BYTES_PER_SECOND),
                    ));
                let claimed = self
                    .usable_ticket(user, ticket, at, base.as_ref(), TicketPurpose::Write)
                    .and_then(|index| self.tickets.get_mut(index))
                    .filter(|held| !held.claimed)
                    .map(|held| {
                        held.claimed = true;
                        held.expires = Instant::now() + upload_time;
                    })
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
                session_sha256,
                reply,
            } => match self
                .checkpoint_hash(user, ticket, at, sha256, session_sha256)
                .await
            {
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
                && match purpose {
                    TicketPurpose::Write => {
                        self.place_can_checkpoint(held.participant, held.client)
                    }
                    TicketPurpose::Verify => self.place_can_verify(held.participant, held.client),
                }
        })
    }

    /// Whether `participant` holds its place for `client`, may edit, can checkpoint and is
    /// connected, exclusions aside.
    fn place_capable(&self, participant: ParticipantNo, client: ClientId) -> bool {
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

    /// The person and build of `participant`'s place.
    fn place_of(&self, participant: ParticipantNo) -> Option<(UserId, LiveBuild)> {
        self.session
            .as_ref()
            .and_then(|session| session.places.get(&participant))
            .map(|place| (place.user, place.build))
    }

    /// Whether `participant` may write a checkpoint: capable, and not excluded after a repeat
    /// dispute unless no capable editor who is not excluded is present (a room is never left
    /// without a writer).
    fn place_can_checkpoint(&self, participant: ParticipantNo, client: ClientId) -> bool {
        self.place_capable(participant, client)
            && self.place_of(participant).is_some_and(|(user, _)| {
                !self.excluded(user) || !self.unexcluded_writer_present(user)
            })
    }

    /// Whether `participant` may verify a checkpoint: capable and not excluded.
    fn place_can_verify(&self, participant: ParticipantNo, client: ClientId) -> bool {
        self.place_capable(participant, client)
            && self
                .place_of(participant)
                .is_some_and(|(user, _)| !self.excluded(user))
    }

    /// Whether a capable editor other than `except`, and not excluded, is present.
    fn unexcluded_writer_present(&self, except: UserId) -> bool {
        self.session.as_ref().is_some_and(|session| {
            session.places.iter().any(|(participant, place)| {
                place.user != except
                    && !self.excluded(place.user)
                    && self.place_capable(*participant, place.client)
            })
        })
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
                    if held.purpose == TicketPurpose::Verify {
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
        if self.recovery.is_some()
            || self
                .returns
                .front()
                .is_some_and(|barrier| barrier.stage != ReturnStage::Batch)
            || self.pending_reconcile.is_some()
            || self.writing
            || self.dispute.is_some()
            || self.head <= self.saved_through
            || self
                .tickets
                .iter()
                .any(|held| held.purpose == TicketPurpose::Write)
        {
            return None;
        }
        if self.warned && !self.urgent_asked {
            // Past a warning threshold: ask at once, once; then the usual cadence.
            return Some(Instant::now());
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
    /// writer, (for a verify) the same build kind as `writer_build`, then by number.
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
                        writer_build.is_some_and(|build| build != place.build),
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
        self.issue_major_ticket();
        if self.recovery.is_some() {
            return;
        }
        self.issue_dispute_ticket();
        if self.dispute.is_some() {
            return;
        }
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
            if self.warned {
                self.urgent_asked = true;
            }
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

    /// In a dispute, asks the verifier once for its own bytes; once that ticket is used up or
    /// cannot be given, settles the dispute with what there is.
    fn issue_dispute_ticket(&mut self) {
        if self.writing || self.recovery.is_some() {
            return;
        }
        let Some(dispute) = &self.dispute else {
            return;
        };
        if self.tickets.iter().any(|held| {
            held.major
                .is_some_and(|major| matches!(major, MajorTicket::DisputedCopy(_)))
        }) || dispute.retry_at.is_some_and(|at| at > Instant::now())
        {
            return;
        }
        let (participant, client, user) = dispute.verifier;
        let at = dispute.at;
        if !dispute.asked && self.head == at && self.place_can_checkpoint(participant, client) {
            if let Some(dispute) = &mut self.dispute {
                dispute.asked = true;
            }
            let base = self.checkpoint_base.clone();
            self.issue_ticket(
                TicketPurpose::Write,
                (participant, client, user),
                at,
                base,
                None,
                Some(MajorTicket::DisputedCopy(user)),
            );
            return;
        }
        // The verifier's bytes are not coming: keep the writer's copy and settle.
        if let Some(dispute) = &mut self.dispute {
            dispute.asked = true;
        }
        self.settle_dispute(None, user, Vec::new());
    }

    /// Keeps both sides of the dispute as major versions (the verifier's `blob` when it sent
    /// one), stores the last verified base again as current, and then ends the session.
    fn settle_dispute(&mut self, blob: Option<BlobRef>, user: UserId, authors: Vec<Author>) {
        let Some(dispute) = &self.dispute else {
            return;
        };
        let Some(current) = self.checkpoint_base.clone() else {
            // Nothing in Git to keep: end the session as is.
            self.dispute = None;
            self.stop_due = true;
            return;
        };
        let (participant, client, _) = dispute.verifier;
        let kept = dispute.kept;
        let owned = OwnedMajor {
            kind: MajorKind::Dispute,
            reason: "dispute_base",
            person: user,
            current,
            restore: dispute.restore.clone(),
            // A retry after a part-way write only stores the restore again.
            disputed: (!kept).then(|| dispute.revision.clone()),
            blob: if kept { None } else { blob },
            authors,
            at: dispute.at,
            session: self
                .session
                .as_ref()
                .map_or_else(Uuid::nil, |session| session.id.0),
            created_at: OffsetDateTime::now_utc(),
        };
        let ticket = self.synthetic_ticket(
            (participant, client, user),
            dispute.at,
            MajorTicket::DisputedCopy(user),
        );
        let (reply, _) = oneshot::channel();
        self.spawn_major(ticket, owned, reply);
    }

    /// A ticket standing for a write the room starts itself.
    fn synthetic_ticket(
        &self,
        (participant, client, user): (ParticipantNo, ClientId, UserId),
        at: Seq,
        major: MajorTicket,
    ) -> Ticket {
        Ticket {
            id: Uuid::nil(),
            purpose: TicketPurpose::Write,
            participant,
            client,
            user,
            at,
            base: self.checkpoint_base.clone(),
            verifies: None,
            major: Some(major),
            expires: Instant::now(),
            claimed: true,
        }
    }

    /// Runs the major-version write `owned` off the room task; its result comes back as
    /// [`LiveCommand::CheckpointWritten`] for `ticket`.
    fn spawn_major(
        &mut self,
        ticket: Ticket,
        owned: OwnedMajor,
        reply: oneshot::Sender<LiveCheckpointReply>,
    ) {
        self.writing = true;
        self.writing_hold = ticket.major.is_some_and(MajorTicket::holds_changes);
        let store = self.store.clone();
        let doc = self.doc;
        let written = self.written_tx.clone();
        let user = ticket.user;
        let at = owned.at;
        let session = owned.session;
        let created_at = owned.created_at;
        tokio::spawn(async move {
            let job = tokio::spawn(async move {
                let write = MajorWrite {
                    kind: owned.kind,
                    reason: owned.reason,
                    person: owned.person,
                    current: &owned.current,
                    restore: owned.restore.as_deref(),
                    disputed: owned.disputed.as_deref(),
                    blob: owned.blob,
                    authors: &owned.authors,
                    at: owned.at,
                    session: owned.session,
                    created_at: owned.created_at,
                };
                store.write_live_major(doc, &write).await
            });
            let result = match job.await {
                Ok(result) => result,
                Err(error) => Err(LiveWriteError::Failed(format!(
                    "major write task failed: {error}"
                ))),
            };
            let done = WriteDone {
                result: WriteResult::Major(result),
                ticket,
                user,
                at,
                sha256: [0; 32],
                session,
                session_data: Vec::new(),
                created_at,
                reply,
            };
            let _ = written.send(LiveCommand::CheckpointWritten(Box::new(done)));
        });
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
        let Some((revision, at)) = self.verify_target() else {
            return;
        };
        let Some(verifier) = self.verifier_for(&revision) else {
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

    /// The checkpoint a verifier is asked about: the newest unverified one, else a base a
    /// sole editor wrote.
    fn verify_target(&self) -> Option<(String, Seq)> {
        self.unverified_checkpoints
            .iter()
            .filter(|checkpoint| checkpoint.through > self.verified_through)
            .max_by_key(|checkpoint| checkpoint.through)
            .map(|checkpoint| (checkpoint.revision.clone(), checkpoint.through))
            .or_else(|| {
                self.sole_base
                    .as_ref()
                    .map(|(revision, at, _, _)| (revision.clone(), *at))
            })
    }

    /// The editor to verify `revision`: never its writer's person (any of their tabs), a person
    /// excluded after a repeat dispute, a verifier that lapsed or one that disagreed with it;
    /// and only a place on the writer's build, since the bytes carry computed values. With
    /// none, the checkpoint stays unverified and the sole-editor rule applies.
    fn verifier_for(&self, revision: &str) -> Option<(ParticipantNo, ClientId, UserId)> {
        let writer = self.checkpoint_writers.get(revision).copied();
        let sole = self
            .sole_base
            .as_ref()
            .filter(|(base, _, _, _)| base == revision);
        let writer_client = writer
            .map(|writer| writer.client)
            .or_else(|| sole.and_then(|(_, _, _, client)| *client));
        let writer_user = writer_client.and_then(|client| self.user_of(client));
        let writer_build = match writer {
            Some(writer) => writer.build,
            None => sole.and(self.sole_base_build),
        };
        let not_asked = self.not_asked.get(revision);
        self.ticket_candidates(writer.map(|writer| writer.participant), writer_build)
            .into_iter()
            .find(|(participant, client, user)| {
                writer_client.is_none_or(|writer| writer != *client)
                    && writer_user.is_none_or(|writer| writer != *user)
                    && self.place_can_verify(*participant, *client)
                    && writer_build.is_none_or(|build| {
                        self.place_of(*participant)
                            .is_some_and(|(_, place)| place == build)
                    })
                    && !self.lapsed.contains(participant)
                    && not_asked.is_none_or(|set| !set.contains(participant))
            })
    }

    /// At a hard limit with no second capable editor to verify the newest checkpoint, it is
    /// accepted as a sole-editor base so changes can resume; a second editor verifies it later.
    async fn accept_sole_editor(&mut self) -> Result<(), RoomFailed> {
        if !self.paused
            || self
                .tickets
                .iter()
                .any(|held| held.purpose == TicketPurpose::Verify)
        {
            return Ok(());
        }
        let Some(target) = self
            .unverified_checkpoints
            .iter()
            .filter(|checkpoint| checkpoint.through > self.verified_through)
            .max_by_key(|checkpoint| checkpoint.through)
            .cloned()
        else {
            return Ok(());
        };
        let target_writer = self.checkpoint_writers.get(&target.revision).copied();
        if self.dispute.is_some()
            || self.other_capable_present(
                target_writer.map(|writer| writer.client),
                target_writer.and_then(|writer| writer.build),
            )
        {
            return Ok(());
        }
        self.store
            .accept_live_sole_editor(
                self.doc,
                &target.revision,
                target.through,
                OffsetDateTime::now_utc(),
            )
            .await?;
        let writer = self.checkpoint_writers.get(&target.revision).copied();
        let user = writer
            .and_then(|writer| self.clients.get(&writer.client))
            .map(|(user, _)| *user);
        self.events.emit(NewEvent {
            kind: "live.checkpoint_sole_editor",
            actor: user,
            subject: None,
            document: Some(self.doc),
            node: Some(self.doc.into()),
            detail: serde_json::json!({ "revision": target.revision, "through": target.through.0 }),
        });
        self.sole_base = Some((
            target.revision.clone(),
            target.through,
            target.sha256,
            writer.map(|writer| writer.client),
        ));
        self.sole_base_build = writer.and_then(|writer| writer.build);
        self.settle_base(target.revision, target.through);
        Ok(())
    }

    /// Whether a capable editor who could verify the writer's checkpoint holds a place: another
    /// person (not the writer's other tab), not excluded, on the writer's build when it is
    /// known, connected or within its reconnect grace, holding a ticket or lapsed alike. Only
    /// with none is a sole editor's checkpoint accepted.
    fn other_capable_present(&self, writer: Option<ClientId>, build: Option<LiveBuild>) -> bool {
        let writer_user = writer.and_then(|client| self.user_of(client));
        self.session.as_ref().is_some_and(|session| {
            session.places.values().any(|place| {
                // A place loaded after a restart counts until its editor joins again and says
                // what it can do.
                place.held
                    && (place.sign_in.is_none()
                        || (place.can_edit
                            && place.can_checkpoint
                            && build.is_none_or(|build| build == place.build)))
                    && !self.excluded(place.user)
                    && writer.is_none_or(|writer| writer != place.client)
                    && writer_user.is_none_or(|writer| writer != place.user)
            })
        })
    }

    /// Sets aside the checkpoint `revision` a verifier disputed: it stays in the history but is
    /// never a base, and its position is free for another writer, the verifier first.
    async fn dispute(&mut self, revision: &str) -> Result<(), RoomFailed> {
        self.store
            .dispute_live_checkpoint(self.doc, revision, OffsetDateTime::now_utc())
            .await?;
        self.checkpoint_writers.remove(revision);
        self.session_hashes.remove(revision);
        self.unverified_checkpoints
            .retain(|checkpoint| checkpoint.revision != revision);
        self.not_asked.remove(revision);
        self.cancel_tickets(|held| held.verifies.as_deref() == Some(revision), false);
        self.saved_through = self
            .unverified_checkpoints
            .iter()
            .map(|checkpoint| checkpoint.through)
            .max()
            .unwrap_or(self.verified_through)
            .max(self.verified_through);
        if self.head > self.saved_through {
            self.first_unsaved.get_or_insert_with(Instant::now);
        }
        self.urgent_asked = false;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn issue_ticket(
        &mut self,
        purpose: TicketPurpose,
        (participant, client, user): (ParticipantNo, ClientId, UserId),
        at: Seq,
        base: Option<String>,
        verifies: Option<String>,
        major: Option<MajorTicket>,
    ) {
        let id = Uuid::new_v4();
        let frame = encode(&LiveServerMsg::CheckpointTicket {
            ticket: id,
            purpose,
            at,
            base: base.clone(),
            major: major.map(MajorTicket::reason),
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
            major,
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
        let uploader = vec![Author {
            id: user.0.to_string(),
            name: credited_name(self.names.get(&user).map_or("", String::as_str)),
        }];
        let now = OffsetDateTime::now_utc();
        // PostgreSQL keeps microseconds; a reconcile compares this exact time.
        let created_at = now
            .replace_nanosecond(now.nanosecond() / 1_000 * 1_000)
            .unwrap_or(now);
        let owned = |kind, reason, person, current: Option<String>, restore, disputed, authors| {
            current.map(|current| OwnedMajor {
                kind,
                reason,
                person,
                current,
                restore,
                disputed,
                blob: Some(blob),
                authors,
                at,
                session: session.0,
                created_at,
            })
        };
        let major_write = match held.major {
            Some(MajorTicket::Outside) => Some(owned(
                MajorKind::Outside,
                "outside_current",
                user,
                self.recovery.as_ref().map(|recovery| recovery.head.clone()),
                self.recovery
                    .as_ref()
                    .map(|recovery| recovery.outside.clone()),
                None,
                self.checkpoint_authors(at, user),
            )),
            Some(MajorTicket::Archive(person)) => Some(owned(
                MajorKind::Archive,
                "returning_client_current",
                person,
                held.base.clone(),
                None,
                None,
                uploader,
            )),
            Some(MajorTicket::DisputedCopy(_)) => {
                // The verifier's own bytes settle the dispute.
                let Some(dispute) = &self.dispute else {
                    let _ = reply.send(LiveCheckpointReply::Refused);
                    return;
                };
                let disputed = Some(dispute.revision.clone());
                let at = dispute.at;
                let restore = dispute.restore.clone();
                Some(
                    owned(
                        MajorKind::Dispute,
                        "dispute_base",
                        user,
                        self.checkpoint_base.clone(),
                        restore,
                        disputed,
                        uploader,
                    )
                    .map(|mut owned| {
                        owned.at = at;
                        owned
                    }),
                )
            }
            _ => None,
        };
        if let Some(major_write) = major_write {
            match major_write {
                Some(owned) => self.spawn_major(held, owned, reply),
                None => {
                    let _ = reply.send(LiveCheckpointReply::Failed);
                }
            }
            return;
        }
        let authors = self.checkpoint_authors(at, user);
        self.writing = true;
        self.writing_hold = held.major.is_some_and(MajorTicket::holds_changes);
        let store = self.store.clone();
        let doc = self.doc;
        let written = self.written_tx.clone();
        let write_base = held.base.clone();
        let major = held.major;
        let writer = Some(LiveCheckpointWriter {
            participant: held.participant.0,
            client: held.client,
        });
        tokio::spawn(async move {
            let job = tokio::spawn(async move {
                let major_point = match major {
                    Some(MajorTicket::Before(person)) => Some((
                        major_reason_name(MajorReason::ReturningClientBefore),
                        person,
                    )),
                    Some(MajorTicket::After(person)) => {
                        Some((major_reason_name(MajorReason::ReturningClientAfter), person))
                    }
                    _ => None,
                };
                let write = LiveWrite {
                    base: write_base.as_deref(),
                    blob,
                    authors: &authors,
                    at,
                    session: session.0,
                    session_data: &session_data,
                    created_at,
                    writer,
                    major: major_point,
                };
                let result =
                    WriteResult::Checkpoint(store.write_live_checkpoint(doc, &write).await);
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
        self.writing_hold = false;
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
            WriteResult::Checkpoint(Ok(revision)) => {
                self.checkpoint_stored(
                    revision.clone(),
                    at,
                    (sha256, session_sha256(&session_data)),
                    session,
                    user,
                    (held.participant, held.client),
                    held.major,
                );
                LiveCheckpointReply::Saved { revision }
            }
            WriteResult::Major(Ok(written)) => self.major_written(&held, user, at, written),
            WriteResult::Major(Err(LiveWriteError::Partial { head, error })) => {
                eprintln!(
                    "sharecraft-server: a major version of document {} stopped part way: {error}",
                    self.doc.0
                );
                match (held.major, &mut self.recovery) {
                    // The outside file is still to be stored again, on top of what was kept.
                    (Some(MajorTicket::Outside), Some(recovery)) => recovery.head = head,
                    _ => self.checkpoint_base = Some(head),
                }
                if matches!(held.major, Some(MajorTicket::DisputedCopy(_))) {
                    // Both copies are kept; the restore is tried again before the session
                    // stops, so a disputed copy never stays the current file.
                    match &mut self.dispute {
                        Some(dispute) if dispute.restore.is_some() => {
                            dispute.kept = true;
                            dispute.asked = true;
                            dispute.retry_at = Some(Instant::now() + DISPUTE_RETRY);
                        }
                        _ => {
                            self.dispute = None;
                            self.stop_due = true;
                        }
                    }
                }
                LiveCheckpointReply::Failed
            }
            WriteResult::Checkpoint(Err(LiveWriteError::Stale { current }))
            | WriteResult::Major(Err(LiveWriteError::Stale { current })) => {
                if let Some(outside) = current.clone() {
                    self.enter_recovery(outside);
                }
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
            | WriteResult::Major(Err(LiveWriteError::Failed(error))) => {
                eprintln!(
                    "sharecraft-server: live checkpoint of document {} failed: {error}",
                    self.doc.0
                );
                match held.major {
                    Some(MajorTicket::Before(_) | MajorTicket::After(_)) if held.id.is_nil() => {
                        self.mark_retry_at = Some(Instant::now() + RECONCILE_RETRY);
                    }
                    Some(MajorTicket::DisputedCopy(_)) => {
                        if let Some(dispute) = &mut self.dispute {
                            dispute.asked = true;
                            dispute.retry_at = Some(Instant::now() + DISPUTE_RETRY);
                        }
                    }
                    _ => {}
                }
                LiveCheckpointReply::Failed
            }
            WriteResult::Major(Err(LiveWriteError::Unrecorded {
                revision, error, ..
            })) => {
                // A major write records no checkpoint, so this is not expected; Git holds it
                // and the next start reconciles the index.
                eprintln!(
                    "sharecraft-server: major revision {revision} of document {} is not \
                     recorded: {error}",
                    self.doc.0
                );
                LiveCheckpointReply::Failed
            }
            WriteResult::Checkpoint(Err(LiveWriteError::Partial { head, error })) => {
                // Not produced by a checkpoint write; treated as its own revision on top.
                eprintln!(
                    "sharecraft-server: live checkpoint of document {} stopped part way: {error}",
                    self.doc.0
                );
                self.checkpoint_base = Some(head);
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
                    major: held.major,
                    tries: 0,
                    retry_at: Instant::now() + RECONCILE_RETRY,
                    delay: RECONCILE_RETRY.saturating_mul(2).min(RECONCILE_RETRY_MAX),
                });
                LiveCheckpointReply::Failed
            }
        };
        let _ = reply.send(answer);
    }

    /// Applies a finished major-version write.
    fn major_written(
        &mut self,
        held: &Ticket,
        user: UserId,
        at: Seq,
        written: MajorWritten,
    ) -> LiveCheckpointReply {
        let kept = written.kept.clone();
        match held.major {
            Some(MajorTicket::Outside) => {
                if let Some(kept) = &kept {
                    self.major_version(kept, "outside_replacement", user, at);
                }
                self.major_version(&written.current, "outside_current", user, at);
                self.rebase_on(written.current.clone(), at);
            }
            Some(MajorTicket::Archive(person)) => {
                if let Some(kept) = &kept {
                    self.major_version(kept, "returning_client_archive", person, at);
                }
                self.major_version(&written.current, "returning_client_current", person, at);
                self.checkpoint_base = Some(written.current.clone());
            }
            Some(MajorTicket::DisputedCopy(person)) => {
                let mut copies = Vec::new();
                for copy in kept.iter().chain(written.copy.iter()) {
                    self.major_version(copy, "disputed_copy", person, at);
                    copies.push(copy.clone());
                }
                self.checkpoint_base = Some(written.current.clone());
                // Owners are told both copies are in the history (G5 Task 13). The current file
                // is the last verified one, so the saved position does not move on.
                self.notify_saved(
                    written.current.clone(),
                    self.verified_through,
                    true,
                    true,
                    true,
                );
                let revision = self
                    .dispute
                    .take()
                    .map(|dispute| dispute.revision)
                    .unwrap_or_default();
                self.events.emit(NewEvent {
                    kind: "live.checkpoint_dispute_stopped",
                    actor: Some(person),
                    subject: None,
                    document: Some(self.doc),
                    node: Some(self.doc.into()),
                    detail: serde_json::json!({
                        "revision": revision,
                        "through": at.0,
                        "copies": copies,
                        "current": written.current,
                    }),
                });
                self.stop_due = true;
            }
            Some(MajorTicket::Before(person) | MajorTicket::After(person)) => {
                let reason = if matches!(held.major, Some(MajorTicket::Before(_))) {
                    MajorReason::ReturningClientBefore
                } else {
                    MajorReason::ReturningClientAfter
                };
                self.mark_retry_at = None;
                self.major_version(&written.current, major_reason_name(reason), person, at);
                self.checkpoint_base = Some(written.current.clone());
                self.return_point_stored();
                self.notify_saved(written.current.clone(), at, false, true, false);
            }
            None => {}
        }
        LiveCheckpointReply::Saved {
            revision: kept.unwrap_or(written.current),
        }
    }

    /// The effects of a stored, unverified checkpoint: the next checkpoint builds on it, its
    /// position is saved, and (through [`Self::issue_due_tickets`]) a different editor is asked
    /// to verify it.
    #[allow(clippy::too_many_arguments)]
    fn checkpoint_stored(
        &mut self,
        revision: String,
        at: Seq,
        (sha256, session_sha): ([u8; 32], [u8; 32]),
        session: Uuid,
        user: UserId,
        (participant, client): (ParticipantNo, ClientId),
        major: Option<MajorTicket>,
    ) {
        self.saved_through = self.saved_through.max(at);
        self.checkpoint_base = Some(revision.clone());
        self.session_hashes.insert(revision.clone(), session_sha);
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
                major_reason: major_point(major).map(|(reason, _)| reason.to_owned()),
                major_person: major_point(major).map(|(_, person)| person),
                writer: Some(LiveCheckpointWriter {
                    participant: participant.0,
                    client,
                }),
                sole_editor: false,
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
        let revision_name = revision.clone();
        self.checkpoint_writers.insert(
            revision.clone(),
            CheckpointWriter {
                participant,
                client,
                build,
            },
        );
        // The store keeps only the newest unverified checkpoints, major versions aside; so
        // does the room.
        self.unverified_checkpoints
            .sort_by_key(|checkpoint| checkpoint.through);
        let ordinary = self
            .unverified_checkpoints
            .iter()
            .filter(|checkpoint| checkpoint.major_reason.is_none())
            .count();
        let mut excess = ordinary.saturating_sub(records::MAX_UNVERIFIED_CHECKPOINTS);
        let mut dropped = Vec::new();
        self.unverified_checkpoints.retain(|checkpoint| {
            if excess > 0 && checkpoint.major_reason.is_none() {
                excess -= 1;
                dropped.push(checkpoint.revision.clone());
                false
            } else {
                true
            }
        });
        for revision in dropped {
            self.checkpoint_writers.remove(&revision);
            self.session_hashes.remove(&revision);
            self.not_asked.remove(&revision);
            self.cancel_tickets(
                |held| held.verifies.as_deref() == Some(revision.as_str()),
                false,
            );
        }
        let is_major = major_point(major).is_some();
        if let Some((reason, person)) = major_point(major) {
            self.major_version(&revision_name, reason, person, at);
            self.return_point_stored();
        }
        self.notify_saved(revision_name, at, false, is_major, false);
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
        session_sha256: [u8; 32],
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
                writer.participant == held.participant
                    || writer.client == held.client
                    || self.user_of(writer.client) == Some(held.user)
            })
        {
            return Ok(LiveCheckpointReply::Refused);
        }
        // The identity data must match as well as the file: either mismatch is a dispute.
        let session_matches = self.session_hashes.get(&revision) == Some(&session_sha256);
        let Some(stored) = self
            .unverified_checkpoints
            .iter()
            .find(|checkpoint| checkpoint.revision == revision && checkpoint.through == at)
            .map(|checkpoint| checkpoint.sha256)
        else {
            if let Some((base, base_at, base_sha, writer)) = self.sole_base.clone()
                && base == revision
                && base_at == at
            {
                // A second editor re-derived a sole editor's base.
                self.sole_base = None;
                if base_sha == sha256 && session_matches {
                    self.store
                        .confirm_live_sole_editor(self.doc, &revision)
                        .await?;
                    self.events.emit(NewEvent {
                        kind: "live.checkpoint_verified",
                        actor: Some(user),
                        subject: None,
                        document: Some(self.doc),
                        node: Some(self.doc.into()),
                        detail: serde_json::json!({ "revision": revision, "through": at.0 }),
                    });
                } else {
                    self.events.emit(NewEvent {
                        kind: "live.checkpoint_mismatch",
                        actor: Some(user),
                        subject: None,
                        document: Some(self.doc),
                        node: Some(self.doc.into()),
                        detail: serde_json::json!({
                            "revision": revision,
                            "through": at.0,
                            "sole_editor": true,
                        }),
                    });
                    // Set aside like any disputed checkpoint: the base goes back to the last
                    // one a peer verified, else to what the room first loaded. A forged base
                    // is never kept, so this dispute always stops the session.
                    let writer = writer.and_then(|client| self.user_of(client));
                    let fallback = self
                        .store
                        .dispute_live_sole_base(self.doc, &revision, OffsetDateTime::now_utc())
                        .await?;
                    let restore = fallback
                        .map(|(revision, _)| revision)
                        .or_else(|| self.first_loaded.clone());
                    self.base_revision.clone_from(&restore);
                    let _ = self.charge_dispute(&[writer, Some(user)]).await?;
                    self.open_dispute(
                        revision,
                        at,
                        (held.participant, held.client, held.user),
                        restore,
                    );
                }
            }
            // Otherwise covered by a later verified checkpoint meanwhile: nothing to verify.
            return Ok(LiveCheckpointReply::Accepted);
        };
        if at <= self.verified_through {
            return Ok(LiveCheckpointReply::Accepted);
        }
        if stored == sha256 && session_matches {
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
            let writer = self
                .checkpoint_writers
                .get(&revision)
                .and_then(|writer| self.user_of(writer.client));
            self.dispute(&revision).await?;
            if self.charge_dispute(&[writer, Some(user)]).await? {
                let restore = self.base_revision.clone();
                self.open_dispute(
                    revision,
                    at,
                    (held.participant, held.client, held.user),
                    restore,
                );
            }
        }
        Ok(LiveCheckpointReply::Accepted)
    }

    /// Two editors disagree on `revision` at `at`: changes wait, nothing new is asked for, and
    /// the verifier is asked for its own bytes before the dispute is settled.
    fn open_dispute(
        &mut self,
        revision: String,
        at: Seq,
        verifier: (ParticipantNo, ClientId, UserId),
        restore: Option<String>,
    ) {
        self.cancel_tickets(|held| held.purpose == TicketPurpose::Write, false);
        self.dispute = Some(OpenDispute {
            revision,
            at,
            verifier,
            asked: false,
            retry_at: None,
            restore,
            kept: false,
        });
    }

    /// The person behind `client`: its place in the session, else what the log knows.
    fn user_of(&self, client: ClientId) -> Option<UserId> {
        self.session
            .as_ref()
            .and_then(|session| {
                session
                    .places
                    .values()
                    .find(|place| place.client == client)
                    .map(|place| place.user)
            })
            .or_else(|| self.clients.get(&client).map(|(user, _)| *user))
    }

    /// Whether `user` is excluded from this document's checkpoint work now.
    fn excluded(&self, user: UserId) -> bool {
        self.dispute_marks
            .get(&user)
            .and_then(|mark| mark.excluded_until)
            .is_some_and(|until| until > OffsetDateTime::now_utc())
    }

    /// The people in a new dispute: whoever already took part in one that stopped the session
    /// within the hour is excluded from checkpoint work for a day, and the session goes on.
    /// Answers whether this dispute may stop the session (nobody in it was in one lately).
    async fn charge_dispute(&mut self, people: &[Option<UserId>]) -> Result<bool, RoomFailed> {
        let now = OffsetDateTime::now_utc();
        let mut people = people.iter().flatten().copied().collect::<Vec<_>>();
        people.sort_unstable_by_key(|user| user.0);
        people.dedup();
        let repeat = people
            .iter()
            .copied()
            .filter(|user| {
                self.dispute_marks
                    .get(user)
                    .and_then(|mark| mark.stopped_at)
                    .is_some_and(|stopped| now - stopped < DISPUTE_STOP_EVERY)
            })
            .collect::<Vec<_>>();
        if repeat.is_empty() {
            for user in people {
                let mark = self
                    .dispute_marks
                    .entry(user)
                    .or_insert(records::LiveDisputeMark {
                        user,
                        stopped_at: None,
                        excluded_until: None,
                    });
                mark.stopped_at = Some(now);
                let mark = mark.clone();
                self.store.set_live_dispute_mark(self.doc, &mark).await?;
            }
            return Ok(true);
        }
        let until = now + DISPUTE_EXCLUSION;
        for user in repeat {
            let mark = self
                .dispute_marks
                .entry(user)
                .or_insert(records::LiveDisputeMark {
                    user,
                    stopped_at: None,
                    excluded_until: None,
                });
            mark.excluded_until = Some(until);
            let mark = mark.clone();
            self.store.set_live_dispute_mark(self.doc, &mark).await?;
            self.cancel_tickets(|held| held.user == user, false);
            self.events.emit(NewEvent {
                kind: "live.checkpoint_participant_excluded",
                actor: None,
                subject: Some(user),
                document: Some(self.doc),
                node: Some(self.doc.into()),
                detail: serde_json::json!({
                    "user": user.0,
                    "until": until.unix_timestamp(),
                }),
            });
        }
        Ok(false)
    }

    /// Ends the live session after a dispute: everyone is told and their places end, so each
    /// client joins again and loads the current file.
    async fn stop_session(&mut self) -> Result<(), RoomFailed> {
        if !self.stop_due || self.writing {
            return Ok(());
        }
        self.stop_due = false;
        self.dispute_ended = true;
        let places = self
            .session
            .as_ref()
            .map(|session| {
                session
                    .places
                    .iter()
                    .filter(|(_, place)| place.held)
                    .map(|(participant, place)| (*participant, place.client))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let frame = encode(&LiveServerMsg::Unavailable {
            reason: UnavailableReason::CheckpointDispute,
        });
        for (participant, client) in places {
            let _ = self
                .members
                .send(Some(client), &Outbound::One(frame.clone()));
            self.end_place(participant, GoneReason::Left).await?;
        }
        Ok(())
    }

    /// The room's base is now the verified checkpoint `revision` at `at`: earlier unverified
    /// checkpoints, entries and agreement positions are no longer needed in memory.
    fn checkpoint_verified(&mut self, revision: String, at: Seq, user: UserId) {
        // Whether it is a major version, read before the base settles and drops it.
        let major = self
            .unverified_checkpoints
            .iter()
            .any(|checkpoint| checkpoint.revision == revision && checkpoint.major_reason.is_some());
        self.settle_base(revision.clone(), at);
        self.events.emit(NewEvent {
            kind: "live.checkpoint_verified",
            actor: Some(user),
            subject: None,
            document: Some(self.doc),
            node: Some(self.doc.into()),
            detail: serde_json::json!({ "revision": revision, "through": at.0 }),
        });
        self.notify_saved(revision, at, true, major, false);
    }

    /// The room's base is now the checkpoint `revision` at `at`.
    fn settle_base(&mut self, revision: String, at: Seq) {
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
        let sole = self.sole_base.as_ref().map(|(revision, _, _, _)| revision);
        self.session_hashes
            .retain(|revision, _| kept.contains(revision) || Some(revision) == sole);
        self.not_asked.retain(|revision, _| kept.contains(revision));
        self.cancel_tickets(
            |held| held.purpose == TicketPurpose::Verify && held.at <= at,
            false,
        );
        self.entries.retain(|entry| entry.seq > at);
        self.witnesses.retain(|seq, _| *seq > at);
        self.viewer_reports.retain(|seq, _| *seq > at);
        self.agreement.retain(|seq, _| *seq > at);
        self.recount();
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
                    major_reason: major_point(pending.major).map(|(reason, _)| reason),
                    major_person: major_point(pending.major).map(|(_, person)| person),
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
                (pending.sha256, session_sha256(&pending.session_data)),
                pending.session,
                pending.user,
                (pending.participant, pending.client),
                pending.major,
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
        } else if pending.tries.saturating_add(1) >= RECONCILE_TRIES {
            // Treated as failed: Git holds the commit, so the next checkpoint is written on top
            // of it from a fresh ticket.
            self.events.emit(NewEvent {
                kind: "live.checkpoint_reconcile_failed",
                actor: Some(pending.user),
                subject: None,
                document: Some(self.doc),
                node: Some(self.doc.into()),
                detail: serde_json::json!({
                    "revision": pending.revision,
                    "through": pending.at.0,
                    "tries": pending.tries.saturating_add(1),
                }),
            });
            // Only a revision Git knows becomes the base; otherwise the current one does.
            let known = matches!(
                self.store
                    .describe_live_revision(self.doc, &pending.revision)
                    .await,
                Ok(Some(_))
            );
            self.checkpoint_base = if known {
                Some(pending.revision)
            } else {
                match self.store.current_revision(self.doc).await {
                    Ok(current) => current,
                    Err(_) => self.checkpoint_base.clone(),
                }
            };
            self.first_unsaved.get_or_insert_with(Instant::now);
        } else {
            let delay = pending.delay;
            self.pending_reconcile = Some(PendingReconcile {
                retry_at: Instant::now() + delay,
                delay: delay.saturating_mul(2).min(RECONCILE_RETRY_MAX),
                tries: pending.tries.saturating_add(1),
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
            self.over_limit_refused.map(|at| at + OVER_LIMIT_LINGER),
            self.returns
                .iter()
                .filter(|barrier| barrier.stage == ReturnStage::Batch)
                .filter_map(|barrier| barrier.batch_since)
                .min()
                .map(|since| since + RETURN_BATCH_LIMIT),
            self.dispute.as_ref().and_then(|dispute| dispute.retry_at),
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
        self.accept_sole_editor().await?;
        self.close_stuck_batches();
        self.advance_returns().await;
        self.issue_due_tickets();
        self.stop_session().await?;
        self.sync_hold();
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

    /// A dispute ended the session: once no write runs, the room ends so the next join loads
    /// the stored state afresh.
    pub(crate) fn ended_by_dispute(&self) -> bool {
        self.dispute_ended && !self.writing && self.session.is_none()
    }

    /// Whether a room that refused an over-limit join stays a while, so reconnects are refused
    /// without counting the log again.
    pub(crate) fn lingers(&self) -> bool {
        self.over_limit_refused
            .is_some_and(|at| at.elapsed() < OVER_LIMIT_LINGER)
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
        let capable_editor = request.role.can_edit() && request.can_checkpoint;
        if self.over_limit && capable_editor && !self.over_limit_loaded {
            // A capable editor can write the checkpoint that brings the log back under: load the
            // whole log once, for it.
            let mut room = Self::load_whole(
                self.doc,
                self.store.clone(),
                self.events.clone(),
                self.timing,
            )
            .await?;
            room.over_limit = true;
            room.over_limit_loaded = true;
            room.over_limit_refused = self.over_limit_refused;
            *self = room;
        }
        if self.over_limit && !(capable_editor && self.over_limit_loaded) {
            // A recoverable fault: raising the limit lets people in again.
            send_direct(
                &request.tx,
                Outbound::One(encode(&LiveServerMsg::Unavailable {
                    reason: UnavailableReason::LogOverLimit,
                })),
            );
            let first = self.over_limit_refused.is_none();
            self.over_limit_refused = Some(Instant::now());
            let _ = request.accepted.send(JoinReply::Refused);
            if !first {
                return Ok(());
            }
            self.events.emit(NewEvent {
                kind: "live.log_over_limit",
                actor: Some(request.user),
                subject: None,
                document: Some(self.doc),
                node: Some(self.doc.into()),
                detail: serde_json::json!({
                    "entries": self.unsaved_entries,
                    "bytes": self.unsaved_bytes,
                }),
            });
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
        // A place whose session detail is past its 30 days is known by its tombstone only:
        // its batch cannot be placed, so its own copy is archived instead (C22).
        let mut archived = None;
        if kept.is_none()
            && returning.is_none()
            && request.returning_through.is_some()
            && let Some(resume) = request.resume
            && !self.known_places.contains_key(&resume.participant)
            && let Some(tombstone) = self
                .store
                .live_return_tombstone(self.doc, resume.session.0, resume.participant.0)
                .await?
            && tombstone.client == request.client
            && tombstone.user == request.user
        {
            archived = Some(resume);
        }
        let returning = returning.or(archived);
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
        // A client that joins again starts its queue over: no earlier hold applies.
        self.held_from.remove(&request.client);
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

        let can_edit_now = self
            .members
            .iter()
            .find(|member| member.client == request.client)
            .is_some_and(|member| member.role.can_edit());
        let mut archive_frame = None;
        let mut ready_frame = None;
        if let (Place::Returning { .. }, Some(through), true) =
            (place, request.returning_through, can_edit_now)
        {
            if archived.is_some() {
                archive_frame = Some(encode(&LiveServerMsg::ArchiveReturn {
                    current: self.checkpoint_base.clone(),
                }));
                if request.can_checkpoint && self.archives.len() < MAX_ARCHIVES {
                    self.archives
                        .push((participant, request.client, request.user));
                }
            } else {
                let now = Instant::now();
                self.return_marks
                    .retain(|_, at| now.saturating_duration_since(*at) < RETURN_MARK_EVERY);
                // The batch was stored before the drop, the person's returns were marked
                // recently, or too many wait: no versions are made and the batch goes at once.
                let stored = self
                    .clients
                    .get(&request.client)
                    .is_some_and(|(_, next)| *next > through.0);
                if stored
                    || self.return_marks.contains_key(&request.user)
                    || self.returns.len() >= MAX_RETURNS
                {
                    ready_frame = Some(encode(&LiveServerMsg::ReturnReady { through }));
                } else {
                    self.return_marks.insert(request.user, now);
                    self.returns.push_back(ReturnBarrier {
                        participant,
                        client: request.client,
                        user: request.user,
                        through,
                        stage: ReturnStage::Before,
                        batch_since: None,
                    });
                }
            }
        } else if let (Some(through), true) = (request.returning_through, can_edit_now)
            && !self.return_waits(request.client)
        {
            // A later join of a client whose barrier has opened (or ended): the `return_ready`
            // sent when it opened may have been lost with the connection, so it is sent again.
            // A client holding no such batch ignores it.
            ready_frame = Some(encode(&LiveServerMsg::ReturnReady { through }));
        }
        let from = request
            .seen
            .filter(|seen| *seen >= self.verified_through && *seen <= self.head)
            .unwrap_or(self.verified_through);
        let mut frames = vec![encode(&LiveServerMsg::LiveJoined {
            session: self.session.as_ref().ok_or_else(no_session)?.id,
            participant,
            place,
            can_edit: self
                .members
                .iter()
                .find(|member| member.client == request.client)
                .is_some_and(|member| member.role.can_edit()),
            heartbeat_ms: u64::try_from(self.timing.heartbeat.as_millis()).unwrap_or(HEARTBEAT_MS),
            people: self.people(),
        })];
        frames.extend(archive_frame);
        // Before the catch-up, so the client never sends into a hold; when nothing holds, a
        // client that missed the resume is released.
        frames.push(encode(&if self.paused || self.holding() {
            LiveServerMsg::ChangesPaused {
                entries: self.unsaved_entries,
                bytes: self.unsaved_bytes,
            }
        } else {
            LiveServerMsg::ChangesResumed
        }));
        frames.push(encode(&LiveServerMsg::Catchup {
            base: self.verified_through,
            revision: self.base_revision.clone(),
            from,
            head: self.head,
        }));
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
        frames.extend(ready_frame);
        let _ = self
            .members
            .send(Some(request.client), &Outbound::Many(frames));
        self.after_join(request.client, participant);
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
            LiveLocal::Stored { .. } => {
                // A batch whose last local was stored before the drop ends here too.
                self.note_return_local(client, local);
                return Ok(());
            }
            LiveLocal::Refused { reason } => {
                self.refuse(client, local, reason_from_store(&reason));
                self.note_return_local(client, local);
                return Ok(());
            }
            LiveLocal::Unseen => {}
        }
        let next_local = self.clients.get(&client).map_or(1, |(_, next)| *next);
        if local.0 < next_local {
            self.refuse(client, local, RefusedReason::OutOfOrder);
            self.note_return_local(client, local);
            return Ok(());
        }
        if self
            .held_from
            .get(&client)
            .is_some_and(|first| local.0 > *first)
        {
            if self.holding() || self.paused || self.return_waits(client) {
                // Sent before the client heard that its earlier change was held: it sends
                // this again, in order, once changes resume.
                return Ok(());
            }
            // The hold is over and the held change did not come first: the client gave it up
            // (it lost its queue or its edit right meanwhile). Never wedge it: forget the hold
            // and resync it; this change goes on as usual.
            self.held_from.remove(&client);
            self.send_resync(client);
        }
        if self.holding() || self.return_waits(client) {
            // Changes wait for a major version at the head. Not refused for good: the number
            // is not used up and the client sends the change again after the hold.
            self.held_from.insert(client, local.0);
            self.refuse(client, local, RefusedReason::Held);
            return Ok(());
        }
        self.held_from.remove(&client);

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
            // Checked before the pause, so a paused room's refusals are paced too.
            Some(RefusedReason::RateLimited)
        } else if self.paused || self.would_cross(payload.len()) {
            Some(RefusedReason::LogPaused)
        } else {
            None
        };
        if reason == Some(RefusedReason::LogPaused) {
            // The log is at its limit: the change waits like any held change, its number not
            // used up, and the client sends it again once changes resume. Nothing a person saw
            // is rolled back.
            self.pause();
            self.held_from.insert(client, local.0);
            self.refuse(client, local, RefusedReason::Held);
            return Ok(());
        }
        if let Some(reason) = reason {
            // A rate refusal is not written down, so a flood costs the database nothing; the
            // number is still used up in this room.
            if reason != RefusedReason::RateLimited {
                let at = OffsetDateTime::now_utc();
                self.store
                    .consume_live_local(self.doc, client, user, local.0, reason_name(reason), at)
                    .await?;
            }
            self.advance_local(client, user, local);
            self.refuse(client, local, reason);
            self.note_return_local(client, local);
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
        let payload_len = payload.len();
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
        self.unsaved_entries = self.unsaved_entries.saturating_add(1);
        self.unsaved_bytes = self
            .unsaved_bytes
            .saturating_add(u64::try_from(payload_len).unwrap_or(u64::MAX));
        let _ = self.members.send(
            None,
            &Outbound::One(encode(&LiveServerMsg::Entry { entry })),
        );
        self.check_limits();
        self.note_return_local(client, local);
        Ok(())
    }

    /// Whether `client` is a returning client whose batch must wait for the version before it.
    fn return_waits(&self, client: ClientId) -> bool {
        self.returns
            .iter()
            .any(|barrier| barrier.client == client && barrier.stage == ReturnStage::Before)
    }

    /// Closes every returning batch that has not reached the local named on the join within
    /// [`RETURN_BATCH_LIMIT`]: its version after is stored and its client resynced, so the
    /// returns behind it are not held up.
    fn close_stuck_batches(&mut self) {
        let now = Instant::now();
        let mut stuck = Vec::new();
        for barrier in &mut self.returns {
            if barrier.stage == ReturnStage::Batch
                && barrier
                    .batch_since
                    .is_some_and(|since| since + RETURN_BATCH_LIMIT <= now)
            {
                barrier.stage = ReturnStage::After;
                stuck.push((barrier.client, barrier.through));
            }
        }
        for (client, through) in stuck {
            // Its `return_ready` may never have arrived: told again before the resync.
            let frame = encode(&LiveServerMsg::ReturnReady { through });
            let _ = self.members.send(Some(client), &Outbound::One(frame));
            self.send_resync(client);
        }
    }

    /// A returning client's local `local` was stored or finally refused: once it reaches the
    /// end of the batch named on the join, the result is stored as a major version at once.
    fn note_return_local(&mut self, client: ClientId, local: LocalNo) {
        if let Some(barrier) = self.returns.iter_mut().find(|barrier| {
            barrier.client == client
                && barrier.stage == ReturnStage::Batch
                && local.0 >= barrier.through.0
        }) {
            barrier.stage = ReturnStage::After;
        }
    }

    /// Whether new changes wait for a major version to be stored at the head.
    fn holding(&self) -> bool {
        self.recovery.is_some()
            || self.dispute.is_some()
            || self.writing_hold
            || self
                .tickets
                .iter()
                .any(|held| held.major.is_some_and(MajorTicket::holds_changes))
    }

    /// Tells everyone when changes start or stop waiting for a major version.
    fn sync_hold(&mut self) {
        let hold = self.holding();
        if hold == self.hold_announced {
            return;
        }
        self.hold_announced = hold;
        if self.paused {
            // The log-limit pause has told everyone already, and tells them when it ends.
            return;
        }
        let frame = if hold {
            LiveServerMsg::ChangesPaused {
                entries: self.unsaved_entries,
                bytes: self.unsaved_bytes,
            }
        } else {
            LiveServerMsg::ChangesResumed
        };
        let _ = self.members.send(None, &Outbound::One(encode(&frame)));
    }

    /// Records the event of a major version stored as `revision`.
    fn major_version(&self, revision: &str, reason: &str, person: UserId, at: Seq) {
        self.events.emit(NewEvent {
            kind: "live.major_version",
            actor: Some(person),
            subject: Some(person),
            document: Some(self.doc),
            node: Some(self.doc.into()),
            detail: serde_json::json!({
                "revision": revision,
                "reason": reason,
                "through": at.0,
            }),
        });
    }

    /// The major version of the first returning client's current step is stored: before the
    /// batch, the client may now send it; after it, the return is complete.
    fn return_point_stored(&mut self) {
        let Some(barrier) = self.returns.front_mut() else {
            return;
        };
        match barrier.stage {
            ReturnStage::Before => {
                barrier.stage = ReturnStage::Batch;
                barrier.batch_since = Some(Instant::now());
                let frame = encode(&LiveServerMsg::ReturnReady {
                    through: barrier.through,
                });
                let client = barrier.client;
                let participant = barrier.participant;
                let _ = self.members.send(Some(client), &Outbound::One(frame));
                self.announce_returning(client, participant);
            }
            ReturnStage::After => {
                self.returns.pop_front();
            }
            ReturnStage::Batch => {}
        }
    }

    /// Settles the first return barrier where nothing new needs writing, and drops or ends one
    /// whose client has gone: a barrier before its batch is dropped; one in it is closed with
    /// a version after whatever of the batch arrived.
    async fn advance_returns(&mut self) {
        while let Some(barrier) = self.returns.front().copied() {
            let held = self
                .session
                .as_ref()
                .and_then(|session| session.places.get(&barrier.participant))
                .is_some_and(|place| place.held && place.client == barrier.client);
            if held || barrier.stage == ReturnStage::After {
                break;
            }
            if barrier.stage == ReturnStage::Before {
                self.returns.pop_front();
            } else if let Some(front) = self.returns.front_mut() {
                front.stage = ReturnStage::After;
            }
        }
        let Some(barrier) = self.returns.front().copied() else {
            return;
        };
        if barrier.stage == ReturnStage::Batch
            || self.head > self.saved_through
            || self.writing
            || self.recovery.is_some()
            || self.pending_reconcile.is_some()
            || self
                .tickets
                .iter()
                .any(|held| held.purpose == TicketPurpose::Write)
            || self.mark_retry_at.is_some_and(|at| at > Instant::now())
        {
            return;
        }
        // The head is stored already: mark it major without new content.
        let Some(current) = self.checkpoint_base.clone() else {
            // Nothing stored at all: there is no version to keep.
            self.return_point_stored();
            return;
        };
        let reason = if barrier.stage == ReturnStage::Before {
            MajorReason::ReturningClientBefore
        } else {
            MajorReason::ReturningClientAfter
        };
        let session = self
            .session
            .as_ref()
            .map_or_else(Uuid::nil, |session| session.id.0);
        let major = if barrier.stage == ReturnStage::Before {
            MajorTicket::Before(barrier.user)
        } else {
            MajorTicket::After(barrier.user)
        };
        let owned = OwnedMajor {
            kind: MajorKind::Mark,
            reason: major_reason_name(reason),
            person: barrier.user,
            current,
            restore: None,
            disputed: None,
            blob: None,
            authors: Vec::new(),
            at: self.head,
            session,
            created_at: OffsetDateTime::now_utc(),
        };
        let ticket = self.synthetic_ticket(
            (barrier.participant, barrier.client, barrier.user),
            self.head,
            major,
        );
        let (reply, _) = oneshot::channel();
        // The mark runs off the room task; changes wait for it (`writing_hold`).
        self.spawn_major(ticket, owned, reply);
    }

    /// Asks for a major-version write that is due: the room's last state after an outside
    /// replacement first, then an expired returning client's own copy, then the version before
    /// or after a returning client's batch. One write at a time.
    fn issue_major_ticket(&mut self) {
        if self.writing
            || self.pending_reconcile.is_some()
            || self
                .tickets
                .iter()
                .any(|held| held.purpose == TicketPurpose::Write)
        {
            return;
        }
        let at = self.head;
        let base = self.checkpoint_base.clone();
        if self.recovery.is_some() {
            if let Some(holder) = self.ticket_candidates(None, None).first().copied() {
                self.issue_ticket(
                    TicketPurpose::Write,
                    holder,
                    at,
                    base,
                    None,
                    Some(MajorTicket::Outside),
                );
            }
            return;
        }
        while let Some((participant, client, user)) = self.archives.pop() {
            if self.place_can_checkpoint(participant, client) {
                self.issue_ticket(
                    TicketPurpose::Write,
                    (participant, client, user),
                    at,
                    base,
                    None,
                    Some(MajorTicket::Archive(user)),
                );
                return;
            }
        }
        let Some(barrier) = self.returns.front().copied() else {
            return;
        };
        let major = match barrier.stage {
            ReturnStage::Before => MajorTicket::Before(barrier.user),
            ReturnStage::After => MajorTicket::After(barrier.user),
            ReturnStage::Batch => return,
        };
        if self.head <= self.saved_through {
            // Marked without new content by `advance_returns`.
            return;
        }
        if let Some(holder) = self.ticket_candidates(None, None).first().copied() {
            self.issue_ticket(TicketPurpose::Write, holder, at, base, None, Some(major));
        }
    }

    /// The file was replaced from outside by `outside` (S4): changes wait, and the room's last
    /// state is asked for once more, to be kept below it.
    fn enter_recovery(&mut self, outside: String) {
        self.recovery = Some(Recovery {
            head: outside.clone(),
            outside,
        });
        self.dispute = None;
        self.cancel_tickets(|_| true, false);
    }

    /// After the room's last state was kept below the outside file, stored again as
    /// `revision`: that file is the verified base at `at` and everyone loads it.
    fn rebase_on(&mut self, revision: String, at: Seq) {
        self.recovery = None;
        self.verified_through = self.verified_through.max(at);
        self.saved_through = self.saved_through.max(at);
        self.base_revision = Some(revision.clone());
        self.checkpoint_base = Some(revision);
        self._base_session_data = Vec::new();
        self.unverified_checkpoints.clear();
        self.checkpoint_writers.clear();
        self.session_hashes.clear();
        self.not_asked.clear();
        self.dispute = None;
        self.pending_reconcile = None;
        self.cancel_tickets(|_| true, false);
        self.entries.retain(|entry| entry.seq > at);
        self.witnesses.retain(|seq, _| *seq > at);
        self.viewer_reports.retain(|seq, _| *seq > at);
        self.agreement.clear();
        if self.head <= at {
            self.first_unsaved = None;
            self.last_change = None;
        }
        self.recount();
        let mut clients = self
            .members
            .iter()
            .map(|member| member.client)
            .collect::<Vec<_>>();
        clients.sort_unstable_by_key(|client| client.0);
        clients.dedup();
        for client in clients {
            self.send_resync(client);
        }
    }

    /// Whether storing one more change of `len` payload bytes would take the log past a hard
    /// limit.
    fn would_cross(&self, len: usize) -> bool {
        let limits = self.timing.limits;
        self.unsaved_entries.saturating_add(1) > limits.hard_entries
            || self
                .unsaved_bytes
                .saturating_add(u64::try_from(len).unwrap_or(u64::MAX))
                > limits.hard_bytes
    }

    fn at_hard_limit(&self) -> bool {
        let limits = self.timing.limits;
        self.unsaved_entries >= limits.hard_entries || self.unsaved_bytes >= limits.hard_bytes
    }

    fn at_warning(&self) -> bool {
        let limits = self.timing.limits;
        self.unsaved_entries >= limits.warn_entries || self.unsaved_bytes >= limits.warn_bytes
    }

    /// After the log grew: warns (asking for a checkpoint at once) and, at a hard limit,
    /// pauses new changes so clients hear of it before sending more.
    fn check_limits(&mut self) {
        if !self.warned && self.at_warning() {
            self.warned = true;
            self.events.emit(NewEvent {
                kind: "live.log_warning",
                actor: None,
                subject: None,
                document: Some(self.doc),
                node: Some(self.doc.into()),
                detail: serde_json::json!({
                    "entries": self.unsaved_entries,
                    "bytes": self.unsaved_bytes,
                }),
            });
        }
        if self.at_hard_limit() {
            self.pause();
        }
    }

    /// Pauses new changes and tells everyone, once.
    fn pause(&mut self) {
        if self.paused {
            return;
        }
        self.paused = true;
        let frame = encode(&LiveServerMsg::ChangesPaused {
            entries: self.unsaved_entries,
            bytes: self.unsaved_bytes,
        });
        let _ = self.members.send(None, &Outbound::One(frame));
        self.events.emit(NewEvent {
            kind: "live.changes_paused",
            actor: None,
            subject: None,
            document: Some(self.doc),
            node: Some(self.doc.into()),
            detail: serde_json::json!({
                "entries": self.unsaved_entries,
                "bytes": self.unsaved_bytes,
            }),
        });
    }

    /// Counts the kept log again after its verified base moved, and resumes changes once it
    /// is under both hard limits.
    fn recount(&mut self) {
        self.unsaved_entries = u64::try_from(self.entries.len()).unwrap_or(u64::MAX);
        self.unsaved_bytes = self
            .entries
            .iter()
            .map(|entry| match &entry.body {
                EntryBody::Change { payload, .. } => u64::try_from(payload.len()).unwrap_or(0),
                EntryBody::Cancel { .. } => records::LIVE_CANCEL_BYTES,
            })
            .fold(0_u64, u64::saturating_add);
        self.warned = self.at_warning();
        self.urgent_asked = false;
        if self.over_limit
            && !beyond_limits(self.timing.limits, self.unsaved_entries, self.unsaved_bytes)
        {
            self.over_limit = false;
        }
        if self.paused && !self.at_hard_limit() {
            self.paused = false;
            if !self.hold_announced {
                let _ = self
                    .members
                    .send(None, &Outbound::One(encode(&LiveServerMsg::ChangesResumed)));
            }
            self.events.emit(NewEvent {
                kind: "live.changes_resumed",
                actor: None,
                subject: None,
                document: Some(self.doc),
                node: Some(self.doc.into()),
                detail: serde_json::json!({
                    "entries": self.unsaved_entries,
                    "bytes": self.unsaved_bytes,
                }),
            });
        }
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
        // `None` is a replaced connection's late leave, or a member already dropped because its
        // queue was full; only the second still needs the others told.
        if self.members.leave(conn, client).is_none()
            && self.members.iter().any(|member| member.client == client)
        {
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
        let freed = u64::try_from(payload.len()).unwrap_or(0);
        payload.clear();
        *cancelled = true;
        self.unsaved_entries = self.unsaved_entries.saturating_add(1);
        self.unsaved_bytes = self
            .unsaved_bytes
            .saturating_sub(freed)
            .saturating_add(records::LIVE_CANCEL_BYTES);
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
        self.check_limits();
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
        // A member dropped while this goes out (its queue was full) is shown as gone at once:
        // the list goes again until nobody more is dropped, and each round drops one at least.
        loop {
            let frame = encode(&LiveServerMsg::People {
                people: self.people(),
            });
            let dropped = self.members.send(None, &Outbound::One(frame)).any();
            if !(self.drop_absent_presence() || dropped) {
                break;
            }
        }
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
        if room.ended_by_dispute()
            || (!room.has_session()
                && !room.writing()
                && !room.lingers()
                && rx.is_empty()
                && access_rx.is_empty())
        {
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
    returning_through: Option<LocalNo>,
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

/// The stored reason and person of the major-version checkpoint `major` makes at a position.
fn major_point(major: Option<MajorTicket>) -> Option<(&'static str, UserId)> {
    match major? {
        MajorTicket::Before(person) => Some((
            major_reason_name(MajorReason::ReturningClientBefore),
            person,
        )),
        MajorTicket::After(person) => {
            Some((major_reason_name(MajorReason::ReturningClientAfter), person))
        }
        MajorTicket::Outside | MajorTicket::Archive(_) | MajorTicket::DisputedCopy(_) => None,
    }
}

/// Whether a kept log of `entries` and `bytes` is beyond what was stored under `limits`. Paused
/// traffic is allowed for: cancel records (at most one per change) may take the log past the
/// hard limits by one record per change allowed.
/// The SHA-256 of a checkpoint's session (identity) data, as a verifier reports it.
fn session_sha256(session_data: &[u8]) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(session_data).into()
}

fn beyond_limits(limits: crate::config::LiveLimits, entries: u64, bytes: u64) -> bool {
    entries > limits.hard_entries.saturating_mul(2)
        || bytes
            > limits.hard_bytes.saturating_add(
                limits
                    .hard_entries
                    .saturating_mul(records::LIVE_CANCEL_BYTES),
            )
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
        RefusedReason::LogPaused => "log_paused",
        RefusedReason::Held => "held",
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
        "log_paused" => RefusedReason::LogPaused,
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
        EntryBody, LiveBuild, LiveResume, LiveServerMsg, LocalNo, MajorReason, ParticipantNo,
        Place, RefusedReason, TicketPurpose,
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

    const FORMAT: &str = "gridcraft/1";

    struct Joined {
        conn: u64,
        client: ClientId,
        rx: mpsc::Receiver<Outbound>,
        session: protocol::live::LiveSessionId,
        participant: ParticipantNo,
        place: Place,
        initial: Vec<LiveServerMsg>,
        /// The join's own frames exactly as sent, `changes_resumed` included.
        raw: Vec<LiveServerMsg>,
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
            limits: crate::config::LiveLimits::default(),
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
            returning_through: None,
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
            raw: Vec::new(),
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
                ..,
            ] if first
                .iter()
                .any(|frame| matches!(frame, LiveServerMsg::Catchup { .. })) =>
            {
                joined.session = *session;
                joined.participant = *participant;
                joined.place = *place;
            }
            other => panic!("unexpected join frames: {other:?}"),
        }
        joined.raw.clone_from(&first);
        joined.initial = without_resumed(first);
        joined.after_join = drain(&mut joined.rx);
        joined
    }

    /// Join frames without the `changes_resumed` every join carries when nothing holds.
    fn without_resumed(frames: Vec<LiveServerMsg>) -> Vec<LiveServerMsg> {
        frames
            .into_iter()
            .filter(|frame| *frame != LiveServerMsg::ChangesResumed)
            .collect()
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
                returning_through: None,
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
            length: 3,
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
        let mut capable = join_capable(&mut room, 3, 3, "editor", LiveBuild::Browser).await;
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
        let mut bob = join_capable(&mut room, 2, 2, "editor", LiveBuild::Browser).await;
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
        let mut bob = join_capable(&mut room, 2, 2, "editor", LiveBuild::Browser).await;
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

    /// The SHA-256 of the session data every test upload carries.
    fn identities_sha() -> [u8; 32] {
        use sha2::Digest;
        sha2::Sha256::digest(b"identities").into()
    }

    async fn hash<S: crate::live_store::LiveStore>(
        room: &mut LiveRoom<S>,
        user: UserId,
        ticket: Uuid,
        at: Seq,
        sha: u8,
    ) -> LiveCheckpointReply {
        hash_full(room, user, ticket, at, sha, identities_sha()).await
    }

    async fn hash_full<S: crate::live_store::LiveStore>(
        room: &mut LiveRoom<S>,
        user: UserId,
        ticket: Uuid,
        at: Seq,
        sha: u8,
        session_sha256: [u8; 32],
    ) -> LiveCheckpointReply {
        let (reply, answer) = oneshot::channel();
        room.handle(LiveCommand::CheckpointHash {
            user,
            ticket,
            at,
            sha256: [sha; 32],
            session_sha256,
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
        let mut bob = join_capable(&mut room, 2, 2, "editor", LiveBuild::Browser).await;
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

    // ---- Task 10: unsaved-log and catch-up limits ----

    fn limited(
        warn_entries: u64,
        hard_entries: u64,
        warn_bytes: u64,
        hard_bytes: u64,
    ) -> LiveTiming {
        LiveTiming {
            limits: crate::config::LiveLimits {
                warn_entries,
                warn_bytes,
                hard_entries,
                hard_bytes,
            },
            ..timing()
        }
    }

    async fn limited_room(timing: LiveTiming) -> (LiveRoom<MemStore>, MemStore) {
        let store = MemStore::new();
        let room = LiveRoom::load(DocId(id(100)), store.clone(), EventSink::discard(), timing)
            .await
            .unwrap();
        (room, store)
    }

    fn refusal(messages: &[LiveServerMsg]) -> Option<RefusedReason> {
        messages.iter().find_map(|message| match message {
            LiveServerMsg::Refused { reason, .. } => Some(*reason),
            _ => None,
        })
    }

    fn paused(messages: &[LiveServerMsg]) -> Option<(u64, u64)> {
        messages.iter().find_map(|message| match message {
            LiveServerMsg::ChangesPaused { entries, bytes } => Some((*entries, *bytes)),
            _ => None,
        })
    }

    #[test]
    fn default_limits_are_the_adopted_values() {
        let limits = crate::config::LiveLimits::default();
        assert_eq!(limits.warn_entries, 4_000);
        assert_eq!(limits.warn_bytes, 48 * 1024 * 1024);
        assert_eq!(limits.hard_entries, 5_000);
        assert_eq!(limits.hard_bytes, 64 * 1024 * 1024);
        assert_eq!(super::MAX_SESSION_DATA_BYTES, 8 * 1024 * 1024);
    }

    #[tokio::test(start_paused = true)]
    async fn warning_threshold_issues_urgent_ticket() {
        let (events, mut emitted) = EventSink::channel();
        let mut room = LiveRoom::load(
            DocId(id(100)),
            MemStore::new(),
            events,
            limited(3, 10, u64::MAX / 2, u64::MAX / 2),
        )
        .await
        .unwrap();
        let mut ann = join_capable(&mut room, 1, 1, "editor", LiveBuild::Browser).await;
        for local in 1..=2 {
            entry(&mut room, &ann, local, local - 1, FORMAT, "x".into())
                .await
                .unwrap();
        }
        assert!(
            checkpoint_ticket(drain(&mut ann.rx)).is_none(),
            "not due yet"
        );
        entry(&mut room, &ann, 3, 2, FORMAT, "x".into())
            .await
            .unwrap();
        // No quiet period: the warning asks at once.
        let (_, purpose, at) = checkpoint_ticket(drain(&mut ann.rx)).unwrap();
        assert_eq!((purpose, at), (TicketPurpose::Write, Seq(3)));
        assert_eq!(
            emitted_kinds(&mut emitted, "live.log"),
            [("live.log_warning", json!({ "entries": 3, "bytes": 3 }))]
        );
        // Changes still go in below the hard limit.
        entry(&mut room, &ann, 4, 3, FORMAT, "x".into())
            .await
            .unwrap();
        assert_eq!(room.head, Seq(4));
    }

    #[tokio::test(start_paused = true)]
    async fn entry_limit_pauses_before_storing_the_next_change() {
        let (events, mut emitted) = EventSink::channel();
        let store = MemStore::new();
        let mut room = LiveRoom::load(
            DocId(id(100)),
            store.clone(),
            events,
            limited(100, 3, u64::MAX / 2, u64::MAX / 2),
        )
        .await
        .unwrap();
        let mut ann = join_as(&mut room, 1, 1, 11, "editor", FORMAT, None, None).await;
        let mut bob = join_as(&mut room, 2, 2, 12, "editor", FORMAT, None, None).await;
        for local in 1..=3 {
            entry(&mut room, &ann, local, local - 1, FORMAT, "x".into())
                .await
                .unwrap();
        }
        assert_eq!(store.entry_count(), 3);
        // The third change reached the limit: everyone is told before sending more.
        assert_eq!(paused(&drain(&mut bob.rx)), Some((3, 3)));
        let _ = drain(&mut ann.rx);
        entry(&mut room, &ann, 4, 3, FORMAT, "x".into())
            .await
            .unwrap();
        assert_eq!(store.entry_count(), 3, "the next change is not stored");
        assert_eq!(room.head, Seq(3));
        let answer = drain(&mut ann.rx);
        assert_eq!(refusal(&answer), Some(RefusedReason::Held));
        assert!(entries(answer).is_empty(), "nothing is broadcast");
        assert!(entries(drain(&mut bob.rx)).is_empty());
        assert_eq!(
            crate::live_store::LiveStore::live_local(&store, DocId(id(100)), ann.client, 4)
                .await
                .unwrap(),
            records::LiveLocal::Unseen,
            "held, its number is not used up"
        );
        assert_eq!(
            emitted_kinds(&mut emitted, "live.changes"),
            [("live.changes_paused", json!({ "entries": 3, "bytes": 3 }))]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn byte_limit_pauses_before_storing_the_next_change() {
        let (mut room, store) = limited_room(limited(100, 100, 100, 10)).await;
        let mut ann = join_as(&mut room, 1, 1, 11, "editor", FORMAT, None, None).await;
        for (local, payload) in [(1, "aaaa"), (2, "bbbb")] {
            entry(&mut room, &ann, local, local - 1, FORMAT, payload.into())
                .await
                .unwrap();
        }
        let _ = drain(&mut ann.rx);
        entry(&mut room, &ann, 3, 2, FORMAT, "ccc".into())
            .await
            .unwrap();
        let answer = drain(&mut ann.rx);
        assert_eq!(refusal(&answer), Some(RefusedReason::Held));
        assert_eq!(paused(&answer), Some((2, 8)));
        assert_eq!(store.entry_count(), 2);
        // Paused: even a change that would fit waits for a checkpoint. Sent again first, the
        // held change is held again; a later one waits quietly behind it.
        entry(&mut room, &ann, 3, 2, FORMAT, "ccc".into())
            .await
            .unwrap();
        assert_eq!(refusal(&drain(&mut ann.rx)), Some(RefusedReason::Held));
        entry(&mut room, &ann, 4, 2, FORMAT, "d".into())
            .await
            .unwrap();
        assert!(refusal(&drain(&mut ann.rx)).is_none());
        assert_eq!(store.entry_count(), 2);
    }

    /// A room paused at three entries, with Ann holding a write ticket and Bob a capable
    /// editor.
    async fn paused_room(
        events: EventSink,
    ) -> (LiveRoom<MemStore>, MemStore, Joined, Joined, Uuid) {
        let store = MemStore::new();
        let mut room = LiveRoom::load(
            DocId(id(100)),
            store.clone(),
            events,
            limited(3, 3, u64::MAX / 2, u64::MAX / 2),
        )
        .await
        .unwrap();
        let mut ann = join_capable(&mut room, 1, 1, "editor", LiveBuild::Browser).await;
        let mut bob = join_capable(&mut room, 2, 2, "editor", LiveBuild::Browser).await;
        let mut ticket = None;
        for local in 1..=3 {
            entry(&mut room, &ann, local, local - 1, FORMAT, "x".into())
                .await
                .unwrap();
            if let Some((id, _, _)) = checkpoint_ticket(drain(&mut ann.rx)) {
                ticket = Some(id);
            }
        }
        let _ = drain(&mut bob.rx);
        assert!(room.paused);
        (room, store, ann, bob, ticket.expect("an urgent ticket"))
    }

    #[tokio::test(start_paused = true)]
    async fn paused_room_still_accepts_control_messages() {
        let (mut room, _store, mut ann, mut bob, _ticket) = paused_room(EventSink::discard()).await;
        room.handle(LiveCommand::Heartbeat {
            conn: bob.conn,
            client: bob.client,
            seen: Seq(3),
        })
        .await
        .unwrap();
        room.handle(LiveCommand::Resync {
            conn: bob.conn,
            client: bob.client,
        })
        .await
        .unwrap();
        let resent = drain(&mut bob.rx);
        assert_eq!(resyncs(&resent), 1);
        assert_eq!(entries(resent).len(), 3);
        fingerprint(&mut room, &bob, 64, &"a".repeat(64)).await;
        room.handle(LiveCommand::Leave {
            conn: ann.conn,
            client: ann.client,
        })
        .await
        .unwrap();
        room.settle().await.unwrap();
        assert!(
            room.people()
                .iter()
                .all(|person| person.participant != ann.participant)
        );
        let _ = drain(&mut ann.rx);
    }

    #[tokio::test(start_paused = true)]
    async fn verified_checkpoint_resumes_changes() {
        let (events, mut emitted) = EventSink::channel();
        let (mut room, store, mut ann, mut bob, ticket) = paused_room(events).await;
        let LiveCheckpointReply::Saved { .. } =
            upload(&mut room, UserId(id(11)), ticket, Seq(3), 9).await
        else {
            panic!("a paused room still stores checkpoints");
        };
        assert!(
            room.paused,
            "a stored but unverified checkpoint does not resume"
        );
        let (verify, purpose, at) = checkpoint_ticket(drain(&mut bob.rx)).unwrap();
        assert_eq!((purpose, at), (TicketPurpose::Verify, Seq(3)));
        let _ = emitted_kinds(&mut emitted, "");
        assert_eq!(
            hash(&mut room, UserId(id(12)), verify, Seq(3), 9).await,
            LiveCheckpointReply::Accepted
        );
        assert!(!room.paused);
        assert!(
            drain(&mut ann.rx)
                .iter()
                .any(|message| matches!(message, LiveServerMsg::ChangesResumed))
        );
        assert_eq!(
            emitted_kinds(&mut emitted, "live.changes"),
            [("live.changes_resumed", json!({ "entries": 0, "bytes": 0 }))]
        );
        entry(&mut room, &ann, 4, 3, FORMAT, "x".into())
            .await
            .unwrap();
        assert_eq!(room.head, Seq(4));
        assert_eq!(store.entry_count(), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn restart_cancels_ticket_and_keeps_pause() {
        let doc = DocId(id(100));
        let (room, store, ann, _bob, ticket) = paused_room(EventSink::discard()).await;
        let resume = LiveResume {
            session: ann.session,
            participant: ann.participant,
        };
        drop(room);
        let mut room = LiveRoom::load(
            doc,
            store.clone(),
            EventSink::discard(),
            limited(3, 3, u64::MAX / 2, u64::MAX / 2),
        )
        .await
        .unwrap();
        assert!(room.paused, "limits are recomputed from durable entries");
        let mut ann = join_full(
            &mut room,
            7,
            1,
            UserId(id(11)),
            "editor",
            FORMAT,
            Some(resume),
            Some(Seq(3)),
            true,
            LiveBuild::Browser,
        )
        .await;
        assert_eq!(paused(&ann.initial), Some((3, 3)));
        assert_eq!(
            upload(&mut room, UserId(id(11)), ticket, Seq(3), 9).await,
            LiveCheckpointReply::Refused,
            "no ticket survives a restart"
        );
        let _ = drain(&mut ann.rx);
        entry(&mut room, &ann, 4, 3, FORMAT, "x".into())
            .await
            .unwrap();
        assert_eq!(refusal(&drain(&mut ann.rx)), Some(RefusedReason::Held));
        assert_eq!(store.entry_count(), 3, "no entry is lost or counted twice");
    }

    #[tokio::test(start_paused = true)]
    async fn catchup_never_exceeds_the_unsaved_limits() {
        let doc = DocId(id(100));
        let (mut room, store) = limited_room(limited(100, 4, u64::MAX / 2, u64::MAX / 2)).await;
        let ann = join_as(&mut room, 1, 1, 11, "editor", FORMAT, None, None).await;
        for local in 1..=6 {
            {
                let seen = room_head(&room);
                entry(&mut room, &ann, local, seen, FORMAT, "x".into())
            }
            .await
            .unwrap();
        }
        assert_eq!(store.entry_count(), 4);
        let late = join_as(&mut room, 2, 2, 12, "editor", FORMAT, None, None).await;
        assert_eq!(entries(late.initial.clone()).len(), 4);
        assert_eq!(paused(&late.initial), Some((4, 4)));

        // A lower limit on durable state: new joins are refused rather than sent an unbounded
        // catch-up, and the fault is recorded.
        let (events, mut emitted) = EventSink::channel();
        let mut lowered = LiveRoom::load(
            doc,
            store,
            events,
            limited(1, 1, u64::MAX / 2, u64::MAX / 2),
        )
        .await
        .unwrap();
        let (tx, mut rx) = mpsc::channel(16);
        let (accepted, answer) = oneshot::channel();
        lowered
            .handle(LiveCommand::Join {
                conn: 9,
                client: ClientId(id(9)),
                user: UserId(id(19)),
                session: SessionId(id(1019)),
                name: "Late".into(),
                role: builtin_access("editor"),
                protocol: 1,
                format: FORMAT.to_owned(),
                resume: None,
                seen: None,
                can_checkpoint: false,
                build: LiveBuild::Browser,
                returning_through: None,
                tx,
                accepted,
            })
            .await
            .unwrap();
        assert_eq!(answer.await.unwrap(), JoinReply::Refused);
        assert!(entries(drain(&mut rx)).is_empty());
        assert_eq!(
            emitted_kinds(&mut emitted, "live.log"),
            [("live.log_over_limit", json!({ "entries": 4, "bytes": 4 }))]
        );
    }

    fn room_head(room: &LiveRoom<MemStore>) -> u64 {
        room.head.0
    }

    #[tokio::test(start_paused = true)]
    async fn five_thousand_small_entries_pause_at_the_adopted_limit() {
        let (mut room, store) = room().await;
        let ann = join_as(&mut room, 1, 1, 11, "editor", FORMAT, None, None).await;
        let mut rx = ann.rx;
        let ann = Joined {
            rx: mpsc::channel(1).1,
            initial: Vec::new(),
            after_join: Vec::new(),
            ..ann
        };
        for local in 1..=5_001 {
            // Within the rate limit.
            tokio::time::advance(Duration::from_millis(40)).await;
            {
                let seen = room_head(&room);
                entry(&mut room, &ann, local, seen, FORMAT, "x".into())
            }
            .await
            .unwrap();
            let _ = drain(&mut rx);
        }
        assert_eq!(store.entry_count(), 5_000);
        assert!(room.paused);
        assert_eq!(room.entries.len(), 5_000);
        assert!(
            room.entries.capacity() < 2 * 5_000,
            "no oversized allocation"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn one_editor_cannot_grow_checkpoint_storage_without_bound() {
        let doc = DocId(id(100));
        let (mut room, store) = room().await;
        let mut ann = join_capable(&mut room, 1, 1, "editor", LiveBuild::Browser).await;
        let mut written = 0;
        for local in 1..=20 {
            {
                let seen = room_head(&room);
                entry(&mut room, &ann, local, seen, FORMAT, "x".into())
            }
            .await
            .unwrap();
            tokio::time::advance(Duration::from_secs(15)).await;
            room.expire_due().await.unwrap();
            if let Some((ticket, TicketPurpose::Write, at, base)) = ticket_full(drain(&mut ann.rx))
            {
                let reply =
                    upload_on(&mut room, UserId(id(11)), ticket, at, base.as_deref(), 9).await;
                assert!(matches!(reply, LiveCheckpointReply::Saved { .. }));
                written += 1;
            }
        }
        assert_eq!(written, 20, "a lone editor keeps checkpointing");
        let loaded = crate::live_store::LiveStore::load_live(&store, doc)
            .await
            .unwrap();
        let holding = loaded
            .unverified_checkpoints
            .iter()
            .filter(|checkpoint| !checkpoint.session_data.is_empty())
            .count();
        assert_eq!(holding, records::MAX_UNVERIFIED_CHECKPOINTS);
        assert_eq!(
            store.session_data_bytes(doc),
            records::MAX_UNVERIFIED_CHECKPOINTS * b"identities".len()
        );
        assert!(room.unverified_checkpoints.len() <= records::MAX_UNVERIFIED_CHECKPOINTS);
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
        let bob = join_capable(&mut room, 6, 2, "editor", LiveBuild::Browser).await;
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
        let mut bob = join_capable(&mut room, 2, 2, "editor", LiveBuild::Browser).await;
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
    async fn silent_verifiers_are_not_asked_again_on_later_checkpoints() {
        let (mut room, store) = room().await;
        let mut ann = join_capable(&mut room, 1, 1, "editor", LiveBuild::Browser).await;
        let mut bob = join_capable(&mut room, 2, 2, "editor", LiveBuild::Browser).await;
        let mut cat = join_capable(&mut room, 3, 3, "editor", LiveBuild::Browser).await;
        let users = [UserId(id(11)), UserId(id(12)), UserId(id(13))];
        let mut verifies = [0; 3];
        let mut writes = Vec::new();
        let mut local = 0;
        for second in 0..=2_400_u64 {
            // Ann keeps changing the workbook: one change at the start and one per checkpoint.
            if second == 0 || writes.len() > usize::try_from(local).unwrap() - 1 {
                local += 1;
                let seen = room.head.0;
                entry(&mut room, &ann, local, seen, FORMAT, "x".into())
                    .await
                    .unwrap();
            }
            tokio::time::advance(Duration::from_secs(1)).await;
            // Everyone stays connected: Bob and Cat are silent verifiers, not gone ones.
            let seen = room.head.0;
            for joined in [&ann, &bob, &cat] {
                heartbeat(&mut room, joined, seen).await;
            }
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
        let mut bob = join_capable(&mut room, 2, 2, "editor", LiveBuild::Browser).await;
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
        let mut bob = join_capable(&mut room, 2, 2, "editor", LiveBuild::Browser).await;
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

        let cat = join_capable(&mut room, 3, 3, "editor", LiveBuild::Browser).await;
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
        let mut cat = join_capable(&mut room, 3, 3, "editor", LiveBuild::Browser).await;
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
    async fn restart_after_a_major_mark_keeps_checkpointing() {
        let doc = DocId(id(100));
        let store = MemStore::new();
        let mut room = LiveRoom::load(doc, store.clone(), EventSink::discard(), timing())
            .await
            .unwrap();
        let mut ann = join_capable(&mut room, 1, 1, "editor", LiveBuild::Browser).await;
        let mut bob = join_capable(&mut room, 2, 2, "editor", LiveBuild::Browser).await;
        entry(&mut room, &ann, 1, 0, FORMAT, "one".into())
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(15)).await;
        room.expire_due().await.unwrap();
        let mut asked = drain(&mut ann.rx);
        asked.extend(drain(&mut bob.rx));
        let (ticket, _, _, _, _) = ticket_major(asked).unwrap();
        let _ = upload_ticket(&mut room, &ann, ticket, Seq(1), None, 1).await;
        let resume = LiveResume {
            session: ann.session,
            participant: ann.participant,
        };
        expire_one(&mut room, &bob, 1).await;
        let returned = join_return(
            &mut room,
            3,
            1,
            UserId(id(11)),
            Some(resume),
            Some(Seq(1)),
            Some(LocalNo(2)),
        )
        .await;
        room.finish_writes().await.unwrap();
        let marked = crate::live_store::LiveStore::current_revision(&store, doc)
            .await
            .unwrap()
            .unwrap();
        // Unsaved work after the mark: were the mark taken for an outside file, the restart
        // would keep this work below it instead of carrying on.
        entry(&mut room, &returned, 2, 1, FORMAT, "late".into())
            .await
            .unwrap();
        drop(room);

        let mut room = LiveRoom::load(doc, store.clone(), EventSink::discard(), timing())
            .await
            .unwrap();
        let mut ann = join_capable(&mut room, 5, 1, "editor", LiveBuild::Browser).await;
        assert!(
            room.recovery.is_none(),
            "the room's own mark is no outside change"
        );
        entry(&mut room, &ann, 3, 2, FORMAT, "two".into())
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(15)).await;
        room.expire_due().await.unwrap();
        let mut asked = ann.after_join.clone();
        asked.extend(drain(&mut ann.rx));
        let (ticket, purpose, at, base, major) = ticket_major(asked).unwrap();
        assert_eq!(
            (purpose, at, base.as_deref(), major),
            (TicketPurpose::Write, Seq(3), Some(marked.as_str()), None),
            "the same bytes stored again are the base, not an outside change"
        );
        assert!(matches!(
            upload_on(&mut room, UserId(id(11)), ticket, Seq(3), Some(&marked), 7).await,
            LiveCheckpointReply::Saved { .. }
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn agreement_uses_entered_digest_only() {
        let (events, mut emitted) = EventSink::channel();
        let mut room = LiveRoom::load(DocId(id(100)), MemStore::new(), events, timing())
            .await
            .unwrap();
        let mut ann = join_capable(&mut room, 1, 1, "editor", LiveBuild::Browser).await;
        let mut bob = join_capable(&mut room, 2, 2, "editor", LiveBuild::Browser).await;
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
        let mut bob = join_capable(&mut room, 2, 2, "editor", LiveBuild::Browser).await;
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

    /// A member dropped because its send queue is full (`Members::send`) leaves the People list
    /// and loses its presence note at once, before its socket task reports the disconnect.
    #[tokio::test(start_paused = true)]
    async fn full_queue_drop_clears_presence_and_people() {
        let (mut room, _) = room().await;
        let mut ann = join_as(&mut room, 1, 1, 11, "editor", FORMAT, None, None).await;
        let bob = join_as(&mut room, 2, 2, 12, "editor", FORMAT, None, None).await;
        let mut cat = join_as(&mut room, 3, 3, 13, "editor", FORMAT, None, None).await;
        let note = |joined: &Joined, payload: &str| LiveCommand::Presence {
            conn: joined.conn,
            client: joined.client,
            payload: payload.to_owned(),
        };
        room.handle(note(&bob, "B2")).await.unwrap();
        // Bob never reads: Ann's notes fill his queue until he is dropped. Cat reads all along.
        let mut seen = drain(&mut cat.rx);
        for round in 0..40 {
            for n in 0..20 {
                room.handle(note(&ann, &format!("A{round}-{n}")))
                    .await
                    .unwrap();
            }
            tokio::time::advance(Duration::from_secs(2)).await;
            let _ = drain(&mut ann.rx);
            seen.extend(drain(&mut cat.rx));
        }
        assert!(
            !room
                .members
                .iter()
                .any(|member| member.client == bob.client),
            "bob's full queue dropped him"
        );
        seen.extend(drain(&mut cat.rx));
        assert!(
            seen.iter().any(|message| matches!(
                message,
                LiveServerMsg::Presence { participant, payload: None } if *participant == bob.participant
            )),
            "bob's note is cleared at once"
        );
        assert!(
            seen.iter().any(|message| matches!(
                message,
                LiveServerMsg::People { people } if people.iter().any(|person| {
                    person.participant == bob.participant && !person.connected
                })
            )),
            "bob shows as not connected at once"
        );
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
                returning_through: None,
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

    // ---- Task 11: stale-base recovery and returning clients -------------------------------

    /// A join stating `returning_through`; the frames sent with it, in order.
    #[allow(clippy::too_many_arguments)]
    async fn join_return(
        room: &mut LiveRoom<MemStore>,
        conn: u64,
        client_n: u128,
        user: UserId,
        resume: Option<LiveResume>,
        seen: Option<Seq>,
        returning_through: Option<LocalNo>,
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
            role: builtin_access("editor"),
            protocol: 1,
            format: FORMAT.to_owned(),
            resume,
            seen,
            can_checkpoint: true,
            build: LiveBuild::Browser,
            returning_through,
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
            raw: Vec::new(),
            after_join: Vec::new(),
        };
        let first = recv_messages(&mut joined.rx).await;
        if let Some(LiveServerMsg::LiveJoined {
            session,
            participant,
            place,
            ..
        }) = first.first()
        {
            joined.session = *session;
            joined.participant = *participant;
            joined.place = *place;
        } else {
            panic!("unexpected join frames: {first:?}");
        }
        joined.raw.clone_from(&first);
        joined.initial = without_resumed(first);
        joined.after_join = drain(&mut joined.rx);
        joined
    }

    /// The first checkpoint ticket in `messages`, with its base and major reason.
    type MajorTicketFrame = (
        Uuid,
        TicketPurpose,
        Seq,
        Option<String>,
        Option<MajorReason>,
    );

    fn ticket_major(messages: Vec<LiveServerMsg>) -> Option<MajorTicketFrame> {
        messages.into_iter().find_map(|message| match message {
            LiveServerMsg::CheckpointTicket {
                ticket,
                purpose,
                at,
                base,
                major,
            } => Some((ticket, purpose, at, base, major)),
            _ => None,
        })
    }

    fn catchups(messages: &[LiveServerMsg]) -> Vec<(Seq, Option<String>, Seq)> {
        messages
            .iter()
            .filter_map(|message| match message {
                LiveServerMsg::Catchup {
                    base,
                    revision,
                    from,
                    ..
                } => Some((*base, revision.clone(), *from)),
                _ => None,
            })
            .collect()
    }

    fn field<'a>(fields: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
        fields.get(key)
    }

    async fn heartbeat(room: &mut LiveRoom<MemStore>, joined: &Joined, seen: u64) {
        room.handle(LiveCommand::Heartbeat {
            conn: joined.conn,
            client: joined.client,
            seen: Seq(seen),
        })
        .await
        .unwrap();
    }

    /// Timing whose ordinary checkpoints never fall due, so only barrier tickets are seen.
    fn no_ordinary_checkpoints() -> LiveTiming {
        LiveTiming {
            checkpoint_quiet: Duration::from_secs(1_000_000),
            checkpoint_wait: Duration::from_secs(1_000_000),
            ..timing()
        }
    }

    /// Lets `gone` expire while `kept` keeps sending heartbeats.
    async fn expire_one(room: &mut LiveRoom<MemStore>, kept: &Joined, seen: u64) {
        for _ in 0..4 {
            tokio::time::advance(Duration::from_secs(60)).await;
            heartbeat(room, kept, seen).await;
            room.expire_due().await.unwrap();
            room.settle().await.unwrap();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn stale_base_recovery_keeps_both_versions_and_resumes_checkpoints() {
        let doc = DocId(id(100));
        let (events, mut emitted) = EventSink::channel();
        let store = MemStore::new();
        let mut room = LiveRoom::load(doc, store.clone(), events, timing())
            .await
            .unwrap();
        let mut ann = join_capable(&mut room, 1, 1, "editor", LiveBuild::Browser).await;
        let mut bob = join_capable(&mut room, 2, 2, "editor", LiveBuild::Browser).await;
        entry(&mut room, &ann, 1, 0, FORMAT, "one".into())
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(15)).await;
        room.expire_due().await.unwrap();
        let mut asked = drain(&mut ann.rx);
        asked.extend(drain(&mut bob.rx));
        let (ticket, _, at, _, _) = ticket_major(asked).unwrap();
        // Someone uploads a new version of the file meanwhile.
        let outside = store.write_outside(doc, [5; 32]);
        assert_eq!(
            upload(&mut room, UserId(id(11)), ticket, at, 9).await,
            LiveCheckpointReply::Stale {
                current: Some(outside.clone())
            }
        );

        // The room asks for its last state again, to keep it below the outside file.
        let mut asked = drain(&mut ann.rx);
        asked.extend(drain(&mut bob.rx));
        let (recovery, purpose, at, base, major) = ticket_major(asked).unwrap();
        assert_eq!(
            (purpose, at, base, major),
            (
                TicketPurpose::Write,
                Seq(1),
                None,
                Some(MajorReason::OutsideReplacement)
            )
        );
        // Changes wait meanwhile, so the state written is the room's last.
        entry(&mut room, &bob, 1, 1, FORMAT, "two".into())
            .await
            .unwrap();
        assert_eq!(refusal(&drain(&mut bob.rx)), Some(RefusedReason::Held));
        assert_eq!(store.entry_count(), 1);

        let holder = if ticket_holder_is(&room, recovery, 1) {
            UserId(id(11))
        } else {
            UserId(id(12))
        };
        let LiveCheckpointReply::Saved { revision: kept } =
            upload_on(&mut room, holder, recovery, Seq(1), None, 6).await
        else {
            panic!("the room's last state must be kept");
        };
        let revisions = store.revisions(doc);
        let current = revisions.last().unwrap().0.clone();
        assert_eq!(
            revisions[revisions.len() - 3..],
            [
                (outside.clone(), [5; 32]),
                (kept.clone(), [6; 32]),
                (current.clone(), [5; 32]),
            ],
            "live state below, the outside file current again on top"
        );
        let kept_fields = store.revision_fields(doc, &kept);
        assert_eq!(field(&kept_fields, "major"), Some(&json!(true)));
        assert_eq!(
            field(&kept_fields, "major_reason"),
            Some(&json!("outside_replacement"))
        );
        let current_fields = store.revision_fields(doc, &current);
        assert_eq!(field(&current_fields, "major"), Some(&json!(true)));
        assert_eq!(field(&current_fields, "restores"), Some(&json!(outside)));
        let loaded = crate::live_store::LiveStore::load_live(&store, doc)
            .await
            .unwrap();
        assert_eq!(loaded.verified_through, Seq(1));
        assert_eq!(
            loaded.verified_checkpoint.unwrap().revision,
            current,
            "the outside file is the new base"
        );

        // Everyone loads the outside file and changes resume.
        let to_ann = drain(&mut ann.rx);
        let to_bob = drain(&mut bob.rx);
        for frames in [&to_ann, &to_bob] {
            assert_eq!(catchups(frames), [(Seq(1), Some(current.clone()), Seq(1))]);
            assert!(frames.contains(&LiveServerMsg::ChangesResumed));
        }
        let kinds = emitted_kinds(&mut emitted, "live.major_version");
        assert_eq!(kinds.len(), 2, "{kinds:?}");

        // Checkpointing resumes on top of the outside file.
        entry(&mut room, &ann, 2, 1, FORMAT, "three".into())
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(15)).await;
        room.expire_due().await.unwrap();
        let mut asked = drain(&mut ann.rx);
        asked.extend(drain(&mut bob.rx));
        let (ticket, purpose, at, base, major) = ticket_major(asked).unwrap();
        assert_eq!(
            (purpose, at, base.clone(), major),
            (TicketPurpose::Write, Seq(2), Some(current.clone()), None)
        );
        let holder = if ticket_holder_is(&room, ticket, 1) {
            UserId(id(11))
        } else {
            UserId(id(12))
        };
        assert!(matches!(
            upload_on(&mut room, holder, ticket, Seq(2), Some(&current), 7).await,
            LiveCheckpointReply::Saved { .. }
        ));
    }

    fn ticket_holder_is(room: &LiveRoom<MemStore>, ticket: Uuid, participant: u32) -> bool {
        room.tickets
            .iter()
            .any(|held| held.id == ticket && held.participant == ParticipantNo(participant))
    }

    #[tokio::test(start_paused = true)]
    async fn restart_prefers_a_newer_outside_revision() {
        let doc = DocId(id(100));
        let (mut room, store) = room().await;
        let mut ann = join_capable(&mut room, 1, 1, "editor", LiveBuild::Browser).await;
        entry(&mut room, &ann, 1, 0, FORMAT, "one".into())
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(15)).await;
        room.expire_due().await.unwrap();
        let ticket = checkpoint_ticket(drain(&mut ann.rx)).unwrap().0;
        assert!(matches!(
            upload(&mut room, UserId(id(11)), ticket, Seq(1), 9).await,
            LiveCheckpointReply::Saved { .. }
        ));
        drop(room);
        let outside = store.write_outside(doc, [5; 32]);

        let mut room = LiveRoom::load(doc, store.clone(), EventSink::discard(), timing())
            .await
            .unwrap();
        let mut ann = join_capable(&mut room, 3, 1, "editor", LiveBuild::Browser).await;
        assert_eq!(
            catchups(&ann.initial),
            [(Seq(1), Some(outside.clone()), Seq(1))],
            "the newer outside file is the base, not the older live checkpoint"
        );
        entry(&mut room, &ann, 2, 1, FORMAT, "two".into())
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(15)).await;
        room.expire_due().await.unwrap();
        let (ticket, _, at, base, major) = ticket_major(drain(&mut ann.rx)).unwrap();
        assert_eq!(
            (at, base.clone(), major),
            (Seq(2), Some(outside.clone()), None)
        );
        assert!(matches!(
            upload_on(&mut room, UserId(id(11)), ticket, Seq(2), Some(&outside), 8).await,
            LiveCheckpointReply::Saved { .. }
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn restart_with_unsaved_work_keeps_it_below_the_outside_revision() {
        let doc = DocId(id(100));
        let (mut room, store) = room().await;
        let mut ann = join_capable(&mut room, 1, 1, "editor", LiveBuild::Browser).await;
        entry(&mut room, &ann, 1, 0, FORMAT, "one".into())
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(15)).await;
        room.expire_due().await.unwrap();
        let ticket = checkpoint_ticket(drain(&mut ann.rx)).unwrap().0;
        let LiveCheckpointReply::Saved { revision: live } =
            upload(&mut room, UserId(id(11)), ticket, Seq(1), 9).await
        else {
            panic!("stored");
        };
        entry(&mut room, &ann, 2, 1, FORMAT, "two".into())
            .await
            .unwrap();
        drop(room);
        let outside = store.write_outside(doc, [5; 32]);

        let mut room = LiveRoom::load(doc, store.clone(), EventSink::discard(), timing())
            .await
            .unwrap();
        let ann = join_capable(&mut room, 3, 1, "editor", LiveBuild::Browser).await;
        let (ticket, _, at, base, major) = ticket_major(ann.after_join.clone()).unwrap();
        assert_eq!(
            (at, base.clone(), major),
            (
                Seq(2),
                Some(live.clone()),
                Some(MajorReason::OutsideReplacement)
            ),
            "the kept entries are written once more, from the live base"
        );
        let LiveCheckpointReply::Saved { revision: kept } =
            upload_on(&mut room, UserId(id(11)), ticket, Seq(2), Some(&live), 6).await
        else {
            panic!("kept below the outside file");
        };
        let revisions = store.revisions(doc);
        let current = revisions.last().unwrap().0.clone();
        assert_eq!(
            revisions[revisions.len() - 3..],
            [
                (outside, [5; 32]),
                (kept, [6; 32]),
                (current.clone(), [5; 32])
            ]
        );
        let loaded = crate::live_store::LiveStore::load_live(&store, doc)
            .await
            .unwrap();
        assert_eq!(loaded.verified_through, Seq(2));
        assert_eq!(loaded.verified_checkpoint.unwrap().revision, current);
    }

    /// Ann (user 11) and Bob (user 12) edit; Ann's place expires with her local 2 and 3
    /// waiting; she returns stating local 3 as the end of her batch.
    async fn returned(events: EventSink, store: MemStore) -> (LiveRoom<MemStore>, Joined, Joined) {
        let mut room = LiveRoom::load(
            DocId(id(100)),
            store.clone(),
            events,
            no_ordinary_checkpoints(),
        )
        .await
        .unwrap();
        let ann = join_capable(&mut room, 1, 1, "editor", LiveBuild::Browser).await;
        let mut bob = join_capable(&mut room, 2, 2, "editor", LiveBuild::Browser).await;
        entry(&mut room, &ann, 1, 0, FORMAT, "ann one".into())
            .await
            .unwrap();
        entry(&mut room, &bob, 1, 1, FORMAT, "bob one".into())
            .await
            .unwrap();
        let resume = LiveResume {
            session: ann.session,
            participant: ann.participant,
        };
        expire_one(&mut room, &bob, 2).await;
        let _ = drain(&mut bob.rx);
        let ann = join_return(
            &mut room,
            3,
            1,
            UserId(id(11)),
            Some(resume),
            Some(Seq(1)),
            Some(LocalNo(3)),
        )
        .await;
        assert_eq!(
            ann.place,
            Place::Returning {
                previous: ParticipantNo(1)
            }
        );
        (room, ann, bob)
    }

    fn return_ready(messages: &[LiveServerMsg]) -> Option<LocalNo> {
        messages.iter().find_map(|message| match message {
            LiveServerMsg::ReturnReady { through } => Some(*through),
            _ => None,
        })
    }

    async fn upload_ticket(
        room: &mut LiveRoom<MemStore>,
        ann: &Joined,
        ticket: Uuid,
        at: Seq,
        base: Option<&str>,
        sha: u8,
    ) -> String {
        let holder = if ticket_holder_is(room, ticket, ann.participant.0) {
            UserId(id(11))
        } else {
            UserId(id(12))
        };
        match upload_on(room, holder, ticket, at, base, sha).await {
            LiveCheckpointReply::Saved { revision } => revision,
            other => panic!("not stored: {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn expired_return_stores_current_then_late_result_as_major() {
        let doc = DocId(id(100));
        let (events, mut emitted) = EventSink::channel();
        let store = MemStore::new();
        let (mut room, mut ann, mut bob) = returned(events, store.clone()).await;
        assert_eq!(
            return_ready(&ann.initial),
            None,
            "the batch waits for the version before it"
        );
        let mut asked = ann.after_join.clone();
        asked.extend(drain(&mut ann.rx));
        asked.extend(drain(&mut bob.rx));
        let (ticket, purpose, at, base, major) = ticket_major(asked).unwrap();
        assert_eq!(
            (purpose, at, base.clone(), major),
            (
                TicketPurpose::Write,
                Seq(2),
                None,
                Some(MajorReason::ReturningClientBefore)
            )
        );
        // Nobody's change slips in between the version and the batch.
        entry(&mut room, &bob, 2, 2, FORMAT, "bob two".into())
            .await
            .unwrap();
        assert_eq!(refusal(&drain(&mut bob.rx)), Some(RefusedReason::Held));
        let before = upload_ticket(&mut room, &ann, ticket, Seq(2), None, 2).await;
        let fields = store.revision_fields(doc, &before);
        assert_eq!(field(&fields, "major"), Some(&json!(true)));
        assert_eq!(
            field(&fields, "major_reason"),
            Some(&json!("returning_client_before"))
        );
        assert_eq!(
            field(&fields, "major_person"),
            Some(&json!(id(11).to_string()))
        );
        assert_eq!(return_ready(&drain(&mut ann.rx)), Some(LocalNo(3)));
        let _ = drain(&mut bob.rx);

        // The batch, then a version after it at once.
        for local in [2, 3] {
            entry(&mut room, &ann, local, 2, FORMAT, format!("late {local}"))
                .await
                .unwrap();
        }
        let mut asked = drain(&mut ann.rx);
        asked.extend(drain(&mut bob.rx));
        let (ticket, _, at, base, major) = ticket_major(asked).unwrap();
        assert_eq!(
            (at, base.as_deref(), major),
            (
                Seq(4),
                Some(before.as_str()),
                Some(MajorReason::ReturningClientAfter)
            )
        );
        let after = upload_ticket(&mut room, &ann, ticket, Seq(4), Some(&before), 4).await;
        let fields = store.revision_fields(doc, &after);
        assert_eq!(
            field(&fields, "major_reason"),
            Some(&json!("returning_client_after"))
        );
        assert!(
            store
                .last_authors()
                .iter()
                .any(|author| author.id == id(11).to_string()),
            "the returning person is credited"
        );
        let majors = emitted_kinds(&mut emitted, "live.major_version");
        assert_eq!(
            majors
                .iter()
                .map(|(_, detail)| detail["reason"].clone())
                .collect::<Vec<_>>(),
            [
                json!("returning_client_before"),
                json!("returning_client_after")
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn return_batch_ends_at_the_local_named_on_join() {
        let store = MemStore::new();
        let (mut room, mut ann, mut bob) = returned(EventSink::discard(), store.clone()).await;
        let mut asked = ann.after_join.clone();
        asked.extend(drain(&mut bob.rx));
        let (ticket, _, at, _, _) = ticket_major(asked).unwrap();
        let before = upload_ticket(&mut room, &ann, ticket, at, None, 2).await;
        let _ = (drain(&mut ann.rx), drain(&mut bob.rx));

        entry(&mut room, &ann, 2, 2, FORMAT, "late 2".into())
            .await
            .unwrap();
        let mut asked = drain(&mut ann.rx);
        asked.extend(drain(&mut bob.rx));
        assert!(
            ticket_major(asked).is_none(),
            "the batch has not ended at local 2"
        );
        // Bob's work between batch entries is ordinary.
        entry(&mut room, &bob, 2, 3, FORMAT, "bob two".into())
            .await
            .unwrap();
        entry(&mut room, &ann, 3, 4, FORMAT, "late 3".into())
            .await
            .unwrap();
        let mut asked = drain(&mut ann.rx);
        asked.extend(drain(&mut bob.rx));
        let (ticket, _, at, _, major) = ticket_major(asked).unwrap();
        assert_eq!(
            (at, major),
            (Seq(5), Some(MajorReason::ReturningClientAfter))
        );
        let _after = upload_ticket(&mut room, &ann, ticket, at, Some(&before), 5).await;

        // Later work is not part of the return.
        entry(&mut room, &ann, 4, 5, FORMAT, "later".into())
            .await
            .unwrap();
        let mut asked = drain(&mut ann.rx);
        asked.extend(drain(&mut bob.rx));
        assert!(ticket_major(asked).is_none());
        assert!(room.returns.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn returning_changes_keep_other_peoples_work() {
        let store = MemStore::new();
        let (mut room, ann, mut bob) = returned(EventSink::discard(), store.clone()).await;
        let mut asked = ann.after_join.clone();
        asked.extend(drain(&mut bob.rx));
        let (ticket, _, at, _, _) = ticket_major(asked).unwrap();
        let _ = upload_ticket(&mut room, &ann, ticket, at, None, 2).await;
        let credited = store
            .last_authors()
            .into_iter()
            .map(|author| author.id)
            .collect::<Vec<_>>();
        assert_eq!(
            credited,
            [id(11).to_string(), id(12).to_string()],
            "the version before the return holds everyone's work"
        );
        for local in [2, 3] {
            entry(&mut room, &ann, local, 2, FORMAT, format!("late {local}"))
                .await
                .unwrap();
        }
        let kept = room
            .entries
            .iter()
            .filter(|entry| {
                matches!(
                    entry.body,
                    EntryBody::Change {
                        cancelled: false,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(
            kept, 4,
            "late changes go on top; nothing of Bob's is undone"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn return_onto_a_stored_head_marks_it_major_without_new_content() {
        let doc = DocId(id(100));
        let store = MemStore::new();
        let mut room = LiveRoom::load(doc, store.clone(), EventSink::discard(), timing())
            .await
            .unwrap();
        let mut ann = join_capable(&mut room, 1, 1, "editor", LiveBuild::Browser).await;
        let mut bob = join_capable(&mut room, 2, 2, "editor", LiveBuild::Browser).await;
        entry(&mut room, &ann, 1, 0, FORMAT, "one".into())
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(15)).await;
        room.expire_due().await.unwrap();
        let mut asked = drain(&mut ann.rx);
        asked.extend(drain(&mut bob.rx));
        let (ticket, _, _, _, _) = ticket_major(asked).unwrap();
        let stored = upload_ticket(&mut room, &ann, ticket, Seq(1), None, 1).await;
        let resume = LiveResume {
            session: ann.session,
            participant: ann.participant,
        };
        expire_one(&mut room, &bob, 1).await;
        let ann = join_return(
            &mut room,
            3,
            1,
            UserId(id(11)),
            Some(resume),
            Some(Seq(1)),
            Some(LocalNo(2)),
        )
        .await;
        let mut ann = ann;
        // The mark runs off the room task; changes wait for it.
        assert!(room.writing());
        room.finish_writes().await.unwrap();
        let mut frames = ann.initial.clone();
        frames.extend(ann.after_join.clone());
        frames.extend(drain(&mut ann.rx));
        assert_eq!(return_ready(&frames), Some(LocalNo(2)));
        assert!(
            frames.iter().all(|message| !matches!(
                message,
                LiveServerMsg::CheckpointTicket {
                    purpose: TicketPurpose::Write,
                    ..
                }
            )),
            "nothing new to write"
        );
        let (marked, sha) = store.revisions(doc).last().unwrap().clone();
        assert_eq!(sha, [1; 32], "the same bytes");
        let fields = store.revision_fields(doc, &marked);
        assert_eq!(
            field(&fields, "major_reason"),
            Some(&json!("returning_client_before"))
        );
        assert_eq!(field(&fields, "restores"), Some(&json!(stored)));
    }

    #[tokio::test(start_paused = true)]
    async fn return_after_thirty_days_archives_old_copy_below_current() {
        let doc = DocId(id(100));
        let store = MemStore::new();
        let file = store.write_outside(doc, [5; 32]);
        let mut room = LiveRoom::load(doc, store.clone(), EventSink::discard(), timing())
            .await
            .unwrap();
        let ann = join_capable(&mut room, 1, 1, "editor", LiveBuild::Browser).await;
        let resume = LiveResume {
            session: ann.session,
            participant: ann.participant,
        };
        room.handle(LiveCommand::Leave {
            conn: ann.conn,
            client: ann.client,
        })
        .await
        .unwrap();
        room.settle().await.unwrap();
        drop(room);
        // Thirty days on, only the place's tombstone is left.
        store.forget_ended_places(doc);

        let mut room = LiveRoom::load(doc, store.clone(), EventSink::discard(), timing())
            .await
            .unwrap();
        let mut ann = join_return(
            &mut room,
            2,
            1,
            UserId(id(11)),
            Some(resume),
            Some(Seq(0)),
            Some(LocalNo(2)),
        )
        .await;
        assert!(matches!(
            ann.initial.as_slice(),
            [
                LiveServerMsg::LiveJoined {
                    place: Place::Returning { .. },
                    ..
                },
                LiveServerMsg::ArchiveReturn { current: Some(_) },
                LiveServerMsg::Catchup { .. },
                ..
            ]
        ));
        let mut asked = ann.after_join.clone();
        asked.extend(drain(&mut ann.rx));
        let (ticket, _, at, base, major) = ticket_major(asked).unwrap();
        assert_eq!(
            (base.clone(), major),
            (
                Some(file.clone()),
                Some(MajorReason::ReturningClientArchive)
            )
        );
        let kept = upload_ticket(&mut room, &ann, ticket, at, Some(&file), 6).await;
        let revisions = store.revisions(doc);
        let current = revisions.last().unwrap().0.clone();
        assert_eq!(
            revisions,
            [
                (file.clone(), [5; 32]),
                (kept.clone(), [6; 32]),
                (current.clone(), [5; 32])
            ],
            "the old copy below, the unchanged current file on top"
        );
        assert_eq!(
            field(&store.revision_fields(doc, &kept), "major_reason"),
            Some(&json!("returning_client_archive"))
        );
        assert_eq!(
            field(&store.revision_fields(doc, &current), "restores"),
            Some(&json!(file))
        );
        let loaded = crate::live_store::LiveStore::load_live(&store, doc)
            .await
            .unwrap();
        assert!(
            loaded.unverified_checkpoints.is_empty() && loaded.verified_checkpoint.is_none(),
            "the old copy never becomes a catch-up base"
        );
    }

    // ---- Task 10 review fixes ---------------------------------------------------------------

    /// All tickets in `messages`.
    fn tickets_in(messages: Vec<LiveServerMsg>) -> Vec<MajorTicketFrame> {
        messages
            .into_iter()
            .filter_map(|message| match message {
                LiveServerMsg::CheckpointTicket {
                    ticket,
                    purpose,
                    at,
                    base,
                    major,
                } => Some((ticket, purpose, at, base, major)),
                _ => None,
            })
            .collect()
    }

    fn resumed(messages: &[LiveServerMsg]) -> bool {
        messages.contains(&LiveServerMsg::ChangesResumed)
    }

    #[tokio::test(start_paused = true)]
    async fn disputed_pause_is_left_after_a_restart() {
        let doc = DocId(id(100));
        let (mut room, store, _ann, mut bob, ticket) = paused_room(EventSink::discard()).await;
        assert!(matches!(
            upload(&mut room, UserId(id(11)), ticket, Seq(3), 9).await,
            LiveCheckpointReply::Saved { .. }
        ));
        let (verify, _, _) = checkpoint_ticket(drain(&mut bob.rx)).unwrap();
        assert_eq!(
            hash(&mut room, UserId(id(12)), verify, Seq(3), 8).await,
            LiveCheckpointReply::Accepted
        );
        drop(room);

        let mut room = LiveRoom::load(
            doc,
            store.clone(),
            EventSink::discard(),
            limited(3, 3, u64::MAX / 2, u64::MAX / 2),
        )
        .await
        .unwrap();
        assert!(room.paused);
        let mut ann = join_capable(&mut room, 5, 1, "editor", LiveBuild::Browser).await;
        let mut written = false;
        for _ in 0..40 {
            let mut asked = std::mem::take(&mut ann.after_join);
            asked.extend(drain(&mut ann.rx));
            for (ticket, purpose, at, base, _) in tickets_in(asked) {
                if purpose == TicketPurpose::Write {
                    assert!(matches!(
                        upload_on(&mut room, UserId(id(11)), ticket, at, base.as_deref(), 7).await,
                        LiveCheckpointReply::Saved { .. }
                    ));
                    written = true;
                }
            }
            if !room.paused {
                break;
            }
            tokio::time::advance(Duration::from_secs(15)).await;
            room.expire_due().await.unwrap();
        }
        assert!(written, "the freed position is written again");
        assert!(!room.paused, "changes resume after the restart");
    }

    #[tokio::test(start_paused = true)]
    async fn lone_editor_at_the_limit_resumes_after_its_own_checkpoint() {
        let (events, mut emitted) = EventSink::channel();
        let mut room = LiveRoom::load(
            DocId(id(100)),
            MemStore::new(),
            events,
            limited(3, 3, u64::MAX / 2, u64::MAX / 2),
        )
        .await
        .unwrap();
        let mut ann = join_capable(&mut room, 1, 1, "editor", LiveBuild::Browser).await;
        let mut ticket = None;
        for local in 1..=3 {
            entry(&mut room, &ann, local, local - 1, FORMAT, "x".into())
                .await
                .unwrap();
            if let Some((id, _, _)) = checkpoint_ticket(drain(&mut ann.rx)) {
                ticket = Some(id);
            }
        }
        assert!(room.paused);
        assert!(matches!(
            upload(&mut room, UserId(id(11)), ticket.unwrap(), Seq(3), 9).await,
            LiveCheckpointReply::Saved { .. }
        ));
        assert!(!room.paused);
        assert!(resumed(&drain(&mut ann.rx)));
        assert_eq!(
            emitted_kinds(&mut emitted, "live.checkpoint_sole_editor"),
            [(
                "live.checkpoint_sole_editor",
                json!({ "revision": "0000000000000000000000000000000000000001", "through": 3 })
            )]
        );
        entry(&mut room, &ann, 4, 3, FORMAT, "x".into())
            .await
            .unwrap();
        assert_eq!(entries(drain(&mut ann.rx)).len(), 1);

        // A second editor who joins later verifies it.
        let bob = join_capable(&mut room, 2, 2, "editor", LiveBuild::Browser).await;
        let (verify, purpose, at) = checkpoint_ticket(bob.after_join.clone()).unwrap();
        assert_eq!((purpose, at), (TicketPurpose::Verify, Seq(3)));
        assert_eq!(
            hash(&mut room, UserId(id(12)), verify, Seq(3), 9).await,
            LiveCheckpointReply::Accepted
        );
        let loaded = crate::live_store::LiveStore::load_live(&room.store, DocId(id(100)))
            .await
            .unwrap();
        assert!(!loaded.verified_checkpoint.unwrap().sole_editor);
    }

    #[tokio::test(start_paused = true)]
    async fn warning_asks_for_one_checkpoint_not_one_per_change() {
        let (mut room, _store) = limited_room(limited(2, 1_000, u64::MAX / 2, u64::MAX / 2)).await;
        let mut ann = join_capable(&mut room, 1, 1, "editor", LiveBuild::Browser).await;
        let mut writes = 0;
        for local in 1..=12_u64 {
            entry(&mut room, &ann, local, local - 1, FORMAT, "x".into())
                .await
                .unwrap();
            tokio::time::advance(Duration::from_secs(1)).await;
            room.expire_due().await.unwrap();
            for (ticket, purpose, at, base, _) in tickets_in(drain(&mut ann.rx)) {
                if purpose == TicketPurpose::Write {
                    writes += 1;
                    let _ =
                        upload_on(&mut room, UserId(id(11)), ticket, at, base.as_deref(), 9).await;
                }
            }
        }
        assert_eq!(writes, 1, "one urgent checkpoint, then the quiet period");
    }

    #[tokio::test(start_paused = true)]
    async fn over_limit_join_gets_a_recoverable_fault() {
        let doc = DocId(id(100));
        let store = MemStore::new();
        {
            let mut room = LiveRoom::load(doc, store.clone(), EventSink::discard(), timing())
                .await
                .unwrap();
            let ann = join_capable(&mut room, 1, 1, "editor", LiveBuild::Browser).await;
            for local in 1..=5 {
                entry(&mut room, &ann, local, local - 1, FORMAT, "x".into())
                    .await
                    .unwrap();
            }
        }
        let (events, mut emitted) = EventSink::channel();
        let mut room = LiveRoom::load(
            doc,
            store.clone(),
            events,
            limited(1, 2, u64::MAX / 2, u64::MAX / 2),
        )
        .await
        .unwrap();
        assert!(room.entries.is_empty(), "the log is counted, not loaded");
        for conn in [7, 8] {
            let (tx, mut rx) = mpsc::channel(8);
            let (accepted, answer) = oneshot::channel();
            room.handle(LiveCommand::Join {
                conn,
                client: ClientId(id(1)),
                user: UserId(id(11)),
                session: SessionId(id(1011)),
                name: "Ann".into(),
                role: builtin_access("editor"),
                protocol: 1,
                format: FORMAT.to_owned(),
                resume: None,
                seen: None,
                can_checkpoint: false,
                build: LiveBuild::Browser,
                returning_through: None,
                tx,
                accepted,
            })
            .await
            .unwrap();
            assert_eq!(answer.await.unwrap(), JoinReply::Refused);
            assert_eq!(
                recv_messages(&mut rx).await,
                [LiveServerMsg::Unavailable {
                    reason: protocol::live::UnavailableReason::LogOverLimit
                }]
            );
        }
        assert_eq!(
            emitted_kinds(&mut emitted, "live.log_over_limit").len(),
            1,
            "one event per room, not per attempt"
        );
        assert!(
            room.lingers(),
            "the refusing room stays to answer reconnects"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn reconcile_gives_up_after_bounded_tries() {
        let doc = DocId(id(100));
        let store = MemStore::new();
        let (events, mut emitted) = EventSink::channel();
        let mut room = LiveRoom::load(doc, store.clone(), events, timing())
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
        entry(&mut room, &ann, 2, 1, FORMAT, "two".into())
            .await
            .unwrap();
        let mut write = None;
        for _ in 0..(24 * 60) {
            tokio::time::advance(Duration::from_secs(60)).await;
            heartbeat(&mut room, &ann, 2).await;
            room.expire_due().await.unwrap();
            if let Some((_, TicketPurpose::Write, at, base, _)) = ticket_major(drain(&mut ann.rx)) {
                write = Some((at, base));
                break;
            }
        }
        // One more description checks the revision before it becomes the base.
        assert_eq!(store.describe_calls(), super::RECONCILE_TRIES as usize + 1);
        assert_eq!(
            write,
            Some((
                Seq(2),
                Some("0000000000000000000000000000000000000001".to_owned())
            )),
            "checkpointing continues on top of what Git holds"
        );
        assert_eq!(
            emitted_kinds(&mut emitted, "live.checkpoint_reconcile_failed").len(),
            1
        );
    }

    #[tokio::test(start_paused = true)]
    async fn capped_checkpoint_cancels_its_verify_ticket() {
        let (mut room, _store, _ann, mut bob, revision, verify) =
            written_and_ticketed(EventSink::discard(), 9).await;
        let _ = drain(&mut bob.rx);
        for n in 2..=(records::MAX_UNVERIFIED_CHECKPOINTS as u64 + 1) {
            room.head = Seq(n);
            room.checkpoint_stored(
                format!("{n:040x}"),
                Seq(n),
                ([9; 32], [0; 32]),
                Uuid::nil(),
                UserId(id(11)),
                (ParticipantNo(1), ClientId(id(1))),
                None,
            );
        }
        assert!(
            room.unverified_checkpoints
                .iter()
                .all(|checkpoint| checkpoint.revision != revision),
            "the oldest is capped away"
        );
        assert!(
            room.tickets.iter().all(|held| held.id != verify),
            "and its verify ticket with it"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn paused_refusals_are_rate_limited() {
        let (mut room, store, mut ann, _bob, _ticket) = paused_room(EventSink::discard()).await;
        let _ = drain(&mut ann.rx);
        let before = store.refused_locals();
        let mut reasons = Vec::new();
        // A client that keeps sending its held change is still paced.
        for _ in 0..(u64::from(super::RATE_BURST) + 20) {
            entry(&mut room, &ann, 4, 3, FORMAT, "x".into())
                .await
                .unwrap();
            reasons.extend(refusal(&drain(&mut ann.rx)));
        }
        assert!(reasons.contains(&RefusedReason::RateLimited), "{reasons:?}");
        assert!(
            store.refused_locals() - before <= super::RATE_BURST as usize + 1,
            "paused refusals write no more rows than the rate allows"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_records_count_their_encoded_size_in_the_room() {
        let (mut room, _store) = room().await;
        room.entries.push(protocol::live::LiveEntry {
            seq: Seq(1),
            at_ms: 0,
            seed: 0,
            body: EntryBody::Cancel {
                cancels: Seq(0),
                reports: 1,
            },
        });
        room.recount();
        assert_eq!(room.unsaved_bytes, records::LIVE_CANCEL_BYTES);
    }

    // ---- Task 11 review fixes ----------------------------------------------------------------

    /// Ann and Bob edit; Ann's checkpoint at 1 is verified by Bob; then Ann's change 2 is
    /// checkpointed by Bob (written `sha`) and Ann holds the verify ticket.
    async fn verified_then_written(
        events: EventSink,
        sha: u8,
    ) -> (
        LiveRoom<MemStore>,
        MemStore,
        Joined,
        Joined,
        String,
        String,
        Uuid,
    ) {
        let (mut room, store, mut ann, mut bob, first, verify) =
            written_and_ticketed(events, 9).await;
        assert_eq!(
            hash(&mut room, UserId(id(12)), verify, Seq(1), 9).await,
            LiveCheckpointReply::Accepted
        );
        assert_eq!(room.verified_through, Seq(1));
        entry(&mut room, &ann, 2, 1, FORMAT, "two".into())
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(15)).await;
        room.expire_due().await.unwrap();
        let mut asked = drain(&mut ann.rx);
        asked.extend(drain(&mut bob.rx));
        let (write, _, at, base, _) = tickets_in(asked)
            .into_iter()
            .find(|(_, purpose, _, _, _)| *purpose == TicketPurpose::Write)
            .unwrap();
        assert!(ticket_holder_is(&room, write, bob.participant.0));
        let LiveCheckpointReply::Saved { revision: second } =
            upload_on(&mut room, UserId(id(12)), write, at, base.as_deref(), sha).await
        else {
            panic!("stored");
        };
        let (verify, purpose, at) = checkpoint_ticket(drain(&mut ann.rx)).unwrap();
        assert_eq!((purpose, at), (TicketPurpose::Verify, Seq(2)));
        let _ = drain(&mut bob.rx);
        (room, store, ann, bob, first, second, verify)
    }

    fn unavailable(messages: &[LiveServerMsg]) -> bool {
        messages.contains(&LiveServerMsg::Unavailable {
            reason: protocol::live::UnavailableReason::CheckpointDispute,
        })
    }

    #[tokio::test(start_paused = true)]
    async fn second_hash_mismatch_keeps_both_files_and_log() {
        let doc = DocId(id(100));
        let (events, mut emitted) = EventSink::channel();
        let (mut room, store, mut ann, mut bob, first, second, verify) =
            verified_then_written(events, 5).await;
        let _ = emitted_kinds(&mut emitted, "");

        // Ann's own bytes disagree with Bob's checkpoint.
        assert_eq!(
            hash(&mut room, UserId(id(11)), verify, Seq(2), 6).await,
            LiveCheckpointReply::Accepted
        );
        let (copy_ticket, purpose, at, base, major) = ticket_major(drain(&mut ann.rx)).unwrap();
        assert_eq!(
            (purpose, at, base.as_deref(), major),
            (
                TicketPurpose::Write,
                Seq(2),
                Some(second.as_str()),
                Some(MajorReason::DisputedCopy)
            )
        );
        // Changes wait while the dispute is settled.
        entry(&mut room, &bob, 1, 2, FORMAT, "bob".into())
            .await
            .unwrap();
        assert_eq!(refusal(&drain(&mut bob.rx)), Some(RefusedReason::Held));

        let LiveCheckpointReply::Saved { revision: kept } = upload_on(
            &mut room,
            UserId(id(11)),
            copy_ticket,
            Seq(2),
            Some(&second),
            6,
        )
        .await
        else {
            panic!("the verifier's copy is kept");
        };
        room.settle().await.unwrap();
        let revisions = store.revisions(doc);
        let tail = revisions[revisions.len() - 3..].to_vec();
        assert_eq!(tail[0], (kept.clone(), [6; 32]), "the verifier's copy");
        assert_eq!(tail[1].1, [5; 32], "the writer's copy, again");
        assert_eq!(tail[2].1, [9; 32], "the last verified base is current");
        for (revision, _) in &tail[..2] {
            assert_eq!(
                store.revision_fields(doc, revision).get("major_reason"),
                Some(&json!("disputed_copy"))
            );
        }
        let loaded = crate::live_store::LiveStore::load_live(&store, doc)
            .await
            .unwrap();
        assert_eq!(
            loaded.verified_through,
            Seq(1),
            "neither side becomes the base"
        );
        assert_eq!(loaded.verified_checkpoint.unwrap().revision, first);
        assert!(loaded.unverified_checkpoints.is_empty());
        assert_eq!(loaded.entries.len(), 1, "the log is kept");
        let stopped = emitted_kinds(&mut emitted, "live.checkpoint_dispute_stopped");
        assert_eq!(stopped.len(), 1, "{stopped:?}");
        // The live session ends: everyone is told and joins again.
        assert!(unavailable(&drain(&mut ann.rx)));
        assert!(unavailable(&drain(&mut bob.rx)));
        assert!(!room.has_session());
    }

    #[tokio::test(start_paused = true)]
    async fn hostile_verifier_cannot_promote_its_own_bytes() {
        let doc = DocId(id(100));
        let (events, mut emitted) = EventSink::channel();
        let (mut room, store, mut ann, mut bob, ticket) = paused_room(events).await;
        let _ = emitted_kinds(&mut emitted, "");
        // Ann, honest, writes the urgent checkpoint at the limit.
        assert!(matches!(
            upload(&mut room, UserId(id(11)), ticket, Seq(3), 9).await,
            LiveCheckpointReply::Saved { .. }
        ));
        let (verify, _, _) = checkpoint_ticket(drain(&mut bob.rx)).unwrap();
        // Bob, hostile, disagrees and uploads bytes of his own.
        assert_eq!(
            hash(&mut room, UserId(id(12)), verify, Seq(3), 8).await,
            LiveCheckpointReply::Accepted
        );
        let (copy_ticket, _, at, base, major) = ticket_major(drain(&mut bob.rx)).unwrap();
        assert_eq!(major, Some(MajorReason::DisputedCopy));
        let _ = upload_on(
            &mut room,
            UserId(id(12)),
            copy_ticket,
            at,
            base.as_deref(),
            8,
        )
        .await;
        room.settle().await.unwrap();
        assert!(
            emitted_kinds(&mut emitted, "live.checkpoint_sole_editor").is_empty(),
            "the disputer's bytes are never accepted"
        );
        let loaded = crate::live_store::LiveStore::load_live(&store, doc)
            .await
            .unwrap();
        assert_eq!(loaded.verified_through, Seq(0));
        assert!(loaded.unverified_checkpoints.is_empty());
        let current = crate::live_store::LiveStore::current_revision(&store, doc)
            .await
            .unwrap()
            .unwrap();
        let current_sha = store
            .revisions(doc)
            .into_iter()
            .find(|(revision, _)| *revision == current)
            .unwrap()
            .1;
        assert_ne!(current_sha, [8; 32], "Bob's bytes are not the current file");
        assert!(unavailable(&drain(&mut ann.rx)));
        assert!(!room.has_session());
    }

    #[tokio::test(start_paused = true)]
    async fn sole_editor_needs_nobody_else_present() {
        let (events, mut emitted) = EventSink::channel();
        let (mut room, _store, ann, mut bob, ticket) = paused_room(events).await;
        assert!(matches!(
            upload(&mut room, UserId(id(11)), ticket, Seq(3), 9).await,
            LiveCheckpointReply::Saved { .. }
        ));
        assert!(checkpoint_ticket(drain(&mut bob.rx)).is_some());
        // Bob stays connected but never answers: his verify ticket lapses.
        for _ in 0..10 {
            tokio::time::advance(Duration::from_secs(30)).await;
            heartbeat(&mut room, &ann, 3).await;
            heartbeat(&mut room, &bob, 3).await;
            room.expire_due().await.unwrap();
        }
        assert!(room.paused, "a present peer is never bypassed");
        assert!(emitted_kinds(&mut emitted, "live.checkpoint_sole_editor").is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn return_with_its_batch_already_stored_is_ready_at_once() {
        let mut room = LiveRoom::load(
            DocId(id(100)),
            MemStore::new(),
            EventSink::discard(),
            no_ordinary_checkpoints(),
        )
        .await
        .unwrap();
        let ann = join_capable(&mut room, 1, 1, "editor", LiveBuild::Browser).await;
        let bob = join_capable(&mut room, 2, 2, "editor", LiveBuild::Browser).await;
        for local in 1..=2 {
            entry(&mut room, &ann, local, local - 1, FORMAT, "x".into())
                .await
                .unwrap();
        }
        let resume = LiveResume {
            session: ann.session,
            participant: ann.participant,
        };
        expire_one(&mut room, &bob, 2).await;
        // Both locals were stored before the drop; the client did not hear so.
        let ann = join_return(
            &mut room,
            3,
            1,
            UserId(id(11)),
            Some(resume),
            Some(Seq(0)),
            Some(LocalNo(2)),
        )
        .await;
        assert_eq!(return_ready(&ann.initial), Some(LocalNo(2)));
        assert!(room.returns.is_empty(), "nothing waits behind it");
    }

    #[tokio::test(start_paused = true)]
    async fn returns_are_marked_at_most_once_per_person_in_ten_minutes() {
        let store = MemStore::new();
        let (mut room, ann, mut bob) = returned(EventSink::discard(), store.clone()).await;
        let mut asked = ann.after_join.clone();
        asked.extend(drain(&mut bob.rx));
        let (ticket, _, at, _, _) = ticket_major(asked).unwrap();
        let _ = upload_ticket(&mut room, &ann, ticket, at, None, 2).await;
        let resume = LiveResume {
            session: ann.session,
            participant: ann.participant,
        };
        // Ann drops and returns again at once with more waiting work.
        let seen = room.head.0;
        expire_one(&mut room, &bob, seen).await;
        room.finish_writes().await.unwrap();
        let _ = drain(&mut bob.rx);
        let again = join_return(
            &mut room,
            9,
            1,
            UserId(id(11)),
            Some(resume),
            Some(Seq(2)),
            Some(LocalNo(5)),
        )
        .await;
        assert_eq!(return_ready(&again.initial), Some(LocalNo(5)));
        assert!(room.returns.is_empty());
        assert!(
            ticket_major(drain(&mut bob.rx)).is_none(),
            "no new major version"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn joiners_during_a_hold_are_told_changes_wait() {
        let (mut room, _ann, _bob) = returned(EventSink::discard(), MemStore::new()).await;
        assert!(room.holding());
        let cat = join_capable(&mut room, 7, 3, "editor", LiveBuild::Browser).await;
        assert!(paused(&cat.initial).is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn held_changes_are_not_used_up_and_go_again_after_the_hold() {
        let doc = DocId(id(100));
        let store = MemStore::new();
        let (mut room, ann, mut bob) = returned(EventSink::discard(), store.clone()).await;
        let mut asked = ann.after_join.clone();
        asked.extend(drain(&mut bob.rx));
        let (ticket, _, at, _, _) = ticket_major(asked).unwrap();
        let before = store.refused_locals();
        entry(&mut room, &bob, 2, 2, FORMAT, "held".into())
            .await
            .unwrap();
        assert_eq!(refusal(&drain(&mut bob.rx)), Some(RefusedReason::Held));
        // Sent before Bob heard: dropped quietly, to keep the order.
        entry(&mut room, &bob, 3, 2, FORMAT, "after".into())
            .await
            .unwrap();
        assert!(refusal(&drain(&mut bob.rx)).is_none());
        assert_eq!(store.refused_locals(), before, "no number is used up");
        let _ = upload_ticket(&mut room, &ann, ticket, at, None, 2).await;
        assert!(resumed(&drain(&mut bob.rx)));
        // Bob sends both again, in order; they are stored.
        for (local, text) in [(2, "held"), (3, "after")] {
            entry(&mut room, &bob, local, 2, FORMAT, text.into())
                .await
                .unwrap();
        }
        assert_eq!(entries(drain(&mut bob.rx)).len(), 2);
        let _ = doc;
    }

    /// A stale write put the room into recovery; returns the room, the people, the outside
    /// revision and the recovery ticket with its holder.
    async fn recovering(
        store: MemStore,
    ) -> (
        LiveRoom<MemStore>,
        Joined,
        Joined,
        String,
        (Uuid, String),
        UserId,
    ) {
        let doc = DocId(id(100));
        let mut room = LiveRoom::load(doc, store.clone(), EventSink::discard(), timing())
            .await
            .unwrap();
        let mut ann = join_capable(&mut room, 1, 1, "editor", LiveBuild::Browser).await;
        let mut bob = join_capable(&mut room, 2, 2, "editor", LiveBuild::Browser).await;
        entry(&mut room, &ann, 1, 0, FORMAT, "one".into())
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(15)).await;
        room.expire_due().await.unwrap();
        let mut asked = drain(&mut ann.rx);
        asked.extend(drain(&mut bob.rx));
        let (ticket, _, _, _, _) = ticket_major(asked).unwrap();
        let first = upload_ticket(&mut room, &ann, ticket, Seq(1), None, 3).await;
        let _ = drain(&mut ann.rx);
        let (verify, _, _) = checkpoint_ticket(drain(&mut bob.rx)).unwrap();
        assert_eq!(
            hash(&mut room, UserId(id(12)), verify, Seq(1), 3).await,
            LiveCheckpointReply::Accepted
        );
        entry(&mut room, &ann, 2, 1, FORMAT, "two".into())
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(15)).await;
        room.expire_due().await.unwrap();
        let mut asked = drain(&mut ann.rx);
        asked.extend(drain(&mut bob.rx));
        let (ticket, _, at, _, _) = tickets_in(asked)
            .into_iter()
            .find(|(_, purpose, _, _, _)| *purpose == TicketPurpose::Write)
            .unwrap();
        let outside = store.write_outside(doc, [5; 32]);
        let holder = if ticket_holder_is(&room, ticket, 1) {
            UserId(id(11))
        } else {
            UserId(id(12))
        };
        assert!(matches!(
            upload_on(&mut room, holder, ticket, at, Some(&first), 9).await,
            LiveCheckpointReply::Stale { .. }
        ));
        let mut asked = drain(&mut ann.rx);
        asked.extend(drain(&mut bob.rx));
        let (recovery, _, _, _, _) = ticket_major(asked).unwrap();
        let holder = if ticket_holder_is(&room, recovery, 1) {
            UserId(id(11))
        } else {
            UserId(id(12))
        };
        (room, ann, bob, outside, (recovery, first), holder)
    }

    #[tokio::test(start_paused = true)]
    async fn restart_after_a_half_done_outside_write_keeps_the_outside_file() {
        let doc = DocId(id(100));
        let store = MemStore::new();
        let (mut room, _ann, _bob, outside, (recovery, first), holder) =
            recovering(store.clone()).await;
        store.fail_major_restore();
        assert_eq!(
            upload_on(&mut room, holder, recovery, Seq(2), Some(&first), 6).await,
            LiveCheckpointReply::Failed
        );
        let kept = crate::live_store::LiveStore::current_revision(&store, doc)
            .await
            .unwrap()
            .unwrap();
        drop(room);

        let mut room = LiveRoom::load(doc, store.clone(), EventSink::discard(), timing())
            .await
            .unwrap();
        let recovery = room.recovery.clone().expect("still recovering");
        assert_eq!((recovery.head, recovery.outside), (kept, outside.clone()));
        let ann = join_capable(&mut room, 5, 1, "editor", LiveBuild::Browser).await;
        let (ticket, _, at, base, major) = ticket_major(ann.after_join.clone()).unwrap();
        assert_eq!(major, Some(MajorReason::OutsideReplacement));
        assert!(matches!(
            upload_on(&mut room, UserId(id(11)), ticket, at, base.as_deref(), 7).await,
            LiveCheckpointReply::Saved { .. }
        ));
        let (current, sha) = store.revisions(doc).last().unwrap().clone();
        assert_eq!(
            sha, [5; 32],
            "the outside file is current again, not the room's state"
        );
        assert_eq!(
            store.revision_fields(doc, &current).get("restores"),
            Some(&json!(outside))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn restart_after_a_mark_on_an_outside_base_is_not_an_outside_change() {
        let doc = DocId(id(100));
        let store = MemStore::new();
        let (mut room, ann, _bob, _outside, (recovery, first), holder) =
            recovering(store.clone()).await;
        assert!(matches!(
            upload_on(&mut room, holder, recovery, Seq(2), Some(&first), 6).await,
            LiveCheckpointReply::Saved { .. }
        ));
        let adopted = crate::live_store::LiveStore::current_revision(&store, doc)
            .await
            .unwrap()
            .unwrap();
        // A major mark of the adopted base (as a return onto a stored head makes).
        let marked = crate::live_store::LiveStore::write_live_major(
            &store,
            doc,
            &crate::live_store::MajorWrite {
                kind: crate::live_store::MajorKind::Mark,
                reason: "returning_client_before",
                person: UserId(id(11)),
                current: &adopted,
                restore: None,
                disputed: None,
                blob: None,
                authors: &[],
                at: Seq(2),
                session: room.session.as_ref().unwrap().id.0,
                created_at: time::OffsetDateTime::UNIX_EPOCH,
            },
        )
        .await
        .unwrap()
        .current;
        // Unsaved work after it.
        room.checkpoint_base = Some(marked.clone());
        entry(&mut room, &ann, 3, 2, FORMAT, "three".into())
            .await
            .unwrap();
        drop(room);

        let room = LiveRoom::load(doc, store.clone(), EventSink::discard(), timing())
            .await
            .unwrap();
        assert!(
            room.recovery.is_none(),
            "the room's own mark is not an outside file"
        );
        assert_eq!(room.checkpoint_base.as_deref(), Some(marked.as_str()));
    }

    #[tokio::test(start_paused = true)]
    async fn restart_after_an_unrecorded_checkpoint_keeps_the_log() {
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
        assert!(matches!(
            upload(&mut room, UserId(id(11)), ticket, Seq(1), 9).await,
            LiveCheckpointReply::Saved { .. }
        ));
        entry(&mut room, &ann, 2, 1, FORMAT, "two".into())
            .await
            .unwrap();
        // Git committed a checkpoint at 2 whose record never reached the database.
        let unrecorded = store.commit_unrecorded(doc, Seq(2), [4; 32]);
        entry(&mut room, &ann, 3, 2, FORMAT, "three".into())
            .await
            .unwrap();
        drop(room);

        let room = LiveRoom::load(doc, store.clone(), EventSink::discard(), timing())
            .await
            .unwrap();
        assert!(
            room.recovery.is_none(),
            "the room's own commit is not an outside file"
        );
        assert_eq!(room.checkpoint_base.as_deref(), Some(unrecorded.as_str()));
        assert_eq!(room.entries.len(), 3, "no entry leaves the log");
    }

    #[tokio::test(start_paused = true)]
    async fn over_limit_room_lets_a_capable_editor_bring_it_back_under() {
        let doc = DocId(id(100));
        let store = MemStore::new();
        {
            let mut room = LiveRoom::load(doc, store.clone(), EventSink::discard(), timing())
                .await
                .unwrap();
            let ann = join_capable(&mut room, 1, 1, "editor", LiveBuild::Browser).await;
            for local in 1..=5 {
                entry(&mut room, &ann, local, local - 1, FORMAT, "x".into())
                    .await
                    .unwrap();
            }
        }
        let mut room = LiveRoom::load(
            doc,
            store.clone(),
            EventSink::discard(),
            limited(1, 2, u64::MAX / 2, u64::MAX / 2),
        )
        .await
        .unwrap();
        assert!(room.over_limit);
        let ann = join_capable(&mut room, 5, 1, "editor", LiveBuild::Browser).await;
        assert_eq!(
            entries(ann.initial.clone()).len(),
            5,
            "the log, for a checkpoint"
        );
        let (ticket, purpose, at, base, _) = ticket_major(ann.after_join.clone()).unwrap();
        assert_eq!((purpose, at), (TicketPurpose::Write, Seq(5)));
        assert!(matches!(
            upload_on(&mut room, UserId(id(11)), ticket, at, base.as_deref(), 9).await,
            LiveCheckpointReply::Saved { .. }
        ));
        assert!(!room.over_limit, "back under: people may join again");
        let bob = join_as(&mut room, 6, 2, 12, "editor", FORMAT, None, None).await;
        assert_eq!(bob.participant, ParticipantNo(3));
    }

    #[test]
    fn paused_cancel_records_fit_under_the_load_limit() {
        let limits = crate::config::LiveLimits {
            warn_entries: 1,
            hard_entries: 2,
            warn_bytes: 5,
            hard_bytes: 10,
        };
        let allowance = 10 + 2 * records::LIVE_CANCEL_BYTES;
        assert!(!super::beyond_limits(limits, 4, allowance));
        assert!(super::beyond_limits(limits, 4, allowance + 1));
        assert!(super::beyond_limits(limits, 5, 0));
    }

    // ---- Task 11 re-review fix round (t11-fix-review.md) ----

    /// The current file's content hash.
    async fn current_sha(store: &MemStore, doc: DocId) -> [u8; 32] {
        let current = crate::live_store::LiveStore::current_revision(store, doc)
            .await
            .unwrap()
            .unwrap();
        store
            .revisions(doc)
            .into_iter()
            .find(|(revision, _)| *revision == current)
            .unwrap()
            .1
    }

    /// Writes the next quiet checkpoint at `at` with `sha` and has the other editor verify it.
    async fn peer_verified(
        room: &mut LiveRoom<MemStore>,
        ann: &mut Joined,
        bob: &mut Joined,
        sha: u8,
    ) -> String {
        tokio::time::advance(Duration::from_secs(15)).await;
        room.expire_due().await.unwrap();
        let mut asked = drain(&mut ann.rx);
        asked.extend(drain(&mut bob.rx));
        let (write, _, at, base, _) = tickets_in(asked)
            .into_iter()
            .find(|(_, purpose, _, _, _)| *purpose == TicketPurpose::Write)
            .unwrap();
        let (writer, verifier) = if ticket_holder_is(room, write, ann.participant.0) {
            (UserId(id(11)), UserId(id(12)))
        } else {
            (UserId(id(12)), UserId(id(11)))
        };
        let LiveCheckpointReply::Saved { revision } =
            upload_on(room, writer, write, at, base.as_deref(), sha).await
        else {
            panic!("stored");
        };
        let mut asked = drain(&mut ann.rx);
        asked.extend(drain(&mut bob.rx));
        let (verify, _, _, _, _) = tickets_in(asked)
            .into_iter()
            .find(|(_, purpose, _, _, _)| *purpose == TicketPurpose::Verify)
            .unwrap();
        assert_eq!(
            hash(room, verifier, verify, at, sha).await,
            LiveCheckpointReply::Accepted
        );
        assert_eq!(room.verified_through, at);
        revision
    }

    /// I1: a second editor disagreeing with a sole editor's base sets it aside like any
    /// disputed checkpoint: the last peer-verified base is current again and the catch-up
    /// base, and both copies are kept.
    #[tokio::test(start_paused = true)]
    async fn disputed_sole_editor_base_is_set_aside_for_the_last_peer_verified_base() {
        let doc = DocId(id(100));
        let (events, mut emitted) = EventSink::channel();
        let store = MemStore::new();
        let mut room = LiveRoom::load(
            doc,
            store.clone(),
            events,
            limited(3, 3, u64::MAX / 2, u64::MAX / 2),
        )
        .await
        .unwrap();
        let mut ann = join_capable(&mut room, 1, 1, "editor", LiveBuild::Browser).await;
        let mut bob = join_capable(&mut room, 2, 2, "editor", LiveBuild::Browser).await;
        entry(&mut room, &ann, 1, 0, FORMAT, "one".into())
            .await
            .unwrap();
        let peer = peer_verified(&mut room, &mut ann, &mut bob, 9).await;
        // Bob goes; Ann edits alone up to the limit and her own checkpoint is accepted.
        expire_one(&mut room, &ann, 1).await;
        let _ = drain(&mut ann.rx);
        let mut ticket = None;
        for local in 2..=4 {
            entry(&mut room, &ann, local, local - 1, FORMAT, "x".into())
                .await
                .unwrap();
            if let Some((ticket_id, _, at, base, _)) = tickets_in(drain(&mut ann.rx))
                .into_iter()
                .find(|(_, purpose, _, _, _)| *purpose == TicketPurpose::Write)
            {
                ticket = Some((ticket_id, at, base));
            }
        }
        assert!(room.paused);
        let (ticket, at, base) = ticket.unwrap();
        let LiveCheckpointReply::Saved { revision: sole } =
            upload_on(&mut room, UserId(id(11)), ticket, at, base.as_deref(), 7).await
        else {
            panic!("stored");
        };
        assert!(!room.paused, "accepted as a sole-editor base");
        let _ = emitted_kinds(&mut emitted, "");

        // Cat joins and disagrees.
        let mut cat = join_capable(&mut room, 7, 3, "editor", LiveBuild::Browser).await;
        let (verify, purpose, verify_at) = checkpoint_ticket(cat.after_join.clone()).unwrap();
        assert_eq!((purpose, verify_at), (TicketPurpose::Verify, at));
        assert_eq!(
            hash(&mut room, UserId(id(13)), verify, at, 8).await,
            LiveCheckpointReply::Accepted
        );
        if let Some((copy, _, copy_at, base, major)) = ticket_major(drain(&mut cat.rx)) {
            assert_eq!(major, Some(MajorReason::DisputedCopy));
            let _ = upload_on(&mut room, UserId(id(13)), copy, copy_at, base.as_deref(), 8).await;
        }
        for _ in 0..3 {
            room.finish_writes().await.unwrap();
            tokio::time::advance(Duration::from_secs(6)).await;
            room.expire_due().await.unwrap();
            room.settle().await.unwrap();
        }
        assert_eq!(
            current_sha(&store, doc).await,
            [9; 32],
            "the last peer-verified base is current again"
        );
        let kept = store
            .revisions(doc)
            .into_iter()
            .filter(|(revision, _)| {
                store.revision_fields(doc, revision).get("major_reason")
                    == Some(&json!("disputed_copy"))
            })
            .map(|(_, sha)| sha)
            .collect::<Vec<_>>();
        assert!(kept.contains(&[7; 32]), "the sole editor's copy is kept");
        let loaded = crate::live_store::LiveStore::load_live(&store, doc)
            .await
            .unwrap();
        assert_eq!(loaded.verified_through, Seq(1));
        assert_eq!(loaded.verified_checkpoint.unwrap().revision, peer);
        assert!(
            loaded
                .unverified_checkpoints
                .iter()
                .all(|checkpoint| checkpoint.revision != sole),
            "the disputed base is never loaded"
        );
        assert_eq!(
            loaded.entries.len(),
            3,
            "the log after the peer base is kept"
        );
        assert!(unavailable(&drain(&mut ann.rx)));
        assert!(!room.has_session());
    }

    /// I2: one dispute stop per person per document per hour. A second dispute within the
    /// hour excludes the people in it from checkpoint work for a day, and the room carries on.
    #[tokio::test(start_paused = true)]
    async fn a_second_dispute_within_the_hour_excludes_instead_of_stopping() {
        let doc = DocId(id(100));
        let (events, mut emitted) = EventSink::channel();
        let (mut room, store, mut ann, _bob, _first, _second, verify) =
            verified_then_written(events.clone(), 5).await;
        assert_eq!(
            hash(&mut room, UserId(id(11)), verify, Seq(2), 6).await,
            LiveCheckpointReply::Accepted
        );
        let (copy, _, at, base, _) = ticket_major(drain(&mut ann.rx)).unwrap();
        let _ = upload_on(&mut room, UserId(id(11)), copy, at, base.as_deref(), 6).await;
        room.settle().await.unwrap();
        assert!(!room.has_session(), "the first dispute stops the session");
        assert_eq!(
            emitted_kinds(&mut emitted, "live.checkpoint_dispute_stopped").len(),
            1
        );
        drop(room);

        // Everyone joins again, and the same two disagree again within the hour.
        let mut room = LiveRoom::load(doc, store.clone(), events, timing())
            .await
            .unwrap();
        let mut ann = join_capable(&mut room, 3, 1, "editor", LiveBuild::Browser).await;
        let mut bob = join_capable(&mut room, 4, 2, "editor", LiveBuild::Browser).await;
        let head = room.head.0;
        entry(&mut room, &ann, 3, head, FORMAT, "three".into())
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(15)).await;
        room.expire_due().await.unwrap();
        let mut asked = drain(&mut ann.rx);
        asked.extend(drain(&mut bob.rx));
        let (write, _, at, base, _) = tickets_in(asked)
            .into_iter()
            .find(|(_, purpose, _, _, _)| *purpose == TicketPurpose::Write)
            .unwrap();
        let (writer, verifier) = if ticket_holder_is(&room, write, ann.participant.0) {
            (UserId(id(11)), UserId(id(12)))
        } else {
            (UserId(id(12)), UserId(id(11)))
        };
        let _ = upload_on(&mut room, writer, write, at, base.as_deref(), 5).await;
        let mut asked = drain(&mut ann.rx);
        asked.extend(drain(&mut bob.rx));
        let (verify, _, _, _, _) = tickets_in(asked)
            .into_iter()
            .find(|(_, purpose, _, _, _)| *purpose == TicketPurpose::Verify)
            .unwrap();
        assert_eq!(
            hash(&mut room, verifier, verify, at, 6).await,
            LiveCheckpointReply::Accepted
        );
        room.settle().await.unwrap();
        assert!(room.has_session(), "the room carries on");
        assert!(!unavailable(&drain(&mut ann.rx)));
        assert!(!unavailable(&drain(&mut bob.rx)));
        let kinds = emitted_kinds(&mut emitted, "live.checkpoint_");
        assert!(
            kinds
                .iter()
                .all(|(kind, _)| *kind != "live.checkpoint_dispute_stopped"),
            "no second stop: {kinds:?}"
        );
        let excluded = kinds
            .iter()
            .filter(|(kind, _)| *kind == "live.checkpoint_participant_excluded")
            .count();
        assert_eq!(excluded, 2, "{kinds:?}");
        // Changes go on; neither is asked for checkpoint work again.
        entry(&mut room, &bob, 1, at.0, FORMAT, "bob".into())
            .await
            .unwrap();
        assert_eq!(entries(drain(&mut bob.rx)).len(), 1);
        tokio::time::advance(Duration::from_secs(15)).await;
        room.expire_due().await.unwrap();
        let mut asked = drain(&mut ann.rx);
        asked.extend(drain(&mut bob.rx));
        let asked = tickets_in(asked);
        assert!(
            asked
                .iter()
                .all(|(_, purpose, _, _, _)| *purpose == TicketPurpose::Write),
            "excluded people never verify, and write only because nobody else can: {asked:?}"
        );
        // The exclusion outlives the room.
        drop(room);
        let mut room = LiveRoom::load(doc, store.clone(), EventSink::discard(), timing())
            .await
            .unwrap();
        let ann = join_capable(&mut room, 5, 1, "editor", LiveBuild::Browser).await;
        assert!(tickets_in(ann.after_join.clone()).is_empty());
        assert!(tickets_in(ann.initial.clone()).is_empty());
    }

    /// I3: a returning batch that does not reach the local named on the join within two
    /// minutes is closed and its client resynced, so later returns are not blocked.
    #[tokio::test(start_paused = true)]
    async fn a_stuck_return_batch_closes_after_two_minutes() {
        let store = MemStore::new();
        let (mut room, mut ann, mut bob) = returned(EventSink::discard(), store.clone()).await;
        let mut asked = ann.after_join.clone();
        asked.extend(drain(&mut bob.rx));
        let (ticket, _, at, _, _) = ticket_major(asked).unwrap();
        let _ = upload_ticket(&mut room, &ann, ticket, at, None, 2).await;
        let mut ready = ann.after_join.clone();
        ready.extend(drain(&mut ann.rx));
        assert_eq!(return_ready(&ready), Some(LocalNo(3)));
        assert!(
            room.returns
                .iter()
                .any(|barrier| barrier.stage == super::ReturnStage::Batch)
        );
        // Ann never sends local 3.
        let seen = room.head.0;
        for _ in 0..3 {
            tokio::time::advance(Duration::from_secs(50)).await;
            heartbeat(&mut room, &ann, seen).await;
            heartbeat(&mut room, &bob, seen).await;
            room.expire_due().await.unwrap();
        }
        assert!(
            room.returns
                .iter()
                .all(|barrier| barrier.stage != super::ReturnStage::Batch),
            "the batch is closed"
        );
        let frames = drain(&mut ann.rx);
        assert_eq!(
            return_ready(&frames),
            Some(LocalNo(3)),
            "told again it may send, in case the first was lost"
        );
        assert!(!catchups(&frames).is_empty(), "its client is resynced");
    }

    /// I5: a client that gave up its held change and sends a later one after the hold is
    /// resynced and its change stored, never dropped for good.
    #[tokio::test(start_paused = true)]
    async fn an_abandoned_held_change_does_not_wedge_the_client() {
        let store = MemStore::new();
        let (mut room, ann, mut bob) = returned(EventSink::discard(), store.clone()).await;
        let mut asked = ann.after_join.clone();
        asked.extend(drain(&mut bob.rx));
        let (ticket, _, at, _, _) = ticket_major(asked).unwrap();
        entry(&mut room, &bob, 2, 2, FORMAT, "held".into())
            .await
            .unwrap();
        assert_eq!(refusal(&drain(&mut bob.rx)), Some(RefusedReason::Held));
        let _ = upload_ticket(&mut room, &ann, ticket, at, None, 2).await;
        assert!(resumed(&drain(&mut bob.rx)));
        // Bob dropped the held change and edits again.
        entry(&mut room, &bob, 3, 2, FORMAT, "later".into())
            .await
            .unwrap();
        let frames = drain(&mut bob.rx);
        assert!(!catchups(&frames).is_empty(), "Bob is resynced");
        assert!(
            entries(frames).iter().any(|entry| entry.seq == Seq(3)),
            "the later change is stored"
        );
        assert_eq!(room.head, Seq(3));
    }

    /// Minor 1: after a restart, an editor whose place is still held counts as present, so
    /// the first to reconnect is not accepted as a sole editor.
    #[tokio::test(start_paused = true)]
    async fn after_a_restart_reconnecting_editors_count_as_present() {
        let doc = DocId(id(100));
        let store = MemStore::new();
        {
            let mut room = LiveRoom::load(doc, store.clone(), EventSink::discard(), timing())
                .await
                .unwrap();
            let _ann = join_capable(&mut room, 1, 1, "editor", LiveBuild::Browser).await;
            let _bob = join_capable(&mut room, 2, 2, "editor", LiveBuild::Browser).await;
        }
        let (events, mut emitted) = EventSink::channel();
        let mut room = LiveRoom::load(
            doc,
            store.clone(),
            events,
            limited(3, 3, u64::MAX / 2, u64::MAX / 2),
        )
        .await
        .unwrap();
        let mut ann = join_capable(&mut room, 3, 1, "editor", LiveBuild::Browser).await;
        let mut ticket = None;
        for local in 1..=3 {
            entry(&mut room, &ann, local, local - 1, FORMAT, "x".into())
                .await
                .unwrap();
            if let Some((ticket_id, TicketPurpose::Write, at)) =
                checkpoint_ticket(drain(&mut ann.rx))
            {
                ticket = Some((ticket_id, at));
            }
        }
        let (ticket, at) = ticket.unwrap();
        assert!(matches!(
            upload(&mut room, UserId(id(11)), ticket, at, 9).await,
            LiveCheckpointReply::Saved { .. }
        ));
        room.expire_due().await.unwrap();
        assert!(
            room.paused,
            "Bob is reconnecting: no sole-editor acceptance"
        );
        assert!(emitted_kinds(&mut emitted, "live.checkpoint_sole_editor").is_empty());
    }

    /// Minor 3: the over-limit room rebuilt for a capable editor runs the same checks as a
    /// load, so an outside replacement under unsaved work is recovered, not written over.
    #[tokio::test(start_paused = true)]
    async fn over_limit_rebuild_finds_an_outside_replacement() {
        let doc = DocId(id(100));
        let store = MemStore::new();
        {
            let mut room = LiveRoom::load(doc, store.clone(), EventSink::discard(), timing())
                .await
                .unwrap();
            let mut ann = join_capable(&mut room, 1, 1, "editor", LiveBuild::Browser).await;
            entry(&mut room, &ann, 1, 0, FORMAT, "x".into())
                .await
                .unwrap();
            tokio::time::advance(Duration::from_secs(15)).await;
            room.expire_due().await.unwrap();
            let (ticket, _, _) = checkpoint_ticket(drain(&mut ann.rx)).unwrap();
            assert!(matches!(
                upload(&mut room, UserId(id(11)), ticket, Seq(1), 9).await,
                LiveCheckpointReply::Saved { .. }
            ));
            for local in 2..=6 {
                entry(&mut room, &ann, local, local - 1, FORMAT, "x".into())
                    .await
                    .unwrap();
            }
        }
        let _outside = store.write_outside(doc, [5; 32]);
        let mut room = LiveRoom::load(
            doc,
            store.clone(),
            EventSink::discard(),
            limited(1, 2, u64::MAX / 2, u64::MAX / 2),
        )
        .await
        .unwrap();
        assert!(room.over_limit);
        let mut ann = join_capable(&mut room, 7, 1, "editor", LiveBuild::Browser).await;
        let mut asked = ann.initial.clone();
        asked.extend(ann.after_join.clone());
        asked.extend(drain(&mut ann.rx));
        let (_, _, _, _, major) = ticket_major(asked).unwrap();
        assert_eq!(major, Some(MajorReason::OutsideReplacement));
    }

    /// Minor 4: a dispute whose final restore fails is retried before the session stops, so
    /// a disputed copy never stays the current file.
    #[tokio::test(start_paused = true)]
    async fn a_dispute_whose_restore_fails_is_retried_before_it_stops() {
        let doc = DocId(id(100));
        let (mut room, store, mut ann, _bob, _first, _second, verify) =
            verified_then_written(EventSink::discard(), 5).await;
        assert_eq!(
            hash(&mut room, UserId(id(11)), verify, Seq(2), 6).await,
            LiveCheckpointReply::Accepted
        );
        let (copy, _, at, base, _) = ticket_major(drain(&mut ann.rx)).unwrap();
        store.fail_major_restore();
        let _ = upload_on(&mut room, UserId(id(11)), copy, at, base.as_deref(), 6).await;
        for _ in 0..3 {
            room.finish_writes().await.unwrap();
            tokio::time::advance(Duration::from_secs(6)).await;
            room.expire_due().await.unwrap();
            room.settle().await.unwrap();
        }
        assert_eq!(
            current_sha(&store, doc).await,
            [9; 32],
            "the last verified base is current"
        );
        assert!(unavailable(&drain(&mut ann.rx)));
        assert!(!room.has_session());
    }

    // ---- G4 final review fixes ----

    /// C1: Ann's connection drops while the version before her batch is written, so the
    /// `return_ready` sent when it is stored reaches nobody. Her kept rejoin is told again.
    #[tokio::test(start_paused = true)]
    async fn a_return_ready_lost_with_the_connection_is_sent_on_the_kept_rejoin() {
        let store = MemStore::new();
        let (mut room, ann, mut bob) = returned(EventSink::discard(), store.clone()).await;
        let mut asked = ann.after_join.clone();
        asked.extend(drain(&mut bob.rx));
        let (ticket, _, at, _, _) = ticket_major(asked).unwrap();
        let holder = if ticket_holder_is(&room, ticket, ann.participant.0) {
            UserId(id(11))
        } else {
            UserId(id(12))
        };
        let (reply, answer) = oneshot::channel();
        room.handle(LiveCommand::CheckpointUpload {
            user: holder,
            ticket,
            at,
            base: None,
            blob: BlobRef {
                sha256: [2; 32],
                size: 3,
            },
            session_data: b"identities".to_vec(),
            reply,
        })
        .await
        .unwrap();
        room.handle(LiveCommand::Disconnected {
            conn: ann.conn,
            client: ann.client,
        })
        .await
        .unwrap();
        room.finish_writes().await.unwrap();
        assert!(matches!(
            answer.await.unwrap(),
            LiveCheckpointReply::Saved { .. }
        ));
        let resume = LiveResume {
            session: ann.session,
            participant: ann.participant,
        };
        let again = join_return(
            &mut room,
            9,
            1,
            UserId(id(11)),
            Some(resume),
            Some(Seq(2)),
            Some(LocalNo(3)),
        )
        .await;
        assert_eq!(again.place, Place::Kept);
        let mut frames = again.initial.clone();
        frames.extend(again.after_join.clone());
        assert_eq!(return_ready(&frames), Some(LocalNo(3)));
    }

    /// I1: a person excluded after a repeat dispute never verifies, and writes only while no
    /// capable editor who is not excluded is present, so a room is never left without a
    /// writer.
    #[tokio::test(start_paused = true)]
    async fn an_excluded_editor_writes_only_when_nobody_else_can() {
        let doc = DocId(id(100));
        let store = MemStore::new();
        crate::live_store::LiveStore::set_live_dispute_mark(
            &store,
            doc,
            &records::LiveDisputeMark {
                user: UserId(id(11)),
                stopped_at: None,
                excluded_until: Some(time::OffsetDateTime::now_utc() + time::Duration::hours(24)),
            },
        )
        .await
        .unwrap();
        let mut room = LiveRoom::load(doc, store.clone(), EventSink::discard(), timing())
            .await
            .unwrap();
        let mut ann = join_capable(&mut room, 1, 1, "editor", LiveBuild::Browser).await;
        entry(&mut room, &ann, 1, 0, FORMAT, "one".into())
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(15)).await;
        room.expire_due().await.unwrap();
        let (write, purpose, at) =
            checkpoint_ticket(drain(&mut ann.rx)).expect("a lone excluded editor still writes");
        assert_eq!((purpose, at), (TicketPurpose::Write, Seq(1)));
        assert!(matches!(
            upload(&mut room, UserId(id(11)), write, Seq(1), 9).await,
            LiveCheckpointReply::Saved { .. }
        ));

        // Bob joins: he verifies Ann's checkpoint, and from now on he writes, not Ann.
        let mut bob = join_capable(&mut room, 2, 2, "editor", LiveBuild::Browser).await;
        let mut asked = bob.after_join.clone();
        asked.extend(drain(&mut bob.rx));
        let (verify, purpose, _) = checkpoint_ticket(asked).unwrap();
        assert_eq!(purpose, TicketPurpose::Verify);
        assert_eq!(
            hash(&mut room, UserId(id(12)), verify, Seq(1), 9).await,
            LiveCheckpointReply::Accepted
        );
        entry(&mut room, &ann, 2, 1, FORMAT, "two".into())
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(15)).await;
        room.expire_due().await.unwrap();
        assert!(
            checkpoint_ticket(drain(&mut ann.rx)).is_none(),
            "not Ann while Bob can write"
        );
        let (write, purpose, at, base) = ticket_full(drain(&mut bob.rx)).unwrap();
        assert_eq!((purpose, at), (TicketPurpose::Write, Seq(2)));
        assert!(matches!(
            upload_on(&mut room, UserId(id(12)), write, Seq(2), base.as_deref(), 8).await,
            LiveCheckpointReply::Saved { .. }
        ));
        for _ in 0..4 {
            tokio::time::advance(Duration::from_secs(30)).await;
            heartbeat(&mut room, &ann, 2).await;
            heartbeat(&mut room, &bob, 2).await;
            room.expire_due().await.unwrap();
        }
        assert!(
            checkpoint_ticket(drain(&mut ann.rx)).is_none(),
            "an excluded person never verifies"
        );
    }

    /// I2: the writer's other tab is the same person: it never verifies the checkpoint, and it
    /// does not stop a lone person's checkpoint being accepted at the limit.
    #[tokio::test(start_paused = true)]
    async fn a_second_tab_of_the_writer_never_verifies() {
        let mut room = LiveRoom::load(
            DocId(id(100)),
            MemStore::new(),
            EventSink::discard(),
            limited(3, 3, u64::MAX / 2, u64::MAX / 2),
        )
        .await
        .unwrap();
        let mut first = join_capable(&mut room, 1, 1, "editor", LiveBuild::Browser).await;
        let mut second = join_full(
            &mut room,
            2,
            3,
            UserId(id(11)),
            "editor",
            FORMAT,
            None,
            None,
            true,
            LiveBuild::Browser,
        )
        .await;
        let mut asked = Vec::new();
        for local in 1..=3 {
            entry(&mut room, &first, local, local - 1, FORMAT, "x".into())
                .await
                .unwrap();
            asked.extend(drain(&mut first.rx));
            asked.extend(drain(&mut second.rx));
        }
        assert!(room.paused);
        let (write, _, at, _, _) = tickets_in(asked)
            .into_iter()
            .find(|(_, purpose, _, _, _)| *purpose == TicketPurpose::Write)
            .unwrap();
        assert!(matches!(
            upload(&mut room, UserId(id(11)), write, at, 9).await,
            LiveCheckpointReply::Saved { .. }
        ));
        let mut asked = drain(&mut first.rx);
        asked.extend(drain(&mut second.rx));
        assert!(
            tickets_in(asked)
                .iter()
                .all(|(_, purpose, _, _, _)| *purpose != TicketPurpose::Verify),
            "the writer's own tab is never asked to verify"
        );
        assert!(!room.paused, "a lone person's checkpoint is accepted");
    }

    /// I3: a verifier checks the writer's identity data as well as the file: a forged
    /// identity table with correct file bytes is a dispute, never verified.
    #[tokio::test(start_paused = true)]
    async fn a_verify_must_match_the_session_data_too() {
        let doc = DocId(id(100));
        let (events, mut emitted) = EventSink::channel();
        let (mut room, store, _ann, _bob, _revision, verify) =
            written_and_ticketed(events, 9).await;
        let _ = emitted_kinds(&mut emitted, "");
        assert_eq!(
            hash_full(&mut room, UserId(id(12)), verify, Seq(1), 9, [0xab; 32]).await,
            LiveCheckpointReply::Accepted
        );
        let loaded = crate::live_store::LiveStore::load_live(&store, doc)
            .await
            .unwrap();
        assert_eq!(
            loaded.verified_through,
            Seq(0),
            "a forged identity table is never verified"
        );
        assert!(
            emitted_kinds(&mut emitted, "live.checkpoint_mismatch").len() == 1,
            "a mismatch on either hash is a dispute"
        );
    }

    /// I4: a claimed write ticket's upload is given time for its bytes at 64 KiB/s plus the
    /// lifetime, so a large workbook on a slow link can still be checkpointed.
    #[tokio::test(start_paused = true)]
    async fn a_claimed_upload_gets_time_for_its_bytes() {
        let (mut room, _) = room().await;
        let mut ann = join_capable(&mut room, 1, 1, "editor", LiveBuild::Browser).await;
        entry(&mut room, &ann, 1, 0, FORMAT, "one".into())
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(15)).await;
        room.expire_due().await.unwrap();
        let ticket = checkpoint_ticket(drain(&mut ann.rx)).unwrap().0;
        let (reply, answer) = oneshot::channel();
        room.handle(LiveCommand::CheckpointClaim {
            user: UserId(id(11)),
            ticket,
            at: Seq(1),
            base: None,
            // Ten minutes at the floor rate.
            length: super::UPLOAD_FLOOR_BYTES_PER_SECOND * 600,
            reply,
        })
        .await
        .unwrap();
        assert!(answer.await.unwrap());
        for _ in 0..7 {
            tokio::time::advance(Duration::from_secs(60)).await;
            heartbeat(&mut room, &ann, 1).await;
            room.expire_due().await.unwrap();
        }
        assert!(matches!(
            upload(&mut room, UserId(id(11)), ticket, Seq(1), 9).await,
            LiveCheckpointReply::Saved { .. }
        ));
    }

    /// Same-build ruling: checkpoint bytes carry computed values, so only a place on the
    /// writer's build verifies them.
    #[tokio::test(start_paused = true)]
    async fn verify_tickets_go_only_to_the_writers_build() {
        let (mut room, _) = room().await;
        let mut ann = join_capable(&mut room, 1, 1, "editor", LiveBuild::Browser).await;
        let mut bob = join_capable(&mut room, 2, 2, "editor", LiveBuild::Desktop).await;
        entry(&mut room, &ann, 1, 0, FORMAT, "one".into())
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(15)).await;
        room.expire_due().await.unwrap();
        let mut asked = drain(&mut ann.rx);
        asked.extend(drain(&mut bob.rx));
        let (write, _, at, _, _) = tickets_in(asked)
            .into_iter()
            .find(|(_, purpose, _, _, _)| *purpose == TicketPurpose::Write)
            .unwrap();
        let writer = if ticket_holder_is(&room, write, ann.participant.0) {
            UserId(id(11))
        } else {
            UserId(id(12))
        };
        assert!(matches!(
            upload(&mut room, writer, write, at, 9).await,
            LiveCheckpointReply::Saved { .. }
        ));
        let mut asked = drain(&mut ann.rx);
        asked.extend(drain(&mut bob.rx));
        assert!(
            tickets_in(asked).is_empty(),
            "the other build is not asked to verify"
        );
        let cat_build = if writer == UserId(id(11)) {
            LiveBuild::Browser
        } else {
            LiveBuild::Desktop
        };
        let cat = join_capable(&mut room, 3, 3, "editor", cat_build).await;
        let (_, purpose, at) = checkpoint_ticket(cat.after_join.clone()).unwrap();
        assert_eq!((purpose, at), (TicketPurpose::Verify, Seq(1)));
    }

    /// I5: every join is told whether changes wait, before its catch-up, so a client never
    /// sends into a hold and a client that missed `changes_resumed` is released.
    #[tokio::test(start_paused = true)]
    async fn joins_are_told_whether_changes_wait_before_their_catch_up() {
        let (mut room, _) = room().await;
        let ann = join_capable(&mut room, 1, 1, "editor", LiveBuild::Browser).await;
        let position = |frames: &[LiveServerMsg], wanted: fn(&LiveServerMsg) -> bool| {
            frames.iter().position(wanted)
        };
        let resumed_at = position(&ann.raw, |frame| {
            matches!(frame, LiveServerMsg::ChangesResumed)
        });
        let catchup_at = position(&ann.raw, |frame| {
            matches!(frame, LiveServerMsg::Catchup { .. })
        });
        assert!(
            resumed_at.is_some_and(|at| catchup_at.is_some_and(|catchup| at < catchup)),
            "{:?}",
            ann.raw
        );

        let (mut room, _ann, _bob) = returned(EventSink::discard(), MemStore::new()).await;
        assert!(room.holding());
        let cat = join_capable(&mut room, 7, 3, "editor", LiveBuild::Browser).await;
        let paused_at = position(&cat.initial, |frame| {
            matches!(frame, LiveServerMsg::ChangesPaused { .. })
        });
        let catchup_at = position(&cat.initial, |frame| {
            matches!(frame, LiveServerMsg::Catchup { .. })
        });
        assert!(
            paused_at.is_some_and(|at| catchup_at.is_some_and(|catchup| at < catchup)),
            "{:?}",
            cat.initial
        );
    }

    /// Pause ruling: a change that reaches a paused room is held, not refused for good: its
    /// number is not used up and the client sends it again once changes resume.
    #[tokio::test(start_paused = true)]
    async fn a_paused_room_holds_changes_without_using_them_up() {
        let (mut room, store) = limited_room(limited(100, 3, u64::MAX / 2, u64::MAX / 2)).await;
        let mut ann = join_as(&mut room, 1, 1, 11, "editor", FORMAT, None, None).await;
        for local in 1..=3 {
            entry(&mut room, &ann, local, local - 1, FORMAT, "x".into())
                .await
                .unwrap();
        }
        let _ = drain(&mut ann.rx);
        entry(&mut room, &ann, 4, 3, FORMAT, "x".into())
            .await
            .unwrap();
        assert_eq!(refusal(&drain(&mut ann.rx)), Some(RefusedReason::Held));
        assert_eq!(
            crate::live_store::LiveStore::live_local(&store, DocId(id(100)), ann.client, 4)
                .await
                .unwrap(),
            records::LiveLocal::Unseen,
            "its number is not used up"
        );
        entry(&mut room, &ann, 5, 3, FORMAT, "y".into())
            .await
            .unwrap();
        assert!(
            refusal(&drain(&mut ann.rx)).is_none(),
            "a later change sent before the client heard waits quietly"
        );
        assert_eq!(store.entry_count(), 3);
    }
}
