//! The opaque-entry live room.

use std::collections::HashMap;
use std::time::Duration;

use axum::extract::ws::Utf8Bytes;
use collab_core::ids::{ClientId, DocId, Seq, SessionId, UserId};
use protocol::RemovedReason;
use protocol::live::{
    EntryBody, HEARTBEAT_MS, LIVE_PROTOCOL, LiveEntry, LivePerson, LiveResume, LiveServerMsg,
    LiveSessionId, LocalNo, MAX_PAYLOAD_BYTES, ParticipantNo, Place, RefusedReason, valid_format,
};
use records::{
    Access, DbError, GoneReason, LiveEntryRow, LiveLoaded, LiveLocal, NewEvent, NewLiveChange,
};
use time::OffsetDateTime;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use uuid::Uuid;

use crate::events::EventSink;
use crate::live_store::LiveStore;
use crate::members::{Admission, Frames, Member, Members};
use crate::room::{ConnId, JoinReply, Outbound};

pub(crate) struct LiveTiming {
    pub heartbeat: Duration,
    pub misses: u32,
}

pub(crate) const RATE_PER_SECOND: u32 = 30;
pub(crate) const RATE_BURST: u32 = 120;

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
    name: String,
    can_edit: bool,
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
    format: Option<String>,
    entries: Vec<LiveEntry>,
    session: Option<Session>,
    known_places: HashMap<ParticipantNo, ClientId>,
    clients: HashMap<ClientId, (UserId, u64)>,
    rates: HashMap<ClientId, RateBucket>,
    pending_ends: Vec<(LiveSessionId, ParticipantNo, GoneReason)>,
}

impl<S: LiveStore> LiveRoom<S> {
    pub(crate) async fn load(
        doc: DocId,
        store: S,
        events: EventSink,
        timing: LiveTiming,
    ) -> Result<Self, RoomFailed> {
        let loaded = store.load_live(doc).await?;
        Self::from_loaded(doc, store, events, timing, loaded)
    }

    fn from_loaded(
        doc: DocId,
        store: S,
        events: EventSink,
        timing: LiveTiming,
        loaded: LiveLoaded,
    ) -> Result<Self, RoomFailed> {
        let entries = loaded
            .entries
            .into_iter()
            .map(wire_entry)
            .collect::<Result<Vec<_>, _>>()?;
        let mut known_places = HashMap::new();
        let mut clients = loaded
            .clients
            .iter()
            .map(|(client, user, next)| (*client, (*user, *next)))
            .collect::<HashMap<_, _>>();
        let mut members = Members::new(doc, events.clone());
        for place in &loaded.participants {
            known_places.insert(ParticipantNo(place.number), place.client);
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
                            name: String::new(),
                            can_edit: false,
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
        Ok(Self {
            doc,
            store,
            events: events.clone(),
            timing,
            members,
            head: loaded.head,
            verified_through: loaded.verified_through,
            format: loaded.format,
            entries,
            session,
            known_places,
            clients,
            rates: HashMap::new(),
            pending_ends: Vec::new(),
        })
    }

    pub(crate) async fn handle(&mut self, cmd: LiveCommand) -> Result<(), RoomFailed> {
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
                self.note_seen(conn, client, seen);
                Ok(())
            }
            LiveCommand::Report { conn, client, seq } => {
                self.note_life(conn, client);
                self.note_seen(conn, client, seq);
                Ok(())
            }
            LiveCommand::Resync { conn, client } => {
                self.note_life(conn, client);
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
        }
    }

    pub(crate) async fn settle(&mut self) -> Result<(), RoomFailed> {
        let at = OffsetDateTime::now_utc();
        while let Some((session, participant, reason)) = self.pending_ends.first().copied() {
            self.store
                .end_live_place(session.0, participant.0, reason, at)
                .await?;
            self.pending_ends.remove(0);
            self.apply_place_end(session, participant, reason);
        }
        let should_end = self
            .session
            .as_ref()
            .is_some_and(|session| session.places.values().all(|place| !place.held));
        if should_end && let Some(session) = &self.session {
            let session_id = session.id;
            let entries = self
                .head
                .0
                .saturating_sub(session.first_seq.0.saturating_sub(1));
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
        Ok(())
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
            session.places.insert(
                participant,
                LivePlace {
                    client: request.client,
                    user: request.user,
                    name: request.name.clone(),
                    can_edit: request.role.can_edit(),
                    held: true,
                    seen: request.seen.unwrap_or(Seq(0)).min(self.head),
                    last_heard: Instant::now(),
                },
            );
        }
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
                revision: None,
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
        self.broadcast_people();
        Ok(())
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
        let Some(participant) = self.current_participant(conn, client) else {
            return Ok(());
        };
        self.note_life(conn, client);
        let (user, can_edit) = match self
            .members
            .iter()
            .find(|member| member.conn == conn && member.client == client)
        {
            Some(member) => (member.user, member.role.can_edit()),
            None => return Ok(()),
        };
        self.note_seen(conn, client, seen);

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
        self.advance_local(client, user, local);
        let _ = self.members.send(
            None,
            &Outbound::One(encode(&LiveServerMsg::Entry { entry })),
        );
        Ok(())
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

    fn note_seen(&mut self, conn: ConnId, client: ClientId, seen: Seq) {
        let Some(participant) = self.current_participant(conn, client) else {
            return;
        };
        if let Some(place) = self
            .session
            .as_mut()
            .and_then(|session| session.places.get_mut(&participant))
        {
            place.seen = place.seen.max(seen.min(self.head));
        }
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
        Ok(())
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
        if reason == GoneReason::Expired {
            let _ = self.members.send(
                Some(client),
                &Outbound::One(encode(&LiveServerMsg::Expired)),
            );
            self.events.emit(NewEvent {
                kind: "live.client_expired",
                actor: None,
                subject: Some(place.user),
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
    }
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

#[cfg(test)]
#[allow(
    clippy::too_many_arguments,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]
mod tests {
    use std::time::Duration;

    use collab_core::ids::{ClientId, DocId, Seq, SessionId, UserId};
    use protocol::live::{
        EntryBody, LiveResume, LiveServerMsg, LocalNo, ParticipantNo, Place, RefusedReason,
    };
    use tokio::sync::{mpsc, oneshot};
    use uuid::Uuid;

    use super::{LiveCommand, LiveRoom, LiveTiming};
    use crate::events::EventSink;
    use crate::live_store::MemStore;
    use crate::members::builtin_access;
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
    }

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn timing() -> LiveTiming {
        LiveTiming {
            heartbeat: Duration::from_secs(60),
            misses: 3,
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

    async fn join_as(
        room: &mut LiveRoom<MemStore>,
        conn: u64,
        client_n: u128,
        user_n: u128,
        role: &str,
        format: &str,
        resume: Option<LiveResume>,
        seen: Option<Seq>,
    ) -> Joined {
        let client = ClientId(id(client_n));
        let user = UserId(id(user_n));
        let (tx, rx) = mpsc::channel(512);
        let (accepted, answer) = oneshot::channel();
        room.handle(LiveCommand::Join {
            conn,
            client,
            user,
            session: SessionId(id(user_n + 1000)),
            name: format!("User {user_n}"),
            role: builtin_access(role),
            protocol: 1,
            format: format.to_owned(),
            resume,
            seen,
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
        let _ = drain(&mut joined.rx);
        joined
    }

    async fn entry(
        room: &mut LiveRoom<MemStore>,
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
