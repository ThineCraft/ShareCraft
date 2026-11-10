//! One file session per open file document: a document opened in a real editor by one person
//! at a time.
//!
//! The session holds no content, only the document's current revision, its stored fields and
//! one editing seat. Whoever may edit and joins while the seat is free takes it, with a fresh
//! random token; everyone else watches. The seat is freed when its holder leaves, is removed or
//! loses the right to edit, and nobody is given it then: a connection takes a free seat only
//! by joining again, because its copy of the file may be old. The holder saves by handing the
//! session the file's bytes with its token ([`Rooms::save_file`]); the session writes them as
//! one checkpoint on a blocking task, one save at a time.
//!
//! Access changes reach a file session on the same channel and with the same meaning as a text
//! room (see [`AccessChange`]). If a save finds the document saved from elsewhere, the session
//! is in conflict: every later save is refused, and it ends as soon as it is empty.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::Utf8Bytes;
use collab_core::ids::{ClientId, DocId, SessionId, UserId};
use protocol::{FileServerMsg, Participant, RemovedReason, SaveState, SeatState};
use records::{NewEvent, Role};
use revisions::{Author, Checkpoint, Outcome, RevisionId};
use serde_json::{Map, Value};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use crate::checkpoint::{Checkpointer, WriteError};
use crate::events::{EventSink, error_text};
use crate::projection::Recorder;
use crate::room::{
    AccessChange, ConnId, JoinReply, Link, Outbound, Place, Rooms, Running, Storage, credited_name,
    finished, grace_end, sleep_until,
};

/// Commands queued for a session before connections wait to send more.
const COMMAND_CAPACITY: usize = 256;
/// Random bytes in a seat token.
const TOKEN_BYTES: usize = 32;

/// What a connection, or a save, asks of a file session. Not `Debug`: a save carries the seat
/// token.
pub(crate) enum FileCommand {
    /// Enter the session, replacing any connection with the same client id. The connection has
    /// looked up who is joining and their role on the document.
    Join {
        conn: ConnId,
        client: ClientId,
        user: UserId,
        session: SessionId,
        /// The account's name.
        name: String,
        role: Role,
        tx: mpsc::Sender<Outbound>,
        /// Answered once the join is handled; dropped if the session ended first, in which
        /// case the connection asks again.
        accepted: oneshot::Sender<JoinReply>,
    },
    /// The connection ended.
    Leave { conn: ConnId, client: ClientId },
    /// Store `bytes` as the document's next revision, on behalf of the seat holder.
    #[allow(dead_code)] // used by Task 4's content route
    Save {
        user: UserId,
        token: String,
        /// The revision the bytes were made from.
        base: Option<RevisionId>,
        bytes: Vec<u8>,
        /// Answered once the save is decided; dropped if the session ended first.
        reply: oneshot::Sender<SaveReply>,
    },
}

/// How a session answers a save.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SaveReply {
    /// The bytes are stored as this new revision.
    Saved(RevisionId),
    /// The bytes equal the stored ones; this is still the current revision.
    Unchanged(RevisionId),
    /// No session is open, or the caller does not hold the seat with that token.
    NotHolder,
    /// The save was not made from the current revision, or the document was saved from
    /// elsewhere; `current` is the revision the session knows of.
    Conflict { current: Option<RevisionId> },
    /// The store failed; nothing was written.
    Failed,
}

/// The ways into one file session.
#[derive(Clone, Debug)]
pub(crate) struct FileLink {
    /// Joins, leaves and saves, handled in arrival order. Bounded: senders wait when the
    /// session falls behind.
    pub commands: mpsc::Sender<FileCommand>,
    /// Access changes, handled before anything else waiting; see [`crate::room::RoomLink`].
    pub access: mpsc::UnboundedSender<AccessChange>,
}

impl FileLink {
    /// A link whose session has ended: every send to it fails.
    pub(crate) fn closed() -> Self {
        let (commands, _) = mpsc::channel(1);
        let (access, _) = mpsc::unbounded_channel();
        Self { commands, access }
    }
}

/// Starts the session for the registry entry `place`, storing revisions under `name`, the
/// document's name. Once empty, it stays open for `grace`.
pub(crate) fn start(place: Place, name: String, storage: Storage, grace: Duration) -> FileLink {
    let (commands, rx) = mpsc::channel(COMMAND_CAPACITY);
    let (access, access_rx) = mpsc::unbounded_channel();
    tokio::spawn(run(place, name, rx, access_rx, storage, grace));
    FileLink { commands, access }
}

impl Rooms {
    /// Asks `doc`'s file session to store `bytes`, made from the revision `base`, for `user`
    /// holding the seat with `token`. `NotHolder` if no session is open (none is opened for
    /// this), or if it ends before answering.
    #[allow(dead_code)] // used by Task 4's content route
    pub(crate) async fn save_file(
        &self,
        doc: DocId,
        user: UserId,
        token: &str,
        base: Option<RevisionId>,
        bytes: Vec<u8>,
    ) -> SaveReply {
        let Some(Link::File(link)) = self.link(doc) else {
            return SaveReply::NotHolder;
        };
        let (reply, answer) = oneshot::channel();
        let save = FileCommand::Save {
            user,
            token: token.to_owned(),
            base,
            bytes,
            reply,
        };
        if link.commands.send(save).await.is_err() {
            return SaveReply::NotHolder;
        }
        answer.await.unwrap_or(SaveReply::NotHolder)
    }
}

