//! The hooks an editor gives the library so it can show, go back and re-run changes.
//!
//! The library never looks inside a change: it hands each one to the editor as an
//! [`EntryView`] and the editor says what happened. Applying an entry may read only the entry,
//! its server stamp and its sender's participant number, never anything that belongs to one
//! person, so every device that applies the same entries in the same order ends the same.
//!
//! [`LiveClient`] keeps two states: the agreed state, which is the log up to
//! [`LiveClient::agreed_seq`], and the shown state, which is the agreed state with the
//! person's own waiting changes on top. When the server's order differs from what is shown,
//! the client goes back to its copy of the agreed state and re-runs.

use std::collections::VecDeque;

use collab_core::ids::{ClientId, Seq};
use protocol::RemovedReason;
use protocol::live::{
    EntryBody, HEARTBEAT_MS, LIVE_PROTOCOL, LiveClientMsg, LiveEntry, LivePerson, LiveResume,
    LiveServerMsg, LiveSessionId, LocalNo, MAX_PAYLOAD_BYTES, ParticipantNo, Place, RefusedReason,
};
use thiserror::Error;

use crate::ids::{IdError, IdMaker, LiveId};

/// What an editor does for the library.
pub trait LiveEditor {
    /// A copy of what is shown, to go back to.
    type Snapshot;

    /// Takes a copy of what is shown.
    fn snapshot(&self) -> Self::Snapshot;

    /// Goes back to a copy taken earlier.
    fn restore(&mut self, snapshot: &Self::Snapshot);

    /// Applies one change. `Err(Crash)` means the editor stopped in this entry.
    fn apply(&mut self, entry: &EntryView<'_>) -> Result<Outcome, Crash>;

    /// Keeps the crash note: the entry about to be applied (`Some`), or none once it is
    /// applied (`None`). The host stores it where it survives the editor stopping; the default
    /// keeps nothing.
    fn note_applying(&mut self, seq: Option<Seq>) {
        let _ = seq;
    }
}

/// The server's stamp on an entry: when it was put in order, and a seed for anything the
/// change needs to choose alike on every device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    pub at_ms: i64,
    pub seed: u32,
}

/// One change as the editor sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EntryView<'a> {
    /// Its position in the log; `None` while it is still waiting for the server.
    pub seq: Option<Seq>,
    /// Who sent it.
    pub participant: ParticipantNo,
    /// Whether this device sent it.
    pub own: bool,
    /// The server's stamp; `None` while it is still waiting for the server.
    pub stamp: Option<Stamp>,
    /// The change's content, exactly as sent.
    pub payload: &'a str,
}

/// What applying a change did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub effect: Effect,
    /// Whether the result depended on the stamp or the position (`seq`), which are missing
    /// while a change waits for the server; such a change must be applied again with the real
    /// ones once the server has put it in order.
    pub read_stamp: bool,
}

/// Whether a change took effect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    Applied,
    /// The change did nothing, on every device alike; `reason` says why.
    NoEffect {
        reason: String,
    },
}

/// The editor stopped in this entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Crash;

/// How many confirmed own changes the client keeps on top of its agreed copy before it goes
/// back and takes a new copy.
pub const MAX_FOLDED: usize = 64;

/// The most entry frames the client sends in any [`PACE_WINDOW_MS`]. The server allows more
/// (30 a second, bursts up to 120), so a paced client is never refused for its rate.
pub const MAX_ENTRIES_PER_WINDOW: usize = 20;

/// The window [`MAX_ENTRIES_PER_WINDOW`] counts over, in milliseconds.
pub const PACE_WINDOW_MS: u64 = 1_000;

/// The shortest heartbeat interval the client takes from the server; the longest is
/// [`HEARTBEAT_MS`].
pub const MIN_HEARTBEAT_MS: u64 = 1_000;

/// When a local change is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Shown at once, before the server has put it in order.
    ShowAtOnce,
    /// Shown only once the server has put it in order.
    WaitForServer,
}

