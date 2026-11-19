//! The live room's presence relay and room notices (G5 Task 4), kept apart from the room's
//! ordering and checkpoint code: presence notes, the `Returning` and `Saved` notices, and the
//! rule that refuses desktop builds in GridCraft rooms.

use std::collections::BTreeSet;

use collab_core::ids::{ClientId, Seq};
use protocol::live::{LiveBuild, LiveServerMsg, ParticipantNo, Place};
use tokio::time::Instant;

use super::{LiveRoom, encode};
use crate::live_store::LiveStore;
use crate::room::{ConnId, Outbound};

/// Rooms whose format starts with this take browser builds only.
///
/// Approach §12, C5 and C16: a desktop GridCraft build computes with a different maths library
/// from the browser build, so the two could disagree on a recalculated value and the room's
/// copies would drift apart. Remove this rule only together with that maths library
/// difference.
const BROWSER_ONLY_FORMAT_PREFIX: &str = "gridcraft/";

/// Whether a `build` client may not join a room of `format`; the connection answers it
/// `BuildRefused` and closes before it reaches the room, so no place is given.
pub(crate) fn build_refused(format: &str, build: LiveBuild) -> bool {
    build == LiveBuild::Desktop && format.starts_with(BROWSER_ONLY_FORMAT_PREFIX)
}

impl<S: LiveStore> LiveRoom<S> {
    /// A presence note from the connection `conn`: kept as its place's latest and relayed to
    /// every other connected member. Dropped when the connection holds no place, or when the
    /// note is too large or comes too fast.
    pub(super) fn relay_presence(&mut self, conn: ConnId, client: ClientId, payload: String) {
        let Some(participant) = self.current_participant(conn, client) else {
            return;
        };
        if let Some(message) = self.presence.note(participant, payload, Instant::now()) {
            self.send_to_others(client, &message);
        }
    }

    /// After a join's catch-up: the joiner gets everyone else's current note, and a returning
    /// client is announced to the others.
    pub(super) fn after_join(
        &mut self,
        client: ClientId,
        participant: ParticipantNo,
        place: Place,
    ) {
        let notes = self
            .presence
            .current(participant)
            .iter()
            .map(encode)
            .collect::<Vec<_>>();
        if !notes.is_empty()
            && self
                .members
                .send(Some(client), &Outbound::Many(notes))
                .any()
        {
            self.broadcast_people();
        }
        if matches!(place, Place::Returning { .. }) {
            self.send_to_others(client, &LiveServerMsg::Returning { participant });
        }
    }

    /// Clears the note of every place that is no longer held or no longer connected, telling
    /// everyone. Called whenever the people list is sent, which every connect, disconnect,
    /// leave, expiry and removal does. Returns whether a member was dropped meanwhile (its
    /// queue was full), so the caller sends the people list again.
    pub(super) fn drop_absent_presence(&mut self) -> bool {
        let present = self
            .session
            .as_ref()
            .into_iter()
            .flat_map(|session| &session.places)
            .filter(|(_, place)| {
                place.held
                    && self
                        .members
                        .iter()
                        .any(|member| member.client == place.client)
            })
            .map(|(participant, _)| *participant)
            .collect::<BTreeSet<_>>();
        let mut dropped = false;
        for cleared in self
            .presence
            .keep_only(|participant| present.contains(&participant))
        {
            dropped |= self
                .members
                .send(None, &Outbound::One(encode(&cleared)))
                .any();
        }
        dropped
    }

    /// Tells everyone a checkpoint was stored (`verified` false) or verified.
    pub(super) fn notify_saved(
        &mut self,
        revision: String,
        through: Seq,
        verified: bool,
        major: bool,
    ) {
        let frame = encode(&LiveServerMsg::Saved {
            revision,
            through,
            verified,
            major,
        });
        if self.members.send(None, &Outbound::One(frame)).any() {
            self.broadcast_people();
        }
    }

    /// Sends `message` to every connected member but `except`. A member dropped because its
    /// queue was full is shown as gone at once.
    fn send_to_others(&mut self, except: ClientId, message: &LiveServerMsg) {
        let frame = Outbound::One(encode(message));
        let others = self
            .members
            .iter()
            .map(|member| member.client)
            .filter(|client| *client != except)
            .collect::<Vec<_>>();
        let mut dropped = false;
        for client in others {
            dropped |= self.members.send(Some(client), &frame).any();
        }
        if dropped {
            self.broadcast_people();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_desktop_builds_of_gridcraft_rooms_are_refused() {
        assert!(build_refused("gridcraft/1", LiveBuild::Desktop));
        assert!(!build_refused("gridcraft/1", LiveBuild::Browser));
        assert!(!build_refused("stand-in/1", LiveBuild::Desktop));
        assert!(!build_refused("gridcraftish/1", LiveBuild::Desktop));
    }
}