/// Someone connected.
struct Member {
    conn: ConnId,
    client: ClientId,
    user: UserId,
    /// The sign-in session the connection belongs to.
    session: SessionId,
    name: String,
    /// What this person may do; it can change while they are connected.
    role: Role,
    tx: mpsc::Sender<Outbound>,
}

/// The editing seat: the connection holding it and the token that proves it.
struct Seat {
    client: ClientId,
    user: UserId,
    /// The holder's credited name, the author of what they save.
    name: String,
    token: String,
}

/// A save waiting for its turn.
struct Waiting {
    user: UserId,
    token: String,
    base: Option<RevisionId>,
    bytes: Vec<u8>,
    reply: oneshot::Sender<SaveReply>,
}

/// One write to the store, taken from the session when it starts.
struct WriteJob {
    bytes: Vec<u8>,
    base: Option<RevisionId>,
    author: Author,
    fields: Map<String, Value>,
}

impl WriteJob {
    /// Writes the job as a checkpoint of `doc` named `file_name`, with its author the only one
    /// credited. Blocks.
    fn write(
        &self,
        doc: DocId,
        file_name: &str,
        store: &dyn Checkpointer,
    ) -> Result<Outcome, revisions::CheckpointError> {
        store.write(&Checkpoint {
            doc,
            file_name,
            bytes: &self.bytes,
            base: self.base.as_ref(),
            authors: std::slice::from_ref(&self.author),
            fields: &self.fields,
        })
    }
}

/// The state of one open file document. Handling a command never blocks: frames are queued
/// with `try_send` and a connection whose queue is full or closed is dropped.
struct FileRoom {
    doc: DocId,
    /// The current revision: loaded when the session started, then each one saved.
    revision: Option<RevisionId>,
    /// The document fields stored with the loaded revision; every save keeps them.
    fields: Map<String, Value>,
    /// In joining order.
    members: Vec<Member>,
    seat: Option<Seat>,
    /// The account that first joined with each client id, kept for the session's lifetime, so
    /// nobody else can take over a connection's place.
    client_owners: HashMap<ClientId, UserId>,
    events: EventSink,
    /// Saves that arrived while another was being written, in arrival order.
    waiting: VecDeque<Waiting>,
    /// The answer owed for the save being written, and who is credited with it.
    writing: Option<(oneshot::Sender<SaveReply>, String)>,
    /// Set once a save found the document saved from elsewhere, to the revision found then.
    conflict: Option<Option<RevisionId>>,
}

impl FileRoom {
    fn new(doc: DocId, revision: Option<RevisionId>, fields: Map<String, Value>) -> Self {
        Self {
            doc,
            revision,
            fields,
            members: Vec::new(),
            seat: None,
            client_owners: HashMap::new(),
            events: EventSink::discard(),
            waiting: VecDeque::new(),
            writing: None,
            conflict: None,
        }
    }

    fn with_events(mut self, events: EventSink) -> Self {
        self.events = events;
        self
    }

    /// An event of this document about `subject`, done by `actor`.
    fn emit(&self, kind: &'static str, actor: Option<UserId>, subject: UserId, detail: Value) {
        self.events.emit(NewEvent {
            kind,
            actor,
            subject: Some(subject),
            document: Some(self.doc),
            detail,
        });
    }

    /// An event of this document about saving, with no person involved.
    fn emit_save(&self, kind: &'static str, detail: Value) {
        self.events.emit(NewEvent {
            kind,
            actor: None,
            subject: None,
            document: Some(self.doc),
            detail,
        });
    }

    fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    fn in_conflict(&self) -> bool {
        self.conflict.is_some()
    }

    /// Whether no save is being written or waiting.
    fn idle(&self) -> bool {
        self.writing.is_none() && self.waiting.is_empty()
    }

    fn handle(&mut self, cmd: FileCommand) {
        match cmd {
            FileCommand::Join {
                conn,
                client,
                user,
                session,
                name,
                role,
                tx,
                accepted,
            } => {
                let owner = *self.client_owners.entry(client).or_insert(user);
                let reply = if owner == user {
                    let member = Member {
                        conn,
                        client,
                        user,
                        session,
                        name: credited_name(&name),
                        role,
                        tx,
                    };
                    self.join(member);
                    JoinReply::Accepted
                } else {
                    JoinReply::Refused
                };
                // The connection may have gone away meanwhile; nothing to do then.
                let _ = accepted.send(reply);
            }
            FileCommand::Leave { conn, client } => self.leave(conn, client),
            FileCommand::Save {
                user,
                token,
                base,
                bytes,
                reply,
            } => self.waiting.push_back(Waiting {
                user,
                token,
                base,
                bytes,
                reply,
            }),
        }
    }

    fn apply_access(&mut self, change: AccessChange) {
        match change {
            AccessChange::SetAccess { user, role } => match role {
                Some(role) => self.change_role(user, role),
                None => self.remove(|m| m.user == user, RemovedReason::AccessRevoked),
            },
            AccessChange::EndSession { session } => {
                self.remove(|m| m.session == session, RemovedReason::SignedOut);
            }
            AccessChange::RemoveUser { user } => {
                self.remove(|m| m.user == user, RemovedReason::AccountDisabled);
            }
        }
    }

