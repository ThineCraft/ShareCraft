//! A small in-memory orderer that plays the server for the library's tests.
//!
//! It keeps one ordered log, stamps each change with a counter time and a seed, drops a
//! repeat of a stored change by (client, local), refuses a viewer's changes, cancels a change
//! as soon as an editor reports it, and serves `catchup` and `resync`.
//!
//! Two of its rules differ from the room's table in plan G2's Task 8, because the library
//! cannot converge without them: a client's numbers must increase but may skip (a client
//! drops waiting changes when its person loses the right to edit, and those may never have
//! reached the server), and a resent number that was refused is refused again (the first
//! refusal may have been lost with the connection) rather than ignored. Frames for each
//! connection wait in an outbox until the test delivers them, so a test chooses when each one
//! arrives. It is an honest partner for the library, not a model of the real room (the
//! server's own tests cover that): a frame an honest client would never send fails the test.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet, VecDeque};

use collab_core::ids::{ClientId, Seq};
use protocol::live::{
    EntryBody, HEARTBEAT_MS, LIVE_PROTOCOL, LiveClientMsg, LiveEntry, LiveServerMsg, LiveSessionId,
    LocalNo, ParticipantNo, Place, RefusedReason,
};

/// One connection to the simulated server.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ConnId(u64);

struct Conn {
    /// The client that joined on this connection, and its place.
    joined: Option<(ClientId, ParticipantNo)>,
    /// Frames waiting to be delivered, oldest first.
    outbox: VecDeque<String>,
    /// Whether the server still reads this connection's frames. A connection whose place
    /// expired is closing: its last frames still go out, but nothing more is read.
    open: bool,
}

struct PlaceRec {
    client: ClientId,
    ended: bool,
}

/// The simulated server.
pub struct Sim {
    session: LiveSessionId,
    format: String,
    log: Vec<LiveEntry>,
    /// Places by participant number, from 1.
    places: Vec<PlaceRec>,
    next_local: HashMap<ClientId, u64>,
    stored: HashSet<(ClientId, u64)>,
    refused: HashMap<(ClientId, u64), RefusedReason>,
    viewers: HashSet<ClientId>,
    /// A durable return barrier whose readiness may need repeating after a dropped connection.
    return_ready: HashMap<ClientId, LocalNo>,
    conns: HashMap<ConnId, Conn>,
    next_conn: u64,
    clock: i64,
    /// Every report received, as (participant, seq).
    pub reports: Vec<(ParticipantNo, Seq)>,
    /// How many resyncs were served.
    pub resyncs: usize,
    /// How many joins were placed as returning.
    pub returning: usize,
    /// How many changes were refused.
    pub refusals: usize,
    /// Whether a reported change waits for [`Sim::give_verdicts`] instead of being cancelled
    /// at once, so a verdict can reach a client long after its report, for example while it
    /// loads the base again.
    pub delay_verdicts: bool,
    /// Reported changes waiting for their verdict.
    awaiting: Vec<Seq>,
}

impl Sim {
    pub fn new(format: &str) -> Sim {
        Sim {
            session: serde_json::from_value(serde_json::json!(
                "00000000-0000-0000-0000-00000000005e"
            ))
            .unwrap_or_else(|error| panic!("session id: {error}")),
            format: format.to_owned(),
            log: Vec::new(),
            places: Vec::new(),
            next_local: HashMap::new(),
            stored: HashSet::new(),
            refused: HashMap::new(),
            viewers: HashSet::new(),
            return_ready: HashMap::new(),
            conns: HashMap::new(),
            next_conn: 1,
            clock: 1_763_000_000_000,
            reports: Vec::new(),
            resyncs: 0,
            returning: 0,
            refusals: 0,
            delay_verdicts: false,
            awaiting: Vec::new(),
        }
    }

    /// The log in order.
    pub fn log(&self) -> &[LiveEntry] {
        &self.log
    }

    fn head(&self) -> Seq {
        Seq(self.log.len() as u64)
    }

