//! One room per open document: a single task owns the document's change log and everyone
//! connected to it, and receives commands from the connections over a channel.
//!
//! The room loads its document when it opens and writes checkpoints while it runs (see
//! [`Schedule`]). When the last participant leaves it saves at once, then stays open for a
//! grace period so a browser that reconnects resumes where it left off. It ends once nobody
//! has been connected for the grace period and everything is saved (or saving was given up).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::extract::ws::Utf8Bytes;
use collab_core::document::Submitted;
use collab_core::ids::{ClientId, DocId, OpId, Seq, SessionId, UserId};
use collab_core::server_log::ServerLog;
use collab_text::{TextDoc, TextOp};
use protocol::{Participant, Resume, RoomEpoch, SaveState, ServerMsg};
use records::Role;
use revisions::{Author, CheckpointError, Outcome, RevisionId};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use uuid::Uuid;

use crate::checkpoint::{Checkpointer, Finished, SaveJob, Schedule, Timing};

/// Identifies one WebSocket connection, so a replaced connection's late messages are ignored.
pub(crate) type ConnId = u64;

/// Messages a room queues for one connection's writer.
#[derive(Clone, Debug)]
pub(crate) enum Outbound {
    /// One frame.
    One(Utf8Bytes),
    /// Several frames that must arrive together and in order (a join and its catch-up).
    Many(Vec<Utf8Bytes>),
}

/// Frames queued per connection before the room gives up on it.
pub(crate) const OUTBOUND_CAPACITY: usize = 256;
/// Commands queued for a room before connections wait to send more.
const COMMAND_CAPACITY: usize = 1024;
/// Largest cursor position relayed; anything larger is dropped.
const MAX_CURSOR: usize = 10_000_000;
/// Log entries kept behind the last checkpoint, so a client that drops briefly can still
/// resume (and keep its unconfirmed typing) instead of being reset.
const KEPT_ENTRIES: u64 = 1000;
/// Name credited when an account's name is not one the revision store accepts.
const DEFAULT_NAME: &str = "Guest";
/// Why a `Submit` from someone who may not edit is refused.
const READ_ONLY: &str = "read_only";

/// How a room answers a join.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum JoinReply {
    /// The connection is in the room.
    Accepted,
    /// The room could not load its document, or the client id belongs to another account;
    /// the connection should close.
    Refused,
}

/// What a connection asks of its room.
#[derive(Debug)]
pub(crate) enum Command {
    /// Enter the room, replacing any connection with the same client id. The connection has
    /// looked up who is joining and their role on the document.
    Join {
        conn: ConnId,
        client: ClientId,
        user: UserId,
        session: SessionId,
        /// The account's name.
        name: String,
        role: Role,
        resume: Option<Resume>,
        tx: mpsc::Sender<Outbound>,
        /// Answered once the join is handled; dropped if the room ended first, in which case
        /// the connection asks again.
        accepted: oneshot::Sender<JoinReply>,
    },
    /// Propose a change.
    Submit {
        conn: ConnId,
        client: ClientId,
        op_id: OpId,
        base: Seq,
        op: TextOp,
    },
    /// Move or hide the cursor.
    Cursor {
        conn: ConnId,
        client: ClientId,
        at: Option<usize>,
    },
    /// The connection ended.
    Leave { conn: ConnId, client: ClientId },
}

/// The state a room starts from.
#[derive(Clone, Debug, Default)]
pub(crate) struct Initial {
    /// The document's text.
    pub doc: TextDoc,
    /// The stored revision the text came from, if any.
    pub revision: Option<RevisionId>,
}

/// The person joining: who they are and what they may do.
struct Joiner {
    user: UserId,
    session: SessionId,
    name: String,
    role: Role,
}

struct Member {
    conn: ConnId,
    client: ClientId,
    user: UserId,
    /// The sign-in session the connection belongs to.
    #[expect(
        dead_code,
        reason = "kept so signing out can close that session's connections"
    )]
    session: SessionId,
    name: String,
    /// What this person may do; it can change while they are connected.
    role: Role,
    cursor: Option<usize>,
    tx: mpsc::Sender<Outbound>,
}