/// What the host gives a new client.
///
/// A loaded editor is always a new client: [`LiveClient::new`] makes a fresh random
/// [`ClientId`], and a place in the session is only ever resumed by the same running client
/// after its connection dropped, so its local numbers and lasting ids carry on in memory. No
/// identifier or place is taken from the host, so a reload can never claim an old client's
/// numbers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveConfig {
    /// The format of the changes the editor makes and reads.
    pub format: String,
    /// The host's crash note: the entry the editor stopped in, if any.
    pub crashed_at: Option<Seq>,
}

/// What a call produced: the frames to send, in order, and what happened.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Step {
    pub send: Vec<String>,
    pub events: Vec<Event>,
}

/// Something the host may want to show or act on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// Reset the editor to the stored revision at `at` (none: the empty document), then call
    /// [`LiveClient::base_ready`].
    LoadBase { revision: Option<String>, at: Seq },
    /// The entry at `seq` was applied to the agreed state.
    Applied { seq: Seq, own: bool, effect: Effect },
    /// The server refused a change; it is no longer shown.
    Refused {
        local: LocalNo,
        reason: RefusedReason,
    },
    /// Waiting changes were dropped because the person may no longer edit.
    DroppedPending { count: usize },
    /// The editor stopped in the entry at `seq`.
    Crashed { seq: Seq },
    /// The document cannot be opened from the log.
    CannotOpen { seq: Seq },
    /// The client's place had ended; it has a new participant number.
    Returned {
        previous: ParticipantNo,
        now: ParticipantNo,
    },
    /// Who holds a place.
    People(Vec<LivePerson>),
    /// The person's right to edit changed.
    AccessChanged { can_edit: bool },
    /// The place ended after missed heartbeats; the client can join again.
    Expired,
    /// The client's protocol or format does not match the session's.
    Incompatible { protocol: u32, format: String },
    /// The server ended the connection; do not reconnect.
    Removed { reason: RemovedReason },
}

/// Where the client is in its session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Not connected.
    Offline,
    /// Connected, waiting for the join to be accepted.
    Joining,
    /// Waiting for the host to load the base.
    LoadingBase,
    /// Applying the entries the client missed.
    CatchingUp,
    /// Up to date; changes are sent as they are made.
    Live,
    /// Waiting for the log again from the base.
    Resyncing,
    /// The editor stopped in the entry at `at`; later entries are held.
    Stalled { at: Seq },
    /// Finished; every call is refused.
    Closed,
}

/// A frame the client cannot follow. The host drops the connection and reconnects.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum ClientError {
    /// The frame is not a live server message.
    #[error("malformed frame: {0}")]
    Malformed(String),
    /// An entry is not the next position in the log.
    #[error("entry {got:?} arrived where {expected:?} was expected")]
    OutOfOrder { expected: Seq, got: Seq },
    /// The server and the client disagree about this client's own changes.
    #[error("out of step with the server: {0}")]
    Desync(String),
}

/// Why a local change was not taken. Nothing changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum LocalError {
    /// The client is not up to date with the session.
    #[error("not live")]
    NotLive,
    /// The person may only view the document.
    #[error("read only")]
    ReadOnly,
    /// The change is over the size limit.
    #[error("the change is too large")]
    TooLarge,
    /// The editor stopped in the change; it was taken back.
    #[error("the editor stopped in the change")]
    Crashed,
}

/// An entry the server put in order, kept to be re-run.
#[derive(Clone, Debug)]
struct Ordered {
    seq: Seq,
    participant: ParticipantNo,
    own: bool,
    stamp: Stamp,
    payload: String,
}

impl Ordered {
    fn view(&self) -> EntryView<'_> {
        EntryView {
            seq: Some(self.seq),
            participant: self.participant,
            own: self.own,
            stamp: Some(self.stamp),
            payload: &self.payload,
        }
    }
}

/// An own change not yet confirmed.
#[derive(Clone, Debug)]
struct Pending {
    local: LocalNo,
    /// The last position the client had applied when the change was made.
    seen: Seq,
    payload: String,
    mode: Mode,
    /// Whether the entry frame went out on the current connection.
    sent: bool,
    /// Whether it is applied in what is shown now.
    shown: bool,
    /// The participant number it was shown under, which may differ from the one the server
    /// stored it under after the client returned with a new number.
    shown_as: ParticipantNo,
    /// Whether showing it read the (missing) stamp.
    read_stamp: bool,
    /// What showing it did.
    effect: Option<Effect>,
}

