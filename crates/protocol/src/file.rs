//! Messages of a file session: a document opened in an editor by one person at a time.
//!
//! A file session holds no content. It tells everyone who is there, which revision is current,
//! and who holds the one editing seat; the file's bytes travel over HTTP. Like the text
//! messages, every message is a JSON object with a `type` field in `snake_case`.

use collab_core::ids::ClientId;
use serde::{Deserialize, Serialize};

use crate::{Participant, RemovedReason, SaveState};

/// Browser to server. Unknown fields are refused, so a text client's `join` (which carries
/// `resume`) is not taken for a file client's.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum FileClientMsg {
    /// Enter the session.
    Join {
        /// The connection's identifier.
        client: ClientId,
    },
}

/// Server to browser.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FileServerMsg {
    /// Join accepted.
    Joined {
        /// Who is here.
        participants: Vec<Participant>,
        /// The document's current revision; `None` if it was never saved.
        revision: Option<String>,
        /// Whether this connection's person may edit the file.
        can_edit: bool,
        /// Who holds the editing seat, as this connection sees it.
        seat: SeatState,
    },
    /// The participant list changed.
    People {
        /// Who is here.
        participants: Vec<Participant>,
    },
    /// The editing seat changed hands.
    Seat {
        /// Who holds it now.
        seat: SeatState,
    },
    /// Save progress.
    Save {
        /// Current state.
        state: SaveState,
        /// The revision written, once saved.
        revision: Option<String>,
    },
    /// This person's rights on the document changed while connected.
    Access {
        /// Whether they may now edit the file.
        can_edit: bool,
    },
    /// The last frame before the server closes the connection; the page must not reconnect.
    Removed {
        /// Why.
        reason: RemovedReason,
    },
}

/// Who holds the editing seat. Written as an object tagged by `state`: `yours`, `taken` or
/// `free`.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SeatState {
    /// This connection holds it. `token` proves it when saving; it is sent to this connection
    /// only, and changes every time the seat is given.
    Yours {
        /// 32 random bytes, hexadecimal.
        token: String,
    },
    /// Someone else holds it.
    Taken {
        /// The holder.
        by: Participant,
    },
    /// Nobody holds it. A connection takes a free seat only by joining again.
    Free,
}

impl std::fmt::Debug for SeatState {
    /// Leaves the token out.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SeatState::Yours { .. } => f.write_str("Yours { token: <redacted> }"),
            SeatState::Taken { by } => f.debug_struct("Taken").field("by", by).finish(),
            SeatState::Free => f.write_str("Free"),
        }
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
    use collab_core::ids::UserId;
    use uuid::Uuid;

    fn someone() -> Participant {
        Participant {
            client: ClientId(Uuid::from_u128(1)),
            user: UserId(Uuid::from_u128(2)),
            name: "Ann".into(),
            can_edit: true,
            cursor: None,
        }
    }

    fn tag_of(json: &str, field: &str) -> String {
        let value: serde_json::Value = serde_json::from_str(json).unwrap();
        value[field].as_str().unwrap().to_owned()
    }

    /// The `type` string of every message, and the `state` string of every seat, which the
    /// page and the server both rely on. Each `match` is exhaustive, so adding a variant fails
    /// to compile until it is listed here.
    #[test]
    fn file_message_tags_are_pinned() {
        fn client_tag(msg: &FileClientMsg) -> &'static str {
            match msg {
                FileClientMsg::Join { .. } => "join",
            }
        }
        fn server_tag(msg: &FileServerMsg) -> &'static str {
            match msg {
                FileServerMsg::Joined { .. } => "joined",
                FileServerMsg::People { .. } => "people",
                FileServerMsg::Seat { .. } => "seat",
                FileServerMsg::Save { .. } => "save",
                FileServerMsg::Access { .. } => "access",
                FileServerMsg::Removed { .. } => "removed",
            }
        }
        fn seat_tag(seat: &SeatState) -> &'static str {
            match seat {
                SeatState::Yours { .. } => "yours",
                SeatState::Taken { .. } => "taken",
                SeatState::Free => "free",
            }
        }
        let client = vec![FileClientMsg::Join {
            client: ClientId(Uuid::nil()),
        }];
        for msg in &client {
            let json = serde_json::to_string(msg).unwrap();
            assert_eq!(tag_of(&json, "type"), client_tag(msg));
            assert_eq!(serde_json::from_str::<FileClientMsg>(&json).unwrap(), *msg);
        }
        let seats = vec![
            SeatState::Yours {
                token: "ab".repeat(32),
            },
            SeatState::Taken { by: someone() },
            SeatState::Free,
        ];
        for seat in &seats {
            let json = serde_json::to_string(seat).unwrap();
            assert_eq!(tag_of(&json, "state"), seat_tag(seat));
        }
        let server = vec![
            FileServerMsg::Joined {
                participants: vec![someone()],
                revision: None,
                can_edit: true,
                seat: SeatState::Free,
            },
            FileServerMsg::People {
                participants: vec![],
            },
            FileServerMsg::Seat {
                seat: SeatState::Free,
            },
            FileServerMsg::Save {
                state: SaveState::Saved,
                revision: Some("abc".into()),
            },
            FileServerMsg::Access { can_edit: false },
            FileServerMsg::Removed {
                reason: RemovedReason::SignedOut,
            },
        ];
        for msg in &server {
            let json = serde_json::to_string(msg).unwrap();
            assert_eq!(tag_of(&json, "type"), server_tag(msg));
            assert_eq!(serde_json::from_str::<FileServerMsg>(&json).unwrap(), *msg);
        }
        // The strings are written out in the matches above; this checks they are the strings
        // on the wire, and that no sample was dropped from the lists.
        assert_eq!(client.len(), 1);
        assert_eq!(seats.len(), 3);
        assert_eq!(server.len(), 6);
        assert_eq!(
            serde_json::to_string(&FileServerMsg::Seat {
                seat: SeatState::Free
            })
            .unwrap(),
            "{\"type\":\"seat\",\"seat\":{\"state\":\"free\"}}"
        );
    }

    /// Only `Yours` carries the token, and its `Debug` form does not show it.
    #[test]
    fn seat_token_is_only_in_yours() {
        let token = "0f".repeat(32);
        let yours = serde_json::to_value(SeatState::Yours {
            token: token.clone(),
        })
        .unwrap();
        assert_eq!(yours["token"], token.as_str());
        for seat in [SeatState::Taken { by: someone() }, SeatState::Free] {
            let json = serde_json::to_string(&seat).unwrap();
            assert!(!json.contains("token"), "{json}");
        }
        let shown = format!(
            "{:?}",
            SeatState::Yours {
                token: token.clone()
            }
        );
        assert!(!shown.contains(&token), "{shown}");
    }

    /// A text client's `join` names `resume`; it is not a file client's `join`, so a text page
    /// that opens a file document is closed.
    #[test]
    fn text_join_is_not_a_file_join() {
        let text = "{\"type\":\"join\",\"client\":\"00000000-0000-0000-0000-000000000000\",\
                    \"resume\":null}";
        assert!(serde_json::from_str::<FileClientMsg>(text).is_err());
        let file = "{\"type\":\"join\",\"client\":\"00000000-0000-0000-0000-000000000000\"}";
        assert!(serde_json::from_str::<FileClientMsg>(file).is_ok());
    }
}