/// The state of one open document. Handling a command never blocks: frames are queued with
/// `try_send` and a connection whose queue is full or closed is dropped.
pub(crate) struct Room {
    doc: DocId,
    /// The document's name when the room opened; checkpoints are stored under it.
    name: String,
    /// This lifetime of the room; log positions only mean something within it.
    epoch: RoomEpoch,
    log: ServerLog<TextDoc>,
    schedule: Schedule,
    /// In joining order.
    members: Vec<Member>,
    /// Each client's last committed change, so a resend is recognised however much of the log
    /// is kept.
    last_committed: HashMap<ClientId, OpId>,
    /// The account that first joined with each client id. A client id stays with that account
    /// for the room's lifetime, also after they leave, so nobody else can take over a
    /// connection's place, its resume position or its record of committed changes.
    client_owners: HashMap<ClientId, UserId>,
}

impl Room {
    /// A room for the document `doc` named `name`, holding `initial`, with its log at
    /// `Seq(0)` and a fresh random epoch.
    pub(crate) fn new(doc: DocId, name: String, initial: Initial, timing: Timing) -> Self {
        Self {
            doc,
            name,
            epoch: RoomEpoch(Uuid::new_v4()),
            log: ServerLog::new(initial.doc, Seq(0)),
            schedule: Schedule::new(initial.revision, Seq(0), timing),
            members: Vec::new(),
            last_committed: HashMap::new(),
            client_owners: HashMap::new(),
        }
    }

    /// When the next checkpoint should start, if one is needed and none is running.
    fn checkpoint_due(&self) -> Option<Instant> {
        self.schedule.due(self.log.head(), self.members.is_empty())
    }

    /// Starts a checkpoint of the current document and tells everyone.
    fn start_checkpoint(&mut self) -> Option<SaveJob> {
        let head = self.log.head();
        let job = self
            .schedule
            .start(head, self.log.doc().as_str().to_owned())?;
        self.broadcast_save(SaveState::Saving, None, head);
        Some(job)
    }

    /// Records how the running checkpoint ended and tells everyone.
    fn finish_checkpoint(&mut self, result: Result<Outcome, String>) {
        let head = self.log.head();
        let finished = self.schedule.finish(result, head, self.members.is_empty());
        match finished {
            Some(Finished::Saved { revision, through }) => {
                self.log
                    .truncate_through(Seq(through.0.saturating_sub(KEPT_ENTRIES)));
                self.broadcast_save(SaveState::Saved, Some(revision.to_string()), through);
            }
            Some(Finished::Failed {
                through,
                error,
                gave_up,
            }) => {
                eprintln!(
                    "sharecraft-server: checkpoint of document {} failed: {error}",
                    self.doc.0
                );
                if gave_up {
                    eprintln!(
                        "sharecraft-server: giving up on document {}; changes after its last \
                         saved revision are lost",
                        self.doc.0
                    );
                }
                self.broadcast_save(SaveState::Failed, None, through);
            }
            None => {}
        }
    }

    /// Whether nobody is connected.
    fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    /// Whether the room can end: nobody is connected and everything is saved (or saving was
    /// given up).
    fn may_end(&self) -> bool {
        self.members.is_empty() && self.schedule.may_end(self.log.head())
    }

    /// Carries out one command.
    pub(crate) fn handle(&mut self, cmd: Command) {
        match cmd {
            Command::Join {
                conn,
                client,
                user,
                session,
                name,
                role,
                resume,
                tx,
                accepted,
            } => {
                let joiner = Joiner {
                    user,
                    session,
                    name,
                    role,
                };
                let owner = *self.client_owners.entry(client).or_insert(user);
                let reply = if owner == user {
                    self.join(conn, client, joiner, resume, tx);
                    JoinReply::Accepted
                } else {
                    JoinReply::Refused
                };
                // The connection may have gone away meanwhile; nothing to do then.
                let _ = accepted.send(reply);
            }
            Command::Submit {
                conn,
                client,
                op_id,
                base,
                op,
            } => self.submit(conn, client, op_id, base, op),
            Command::Cursor { conn, client, at } => self.cursor(conn, client, at),
            Command::Leave { conn, client } => self.leave(conn, client),
        }
    }