/// The library's side of one editor in a live session.
pub struct LiveClient<E: LiveEditor> {
    config: LiveConfig,
    /// This client's identifier, made fresh by [`LiveClient::new`].
    client: ClientId,
    phase: Phase,
    session: Option<LiveSessionId>,
    participant: Option<ParticipantNo>,
    ids: Option<IdMaker>,
    can_edit: bool,
    /// Whether the server accepted the join on the current connection.
    joined: bool,
    heartbeat_ms: u64,
    /// When the last heartbeat (or the join) was sent.
    last_heartbeat_ms: u64,
    /// When each of the latest entry frames was sent, oldest first; at most
    /// [`MAX_ENTRIES_PER_WINDOW`], all within the last [`PACE_WINDOW_MS`].
    entry_times: VecDeque<u64>,
    /// Whether the editor holds a state of this session (a base and entries on it).
    has_state: bool,
    /// A copy of the agreed state; `None` means what is shown is exactly the agreed state,
    /// with nothing folded and nothing waiting shown.
    agreed: Option<E::Snapshot>,
    agreed_seq: Seq,
    /// The last position in the log, as the latest catch-up named it.
    head: Seq,
    /// Own entries confirmed by the server and shown, but not yet in the agreed copy.
    folded: Vec<Ordered>,
    /// Own changes not yet confirmed, in order.
    pending: VecDeque<Pending>,
    next_local: u64,
    /// Entries received while the client cannot apply them yet.
    held: Vec<LiveEntry>,
    /// The base position named by the catch-up while loading the base.
    base_at: Seq,
}

impl<E: LiveEditor> LiveClient<E> {
    /// A client that has not connected yet.
    pub fn new(config: LiveConfig) -> Self {
        LiveClient {
            config,
            client: ClientId(uuid::Uuid::new_v4()),
            phase: Phase::Offline,
            session: None,
            participant: None,
            ids: None,
            can_edit: false,
            joined: false,
            heartbeat_ms: HEARTBEAT_MS,
            last_heartbeat_ms: 0,
            entry_times: VecDeque::new(),
            has_state: false,
            agreed: None,
            agreed_seq: Seq(0),
            head: Seq(0),
            folded: Vec::new(),
            pending: VecDeque::new(),
            next_local: 1,
            held: Vec::new(),
            base_at: Seq(0),
        }
    }

    /// The connection is open: returns the `live_join` frame.
    pub fn connected(&mut self, now_ms: u64) -> Vec<String> {
        if self.phase == Phase::Closed {
            return Vec::new();
        }
        self.phase = Phase::Joining;
        self.joined = false;
        self.held.clear();
        // The join is a sign of life: the next heartbeat is due a full interval after it.
        self.last_heartbeat_ms = now_ms;
        for pending in &mut self.pending {
            pending.sent = false;
        }
        let join = LiveClientMsg::LiveJoin {
            client: self.client,
            protocol: LIVE_PROTOCOL,
            format: self.config.format.clone(),
            resume: self.resume(),
            seen: self.has_state.then_some(self.agreed_seq),
        };
        vec![frame(&join)]
    }

    /// The connection dropped.
    pub fn disconnected(&mut self, now_ms: u64) {
        let _ = now_ms;
        if self.phase == Phase::Closed {
            return;
        }
        self.phase = Phase::Offline;
        self.joined = false;
        // Entries held for this connection come again in the next catch-up.
        self.held.clear();
        for pending in &mut self.pending {
            pending.sent = false;
        }
    }