    fn join(&mut self, member: Member) {
        let (client, user) = (member.client, member.user);
        let can_edit = member.role.can_edit();
        let name = member.name.clone();
        // Replacing the member drops the older connection's queue, which closes it.
        let replaced = match self.members.iter_mut().find(|m| m.client == client) {
            Some(existing) => {
                *existing = member;
                true
            }
            None => {
                self.members.push(member);
                false
            }
        };
        if !replaced {
            self.emit(
                "editing.joined",
                Some(user),
                user,
                serde_json::json!({ "can_edit": can_edit }),
            );
        }
        // A connection that joins again in place of its own gives up the seat its older
        // connection held, and takes it again below, with a new token, if it still may edit.
        let freed = replaced && self.seat.as_ref().is_some_and(|s| s.client == client);
        if freed {
            self.seat = None;
            self.emit("seat.freed", Some(user), user, serde_json::json!({}));
        }
        let took = can_edit && self.seat.is_none() && self.give_seat(client, user, name);

        let mut frames = Vec::new();
        frames.extend(encode(&FileServerMsg::Joined {
            participants: self.participants(),
            revision: self.revision.as_ref().map(ToString::to_string),
            can_edit,
            seat: self.seat_for(client),
        }));
        if self.in_conflict() {
            frames.extend(encode(&FileServerMsg::Save {
                state: SaveState::Conflict,
                revision: None,
            }));
        }
        self.send(Some(client), &Outbound::Many(frames));
        if took || freed {
            let seat = FileServerMsg::Seat {
                seat: self.seat_for_others(),
            };
            self.send_others(client, &seat);
        }
        self.broadcast_people();
    }

    /// Gives the free seat to `client`, `user`'s connection named `name`, with a new token.
    /// Returns whether it was given; it stays free if no token could be made.
    fn give_seat(&mut self, client: ClientId, user: UserId, name: String) -> bool {
        let Some(token) = new_token() else {
            eprintln!(
                "sharecraft-server: no random token for the editing seat of document {}; it \
                 stays free",
                self.doc.0
            );
            return false;
        };
        self.seat = Some(Seat {
            client,
            user,
            name,
            token,
        });
        self.emit("seat.taken", Some(user), user, serde_json::json!({}));
        true
    }

    /// The seat as `client` sees it.
    fn seat_for(&self, client: ClientId) -> SeatState {
        match &self.seat {
            Some(seat) if seat.client == client => SeatState::Yours {
                token: seat.token.clone(),
            },
            _ => self.seat_for_others(),
        }
    }

    /// The seat as everyone but its holder sees it.
    fn seat_for_others(&self) -> SeatState {
        let holder = self.seat.as_ref().and_then(|seat| {
            self.participants()
                .into_iter()
                .find(|p| p.client == seat.client)
        });
        match holder {
            Some(by) => SeatState::Taken { by },
            None => SeatState::Free,
        }
    }

    fn leave(&mut self, conn: ConnId, client: ClientId) {
        let left = self
            .members
            .iter()
            .find(|m| m.conn == conn && m.client == client)
            .map(|m| m.user);
        self.members
            .retain(|m| !(m.conn == conn && m.client == client));
        if let Some(user) = left {
            self.emit("editing.left", Some(user), user, serde_json::json!({}));
            self.members_left(Some(user));
        }
    }

    /// Gives every connection of `user` the role `role`, telling each whose role changes with
    /// `Access`. A holder who may no longer edit loses the seat.
    fn change_role(&mut self, user: UserId, role: Role) {
        let mut told = Vec::new();
        for member in self.members.iter_mut().filter(|m| m.user == user) {
            if member.role == role {
                continue;
            }
            told.push(member.client);
            member.role = role;
        }
        if told.is_empty() {
            return;
        }
        if let Some(frame) = encode(&FileServerMsg::Access {
            can_edit: role.can_edit(),
        }) {
            for client in told {
                self.send(Some(client), &Outbound::One(frame.clone()));
            }
        }
        if !role.can_edit() && self.seat.as_ref().is_some_and(|s| s.user == user) {
            self.free_seat(None);
        }
        self.broadcast_people();
    }

    /// Removes every member `which` selects: queues `Removed { reason }` as the connection's
    /// last frame and drops its queue, which ends the connection once the frame is written.
    fn remove(&mut self, which: impl Fn(&Member) -> bool, reason: RemovedReason) {
        let Some(frame) = encode(&FileServerMsg::Removed { reason }) else {
            return;
        };
        let doc = self.doc;
        let events = &self.events;
        let detail = serde_json::json!({ "reason": reason });
        let before = self.members.len();
        self.members.retain(|m| {
            if !which(m) {
                return true;
            }
            events.emit(NewEvent {
                kind: "editing.removed",
                actor: None,
                subject: Some(m.user),
                document: Some(doc),
                detail: detail.clone(),
            });
            // A closed queue means the connection is already gone. A full one cannot take the
            // frame; the connection still ends, but the page is not told why.
            if let Err(mpsc::error::TrySendError::Full(_)) =
                m.tx.try_send(Outbound::One(frame.clone()))
            {
                eprintln!(
                    "sharecraft-server: could not tell user {} they were removed from document \
                     {}: their connection was not keeping up",
                    m.user.0, doc.0
                );
            }
            false
        });
        if self.members.len() != before {
            self.members_left(None);
        }
    }