    fn join(
        &mut self,
        conn: ConnId,
        client: ClientId,
        joiner: Joiner,
        resume: Option<Resume>,
        tx: mpsc::Sender<Outbound>,
    ) {
        let can_edit = joiner.role.can_edit();
        let member = Member {
            conn,
            client,
            user: joiner.user,
            session: joiner.session,
            name: credited_name(&joiner.name),
            role: joiner.role,
            cursor: None,
            tx,
        };
        self.schedule.joined();
        // Replacing the member drops the older connection's queue, which closes it.
        match self.members.iter_mut().find(|m| m.client == client) {
            Some(existing) => *existing = member,
            None => self.members.push(member),
        }

        let epoch = self.epoch;
        let seq = self.log.head();
        let text = self.log.doc().as_str().to_owned();
        let revision = self.schedule.revision().map(ToString::to_string);
        let saved_through = self.schedule.saved_through();
        let mut msgs = Vec::new();
        // A position from another lifetime of this room means nothing here.
        let missed = resume
            .filter(|r| r.epoch == epoch)
            .and_then(|r| self.log.since(r.seq));
        match (resume, missed) {
            (None, _) => msgs.push(ServerMsg::Joined {
                epoch,
                seq,
                snapshot: Some(text),
                revision,
                participants: self.participants(),
                can_edit,
                saved_through,
            }),
            (Some(_), Some(missed)) => {
                msgs.push(ServerMsg::Joined {
                    epoch,
                    seq,
                    snapshot: None,
                    revision,
                    participants: self.participants(),
                    can_edit,
                    saved_through,
                });
                msgs.extend(missed.iter().map(|entry| ServerMsg::Committed {
                    entry: entry.clone(),
                }));
            }
            (Some(_), None) => msgs.push(ServerMsg::Reset {
                epoch,
                seq,
                snapshot: text,
                revision,
                can_edit,
                saved_through,
            }),
        }
        let frames = msgs.iter().filter_map(encode).collect();
        self.send(Some(client), &Outbound::Many(frames));
        self.broadcast_people();
    }

    fn submit(&mut self, conn: ConnId, client: ClientId, op_id: OpId, base: Seq, op: TextOp) {
        let Some(member) = self
            .members
            .iter()
            .find(|m| m.conn == conn && m.client == client)
        else {
            return;
        };
        // A resend of a committed change is dropped silently: this connection has already
        // received the entry through its snapshot, its catch-up or the broadcast.
        if self.last_committed.get(&client) == Some(&op_id) {
            return;
        }
        if !member.role.can_edit() {
            self.refuse(client, op_id, READ_ONLY.to_owned());
            return;
        }
        // Credited by account, so one person in two tabs is one author.
        let author = Author {
            id: member.user.0.to_string(),
            name: member.name.clone(),
        };
        match self.log.submit(client, op_id, base, op) {
            Ok(Submitted::Committed(entry)) => {
                self.last_committed.insert(client, op_id);
                self.schedule.changed(author, entry.seq);
                if let Some(frame) = encode(&ServerMsg::Committed { entry }) {
                    self.send(None, &Outbound::One(frame));
                }
            }
            Ok(Submitted::Duplicate(_)) => {}
            Err(err) => self.refuse(client, op_id, err.to_string()),
        }
    }

    /// Tells `client` its change `op_id` was refused.
    fn refuse(&mut self, client: ClientId, op_id: OpId, reason: String) {
        if let Some(frame) = encode(&ServerMsg::Refused { op_id, reason }) {
            self.send(Some(client), &Outbound::One(frame));
        }
    }