    /// Handles one frame from the server.
    pub fn receive(
        &mut self,
        editor: &mut E,
        frame: &str,
        now_ms: u64,
    ) -> Result<Step, ClientError> {
        let mut step = Step::default();
        if self.phase == Phase::Closed {
            // Nothing more is read once closed, and nothing here asks the host to reconnect.
            return Ok(step);
        }
        let msg: LiveServerMsg = serde_json::from_str(frame)
            .map_err(|error| ClientError::Malformed(error.to_string()))?;
        match msg {
            LiveServerMsg::LiveJoined {
                session,
                participant,
                place,
                can_edit,
                heartbeat_ms,
                people,
            } => {
                self.joined_with(
                    editor,
                    session,
                    participant,
                    place,
                    can_edit,
                    heartbeat_ms,
                    &mut step,
                )?;
                step.events.push(Event::People(people));
            }
            LiveServerMsg::Catchup {
                base,
                revision,
                from,
                head,
            } => {
                if !self.joined {
                    return Err(ClientError::Desync(
                        "catch-up before the join was accepted".to_owned(),
                    ));
                }
                self.head = head;
                if self.has_state && from == self.agreed_seq {
                    self.phase = Phase::CatchingUp;
                } else {
                    self.phase = Phase::LoadingBase;
                    self.held.clear();
                    self.base_at = base;
                    step.events.push(Event::LoadBase { revision, at: base });
                }
                self.check_live(now_ms, &mut step);
            }
            LiveServerMsg::Entry { entry } => match self.phase {
                Phase::Live | Phase::CatchingUp => {
                    self.take_entry(editor, entry, &mut step)?;
                    self.check_live(now_ms, &mut step);
                }
                Phase::LoadingBase | Phase::Resyncing | Phase::Stalled { .. } => {
                    self.held.push(entry);
                }
                Phase::Offline | Phase::Joining | Phase::Closed => {
                    return Err(ClientError::Desync(format!(
                        "entry {:?} before the catch-up",
                        entry.seq
                    )));
                }
            },
            LiveServerMsg::Refused { local, reason } => {
                let first = self.pending.front().map(|pending| pending.local);
                // A refusal of a change the client already took off its queue (for example one
                // dropped when the person lost the right to edit) has nothing left to undo.
                let already_gone =
                    local.0 < self.next_local && first.is_none_or(|first| local.0 < first.0);
                if already_gone {
                    return Ok(step);
                }
                if first != Some(local) {
                    return Err(ClientError::Desync(format!(
                        "refusal of {local:?} while {first:?} is first in line"
                    )));
                }
                if let Some(refused) = self.pending.pop_front()
                    && refused.shown
                {
                    self.go_back(editor, None);
                }
                self.settle();
                step.events.push(Event::Refused { local, reason });
            }
            LiveServerMsg::CannotOpen { seq } => {
                self.phase = Phase::Closed;
                step.events.push(Event::CannotOpen { seq });
            }
            LiveServerMsg::People { people } => step.events.push(Event::People(people)),
            LiveServerMsg::Access { can_edit } => {
                self.can_edit = can_edit;
                if !can_edit {
                    self.drop_pending(editor, &mut step);
                }
                step.events.push(Event::AccessChanged { can_edit });
            }
            LiveServerMsg::Incompatible { protocol, format } => {
                self.phase = Phase::Closed;
                step.events.push(Event::Incompatible { protocol, format });
            }
            LiveServerMsg::Expired => {
                self.disconnected(now_ms);
                step.events.push(Event::Expired);
            }
            LiveServerMsg::Removed { reason } => {
                self.phase = Phase::Closed;
                step.events.push(Event::Removed { reason });
            }
        }
        Ok(step)
    }

    /// The host has reset the editor to the base named by [`Event::LoadBase`].
    pub fn base_ready(&mut self, editor: &mut E, now_ms: u64) -> Result<Step, ClientError> {
        let mut step = Step::default();
        if self.phase != Phase::LoadingBase {
            return Err(ClientError::Desync(
                "base ready while not loading the base".to_owned(),
            ));
        }
        // The editor shows the base exactly: nothing waiting is shown on it yet.
        self.agreed = None;
        self.folded.clear();
        self.agreed_seq = self.base_at;
        self.has_state = true;
        for pending in &mut self.pending {
            pending.shown = false;
        }
        self.phase = Phase::CatchingUp;
        let mut rest = std::mem::take(&mut self.held).into_iter();
        for entry in rest.by_ref() {
            self.take_entry(editor, entry, &mut step)?;
            if matches!(self.phase, Phase::Stalled { .. }) {
                break;
            }
        }
        self.held.extend(rest);
        if self
            .pending
            .iter()
            .any(|pending| pending.mode == Mode::ShowAtOnce && !pending.shown)
        {
            self.go_back(editor, None);
        }
        self.check_live(now_ms, &mut step);
        Ok(step)
    }