    /// A new connection.
    pub fn connect(&mut self) -> ConnId {
        let id = ConnId(self.next_conn);
        self.next_conn += 1;
        self.conns.insert(
            id,
            Conn {
                joined: None,
                outbox: VecDeque::new(),
                open: true,
            },
        );
        id
    }

    /// The connection dropped: frames still waiting for it are lost.
    pub fn drop_conn(&mut self, conn: ConnId) {
        self.conns.remove(&conn);
    }

    /// The next frame waiting for `conn`, if any.
    pub fn next_out(&mut self, conn: ConnId) -> Option<String> {
        let state = self.conns.get_mut(&conn)?;
        let frame = state.outbox.pop_front()?;
        let client = state.joined.map(|(client, _)| client);
        if matches!(
            serde_json::from_str(&frame),
            Ok(LiveServerMsg::ReturnReady { .. })
        ) && let Some(client) = client
        {
            self.return_ready.remove(&client);
        }
        Some(frame)
    }

    /// How many frames wait for `conn`.
    pub fn waiting(&self, conn: ConnId) -> usize {
        self.conns.get(&conn).map_or(0, |conn| conn.outbox.len())
    }

    /// Ends `client`'s place, as after three missed heartbeats. An open connection gets
    /// `expired` as its last frame and nothing more of it is read.
    pub fn expire(&mut self, client: ClientId) {
        for place in &mut self.places {
            if place.client == client {
                place.ended = true;
            }
        }
        let expired = frame(&LiveServerMsg::Expired);
        for conn in self.conns.values_mut() {
            if conn.open && conn.joined.is_some_and(|(held, _)| held == client) {
                conn.open = false;
                conn.outbox.push_back(expired.clone());
            }
        }
    }

    /// Changes whether `client`'s person may edit; open connections are told.
    pub fn set_can_edit(&mut self, client: ClientId, can_edit: bool) {
        let changed = if can_edit {
            self.viewers.remove(&client)
        } else {
            self.viewers.insert(client)
        };
        if !changed {
            return;
        }
        let access = frame(&LiveServerMsg::Access { can_edit });
        for conn in self.conns.values_mut() {
            if conn.open && conn.joined.is_some_and(|(held, _)| held == client) {
                conn.outbox.push_back(access.clone());
            }
        }
    }

    /// Takes one frame from `conn`.
    pub fn receive(&mut self, conn: ConnId, text: &str) {
        let Some(state) = self.conns.get(&conn) else {
            return;
        };
        if !state.open {
            return;
        }
        let msg: LiveClientMsg = serde_json::from_str(text)
            .unwrap_or_else(|error| panic!("the client sent a malformed frame {text}: {error}"));
        match msg {
            LiveClientMsg::LiveJoin {
                client,
                protocol,
                format,
                resume,
                seen,
                can_checkpoint,
                returning_through,
                ..
            } => self.join(
                conn,
                client,
                protocol,
                &format,
                resume,
                seen,
                can_checkpoint,
                returning_through,
            ),
            LiveClientMsg::Entry {
                local,
                seen,
                format,
                payload,
            } => self.entry(conn, local, seen, &format, payload),
            LiveClientMsg::Heartbeat { seen } => {
                if self.place_of(conn).is_some() {
                    assert!(seen <= self.head(), "heartbeat seen {seen:?} is ahead");
                }
            }
            LiveClientMsg::Report { seq } => self.report(conn, seq),
            LiveClientMsg::Fingerprint { .. }
            | LiveClientMsg::CheckpointReady { .. }
            | LiveClientMsg::CheckpointFailed { .. } => {
                panic!("checkpoint controls are not implemented by the G2 simulator")
            }
            LiveClientMsg::Resync => {
                if self.place_of(conn).is_some() {
                    self.resyncs += 1;
                    self.send_catchup(conn, Seq(0));
                }
            }
            LiveClientMsg::Leave => {
                if let Some((_, participant)) = self.place_of(conn)
                    && let Some(place) = self.place_mut(participant)
                {
                    place.ended = true;
                }
                self.conns.remove(&conn);
            }
        }
    }