    fn cursor(&mut self, conn: ConnId, client: ClientId, at: Option<usize>) {
        if at.is_some_and(|at| at > MAX_CURSOR) {
            return;
        }
        let Some(member) = self
            .members
            .iter_mut()
            .find(|m| m.conn == conn && m.client == client)
        else {
            return;
        };
        if member.cursor == at {
            return;
        }
        member.cursor = at;
        self.broadcast_people();
    }

    fn leave(&mut self, conn: ConnId, client: ClientId) {
        let before = self.members.len();
        self.members
            .retain(|m| !(m.conn == conn && m.client == client));
        if self.members.len() != before {
            self.broadcast_people();
        }
    }

    fn participants(&self) -> Vec<Participant> {
        self.members
            .iter()
            .map(|m| Participant {
                client: m.client,
                user: m.user,
                name: m.name.clone(),
                can_edit: m.role.can_edit(),
                cursor: m.cursor,
            })
            .collect()
    }

    fn broadcast_people(&mut self) {
        let people = ServerMsg::<TextOp>::People {
            participants: self.participants(),
        };
        if let Some(frame) = encode(&people) {
            self.send(None, &Outbound::One(frame));
        }
    }

    fn broadcast_save(&mut self, state: SaveState, revision: Option<String>, through: Seq) {
        let save = ServerMsg::<TextOp>::Save {
            state,
            revision,
            through,
        };
        if let Some(frame) = encode(&save) {
            self.send(None, &Outbound::One(frame));
        }
    }

    /// Queues `out` for `to`, or for everyone when `to` is `None`. A member whose queue is full
    /// or closed is removed (it reconnects and catches up), and the others are told.
    fn send(&mut self, to: Option<ClientId>, out: &Outbound) {
        let mut dropped = false;
        self.members.retain(|m| {
            if to.is_some_and(|c| c != m.client) {
                return true;
            }
            let delivered = m.tx.try_send(out.clone()).is_ok();
            dropped |= !delivered;
            delivered
        });
        // Each round removes at least one member, so this ends.
        if dropped {
            self.broadcast_people();
        }
    }
}

fn encode(msg: &ServerMsg<TextOp>) -> Option<Utf8Bytes> {
    serde_json::to_string(msg).ok().map(Utf8Bytes::from)
}

/// `name` without control characters, `<` or `>`, and trimmed; "Guest" if what is left is not
/// a name the revision store accepts as an author name.
fn credited_name(name: &str) -> String {
    let kept: String = name
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, '<' | '>'))
        .collect();
    let trimmed = kept.trim();
    if revisions::is_valid_author_name(trimmed) {
        trimmed.to_owned()
    } else {
        DEFAULT_NAME.to_owned()
    }
}

struct RoomHandle {
    tx: mpsc::Sender<Command>,
    generation: u64,
}

/// The open rooms, by document.
#[derive(Default)]
pub(crate) struct Rooms {
    open: Mutex<HashMap<DocId, RoomHandle>>,
    next_generation: AtomicU64,
}

impl Rooms {
    /// The command channel of `doc`'s room, starting the room if it is not open. A new room
    /// stores its checkpoints under `name`, the document's name, and loads its document from
    /// `store` before handling commands; commands wait in its channel meanwhile. Once empty,
    /// the room stays open for `grace`.
    ///
    /// The channel can be closed by the time it is used, if the room ended meanwhile; the
    /// caller then asks again.
    pub(crate) fn open(
        self: &Arc<Self>,
        doc: DocId,
        name: String,
        store: &Arc<dyn Checkpointer>,
        timing: Timing,
        grace: Duration,
    ) -> mpsc::Sender<Command> {
        let mut open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(handle) = open.get(&doc)
            && !handle.tx.is_closed()
        {
            return handle.tx.clone();
        }
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel(COMMAND_CAPACITY);
        let place = Place {
            rooms: Arc::clone(self),
            doc,
            generation,
        };
        tokio::spawn(run(place, name, rx, Arc::clone(store), timing, grace));
        open.insert(
            doc,
            RoomHandle {
                tx: tx.clone(),
                generation,
            },
        );
        tx
    }