    /// The person made a change.
    pub fn local(
        &mut self,
        editor: &mut E,
        payload: String,
        mode: Mode,
        now_ms: u64,
    ) -> Result<Step, LocalError> {
        if self.phase != Phase::Live {
            return Err(LocalError::NotLive);
        }
        if !self.can_edit {
            return Err(LocalError::ReadOnly);
        }
        if payload.len() > MAX_PAYLOAD_BYTES {
            return Err(LocalError::TooLarge);
        }
        if self.agreed.is_none() {
            self.agreed = Some(editor.snapshot());
        }
        let (shown, read_stamp, effect) = match mode {
            Mode::ShowAtOnce => {
                let view = EntryView {
                    seq: None,
                    participant: self.own_participant(),
                    own: true,
                    stamp: None,
                    payload: &payload,
                };
                match editor.apply(&view) {
                    Ok(outcome) => (true, outcome.read_stamp, Some(outcome.effect)),
                    Err(Crash) => {
                        self.go_back(editor, None);
                        self.settle();
                        return Err(LocalError::Crashed);
                    }
                }
            }
            Mode::WaitForServer => (false, false, None),
        };
        let local = LocalNo(self.next_local);
        self.next_local = self.next_local.saturating_add(1);
        self.pending.push_back(Pending {
            local,
            seen: self.agreed_seq,
            payload,
            mode,
            sent: false,
            shown,
            shown_as: self.own_participant(),
            read_stamp,
            effect,
        });
        let mut step = Step::default();
        self.send_unsent(now_ms, &mut step.send);
        Ok(step)
    }

    /// Time passed: returns the heartbeat when it is due, and any changes not yet sent.
    ///
    /// The heartbeat goes every `heartbeat_ms` whatever else was sent. Changes go at most
    /// [`MAX_ENTRIES_PER_WINDOW`] in any [`PACE_WINDOW_MS`]; the ones held back by the pace go
    /// on a later tick, so the host calls this often (every few hundred milliseconds) while
    /// [`LiveClient::unsent`] is above zero, and at least once a second otherwise.
    ///
    /// Every `now_ms` the host passes must come from a monotonic clock (`performance.now()` in
    /// the browser, `Instant` on the desktop), never the wall clock. If it goes backwards all
    /// the same, the client starts its heartbeat and pacing timers again from the new time
    /// rather than stall.
    pub fn tick(&mut self, now_ms: u64) -> Vec<String> {
        self.clock_check(now_ms);
        let mut send = Vec::new();
        let connected =
            self.joined && !matches!(self.phase, Phase::Offline | Phase::Joining | Phase::Closed);
        if connected && now_ms.saturating_sub(self.last_heartbeat_ms) >= self.heartbeat_ms {
            self.last_heartbeat_ms = now_ms;
            send.push(frame(&LiveClientMsg::Heartbeat {
                seen: self.agreed_seq,
            }));
        }
        self.send_unsent(now_ms, &mut send);
        send
    }

    /// The person is leaving: returns the `leave` frame, and the client is closed.
    pub fn leave(&mut self) -> Option<String> {
        if self.phase == Phase::Closed {
            return None;
        }
        self.phase = Phase::Closed;
        Some(frame(&LiveClientMsg::Leave))
    }

    /// Makes `count` new lasting ids under this client's participant number.
    ///
    /// Refused with [`IdError::NoPlace`] before the first join is accepted and once the client
    /// is closed. Ids are never made under participant 0, which names the base rows: the
    /// client refuses a join that gives it that number.
    pub fn new_ids(&mut self, count: u32) -> Result<LiveId, IdError> {
        if self.phase == Phase::Closed {
            return Err(IdError::NoPlace);
        }
        self.ids.as_mut().ok_or(IdError::NoPlace)?.take(count)
    }

    /// Where the client is in its session.
    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// The last log position in the agreed state.
    pub fn agreed_seq(&self) -> Seq {
        self.agreed_seq
    }

