#![forbid(unsafe_code)]
//! JSON messages exchanged between the browser and the server.
//!
//! Every message is a JSON object with a `type` field in `snake_case`. The messages are generic
//! over the change type `Op` so this crate knows nothing about what a change means.

use collab_core::document::Entry;
use collab_core::ids::{ClientId, OpId, Seq};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Identifies one lifetime of a document's room. Log positions restart whenever a room is
/// created, so a position only means something together with the epoch it was issued in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RoomEpoch(pub Uuid);

/// Where a reconnecting client left off.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resume {
    /// The room lifetime the client was in.
    pub epoch: RoomEpoch,
    /// Last sequence the client applied in that room.
    pub seq: Seq,
}

/// Someone connected to a document.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Participant {
    /// The connection.
    pub client: ClientId,
    /// Display name.
    pub name: String,
    /// Cursor position in Unicode scalar values, if they have one.
    pub cursor: Option<usize>,
}

/// Whether the latest changes have reached Git.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SaveState {
    /// Everything up to `through` is stored.
    Saved,
    /// A checkpoint is in progress.
    Saving,
    /// The last checkpoint failed.
    Failed,
}

/// Browser to server.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg<Op> {
    /// Enter a document. `resume` says where the client left off, when reconnecting.
    Join {
        /// The connection's identifier.
        client: ClientId,
        /// The room lifetime and last sequence the client applied, if reconnecting.
        resume: Option<Resume>,
    },
    /// Propose a change.
    Submit {
        /// The client's identifier for the change.
        op_id: OpId,
        /// The sequence the change was made against.
        base: Seq,
        /// The change.
        op: Op,
    },
    /// Move the cursor.
    Cursor {
        /// New position, or `None` to hide it.
        at: Option<usize>,
    },
}

/// Server to browser.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg<Op> {
    /// Join accepted. `snapshot` is `None` when `resume` was honoured; the missed entries
    /// follow as `Committed` messages.
    Joined {
        /// The room's lifetime; send it back in `resume` when reconnecting.
        epoch: RoomEpoch,
        /// Current sequence.
        seq: Seq,
        /// Full text, unless `resume` was honoured.
        snapshot: Option<String>,
        /// Stored revision the document is based on.
        revision: Option<String>,
        /// Who is here.
        participants: Vec<Participant>,
    },
    /// `resume` could not be honoured; replace local state.
    Reset {
        /// The room's lifetime; send it back in `resume` when reconnecting.
        epoch: RoomEpoch,
        /// Current sequence.
        seq: Seq,
        /// Full text.
        snapshot: String,
        /// Stored revision the document is based on.
        revision: Option<String>,
    },
    /// A change was committed.
    Committed {
        /// The committed entry.
        entry: Entry<Op>,
    },
    /// The participant list changed.
    People {
        /// Who is here.
        participants: Vec<Participant>,
    },
    /// Save progress.
    Save {
        /// Current state.
        state: SaveState,
        /// Revision written, if any.
        revision: Option<String>,
        /// Highest sequence covered.
        through: Seq,
    },
    /// A submitted change was refused.
    Refused {
        /// The refused change.
        op_id: OpId,
        /// Why.
        reason: String,
    },
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
    use collab_text::{Component, TextOp};
    use uuid::Uuid;

    #[test]
    fn submit_serializes_with_type_tag() {
        let msg = ClientMsg::Submit {
            op_id: OpId(Uuid::nil()),
            base: Seq(3),
            op: TextOp::from(vec![Component::Retain(2), Component::Insert("hi".into())]),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"submit\""), "{json}");
        let back: ClientMsg<TextOp> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn join_carries_epoch_and_seq() {
        let msg = ClientMsg::<TextOp>::Join {
            client: ClientId(Uuid::nil()),
            resume: Some(Resume {
                epoch: RoomEpoch(Uuid::nil()),
                seq: Seq(4),
            }),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(
            json,
            "{\"type\":\"join\",\"client\":\"00000000-0000-0000-0000-000000000000\",\
             \"resume\":{\"epoch\":\"00000000-0000-0000-0000-000000000000\",\"seq\":4}}"
        );
        let back: ClientMsg<TextOp> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn save_state_is_snake_case() {
        assert_eq!(
            serde_json::to_string(&SaveState::Saved).unwrap(),
            "\"saved\""
        );
        assert_eq!(
            serde_json::to_string(&SaveState::Saving).unwrap(),
            "\"saving\""
        );
        assert_eq!(
            serde_json::to_string(&SaveState::Failed).unwrap(),
            "\"failed\""
        );
    }

    #[test]
    fn server_messages_round_trip() {
        let msgs: Vec<ServerMsg<u32>> = vec![
            ServerMsg::Joined {
                epoch: RoomEpoch(Uuid::from_u128(5)),
                seq: Seq(1),
                snapshot: None,
                revision: Some("abc".into()),
                participants: vec![Participant {
                    client: ClientId(Uuid::nil()),
                    name: "A".into(),
                    cursor: Some(2),
                }],
            },
            ServerMsg::Reset {
                epoch: RoomEpoch(Uuid::from_u128(6)),
                seq: Seq(2),
                snapshot: "x".into(),
                revision: None,
            },
            ServerMsg::Committed {
                entry: Entry {
                    seq: Seq(1),
                    client: ClientId(Uuid::nil()),
                    op_id: OpId(Uuid::nil()),
                    op: 7,
                },
            },
            ServerMsg::People {
                participants: vec![],
            },
            ServerMsg::Save {
                state: SaveState::Saving,
                revision: None,
                through: Seq(4),
            },
            ServerMsg::Refused {
                op_id: OpId(Uuid::nil()),
                reason: "no".into(),
            },
        ];
        for m in msgs {
            let json = serde_json::to_string(&m).unwrap();
            let back: ServerMsg<u32> = serde_json::from_str(&json).unwrap();
            assert_eq!(back, m);
        }
    }
}
