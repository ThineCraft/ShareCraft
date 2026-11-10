//! One room per open document: a single task owns the document's change log and everyone
//! connected to it, and receives commands from the connections over a channel.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use axum::extract::ws::Utf8Bytes;
use collab_core::document::Submitted;
use collab_core::ids::{ClientId, DocId, OpId, Seq};
use collab_core::server_log::ServerLog;
use collab_text::{TextDoc, TextOp};
use protocol::{Participant, Resume, RoomEpoch, ServerMsg};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

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

/// What a connection asks of its room.
#[derive(Debug)]
pub(crate) enum Command {
    /// Enter the room, replacing any connection with the same client id.
    Join {
        conn: ConnId,
        client: ClientId,
        name: String,
        resume: Option<Resume>,
        tx: mpsc::Sender<Outbound>,
        /// Answered once the join is handled; dropped if the room closed first.
        accepted: oneshot::Sender<()>,
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
    pub revision: Option<String>,
}

struct Member {
    conn: ConnId,
    client: ClientId,
    name: String,
    cursor: Option<usize>,
    tx: mpsc::Sender<Outbound>,
}

/// The state of one open document. Handling a command never blocks: frames are queued with
/// `try_send` and a connection whose queue is full or closed is dropped.
pub(crate) struct Room {
    /// This lifetime of the room; log positions only mean something within it.
    epoch: RoomEpoch,
    log: ServerLog<TextDoc>,
    revision: Option<String>,
    /// In joining order.
    members: Vec<Member>,
    /// Each client's last committed change, so a resend is recognised however much of the log
    /// is kept.
    last_committed: HashMap<ClientId, OpId>,
}

impl Room {
    /// A room holding `initial`, with its log at `Seq(0)` and a fresh random epoch.
    pub(crate) fn new(initial: Initial) -> Self {
        Self {
            epoch: RoomEpoch(Uuid::new_v4()),
            log: ServerLog::new(initial.doc, Seq(0)),
            revision: initial.revision,
            members: Vec::new(),
            last_committed: HashMap::new(),
        }
    }

    /// Whether nobody is connected.
    pub(crate) fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    /// Carries out one command.
    pub(crate) fn handle(&mut self, cmd: Command) {
        match cmd {
            Command::Join {
                conn,
                client,
                name,
                resume,
                tx,
                accepted,
            } => {
                self.join(conn, client, name, resume, tx);
                // The connection may have gone away meanwhile; nothing to do then.
                let _ = accepted.send(());
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
        name: String,
        resume: Option<Resume>,
        tx: mpsc::Sender<Outbound>,
    ) {
        let member = Member {
            conn,
            client,
            name,
            cursor: None,
            tx,
        };
        // Replacing the member drops the older connection's queue, which closes it.
        match self.members.iter_mut().find(|m| m.client == client) {
            Some(existing) => *existing = member,
            None => self.members.push(member),
        }

        let epoch = self.epoch;
        let seq = self.log.head();
        let text = self.log.doc().as_str().to_owned();
        let revision = self.revision.clone();
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
            }),
            (Some(_), Some(missed)) => {
                msgs.push(ServerMsg::Joined {
                    epoch,
                    seq,
                    snapshot: None,
                    revision,
                    participants: self.participants(),
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
            }),
        }
        let frames = msgs.iter().filter_map(encode).collect();
        self.send(Some(client), &Outbound::Many(frames));
        self.broadcast_people();
    }

    fn submit(&mut self, conn: ConnId, client: ClientId, op_id: OpId, base: Seq, op: TextOp) {
        if !self.is_current(conn, client) {
            return;
        }
        // A resend of a committed change is dropped silently: this connection has already
        // received the entry through its snapshot, its catch-up or the broadcast.
        if self.last_committed.get(&client) == Some(&op_id) {
            return;
        }
        match self.log.submit(client, op_id, base, op) {
            Ok(Submitted::Committed(entry)) => {
                self.last_committed.insert(client, op_id);
                if let Some(frame) = encode(&ServerMsg::Committed { entry }) {
                    self.send(None, &Outbound::One(frame));
                }
            }
            Ok(Submitted::Duplicate(_)) => {}
            Err(err) => {
                let refused = ServerMsg::Refused {
                    op_id,
                    reason: err.to_string(),
                };
                if let Some(frame) = encode(&refused) {
                    self.send(Some(client), &Outbound::One(frame));
                }
            }
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

    fn is_current(&self, conn: ConnId, client: ClientId) -> bool {
        self.members
            .iter()
            .any(|m| m.conn == conn && m.client == client)
    }

    fn participants(&self) -> Vec<Participant> {
        self.members
            .iter()
            .map(|m| Participant {
                client: m.client,
                name: m.name.clone(),
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
    /// The command channel of `doc`'s room, starting the room from `load` if it is not open.
    ///
    /// The channel can be closed by the time it is used, if the room ended meanwhile; the
    /// caller then asks again.
    pub(crate) fn open(
        self: &Arc<Self>,
        doc: DocId,
        load: impl FnOnce(DocId) -> Initial,
    ) -> mpsc::Sender<Command> {
        let mut open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(handle) = open.get(&doc)
            && !handle.tx.is_closed()
        {
            return handle.tx.clone();
        }
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel(COMMAND_CAPACITY);
        let room = Room::new(load(doc));
        tokio::spawn(run(room, rx, Arc::clone(self), doc, generation));
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

/// Handles commands until the last participant leaves.
async fn run(
    mut room: Room,
    mut rx: mpsc::Receiver<Command>,
    rooms: Arc<Rooms>,
    doc: DocId,
    generation: u64,
) {
    while let Some(cmd) = rx.recv().await {
        room.handle(cmd);
        if room.is_empty() {
            break;
        }
    }
    rooms.close(doc, generation);
    rx.close();
    // Joins queued after the room decided to end are dropped unanswered, so those
    // connections open a fresh room.
    while rx.try_recv().is_ok() {}
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
    use uuid::Uuid;

    fn join(room: &mut Room, conn: ConnId, client: ClientId) -> mpsc::Receiver<Outbound> {
        let (tx, rx) = mpsc::channel(OUTBOUND_CAPACITY);
        let (accepted, _) = oneshot::channel();
        room.handle(Command::Join {
            conn,
            client,
            name: "A".into(),
            resume: None,
            tx,
            accepted,
        });
        rx
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
        let mut room = Room::new(Initial::default());
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
        let mut room = Room::new(Initial::default());
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
        let mut room = Room::new(Initial::default());
        let a = ClientId(Uuid::from_u128(1));
        let _rx_a = join(&mut room, 1, a);
        submit(&mut room, a, OpId(Uuid::from_u128(2)), Seq(0), "abc");

        let (tx, mut rx) = mpsc::channel(OUTBOUND_CAPACITY);
        let (accepted, _) = oneshot::channel();
        room.handle(Command::Join {
            conn: 2,
            client: ClientId(Uuid::from_u128(5)),
            name: "B".into(),
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
        let mut room = Room::new(Initial::default());
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
        let mut room = Room::new(Initial::default());
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
}