    /// How many own changes wait for the server.
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// How many waiting changes have not gone out on the current connection.
    pub fn unsent(&self) -> usize {
        self.pending.iter().filter(|pending| !pending.sent).count()
    }

    /// This client's place, once joined.
    pub fn participant(&self) -> Option<ParticipantNo> {
        self.participant
    }

    /// The place this client names when it joins again after a dropped connection.
    pub fn resume(&self) -> Option<LiveResume> {
        match (self.session, self.participant) {
            (Some(session), Some(participant)) => Some(LiveResume {
                session,
                participant,
            }),
            _ => None,
        }
    }

    /// This client's identifier, made fresh for every new client.
    pub fn client(&self) -> ClientId {
        self.client
    }

    /// Whether the person may make changes.
    pub fn can_edit(&self) -> bool {
        self.can_edit
    }

    fn own_participant(&self) -> ParticipantNo {
        self.participant.unwrap_or(ParticipantNo(0))
    }

    /// Sends the waiting changes not yet sent on this connection, in order, as far as the pace
    /// allows. What is sent is always a prefix of the queue.
    fn send_unsent(&mut self, now_ms: u64, send: &mut Vec<String>) {
        if self.phase != Phase::Live || !self.can_edit {
            return;
        }
        self.clock_check(now_ms);
        while self
            .entry_times
            .front()
            .is_some_and(|&at| at.saturating_add(PACE_WINDOW_MS) <= now_ms)
        {
            self.entry_times.pop_front();
        }
        for pending in self.pending.iter_mut().filter(|pending| !pending.sent) {
            if self.entry_times.len() >= MAX_ENTRIES_PER_WINDOW {
                break;
            }
            self.entry_times.push_back(now_ms);
            pending.sent = true;
            send.push(frame(&LiveClientMsg::Entry {
                local: pending.local,
                seen: pending.seen,
                format: self.config.format.clone(),
                payload: pending.payload.clone(),
            }));
        }
    }

    /// Moves from catching up to live once the log's head is reached.
    fn check_live(&mut self, now_ms: u64, step: &mut Step) {
        if self.phase == Phase::CatchingUp && self.agreed_seq >= self.head {
            self.phase = Phase::Live;
            self.send_unsent(now_ms, &mut step.send);
        }
    }

    /// Starts the timers again from `now_ms` if the host's clock went back before them, so a
    /// clock that jumps back never stops the heartbeat or holds the pace window shut.
    fn clock_check(&mut self, now_ms: u64) {
        if now_ms < self.last_heartbeat_ms {
            self.last_heartbeat_ms = now_ms;
        }
        if self.entry_times.back().is_some_and(|&at| now_ms < at) {
            self.entry_times.clear();
        }
    }

    /// Takes the server's acceptance of a join.
    #[allow(clippy::too_many_arguments)]
    fn joined_with(
        &mut self,
        editor: &mut E,
        session: LiveSessionId,
        participant: ParticipantNo,
        place: Place,
        can_edit: bool,
        heartbeat_ms: u64,
        step: &mut Step,
    ) -> Result<(), ClientError> {
        if self.phase != Phase::Joining || self.joined {
            return Err(ClientError::Desync(
                "join accepted while not joining".to_owned(),
            ));
        }
        if participant.0 == 0 {
            // Participant 0 names the base rows; ids made under it would clash with them.
            return Err(ClientError::Desync(
                "participant 0 names the base document".to_owned(),
            ));
        }
        if place == Place::Kept && self.participant.is_some_and(|held| held != participant) {
            return Err(ClientError::Desync(format!(
                "place kept under {participant:?}, not {:?}",
                self.participant
            )));
        }
        // A kept place keeps its id maker, so ids carry on; any other number starts afresh.
        if self.ids.is_none() || self.participant != Some(participant) {
            self.ids = Some(IdMaker::new(participant));
        }
        let renumbered = self.participant.is_some_and(|held| held != participant);
        self.session = Some(session);
        self.participant = Some(participant);
        self.can_edit = can_edit;
        // A server's interval outside 1 s to 60 s would flood it or let the place expire.
        self.heartbeat_ms = heartbeat_ms.clamp(MIN_HEARTBEAT_MS, HEARTBEAT_MS);
        self.joined = true;
        if let Place::Returning { previous } = place {
            step.events.push(Event::Returned {
                previous,
                now: participant,
            });
        }
        if !can_edit {
            // The person lost the right to edit while away (S5).
            self.drop_pending(editor, step);
        } else if renumbered && self.pending.iter().any(|pending| pending.shown) {
            // Waiting changes are shown again as coming from the new number. They keep their
            // local numbers and `seen`, and go to the server as normal entries.
            self.go_back(editor, None);
        }
        Ok(())
    }