    fn close(&self, doc: DocId, generation: u64) {
        let mut open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
        if open.get(&doc).is_some_and(|h| h.generation == generation) {
            open.remove(&doc);
        }
    }
}

/// Which registry entry a room task owns.
struct Place {
    rooms: Arc<Rooms>,
    doc: DocId,
    generation: u64,
}

type Running = JoinHandle<Result<Outcome, CheckpointError>>;

/// Loads the document, then handles commands and writes checkpoints until nobody has been
/// connected for `grace` and everything is saved.
async fn run(
    place: Place,
    name: String,
    mut rx: mpsc::Receiver<Command>,
    store: Arc<dyn Checkpointer>,
    timing: Timing,
    grace: Duration,
) {
    let doc = place.doc;
    let initial = match load(&store, doc).await {
        Ok(initial) => initial,
        Err(err) => {
            eprintln!("sharecraft-server: cannot open document {}: {err}", doc.0);
            // Starting empty would later overwrite the stored document. Refuse everyone who
            // is waiting; a later join opens a fresh room that tries the load again.
            place.rooms.close(doc, place.generation);
            rx.close();
            while let Ok(cmd) = rx.try_recv() {
                if let Command::Join { accepted, .. } = cmd {
                    let _ = accepted.send(JoinReply::Refused);
                }
            }
            return;
        }
    };

    let mut room = Room::new(doc, name, initial, timing);
    let mut running: Option<Running> = None;
    // The room opens for a join waiting in its channel; until that is handled it is empty.
    let mut empty_since = Some(Instant::now());
    loop {
        let due = match running {
            Some(_) => None,
            None => room.checkpoint_due(),
        };
        let now = Instant::now();
        // Only a grace period still running needs a wake-up; once it is over, the room ends
        // as soon as everything is saved, which a finishing checkpoint reports.
        let grace_ends = empty_since
            .map(|since| grace_end(since, grace))
            .filter(|end| *end > now);
        tokio::select! {
            // A waiting join is handled before the room can decide to end.
            biased;
            cmd = rx.recv() => match cmd {
                Some(cmd) => room.handle(cmd),
                // The registry holds a sender until this task removes it, so this does not
                // happen; there is nobody left to serve if it does.
                None => break,
            },
            result = finished(&mut running) => {
                running = None;
                room.finish_checkpoint(result);
            }
            () = sleep_until(due) => {
                if let Some(job) = room.start_checkpoint() {
                    let store = Arc::clone(&store);
                    let file_name = room.name.clone();
                    running = Some(tokio::task::spawn_blocking(move || {
                        job.write(doc, &file_name, &*store)
                    }));
                }
            }
            () = sleep_until(grace_ends) => {}
        }
        empty_since = match (room.is_empty(), empty_since) {
            (false, _) => None,
            (true, since) => since.or_else(|| Some(Instant::now())),
        };
        let grace_over = empty_since.is_some_and(|since| grace_end(since, grace) <= Instant::now());
        if grace_over && running.is_none() && room.may_end() {
            break;
        }
    }
    place.rooms.close(doc, place.generation);
    rx.close();
    // Joins queued after the room decided to end are dropped unanswered, so those
    // connections open a fresh room, which loads what this one saved.
    while rx.try_recv().is_ok() {}
}

/// When the grace period of a room empty since `since` ends.
fn grace_end(since: Instant, grace: Duration) -> Instant {
    since.checked_add(grace).unwrap_or(since)
}

/// Reads the document's current revision without blocking the async workers.
async fn load(store: &Arc<dyn Checkpointer>, doc: DocId) -> Result<Initial, String> {
    let store = Arc::clone(store);
    let loaded = tokio::task::spawn_blocking(move || store.load(doc))
        .await
        .map_err(|e| format!("load task failed: {e}"))?
        .map_err(|e| e.to_string())?;
    let Some(stored) = loaded else {
        return Ok(Initial::default());
    };
    let text = String::from_utf8(stored.bytes)
        .map_err(|_| format!("revision {} is not valid UTF-8 text", stored.revision))?;
    Ok(Initial {
        doc: TextDoc::new(text),
        revision: Some(stored.revision),
    })
}