    /// After members left: frees the seat if its holder was among them, crediting `actor`, and
    /// tells everyone who is here.
    fn members_left(&mut self, actor: Option<UserId>) {
        if let Some(seat) = &self.seat
            && !self.members.iter().any(|m| m.client == seat.client)
        {
            self.free_seat(actor);
        }
        self.broadcast_people();
    }

    /// Frees the seat and tells everyone. `actor` is the holder when they gave it up
    /// themselves; `None` when the server took it from them.
    fn free_seat(&mut self, actor: Option<UserId>) {
        if let Some(seat) = self.seat.take() {
            self.emit("seat.freed", actor, seat.user, serde_json::json!({}));
            if let Some(frame) = encode(&FileServerMsg::Seat {
                seat: SeatState::Free,
            }) {
                self.send(None, &Outbound::One(frame));
            }
        }
    }

    /// The next waiting save that may be written, once nothing is being written. Each save
    /// passed over on the way is answered: one from someone not holding the seat with that
    /// token is `NotHolder`; once in conflict, or on a revision other than the current one,
    /// it is `Conflict`.
    fn next_write(&mut self) -> Option<WriteJob> {
        if self.writing.is_some() {
            return None;
        }
        while let Some(save) = self.waiting.pop_front() {
            let holder = self
                .seat
                .as_ref()
                .filter(|s| s.user == save.user && same_token(&s.token, &save.token));
            let Some(holder) = holder else {
                let _ = save.reply.send(SaveReply::NotHolder);
                continue;
            };
            if let Some(current) = &self.conflict {
                let _ = save.reply.send(SaveReply::Conflict {
                    current: current.clone(),
                });
                continue;
            }
            if save.base != self.revision {
                let _ = save.reply.send(SaveReply::Conflict {
                    current: self.revision.clone(),
                });
                continue;
            }
            let author = Author {
                id: holder.user.0.to_string(),
                name: holder.name.clone(),
            };
            self.writing = Some((save.reply, author.id.clone()));
            self.broadcast_save(SaveState::Saving, None);
            return Some(WriteJob {
                bytes: save.bytes,
                base: save.base,
                author,
                fields: self.fields.clone(),
            });
        }
        None
    }

    /// Records how the save being written ended, tells everyone and answers it.
    fn finish_write(&mut self, result: Result<Outcome, WriteError>) {
        let Some((reply, author)) = self.writing.take() else {
            return;
        };
        let answer = match result {
            Ok(Outcome::Written(revision)) => {
                self.revision = Some(revision.clone());
                self.emit_save(
                    "revision.saved",
                    serde_json::json!({ "revision": revision.to_string(), "authors": [author] }),
                );
                self.broadcast_save(SaveState::Saved, Some(revision.to_string()));
                SaveReply::Saved(revision)
            }
            Ok(Outcome::Unchanged(revision)) => {
                self.revision = Some(revision.clone());
                self.broadcast_save(SaveState::Saved, Some(revision.to_string()));
                SaveReply::Unchanged(revision)
            }
            Err(WriteError::Stale { current }) => {
                self.emit_save(
                    "revision.conflict",
                    serde_json::json!({ "current": current.as_ref().map(ToString::to_string) }),
                );
                eprintln!(
                    "sharecraft-server: document {} was saved from elsewhere; its file session \
                     takes no more saves",
                    self.doc.0
                );
                self.conflict = Some(current.clone());
                self.broadcast_save(SaveState::Conflict, None);
                SaveReply::Conflict { current }
            }
            Err(WriteError::Failed(error)) => {
                eprintln!(
                    "sharecraft-server: save of document {} failed: {error}",
                    self.doc.0
                );
                self.emit_save(
                    "revision.save_failed",
                    serde_json::json!({ "error": error_text(&error) }),
                );
                self.broadcast_save(SaveState::Failed, None);
                SaveReply::Failed
            }
        };
        // The caller may have gone away meanwhile; what was written stays written.
        let _ = reply.send(answer);
    }

    fn participants(&self) -> Vec<Participant> {
        self.members
            .iter()
            .map(|m| Participant {
                client: m.client,
                user: m.user,
                name: m.name.clone(),
                can_edit: m.role.can_edit(),
                cursor: None,
            })
            .collect()
    }

    fn broadcast_people(&mut self) {
        let people = FileServerMsg::People {
            participants: self.participants(),
        };
        if let Some(frame) = encode(&people) {
            self.send(None, &Outbound::One(frame));
        }
    }

    fn broadcast_save(&mut self, state: SaveState, revision: Option<String>) {
        if let Some(frame) = encode(&FileServerMsg::Save { state, revision }) {
            self.send(None, &Outbound::One(frame));
        }
    }

    /// Queues `msg` for everyone but `client`.
    fn send_others(&mut self, client: ClientId, msg: &FileServerMsg) {
        let Some(frame) = encode(msg) else {
            return;
        };
        let others: Vec<ClientId> = self
            .members
            .iter()
            .map(|m| m.client)
            .filter(|c| *c != client)
            .collect();
        for other in others {
            self.send(Some(other), &Outbound::One(frame.clone()));
        }
    }