    /// Drops every waiting change because the person may no longer edit (S5).
    fn drop_pending(&mut self, editor: &mut E, step: &mut Step) {
        let count = self.pending.len();
        let any_shown = self.pending.iter().any(|pending| pending.shown);
        self.pending.clear();
        if any_shown {
            self.go_back(editor, None);
        }
        self.settle();
        if count > 0 {
            step.events.push(Event::DroppedPending { count });
        }
    }

    /// When nothing waits, what is shown is the agreed state, so no copy is needed.
    ///
    /// Only called when no waiting change is shown that the copy would have to go back over.
    fn settle(&mut self) {
        if self.pending.is_empty() {
            self.agreed = None;
            self.folded.clear();
        }
    }

    /// Applies the next entry of the log.
    fn take_entry(
        &mut self,
        editor: &mut E,
        entry: LiveEntry,
        step: &mut Step,
    ) -> Result<(), ClientError> {
        let expected = Seq(self.agreed_seq.0.saturating_add(1));
        if entry.seq != expected {
            return Err(ClientError::OutOfOrder {
                expected,
                got: entry.seq,
            });
        }
        let seq = entry.seq;
        let stamp = Stamp {
            at_ms: entry.at_ms,
            seed: entry.seed,
        };
        let (participant, client, local, payload, cancelled) = match entry.body {
            EntryBody::Cancel { .. } => {
                self.agreed_seq = seq;
                return Ok(());
            }
            EntryBody::Change {
                participant,
                client,
                local,
                payload,
                cancelled,
                ..
            } => (participant, client, local, payload, cancelled),
        };
        let own = client == self.client;
        if own && local.0 >= self.next_local {
            return Err(ClientError::Desync(format!(
                "own change {local:?} was never made"
            )));
        }
        let first = self.pending.front().map(|pending| pending.local);
        // An own change confirmed now must be the first in line. An own change with a lower
        // number was confirmed before (it is met again when catching up from the base) and
        // is applied like anyone's.
        let confirms = own && first.is_some_and(|first| local.0 >= first.0);
        if confirms && first != Some(local) {
            return Err(ClientError::Desync(format!(
                "own change {local:?} confirmed while {first:?} is first in line"
            )));
        }
        if confirms
            && !cancelled
            && self
                .pending
                .front()
                .is_some_and(|pending| pending.payload != payload)
        {
            return Err(ClientError::Desync(format!(
                "own change {local:?} came back with a different content"
            )));
        }
        if cancelled {
            // A cancelled change only moves the agreed position on.
            if confirms
                && let Some(taken) = self.pending.pop_front()
                && taken.shown
            {
                self.go_back(editor, None);
            }
            self.settle();
            self.agreed_seq = seq;
            return Ok(());
        }
        let ordered = Ordered {
            seq,
            participant,
            own,
            stamp,
            payload,
        };
        if confirms && let Some(confirmed) = self.pending.pop_front() {
            if confirmed.shown && !confirmed.read_stamp && confirmed.shown_as == participant {
                // Already shown as the server ordered it: keep it on top of the copy.
                self.folded.push(ordered);
                self.agreed_seq = seq;
                if self.pending.is_empty() {
                    self.settle();
                } else if self.folded.len() >= MAX_FOLDED {
                    self.go_back(editor, None);
                }
                step.events.push(Event::Applied {
                    seq,
                    own: true,
                    effect: confirmed.effect.unwrap_or(Effect::Applied),
                });
                return Ok(());
            }
            // It waited for the server, or its result depends on the stamp.
            let result = self.go_back(editor, Some(&ordered));
            self.after_received(result, seq, own, step);
            return Ok(());
        }
        let result = if self.agreed.is_none() {
            editor.note_applying(Some(seq));
            let result = editor.apply(&ordered.view());
            if result.is_ok() {
                editor.note_applying(None);
            }
            Some(result)
        } else {
            self.go_back(editor, Some(&ordered))
        };
        self.after_received(result, seq, own, step);
        Ok(())
    }