/// The result of the running checkpoint; never ready when none is running. A panic in the
/// write counts as a failed checkpoint.
async fn finished(running: &mut Option<Running>) -> Result<Outcome, String> {
    match running {
        Some(handle) => match handle.await {
            Ok(result) => result.map_err(|e| e.to_string()),
            Err(e) => Err(format!("checkpoint task failed: {e}")),
        },
        None => std::future::pending().await,
    }
}

/// Waits until `due`; never ready when there is nothing due.
async fn sleep_until(due: Option<Instant>) {
    match due {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
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
    use std::time::Duration;
    use uuid::Uuid;

    fn new_room() -> Room {
        let timing = Timing {
            idle: Duration::from_millis(100),
            max: Duration::from_secs(30),
        };
        Room::new(
            DocId(Uuid::from_u128(77)),
            "Notes.txt".into(),
            Initial::default(),
            timing,
        )
    }

    #[test]
    fn names_are_made_valid_authors() {
        assert_eq!(credited_name("<Ann>\nX"), "AnnX");
        assert_eq!(credited_name(" <> \t"), "Guest");
        assert_eq!(credited_name("Bob"), "Bob");
        for unusable in [";", "'", "\"", "\\", ":", " ,; "] {
            assert_eq!(credited_name(unusable), "Guest", "{unusable:?}");
        }
        assert_eq!(credited_name(":)"), ":)");
    }

    #[test]
    fn saving_keeps_recent_entries_for_resume() {
        let mut room = new_room();
        let a = ClientId(Uuid::from_u128(1));
        let mut rx = join(&mut room, 1, a);
        for n in 0..1005u64 {
            frames(&mut rx);
            let len = room.log.doc().len_chars();
            room.handle(Command::Submit {
                conn: 1,
                client: a,
                op_id: OpId(Uuid::from_u128(1000 + u128::from(n))),
                base: room.log.head(),
                op: TextOp::replace(len, len, 0, "x").unwrap(),
            });
        }
        room.start_checkpoint().unwrap();
        room.finish_checkpoint(Ok(Outcome::Written(
            "0123456789012345678901234567890123456789".parse().unwrap(),
        )));
        assert_eq!(room.log.head(), Seq(1005));
        assert!(room.log.since(Seq(4)).is_none());
        assert_eq!(room.log.since(Seq(5)).unwrap().len(), 1000);
        let saved = frames(&mut rx);
        assert!(
            saved.iter().any(|m| matches!(
                m,
                ServerMsg::Save {
                    state: SaveState::Saved,
                    revision: Some(_),
                    through: Seq(1005)
                }
            )),
            "{saved:?}"
        );
    }

    fn join(room: &mut Room, conn: ConnId, client: ClientId) -> mpsc::Receiver<Outbound> {
        join_as(room, conn, client, Role::Editor)
    }

    /// Joins `client` as an account of its own (the same one for every join with `client`).
    fn join_as(
        room: &mut Room,
        conn: ConnId,
        client: ClientId,
        role: Role,
    ) -> mpsc::Receiver<Outbound> {
        join_user(room, conn, client, UserId(client.0), role).0
    }

    /// Joins `client` as `user`; returns the connection's queue and the room's answer.
    fn join_user(
        room: &mut Room,
        conn: ConnId,
        client: ClientId,
        user: UserId,
        role: Role,
    ) -> (mpsc::Receiver<Outbound>, JoinReply) {
        let (tx, rx) = mpsc::channel(OUTBOUND_CAPACITY);
        let (accepted, mut reply) = oneshot::channel();
        room.handle(Command::Join {
            conn,
            client,
            user,
            session: SessionId(Uuid::from_u128(600 + u128::from(conn))),
            name: "A".into(),
            role,
            resume: None,
            tx,
            accepted,
        });
        (rx, reply.try_recv().unwrap())
    }

    fn frames(rx: &mut mpsc::Receiver<Outbound>) -> Vec<ServerMsg<TextOp>> {
        let mut out = Vec::new();
        while let Ok(o) = rx.try_recv() {
            let list = match o {
                Outbound::One(f) => vec![f],
                Outbound::Many(fs) => fs,
            };
            out.extend(
                list.iter()
                    .map(|f| serde_json::from_str(f.as_str()).unwrap()),
            );
        }
        out
    }

    fn submit(room: &mut Room, client: ClientId, op_id: OpId, base: Seq, text: &str) {
        room.handle(Command::Submit {
            conn: 1,
            client,
            op_id,
            base,
            op: TextOp::replace(0, 0, 0, text).unwrap(),
        });
    }

    #[test]
    fn resend_after_log_truncation_is_not_applied_twice() {
        let mut room = new_room();
        let client = ClientId(Uuid::from_u128(1));
        let op_id = OpId(Uuid::from_u128(2));
        let mut rx = join(&mut room, 1, client);
        submit(&mut room, client, op_id, Seq(0), "abc");
        assert_eq!(room.log.doc().as_str(), "abc");
        room.log.truncate_through(Seq(1));
        frames(&mut rx);

        submit(&mut room, client, op_id, Seq(0), "abc");
        assert_eq!(room.log.doc().as_str(), "abc");
        assert_eq!(room.log.head(), Seq(1));
        // The entry is gone from the log; the resend is still recognised and dropped silently.
        assert!(frames(&mut rx).is_empty());
    }

    #[test]
    fn resend_of_kept_entry_is_dropped_silently() {
        let mut room = new_room();
        let a = ClientId(Uuid::from_u128(1));
        let b = ClientId(Uuid::from_u128(3));
        let first = OpId(Uuid::from_u128(2));
        let second = OpId(Uuid::from_u128(4));
        let mut rx_a = join(&mut room, 1, a);
        let mut rx_b = join(&mut room, 2, b);
        submit(&mut room, a, first, Seq(0), "abc");
        room.handle(Command::Submit {
            conn: 1,
            client: a,
            op_id: second,
            base: Seq(1),
            op: TextOp::replace(3, 3, 0, "d").unwrap(),
        });
        frames(&mut rx_a);
        frames(&mut rx_b);

        // The last change is caught by the room's own record ...
        submit(&mut room, a, second, Seq(1), "x");
        // ... and an earlier one by the log's duplicate check.
        submit(&mut room, a, first, Seq(0), "abc");
        assert_eq!(room.log.head(), Seq(2));
        assert_eq!(room.log.doc().as_str(), "abcd");
        assert!(frames(&mut rx_a).is_empty());
        assert!(frames(&mut rx_b).is_empty());
    }

    #[test]
    fn resume_from_another_epoch_is_reset() {
        let mut room = new_room();
        let a = ClientId(Uuid::from_u128(1));
        let _rx_a = join(&mut room, 1, a);
        submit(&mut room, a, OpId(Uuid::from_u128(2)), Seq(0), "abc");

        let (tx, mut rx) = mpsc::channel(OUTBOUND_CAPACITY);
        let (accepted, _) = oneshot::channel();
        room.handle(Command::Join {
            conn: 2,
            client: ClientId(Uuid::from_u128(5)),
            user: UserId(Uuid::from_u128(6)),
            session: SessionId(Uuid::from_u128(7)),
            name: "B".into(),
            role: Role::Editor,
            resume: Some(Resume {
                epoch: RoomEpoch(Uuid::from_u128(99)),
                seq: Seq(1),
            }),
            tx,
            accepted,
        });
        let got = frames(&mut rx);
        assert!(
            matches!(got.first(), Some(ServerMsg::Reset { epoch, seq: Seq(1), snapshot, .. })
                if *epoch == room.epoch && snapshot == "abc"),
            "{got:?}"
        );
    }

    #[test]
    fn full_queue_drops_that_member_only() {
        let mut room = new_room();
        let slow = ClientId(Uuid::from_u128(1));
        let fast = ClientId(Uuid::from_u128(2));
        let _slow_rx = join(&mut room, 1, slow);
        let mut fast_rx = join(&mut room, 2, fast);
        for n in 0..OUTBOUND_CAPACITY {
            frames(&mut fast_rx);
            let op_id = OpId(Uuid::from_u128(100 + n as u128));
            let base = room.log.head();
            let len = room.log.doc().len_chars();
            room.handle(Command::Submit {
                conn: 2,
                client: fast,
                op_id,
                base,
                op: TextOp::replace(len, len, 0, "x").unwrap(),
            });
        }
        assert_eq!(room.members.len(), 1);
        assert_eq!(room.members[0].client, fast);
    }

    #[test]
    fn stale_connection_cannot_act_after_replacement() {
        let mut room = new_room();
        let a = ClientId(Uuid::from_u128(1));
        let _old = join(&mut room, 1, a);
        let mut new = join(&mut room, 2, a);
        assert_eq!(room.members.len(), 1);
        room.handle(Command::Leave { conn: 1, client: a });
        assert_eq!(room.members.len(), 1);
        frames(&mut new);
        room.handle(Command::Submit {
            conn: 1,
            client: a,
            op_id: OpId(Uuid::from_u128(9)),
            base: Seq(0),
            op: TextOp::replace(0, 0, 0, "z").unwrap(),
        });
        assert_eq!(room.log.head(), Seq(0));
    }

    #[test]
    fn viewer_submit_is_refused_as_read_only() {
        let mut room = new_room();
        let viewer = ClientId(Uuid::from_u128(1));
        let mut rx = join_as(&mut room, 1, viewer, Role::Viewer);
        let joined = frames(&mut rx);
        assert!(
            matches!(
                joined.first(),
                Some(ServerMsg::Joined {
                    can_edit: false,
                    ..
                })
            ),
            "{joined:?}"
        );
        let op_id = OpId(Uuid::from_u128(2));
        submit(&mut room, viewer, op_id, Seq(0), "x");
        assert_eq!(room.log.head(), Seq(0));
        assert!(room.checkpoint_due().is_none());
        let got = frames(&mut rx);
        assert!(
            matches!(got.as_slice(), [ServerMsg::Refused { op_id: id, reason }]
                if *id == op_id && reason == "read_only"),
            "{got:?}"
        );
    }

    #[test]
    fn client_id_stays_with_the_account_that_first_used_it() {
        let mut room = new_room();
        let ann = UserId(Uuid::from_u128(10));
        let bob = UserId(Uuid::from_u128(11));
        let client = ClientId(Uuid::from_u128(1));
        let (mut ann_rx, reply) = join_user(&mut room, 1, client, ann, Role::Editor);
        assert_eq!(reply, JoinReply::Accepted);
        frames(&mut ann_rx);

        // While Ann is here, Bob cannot take her place.
        let (mut bob_rx, reply) = join_user(&mut room, 2, client, bob, Role::Owner);
        assert_eq!(reply, JoinReply::Refused);
        assert!(frames(&mut bob_rx).is_empty());
        assert_eq!(room.members.len(), 1);
        assert_eq!(room.members[0].conn, 1);
        assert!(
            frames(&mut ann_rx).is_empty(),
            "nobody is told of a refused join"
        );

        // Nor after she has left.
        room.handle(Command::Leave { conn: 1, client });
        assert!(room.members.is_empty());
        let (mut bob_rx, reply) = join_user(&mut room, 3, client, bob, Role::Owner);
        assert_eq!(reply, JoinReply::Refused);
        assert!(frames(&mut bob_rx).is_empty());
        assert!(room.members.is_empty());

        // Ann herself comes back with it.
        let (mut ann_rx, reply) = join_user(&mut room, 4, client, ann, Role::Editor);
        assert_eq!(reply, JoinReply::Accepted);
        assert!(matches!(
            frames(&mut ann_rx).first(),
            Some(ServerMsg::Joined { .. })
        ));
        assert_eq!(room.members.len(), 1);
        assert_eq!(room.members[0].user, ann);
    }
}