    /// The client and place joined on `conn`, while the place is held.
    fn place_of(&self, conn: ConnId) -> Option<(ClientId, ParticipantNo)> {
        let (client, participant) = self.conns.get(&conn)?.joined?;
        let place = self.places.get(participant.0 as usize - 1)?;
        (!place.ended).then_some((client, participant))
    }

    fn place_mut(&mut self, participant: ParticipantNo) -> Option<&mut PlaceRec> {
        self.places
            .get_mut((participant.0 as usize).checked_sub(1)?)
    }

    fn new_returning(&mut self, client: ClientId) -> ParticipantNo {
        self.returning += 1;
        self.new_place(client)
    }

    fn new_place(&mut self, client: ClientId) -> ParticipantNo {
        self.places.push(PlaceRec {
            client,
            ended: false,
        });
        ParticipantNo(self.places.len() as u32)
    }

    #[allow(clippy::too_many_arguments)]
    fn join(
        &mut self,
        conn: ConnId,
        client: ClientId,
        protocol: u32,
        format: &str,
        resume: Option<protocol::live::LiveResume>,
        seen: Option<Seq>,
        _can_checkpoint: bool,
        returning_through: Option<LocalNo>,
    ) {
        assert_eq!(protocol, LIVE_PROTOCOL);
        assert_eq!(format, self.format);
        let (participant, place) = match resume {
            Some(resume)
                if resume.session == self.session
                    && self
                        .places
                        .get(resume.participant.0 as usize - 1)
                        .is_some_and(|place| place.client == client && !place.ended) =>
            {
                (resume.participant, Place::Kept)
            }
            Some(resume) => (
                self.new_returning(client),
                Place::Returning {
                    previous: resume.participant,
                },
            ),
            None => (self.new_place(client), Place::New),
        };
        let joined = frame(&LiveServerMsg::LiveJoined {
            session: self.session,
            participant,
            place,
            can_edit: !self.viewers.contains(&client),
            heartbeat_ms: HEARTBEAT_MS,
            people: Vec::new(),
        });
        let Some(state) = self.conns.get_mut(&conn) else {
            return;
        };
        state.joined = Some((client, participant));
        state.outbox.push_back(joined);
        let head = self.head();
        let from = match seen {
            Some(seen) if seen <= head => seen,
            _ => Seq(0),
        };
        self.send_catchup(conn, from);
        if matches!(place, Place::Returning { .. })
            && let Some(through) = returning_through
        {
            self.return_ready.insert(client, through);
        }
        let ready = returning_through.filter(|through| {
            self.return_ready
                .get(&client)
                .is_some_and(|held| held == through)
        });
        if let Some(through) = ready
            && let Some(state) = self.conns.get_mut(&conn)
        {
            state
                .outbox
                .push_back(frame(&LiveServerMsg::ReturnReady { through }));
        }
    }

    /// Sends `catchup` and every entry after `from`, cancelled changes emptied.
    fn send_catchup(&mut self, conn: ConnId, from: Seq) {
        let head = self.head();
        let mut frames = vec![frame(&LiveServerMsg::Catchup {
            base: Seq(0),
            revision: None,
            from,
            head,
        })];
        for entry in self.log.iter().skip(from.0 as usize) {
            frames.push(frame(&LiveServerMsg::Entry {
                entry: as_sent(entry),
            }));
        }
        if let Some(state) = self.conns.get_mut(&conn) {
            state.outbox.extend(frames);
        }
    }