    /// Records the result of applying a received entry.
    fn after_received(
        &mut self,
        result: Option<Result<Outcome, Crash>>,
        seq: Seq,
        own: bool,
        step: &mut Step,
    ) {
        match result {
            Some(Ok(outcome)) => {
                self.agreed_seq = seq;
                step.events.push(Event::Applied {
                    seq,
                    own,
                    effect: outcome.effect,
                });
            }
            Some(Err(Crash)) => {
                self.phase = Phase::Stalled { at: seq };
                step.send.push(frame(&LiveClientMsg::Report { seq }));
                step.events.push(Event::Crashed { seq });
            }
            None => {}
        }
    }

    /// Goes back to the agreed copy, applies the folded entries and `extra` with their server
    /// stamps, takes a new copy, and re-runs the waiting changes shown at once. Returns what
    /// applying `extra` did.
    ///
    /// If `extra` stops the editor, nothing after it is applied, and the copy and the folded
    /// entries are kept, so going back again later starts from the same place.
    fn go_back(
        &mut self,
        editor: &mut E,
        extra: Option<&Ordered>,
    ) -> Option<Result<Outcome, Crash>> {
        if let Some(agreed) = &self.agreed {
            editor.restore(agreed);
        }
        self.apply_folded(editor);
        let result = match extra {
            Some(ordered) => {
                editor.note_applying(Some(ordered.seq));
                let result = editor.apply(&ordered.view());
                if result.is_err() {
                    return Some(result);
                }
                editor.note_applying(None);
                Some(result)
            }
            None => None,
        };
        self.folded.clear();
        if self.pending.is_empty() {
            self.agreed = None;
            return result;
        }
        let copy = editor.snapshot();
        self.rerun_pending(editor, &copy);
        self.agreed = Some(copy);
        result
    }

    /// Applies the folded entries again. Each was applied before without reading the stamp,
    /// so it applies alike.
    fn apply_folded(&self, editor: &mut E) {
        for folded in &self.folded {
            let _ = editor.apply(&folded.view());
        }
    }

    /// Re-runs the waiting changes shown at once on top of `copy`, which the editor shows.
    ///
    /// A waiting change whose re-run stops the editor is left out of what is shown until it
    /// is confirmed: the editor goes back to `copy` and re-runs the ones before it.
    fn rerun_pending(&mut self, editor: &mut E, copy: &E::Snapshot) {
        let participant = self.own_participant();
        for at in 0..self.pending.len() {
            let Some(pending) = self.pending.get(at) else {
                break;
            };
            if pending.mode != Mode::ShowAtOnce {
                continue;
            }
            let view = EntryView {
                seq: None,
                participant,
                own: true,
                stamp: None,
                payload: &pending.payload,
            };
            let result = editor.apply(&view);
            if result.is_err() {
                editor.restore(copy);
                for earlier in self.pending.iter().take(at).filter(|earlier| earlier.shown) {
                    let _ = editor.apply(&EntryView {
                        seq: None,
                        participant,
                        own: true,
                        stamp: None,
                        payload: &earlier.payload,
                    });
                }
            }
            let Some(pending) = self.pending.get_mut(at) else {
                break;
            };
            match result {
                Ok(outcome) => {
                    pending.shown = true;
                    pending.shown_as = participant;
                    pending.read_stamp = outcome.read_stamp;
                    pending.effect = Some(outcome.effect);
                }
                Err(Crash) => {
                    pending.shown = false;
                    pending.read_stamp = false;
                    pending.effect = None;
                }
            }
        }
    }
}

/// The text of a client frame. Serialising a client message cannot fail: every field is a
/// string, a number, an identifier or an option of one.
fn frame(msg: &LiveClientMsg) -> String {
    serde_json::to_string(msg).unwrap_or_default()
}
