//! The people connected to one room, shared by text rooms and file sessions: who is there, which
//! account each client id belongs to, and the rules every room keeps when it sends to them,
//! removes them or changes their role.
//!
//! Each room kind encodes its own frames (see [`Frames`]). Whenever members leave, through
//! [`Members::leave`], [`Members::remove`] or because their queue was full or closed when
//! something was sent, the call says so with [`Departures`]; the room then runs its own
//! "members left" step (a new people list, and in a file session the editing seat).

use std::collections::HashMap;
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};

use axum::extract::ws::Utf8Bytes;
use collab_core::ids::{ClientId, DocId, SessionId, UserId};
use protocol::{Participant, RemovedReason};
use records::{Access, NewEvent};
use tokio::sync::mpsc;

use crate::events::EventSink;
use crate::room::{AccessChange, ConnId, Outbound};

/// The access the built-in role `id` gives, for tests.
#[cfg(test)]
pub(crate) fn builtin_access(id: &str) -> Access {
    records::RoleDef::builtin(id).map_or_else(
        || Access {
            perms: records::Perms::NONE,
            roles: Vec::new(),
        },
        |role| Access::from_role(&role),
    )
}

/// How one room kind writes the frames [`Members`] sends on its behalf. `None` if a frame
/// cannot be encoded; it is then not sent.
pub(crate) trait Frames {
    /// Who is here.
    fn people(participants: Vec<Participant>) -> Option<Utf8Bytes>;
    /// This connection's rights changed.
    fn access(can_edit: bool) -> Option<Utf8Bytes>;
    /// The last frame of a connection the server removes.
    fn removed(reason: RemovedReason) -> Option<Utf8Bytes>;
}

/// One connection in a room.
pub(crate) struct Member {
    pub conn: ConnId,
    pub client: ClientId,
    pub user: UserId,
    /// The sign-in session the connection belongs to.
    pub session: SessionId,
    /// The account's name, made a valid author name.
    pub name: String,
    /// What this person may do; it can change while they are connected.
    pub role: Access,
    /// Cursor position in a text room; always `None` in a file session.
    pub cursor: Option<usize>,
    pub tx: mpsc::Sender<Outbound>,
}

/// Whether members left during a call. The room must then run its "members left" step.
#[must_use = "members left: the room must run its members-left step"]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Departures(bool);

impl Departures {
    /// Whether anyone left.
    pub(crate) fn any(self) -> bool {
        self.0
    }
}

/// How [`Members::admit`] answered a join.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Admission {
    /// The client id belongs to another account; nothing changed.
    Refused,
    /// The member is in; `replaced` when it took the place of a connection with the same
    /// client id, which is then closed.
    Joined { replaced: bool },
}

/// What [`Members::change_role`] did.
#[must_use]
pub(crate) struct RoleChange {
    /// Some connection's role changed.
    pub changed: bool,
    /// Members dropped because they could not take their `Access` frame.
    pub departed: Departures,
}

/// The members of one room, in joining order. Read as a slice of [`Member`]; changed only
/// through the methods below, which keep the rules.
pub(crate) struct Members<F> {
    doc: DocId,
    list: Vec<Member>,
    /// The account that first joined with each client id. A client id stays with that account
    /// for the room's lifetime, also after they leave, so nobody else can take over a
    /// connection's place.
    client_owners: HashMap<ClientId, UserId>,
    /// Where joins, departures and removals are reported.
    events: EventSink,
    frames: PhantomData<F>,
}

impl<F> Deref for Members<F> {
    type Target = [Member];

    fn deref(&self) -> &[Member] {
        &self.list
    }
}

impl<F> DerefMut for Members<F> {
    fn deref_mut(&mut self) -> &mut [Member] {
        &mut self.list
    }
}

impl<F: Frames> Members<F> {
    /// Nobody, in the room of `doc`, reporting to `events`.
    pub(crate) fn new(doc: DocId, events: EventSink) -> Self {
        Self {
            doc,
            list: Vec::new(),
            client_owners: HashMap::new(),
            events,
            frames: PhantomData,
        }
    }

    /// Whether the client id `client` may be used by `user`: it is theirs, or nobody's yet, in
    /// which case it becomes theirs.
    pub(crate) fn claim(&mut self, client: ClientId, user: UserId) -> bool {
        *self.client_owners.entry(client).or_insert(user) == user
    }

    /// Lets `member` in, replacing any connection with the same client id (dropping the older
    /// connection's queue closes it), unless the client id belongs to another account. A new
    /// arrival is reported as `editing.joined`; replacing one's own live connection is not one.
    pub(crate) fn admit(&mut self, member: Member) -> Admission {
        let (client, user) = (member.client, member.user);
        if !self.claim(client, user) {
            return Admission::Refused;
        }
        let can_edit = member.role.can_edit();
        let replaced = match self.list.iter_mut().find(|m| m.client == client) {
            Some(existing) => {
                *existing = member;
                true
            }
            None => {
                self.list.push(member);
                false
            }
        };
        if !replaced {
            self.events.emit(NewEvent {
                kind: "editing.joined",
                actor: Some(user),
                subject: Some(user),
                document: Some(self.doc),
                node: Some(self.doc.into()),
                detail: serde_json::json!({ "can_edit": can_edit }),
            });
        }
        Admission::Joined { replaced }
    }

