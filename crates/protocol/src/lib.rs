#![forbid(unsafe_code)]
//! JSON messages exchanged between the browser and the server.
//!
//! Every message is a JSON object with a `type` field in `snake_case`. The messages are generic
//! over the change type `Op` so this crate knows nothing about what a change means.

use collab_core::document::Entry;
use collab_core::ids::{ClientId, OpId, Seq, UserId};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub mod file;

pub use file::{FileClientMsg, FileServerMsg, SeatState};

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
    /// The connection. One person with the document open in two tabs is two participants.
    pub client: ClientId,
    /// The account.
    pub user: UserId,
    /// The account's name.
    pub name: String,
    /// Whether they may change the text.
    pub can_edit: bool,
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
    /// The stored document was changed somewhere else; this room no longer saves.
    Conflict,
}

/// Why the server ended a connection with `Removed`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemovedReason {
    /// The person no longer has a role on the document.
    AccessRevoked,
    /// The session was signed out or expired.
    SignedOut,
    /// The account was disabled.
    AccountDisabled,
    /// The document was deleted.
    Deleted,
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
        /// Whether this connection may change the text.
        can_edit: bool,
        /// The log position the stored revision covers; anything after it is not saved yet.
        saved_through: Seq,
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
        /// Whether this connection may change the text.
        can_edit: bool,
        /// The log position the stored revision covers; anything after it is not saved yet.
        saved_through: Seq,
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
    /// This person's rights on the document changed while connected.
    Access {
        /// Whether they may now change the text.
        can_edit: bool,
    },
    /// The last frame before the server closes the connection; the page must not reconnect.
    Removed {
        /// Why.
        reason: RemovedReason,
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

    /// A text `join` always names `resume`, as `null` when not resuming. The server tells a
    /// text client's `join` from a file client's (`protocol::file`) by this key, so `resume`
    /// must never gain `skip_serializing_if`.
    #[test]
    fn join_without_resume_still_names_it() {
        let json = serde_json::to_string(&ClientMsg::<TextOp>::Join {
            client: ClientId(Uuid::nil()),
            resume: None,
        })
        .unwrap();
        assert!(json.contains("\"resume\":null"), "{json}");
        assert!(serde_json::from_str::<FileClientMsg>(&json).is_err());
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
        assert_eq!(
            serde_json::to_string(&SaveState::Conflict).unwrap(),
            "\"conflict\""
        );
    }

    #[test]
    fn removed_reason_is_snake_case() {
        let json = serde_json::to_string(&ServerMsg::<u32>::Removed {
            reason: RemovedReason::AccessRevoked,
        })
        .unwrap();
        assert_eq!(json, "{\"type\":\"removed\",\"reason\":\"access_revoked\"}");
        assert_eq!(
            serde_json::to_string(&RemovedReason::SignedOut).unwrap(),
            "\"signed_out\""
        );
        assert_eq!(
            serde_json::to_string(&RemovedReason::AccountDisabled).unwrap(),
            "\"account_disabled\""
        );
        assert_eq!(
            serde_json::to_string(&RemovedReason::Deleted).unwrap(),
            "\"deleted\""
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
                    user: UserId(Uuid::from_u128(9)),
                    name: "A".into(),
                    can_edit: false,
                    cursor: Some(2),
                }],
                can_edit: true,
                saved_through: Seq(0),
            },
            ServerMsg::Reset {
                epoch: RoomEpoch(Uuid::from_u128(6)),
                seq: Seq(2),
                snapshot: "x".into(),
                revision: None,
                can_edit: false,
                saved_through: Seq(1),
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
            ServerMsg::Save {
                state: SaveState::Conflict,
                revision: None,
                through: Seq(4),
            },
            ServerMsg::Access { can_edit: false },
            ServerMsg::Removed {
                reason: RemovedReason::SignedOut,
            },
        ];
        for m in msgs {
            let json = serde_json::to_string(&m).unwrap();
            let back: ServerMsg<u32> = serde_json::from_str(&json).unwrap();
            assert_eq!(back, m);
        }
    }

    /// The `type` string of every message, which the page and the server both rely on. Each
    /// `match` is exhaustive, so adding a variant fails to compile until it is listed here.
    #[test]
    fn message_type_tags_are_pinned() {
        fn client_tag(msg: &ClientMsg<u32>) -> &'static str {
            match msg {
                ClientMsg::Join { .. } => "join",
                ClientMsg::Submit { .. } => "submit",
                ClientMsg::Cursor { .. } => "cursor",
            }
        }
        fn server_tag(msg: &ServerMsg<u32>) -> &'static str {
            match msg {
                ServerMsg::Joined { .. } => "joined",
                ServerMsg::Reset { .. } => "reset",
                ServerMsg::Committed { .. } => "committed",
                ServerMsg::People { .. } => "people",
                ServerMsg::Save { .. } => "save",
                ServerMsg::Refused { .. } => "refused",
                ServerMsg::Access { .. } => "access",
                ServerMsg::Removed { .. } => "removed",
            }
        }
        let tag_of = |json: String| -> String {
            let value: serde_json::Value = serde_json::from_str(&json).unwrap();
            value["type"].as_str().unwrap().to_owned()
        };
        let epoch = RoomEpoch(Uuid::nil());
        let client: Vec<ClientMsg<u32>> = vec![
            ClientMsg::Join {
                client: ClientId(Uuid::nil()),
                resume: None,
            },
            ClientMsg::Submit {
                op_id: OpId(Uuid::nil()),
                base: Seq(0),
                op: 1,
            },
            ClientMsg::Cursor { at: None },
        ];
        for msg in &client {
            assert_eq!(tag_of(serde_json::to_string(msg).unwrap()), client_tag(msg));
        }
        let server: Vec<ServerMsg<u32>> = vec![
            ServerMsg::Joined {
                epoch,
                seq: Seq(0),
                snapshot: None,
                revision: None,
                participants: vec![],
                can_edit: true,
                saved_through: Seq(0),
            },
            ServerMsg::Reset {
                epoch,
                seq: Seq(0),
                snapshot: String::new(),
                revision: None,
                can_edit: true,
                saved_through: Seq(0),
            },
            ServerMsg::Committed {
                entry: Entry {
                    seq: Seq(1),
                    client: ClientId(Uuid::nil()),
                    op_id: OpId(Uuid::nil()),
                    op: 1,
                },
            },
            ServerMsg::People {
                participants: vec![],
            },
            ServerMsg::Save {
                state: SaveState::Saved,
                revision: None,
                through: Seq(0),
            },
            ServerMsg::Refused {
                op_id: OpId(Uuid::nil()),
                reason: String::new(),
            },
            ServerMsg::Access { can_edit: true },
            ServerMsg::Removed {
                reason: RemovedReason::SignedOut,
            },
        ];
        for msg in &server {
            assert_eq!(tag_of(serde_json::to_string(msg).unwrap()), server_tag(msg));
        }
        // The strings are written out in the matches above; this checks they are the
        // strings on the wire, and that no sample was dropped from the lists.
        assert_eq!(client.len(), 3);
        assert_eq!(server.len(), 8);
    }
}