    /// Queues `out` for `to`, or for everyone when `to` is `None`. A member whose queue is full
    /// or closed is dropped (it joins again), and the others are told.
    fn send(&mut self, to: Option<ClientId>, out: &Outbound) {
        let mut dropped = false;
        let events = &self.events;
        let doc = self.doc;
        self.members.retain(|m| {
            if to.is_some_and(|c| c != m.client) {
                return true;
            }
            let delivered = m.tx.try_send(out.clone()).is_ok();
            if !delivered {
                events.emit(NewEvent {
                    kind: "editing.left",
                    actor: Some(m.user),
                    subject: Some(m.user),
                    document: Some(doc),
                    detail: serde_json::json!({}),
                });
            }
            dropped |= !delivered;
            delivered
        });
        // Each round removes at least one member, so this ends.
        if dropped {
            self.members_left(None);
        }
    }
}

fn encode(msg: &FileServerMsg) -> Option<Utf8Bytes> {
    serde_json::to_string(msg).ok().map(Utf8Bytes::from)
}

/// A new seat token: [`TOKEN_BYTES`] random bytes from the operating system, as lowercase
/// hexadecimal. `None` if the system has no randomness to give.
fn new_token() -> Option<String> {
    let mut bytes = [0u8; TOKEN_BYTES];
    getrandom::fill(&mut bytes).ok()?;
    Some(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Whether `a` and `b` are the same token, taking as long whichever byte differs, so the time
/// taken tells nothing about the held token.
fn same_token(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let diff = a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    std::hint::black_box(diff) == 0
}

/// Loads the document's current revision and fields, then handles commands and writes saves
/// until nobody has been connected for `grace` and no save is under way, or at once when
/// empty once in conflict.
async fn run(
    place: Place,
    name: String,
    mut rx: mpsc::Receiver<FileCommand>,
    mut access_rx: mpsc::UnboundedReceiver<AccessChange>,
    storage: Storage,
    grace: Duration,
) {
    let doc = place.doc();
    let Storage { store, db, events } = storage;
    let (revision, fields) = match load(&store, doc).await {
        Ok(loaded) => loaded,
        Err(err) => {
            eprintln!("sharecraft-server: cannot open document {}: {err}", doc.0);
            // Refuse everyone waiting; a later join opens a fresh session that tries again.
            // Saves waiting are dropped, which answers them `NotHolder`.
            place.close();
            access_rx.close();
            rx.close();
            while let Ok(cmd) = rx.try_recv() {
                if let FileCommand::Join { accepted, .. } = cmd {
                    let _ = accepted.send(JoinReply::Refused);
                }
            }
            return;
        }
    };
    let mut room = FileRoom::new(doc, revision, fields).with_events(events.clone());
    // Revisions are recorded off this task, in the order written: one save is written at a
    // time, and the blocking task that writes it queues it before the session learns of it.
    let recorder = db.map(|db| Recorder::start(doc, Arc::clone(&store), db, events));
    let mut running: Option<Running> = None;
    // The session opens for a join waiting in its channel; until that is handled it is empty.
    let mut empty_since = Some(Instant::now());
    // The registry holds an access sender until this task removes it, so this stays true.
    let mut access_open = true;
    loop {
        if running.is_none()
            && let Some(job) = room.next_write()
        {
            let store = Arc::clone(&store);
            let file_name = name.clone();
            let recorder = recorder.clone();
            running = Some(tokio::task::spawn_blocking(move || {
                let result = job.write(doc, &file_name, &*store);
                if let (Ok(Outcome::Written(revision)), Some(recorder)) = (&result, &recorder) {
                    recorder.written(job.base.as_ref(), revision);
                }
                result
            }));
        }
        let now = Instant::now();
        let grace_ends = empty_since
            .map(|since| grace_end(since, grace))
            .filter(|end| *end > now);
        tokio::select! {
            // A waiting join is handled before the session can decide to end.
            biased;
            // Access changes go first, so nobody keeps rights while other commands wait; they
            // are handled while a save is being written, too.
            change = access_rx.recv(), if access_open => match change {
                Some(change) => room.apply_access(change),
                None => access_open = false,
            },
            cmd = rx.recv() => match cmd {
                Some(cmd) => room.handle(cmd),
                // The registry holds a sender until this task removes it.
                None => break,
            },
            result = finished(&mut running) => {
                running = None;
                room.finish_write(result);
            }
            () = sleep_until(grace_ends) => {}
        }
        empty_since = match (room.is_empty(), empty_since) {
            (false, _) => None,
            (true, since) => since.or_else(|| Some(Instant::now())),
        };
        // A session in conflict has nothing to come back to, so it does not wait out the grace
        // period.
        let grace_over = empty_since
            .is_some_and(|since| room.in_conflict() || grace_end(since, grace) <= Instant::now());
        // A command that arrived meanwhile, a join above all, is handled before it ends.
        if grace_over && running.is_none() && room.idle() && rx.is_empty() && access_rx.is_empty() {
            break;
        }
    }
    place.close();
    access_rx.close();
    rx.close();
    // Joins queued after the session decided to end are dropped unanswered, so those
    // connections open a fresh session; saves dropped so are answered `NotHolder`.
    while rx.try_recv().is_ok() {}
}

/// Reads the document's current revision and its fields, without its content and without
/// blocking the async workers. A document never saved has neither.
async fn load(
    store: &Arc<dyn Checkpointer>,
    doc: DocId,
) -> Result<(Option<RevisionId>, Map<String, Value>), String> {
    let store = Arc::clone(store);
    let loaded = tokio::task::spawn_blocking(move || store.load(doc))
        .await
        .map_err(|e| format!("load task failed: {e}"))?
        .map_err(|e| e.to_string())?;
    Ok(match loaded {
        Some(stored) => (Some(stored.revision), stored.meta.fields),
        None => (None, Map::new()),
    })
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    use records::test_support::TestDb;
    use records::{Db, Editor, NewUser};
    use revisions::{CheckpointError, RevisionInfo, RevisionStore, Stored};
    use uuid::Uuid;

    use crate::room::OUTBOUND_CAPACITY;

    const NAME: &str = "Plan.xlsx";

    /// A file document in a fresh database and library: Ann owns it, Bob may edit it.
    struct Fixture {
        _test: TestDb,
        db: Db,
        dir: tempfile::TempDir,
        rooms: Arc<Rooms>,
        doc: DocId,
        ann: UserId,
        bob: UserId,
    }

    async fn account(db: &Db, name: &str) -> UserId {
        db.create_user(
            None,
            NewUser {
                email: &format!("{name}@test.invalid"),
                name,
                password: "a long enough password",
                is_admin: false,
            },
        )
        .await
        .unwrap()
        .id
    }

    async fn fixture() -> Fixture {
        let test = TestDb::create().await;
        let db = test.db.clone();
        let ann = account(&db, "Ann").await;
        let bob = account(&db, "Bob").await;
        let doc = DocId(Uuid::new_v4());
        db.create_document(ann, doc, NAME, Editor::GridCraft)
            .await
            .unwrap();
        db.set_access(ann, doc, bob, Some(Role::Editor))
            .await
            .unwrap();
        Fixture {
            _test: test,
            db,
            dir: tempfile::tempdir().unwrap(),
            rooms: Arc::new(Rooms::default()),
            doc,
            ann,
            bob,
        }
    }

    impl Fixture {
        fn library(&self) -> RevisionStore {
            RevisionStore::open(&crate::library_path(self.dir.path())).unwrap()
        }

        /// Opens the document's session on the library, recording revisions in the database.
        fn open(&self) -> FileLink {
            self.open_with(Arc::new(self.library()), Some(self.db.clone()))
        }

        fn open_with(&self, store: Arc<dyn Checkpointer>, db: Option<Db>) -> FileLink {
            self.rooms.open_file(
                self.doc,
                NAME.into(),
                Storage {
                    store,
                    db,
                    events: EventSink::discard(),
                },
                Duration::from_secs(60),
            )
        }
    }

    /// One connection's view of the session.
    struct Tab {
        client: ClientId,
        out: mpsc::Receiver<Outbound>,
        seen: Vec<FileServerMsg>,
    }

    impl Tab {
        /// Reads frames until `cond` holds for one; fails after five seconds.
        async fn until(&mut self, cond: impl Fn(&FileServerMsg) -> bool) -> FileServerMsg {
            loop {
                let next = tokio::time::timeout(Duration::from_secs(5), self.out.recv())
                    .await
                    .expect("the expected frame did not arrive")
                    .expect("the session let go of this connection");
                let frames = match next {
                    Outbound::One(f) => vec![f],
                    Outbound::Many(fs) => fs,
                };
                for frame in frames {
                    let msg: FileServerMsg = serde_json::from_str(frame.as_str()).unwrap();
                    self.seen.push(msg.clone());
                    if cond(&msg) {
                        return msg;
                    }
                }
            }
        }

        /// Every frame until the session lets go of the connection.
        async fn rest(&mut self) -> Vec<FileServerMsg> {
            let mut got = Vec::new();
            while let Some(next) = tokio::time::timeout(Duration::from_secs(5), self.out.recv())
                .await
                .expect("the session kept the connection")
            {
                let frames = match next {
                    Outbound::One(f) => vec![f],
                    Outbound::Many(fs) => fs,
                };
                got.extend(
                    frames
                        .iter()
                        .map(|f| serde_json::from_str::<FileServerMsg>(f.as_str()).unwrap()),
                );
            }
            got
        }
    }

    /// `user`, named `name`, joins as connection `conn` with `role`; returns the tab and its
    /// seat token, if it got the seat.
    async fn join(
        link: &FileLink,
        conn: ConnId,
        (user, name): (UserId, &str),
        role: Role,
    ) -> (Tab, Option<String>) {
        let (tx, out) = mpsc::channel(OUTBOUND_CAPACITY);
        let (accepted, reply) = oneshot::channel();
        let client = ClientId(Uuid::new_v4());
        link.commands
            .send(FileCommand::Join {
                conn,
                client,
                user,
                session: SessionId(Uuid::new_v4()),
                name: name.into(),
                role,
                tx,
                accepted,
            })
            .await
            .unwrap();
        assert_eq!(reply.await.unwrap(), JoinReply::Accepted);
        let mut tab = Tab {
            client,
            out,
            seen: Vec::new(),
        };
        let joined = tab
            .until(|m| matches!(m, FileServerMsg::Joined { .. }))
            .await;
        let token = match joined {
            FileServerMsg::Joined {
                seat: SeatState::Yours { token },
                ..
            } => Some(token),
            _ => None,
        };
        (tab, token)
    }

    fn is_save(msg: &FileServerMsg, state: SaveState) -> bool {
        matches!(msg, FileServerMsg::Save { state: s, .. } if *s == state)
    }

    #[tokio::test]
    async fn save_by_the_holder_is_stored_and_credited() {
        let f = fixture().await;
        let link = f.open();
        let (mut ann, token) = join(&link, 1, (f.ann, "Ann"), Role::Owner).await;
        let token = token.unwrap();
        let bytes = b"PK\x03\x04 not really a workbook".to_vec();

        let reply = f
            .rooms
            .save_file(f.doc, f.ann, &token, None, bytes.clone())
            .await;
        let SaveReply::Saved(revision) = reply else {
            panic!("{reply:?}");
        };
        ann.until(|m| is_save(m, SaveState::Saving)).await;
        let saved = ann.until(|m| is_save(m, SaveState::Saved)).await;
        assert_eq!(
            saved,
            FileServerMsg::Save {
                state: SaveState::Saved,
                revision: Some(revision.to_string())
            }
        );

        // Read back from Git: the bytes, under the document's name, credited to Ann alone.
        let store = f.library();
        let stored = store.read(f.doc, None).unwrap().unwrap();
        assert_eq!(stored.revision, revision);
        assert_eq!(stored.bytes, bytes);
        let info = store.describe(f.doc, &revision).unwrap().unwrap();
        assert_eq!(info.file_name, NAME);
        assert_eq!(
            info.authors,
            vec![Author {
                id: f.ann.0.to_string(),
                name: "Ann".into()
            }]
        );

        // The revision is recorded in the database.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let rows = f.db.revisions(f.doc).await.unwrap();
            if rows.iter().any(|r| r.revision == revision.to_string()) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the revision was never recorded"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn save_with_a_wrong_token_is_refused() {
        let f = fixture().await;
        // No session is open, and saving does not open one.
        let reply = f
            .rooms
            .save_file(f.doc, f.ann, &"0".repeat(64), None, b"x".to_vec())
            .await;
        assert_eq!(reply, SaveReply::NotHolder);
        assert!(f.rooms.link(f.doc).is_none());

        let link = f.open();
        let (_ann, token) = join(&link, 1, (f.ann, "Ann"), Role::Owner).await;
        let token = token.unwrap();
        let (_bob, bobs) = join(&link, 2, (f.bob, "Bob"), Role::Editor).await;
        assert!(bobs.is_none(), "Bob does not hold the seat");

        let mut wrong = token.clone().into_bytes();
        wrong[63] = if wrong[63] == b'0' { b'1' } else { b'0' };
        let wrong = String::from_utf8(wrong).unwrap();
        for (user, token) in [
            (f.ann, wrong.as_str()),
            (f.ann, ""),
            (f.ann, &token[..32]),
            // The right token, from someone else.
            (f.bob, token.as_str()),
        ] {
            let reply = f
                .rooms
                .save_file(f.doc, user, token, None, b"x".to_vec())
                .await;
            assert_eq!(reply, SaveReply::NotHolder);
        }
        assert!(f.library().read(f.doc, None).unwrap().is_none());
    }

    #[tokio::test]
    async fn save_on_an_old_revision_is_a_conflict() {
        let f = fixture().await;
        let link = f.open();
        let (_ann, token) = join(&link, 1, (f.ann, "Ann"), Role::Owner).await;
        let token = token.unwrap();
        let SaveReply::Saved(first) = f
            .rooms
            .save_file(f.doc, f.ann, &token, None, b"one".to_vec())
            .await
        else {
            panic!("the first save failed");
        };

        // Made from no revision, after the first was saved.
        let reply = f
            .rooms
            .save_file(f.doc, f.ann, &token, None, b"two".to_vec())
            .await;
        assert_eq!(
            reply,
            SaveReply::Conflict {
                current: Some(first.clone())
            }
        );
        let stored = f.library().read(f.doc, None).unwrap().unwrap();
        assert_eq!(stored.bytes, b"one");

        // Made from the current revision, it is stored.
        let reply = f
            .rooms
            .save_file(f.doc, f.ann, &token, Some(first), b"two".to_vec())
            .await;
        assert!(matches!(reply, SaveReply::Saved(_)), "{reply:?}");
    }

    #[tokio::test]
    async fn same_bytes_again_is_unchanged() {
        let f = fixture().await;
        let link = f.open();
        let (mut ann, token) = join(&link, 1, (f.ann, "Ann"), Role::Owner).await;
        let token = token.unwrap();
        let SaveReply::Saved(first) = f
            .rooms
            .save_file(f.doc, f.ann, &token, None, b"same".to_vec())
            .await
        else {
            panic!("the first save failed");
        };
        ann.until(|m| is_save(m, SaveState::Saved)).await;

        let reply = f
            .rooms
            .save_file(f.doc, f.ann, &token, Some(first.clone()), b"same".to_vec())
            .await;
        assert_eq!(reply, SaveReply::Unchanged(first.clone()));
        ann.until(|m| is_save(m, SaveState::Saving)).await;
        let saved = ann.until(|m| is_save(m, SaveState::Saved)).await;
        assert_eq!(
            saved,
            FileServerMsg::Save {
                state: SaveState::Saved,
                revision: Some(first.to_string())
            }
        );
        assert_eq!(f.library().history(f.doc).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn stale_store_puts_the_room_in_conflict() {
        let f = fixture().await;
        let link = f.open();
        let (mut ann, token) = join(&link, 1, (f.ann, "Ann"), Role::Owner).await;
        let token = token.unwrap();

        // The document is saved behind the session's back.
        let elsewhere = match f
            .library()
            .checkpoint(&Checkpoint {
                doc: f.doc,
                file_name: NAME,
                bytes: b"from elsewhere",
                base: None,
                authors: &[Author {
                    id: "someone".into(),
                    name: "Someone".into(),
                }],
                fields: &Map::new(),
            })
            .unwrap()
        {
            Outcome::Written(revision) => revision,
            Outcome::Unchanged(_) => panic!("nothing was written"),
        };

        // The session still thinks the document was never saved, as does the holder.
        let reply = f
            .rooms
            .save_file(f.doc, f.ann, &token, None, b"mine".to_vec())
            .await;
        assert_eq!(
            reply,
            SaveReply::Conflict {
                current: Some(elsewhere.clone())
            }
        );
        ann.until(|m| is_save(m, SaveState::Conflict)).await;

        // Every later save is refused, even one made from the revision found.
        let reply = f
            .rooms
            .save_file(
                f.doc,
                f.ann,
                &token,
                Some(elsewhere.clone()),
                b"mine".to_vec(),
            )
            .await;
        assert_eq!(
            reply,
            SaveReply::Conflict {
                current: Some(elsewhere)
            }
        );
        let stored = f.library().read(f.doc, None).unwrap().unwrap();
        assert_eq!(stored.bytes, b"from elsewhere");

        // Once empty, the session ends without waiting out its grace period.
        link.commands
            .send(FileCommand::Leave {
                conn: 1,
                client: ann.client,
            })
            .await
            .unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while f.rooms.link(f.doc).is_some() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the session in conflict did not end"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// A write that waits until the test lets it finish, and says when it starts.
    struct Gated {
        started: std::sync::Mutex<std::sync::mpsc::Sender<()>>,
        gate: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    }

    impl Checkpointer for Gated {
        fn write(&self, _c: &Checkpoint<'_>) -> Result<Outcome, CheckpointError> {
            self.started.lock().unwrap().send(()).unwrap();
            self.gate.lock().unwrap().recv().unwrap();
            Ok(Outcome::Written(format!("{:040x}", 1).parse().unwrap()))
        }

        fn load(&self, _doc: DocId) -> Result<Option<Stored>, CheckpointError> {
            Ok(None)
        }

        fn describe(
            &self,
            _doc: DocId,
            _revision: &RevisionId,
        ) -> Result<Option<RevisionInfo>, CheckpointError> {
            Ok(None)
        }

        fn history(&self, _doc: DocId) -> Result<Vec<RevisionInfo>, CheckpointError> {
            Ok(Vec::new())
        }

        fn documents(&self) -> Result<Vec<DocId>, revisions::GitError> {
            Ok(Vec::new())
        }
    }

    /// The holder is removed while their save is being written. The removal is not held up by
    /// the write, and the write still finishes and is answered.
    #[tokio::test]
    async fn removal_during_a_write_still_removes() {
        let f = fixture().await;
        let (started_tx, started) = std::sync::mpsc::channel();
        let (release, gate) = std::sync::mpsc::channel();
        let link = f.open_with(
            Arc::new(Gated {
                started: std::sync::Mutex::new(started_tx),
                gate: std::sync::Mutex::new(gate),
            }),
            None,
        );
        let (mut ann, token) = join(&link, 1, (f.ann, "Ann"), Role::Owner).await;
        let token = token.unwrap();
        let (mut bob, _) = join(&link, 2, (f.bob, "Bob"), Role::Editor).await;
        ann.until(
            |m| matches!(m, FileServerMsg::People { participants } if participants.len() == 2),
        )
        .await;

        let save = tokio::spawn({
            let rooms = Arc::clone(&f.rooms);
            let doc = f.doc;
            let ann = f.ann;
            async move { rooms.save_file(doc, ann, &token, None, b"x".to_vec()).await }
        });
        let started = tokio::task::spawn_blocking(move || {
            started.recv_timeout(Duration::from_secs(5)).unwrap();
            started
        })
        .await
        .unwrap();

        // While the write is held, Ann's access ends.
        f.rooms.send_to(
            f.doc,
            AccessChange::SetAccess {
                user: f.ann,
                role: None,
            },
        );
        let rest = ann.rest().await;
        assert_eq!(
            rest.last(),
            Some(&FileServerMsg::Removed {
                reason: RemovedReason::AccessRevoked
            }),
            "{rest:?}"
        );
        bob.until(|m| {
            *m == FileServerMsg::Seat {
                seat: SeatState::Free,
            }
        })
        .await;
        assert!(!save.is_finished(), "the write was not held");

        // The write finishes and its answer still goes back.
        release.send(()).unwrap();
        let reply = tokio::time::timeout(Duration::from_secs(5), save)
            .await
            .expect("the save was not answered")
            .unwrap();
        assert_eq!(
            reply,
            SaveReply::Saved(format!("{:040x}", 1).parse().unwrap())
        );
        bob.until(|m| is_save(m, SaveState::Saved)).await;
        drop(started);
    }

    #[test]
    fn tokens_are_compared_whole() {
        let token = new_token().unwrap();
        assert_eq!(token.len(), 64);
        assert!(same_token(&token, &token.clone()));
        assert!(!same_token(&token, &token[..63]));
        assert!(!same_token(&token, ""));
        assert_ne!(new_token().unwrap(), token);
    }
}