    /// The connection `conn` with `client` ended: it is no longer a member, reported as
    /// `editing.left`. Returns whose it was, if it still was a member (a replaced connection's
    /// late leave is ignored).
    pub(crate) fn leave(&mut self, conn: ConnId, client: ClientId) -> Option<UserId> {
        let left = self
            .list
            .iter()
            .find(|m| m.conn == conn && m.client == client)
            .map(|m| m.user);
        self.list
            .retain(|m| !(m.conn == conn && m.client == client));
        if let Some(user) = left {
            self.events.emit(NewEvent {
                kind: "editing.left",
                actor: Some(user),
                subject: Some(user),
                document: Some(self.doc),
                node: Some(self.doc.into()),
                detail: serde_json::json!({}),
            });
        }
        left
    }

    /// Gives every connection of `user` the access `role`. Each connection whose access changes
    /// is told with `Access`, even when it may edit as before (an owner becoming an editor,
    /// say), so the page can read its access again.
    pub(crate) fn change_role(&mut self, user: UserId, role: Access) -> RoleChange {
        let mut told = Vec::new();
        for member in self.list.iter_mut().filter(|m| m.user == user) {
            if member.role == role {
                continue;
            }
            told.push(member.client);
            member.role = role.clone();
        }
        let changed = !told.is_empty();
        let mut departed = Departures(false);
        if let Some(frame) = F::access(role.can_edit()) {
            for client in told {
                departed.0 |= self.send(Some(client), &Outbound::One(frame.clone())).any();
            }
        }
        RoleChange { changed, departed }
    }

    /// Removes every member `which` selects: queues `Removed { reason }` as the connection's
    /// last frame and drops its queue, which ends the connection once the frame is written.
    /// Anything the connection sends afterwards is ignored, as it is no longer a member. Each
    /// removal is reported as `editing.removed`.
    pub(crate) fn remove(
        &mut self,
        which: impl Fn(&Member) -> bool,
        reason: RemovedReason,
    ) -> Departures {
        let Some(frame) = F::removed(reason) else {
            return Departures(false);
        };
        let doc = self.doc;
        let events = &self.events;
        let detail = serde_json::json!({ "reason": reason });
        let before = self.list.len();
        self.list.retain(|m| {
            if !which(m) {
                return true;
            }
            events.emit(NewEvent {
                kind: "editing.removed",
                actor: None,
                subject: Some(m.user),
                document: Some(doc),
                node: Some(doc.into()),
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
        Departures(self.list.len() != before)
    }

    /// Who is here, in joining order.
    pub(crate) fn participants(&self) -> Vec<Participant> {
        self.list
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

    /// Sends everyone the people list.
    pub(crate) fn broadcast_people(&mut self) -> Departures {
        match F::people(self.participants()) {
            Some(frame) => self.send(None, &Outbound::One(frame)),
            None => Departures(false),
        }
    }

    /// Queues `out` for `to`, or for everyone when `to` is `None`. A member whose queue is full
    /// or closed is dropped (it reconnects and catches up), reported as `editing.left`.
    pub(crate) fn send(&mut self, to: Option<ClientId>, out: &Outbound) -> Departures {
        let mut dropped = false;
        let events = &self.events;
        let doc = self.doc;
        self.list.retain(|m| {
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
                    node: Some(doc.into()),
                    detail: serde_json::json!({}),
                });
            }
            dropped |= !delivered;
            delivered
        });
        Departures(dropped)
    }
}

/// A room's handling of the access changes, which every room kind takes with the same meaning
/// (see [`apply_access`]).
pub(crate) trait AccessRules {
    /// `user`'s connections now have `role`.
    fn change_role(&mut self, user: UserId, role: Access);
    /// The connections `which` selects are removed with `Removed { reason }`.
    fn remove(&mut self, which: &dyn Fn(&Member) -> bool, reason: RemovedReason);
    /// The document is now named `name`; later revisions are stored under it.
    fn rename(&mut self, name: String);
    /// Everyone connected, each once, including connections still being let in.
    fn connected_users(&self) -> Vec<UserId>;
}

/// The accounts of `members`, each once, in the order first seen.
pub(crate) fn users_of<'a>(members: impl IntoIterator<Item = &'a Member>) -> Vec<UserId> {
    let mut users = Vec::new();
    for member in members {
        if !users.contains(&member.user) {
            users.push(member.user);
        }
    }
    users
}

/// Carries out one access change in `room`: a role adjusts every connection of that person;
/// no role, an ended sign-in or a disabled account removes the connections concerned; a
/// deleted document removes everyone; a rename changes the name later revisions are stored
/// under; a question about who is connected is answered.
pub(crate) fn apply_access(room: &mut impl AccessRules, change: AccessChange) {
    match change {
        AccessChange::SetAccess { user, role } => match role {
            Some(role) => room.change_role(user, role),
            None => room.remove(&|m| m.user == user, RemovedReason::AccessRevoked),
        },
        AccessChange::EndSession { session } => {
            room.remove(&|m| m.session == session, RemovedReason::SignedOut);
        }
        AccessChange::RemoveUser { user } => {
            room.remove(&|m| m.user == user, RemovedReason::AccountDisabled);
        }
        AccessChange::Deleted => room.remove(&|_| true, RemovedReason::Deleted),
        AccessChange::Renamed { name } => room.rename(name),
        AccessChange::ConnectedUsers { reply } => {
            // The asker may have gone away; nothing to do then.
            let _ = reply.send(room.connected_users());
        }
    }
}