    fn entry(&mut self, conn: ConnId, local: LocalNo, seen: Seq, format: &str, payload: String) {
        let Some((client, participant)) = self.place_of(conn) else {
            // From a place that has ended: ignored, not stored.
            return;
        };
        self.return_ready.remove(&client);
        assert_eq!(format, self.format);
        let expected = *self.next_local.get(&client).unwrap_or(&1);
        if local.0 < expected {
            if !self.stored.contains(&(client, local.0)) {
                // A refusal is final: the same number is refused again.
                let reason = self
                    .refused
                    .get(&(client, local.0))
                    .copied()
                    .unwrap_or(RefusedReason::OutOfOrder);
                self.push(conn, frame(&LiveServerMsg::Refused { local, reason }));
            }
            return;
        }
        // A number above the next one is taken too: the numbers between are changes the
        // client dropped before they reached the server (the person lost the right to edit).
        self.next_local.insert(client, local.0 + 1);
        if self.viewers.contains(&client) {
            self.refused
                .insert((client, local.0), RefusedReason::ReadOnly);
            self.push(
                conn,
                frame(&LiveServerMsg::Refused {
                    local,
                    reason: RefusedReason::ReadOnly,
                }),
            );
            return;
        }
        assert!(seen <= self.head(), "change made against {seen:?}, ahead");
        self.stored.insert((client, local.0));
        let body = EntryBody::Change {
            participant,
            client,
            local,
            seen,
            payload,
            cancelled: false,
        };
        self.append(body);
    }

    fn report(&mut self, conn: ConnId, seq: Seq) {
        let Some((_, participant)) = self.place_of(conn) else {
            return;
        };
        self.reports.push((participant, seq));
        if self.delay_verdicts {
            if !self.awaiting.contains(&seq) {
                self.awaiting.push(seq);
            }
            return;
        }
        self.cancel(seq);
    }

    /// Cancels every reported change still waiting for its verdict.
    pub fn give_verdicts(&mut self) {
        for seq in std::mem::take(&mut self.awaiting) {
            self.cancel(seq);
        }
    }

    /// Cancels the change at `seq`, unless it is not a change or is cancelled already.
    fn cancel(&mut self, seq: Seq) {
        let Some(reported) = self.log.get_mut((seq.0 as usize).wrapping_sub(1)) else {
            return;
        };
        let EntryBody::Change { cancelled, .. } = &mut reported.body else {
            return;
        };
        if *cancelled {
            return;
        }
        // The simulated verdict: cancelled at the first report.
        *cancelled = true;
        self.append(EntryBody::Cancel {
            cancels: seq,
            reports: 1,
        });
    }

    /// Stamps and stores an entry, then sends it to every joined connection.
    fn append(&mut self, body: EntryBody) {
        self.clock += 7;
        let seq = Seq(self.log.len() as u64 + 1);
        let entry = LiveEntry {
            seq,
            at_ms: self.clock,
            seed: (seq.0 as u32).wrapping_mul(2_654_435_761),
            body,
        };
        let sent = frame(&LiveServerMsg::Entry {
            entry: as_sent(&entry),
        });
        self.log.push(entry);
        let joined: Vec<ConnId> = self
            .conns
            .iter()
            .filter(|(_, conn)| conn.open && conn.joined.is_some())
            .map(|(id, _)| *id)
            .collect();
        for id in joined {
            if self.place_of(id).is_some() {
                self.push(id, sent.clone());
            }
        }
    }

    fn push(&mut self, conn: ConnId, text: String) {
        if text.contains("\"type\":\"refused\"") {
            self.refusals += 1;
        }
        if let Some(state) = self.conns.get_mut(&conn) {
            state.outbox.push_back(text);
        }
    }
}

/// An entry as the server sends it: a cancelled change goes out empty.
fn as_sent(entry: &LiveEntry) -> LiveEntry {
    let mut sent = entry.clone();
    if let EntryBody::Change {
        payload, cancelled, ..
    } = &mut sent.body
        && *cancelled
    {
        payload.clear();
    }
    sent
}

fn frame(msg: &LiveServerMsg) -> String {
    serde_json::to_string(msg).unwrap_or_else(|error| panic!("server frame: {error}"))
}
