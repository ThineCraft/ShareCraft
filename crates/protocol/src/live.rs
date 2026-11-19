//! Messages of a live session: a document edited by several people at once, where the server
//! puts every change in one order without ever looking inside it.
//!
//! A change's content (`payload`) is opaque to the server: it is stored and relayed byte for
//! byte, and only the editors read it. Like the text and file messages, every message is a
//! JSON object with a `type` field in `snake_case`. A live client joins with `live_join`, not
//! `join`, so it can never be taken for a text or file client.

use collab_core::ids::{ClientId, Seq, UserId};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::RemovedReason;

/// The live protocol's version. A client and server that disagree end with `Incompatible`.
pub const LIVE_PROTOCOL: u32 = 1;
/// The most bytes a change's content may hold.
pub const MAX_PAYLOAD_BYTES: usize = 262_144;
/// The longest format name, in characters.
pub const MAX_FORMAT_LEN: usize = 64;
/// How often a client sends a heartbeat, in milliseconds.
pub const HEARTBEAT_MS: u64 = 60_000;
/// How many heartbeat periods may pass without any message before a place ends.
pub const EXPIRY_MISSES: u32 = 3;

/// Whether `format` is a valid format name: 1 to [`MAX_FORMAT_LEN`] characters, each one of
/// `a-z`, `0-9`, `.`, `_`, `/` or `-`.
pub fn valid_format(format: &str) -> bool {
    !format.is_empty()
        && format.len() <= MAX_FORMAT_LEN
        && format.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'/' | b'-')
        })
}

/// Identifies one live session of a document: from the first person joining until the last
/// one has left or expired.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LiveSessionId(pub Uuid);

/// A person's place in a document's live sessions. Numbered from 1, and never reused within
/// a document.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ParticipantNo(pub u32);

/// A client's own number for one of its changes, from 1 for each client id, in the order the
/// client made them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LocalNo(pub u64);

/// The place a reconnecting client held.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveResume {
    /// The session the client was in.
    pub session: LiveSessionId,
    /// Its participant number in that session.
    pub participant: ParticipantNo,
}

/// The kind of editor build connected to a live session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveBuild {
    /// An editor running in a browser.
    Browser,
    /// A native desktop editor.
    Desktop,
}

/// What a checkpoint ticket asks its holder to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TicketPurpose {
    /// Write and upload the checkpoint bytes and session data.
    Write,
    /// Independently write the same checkpoint and report its hash.
    Verify,
}

/// Why a checkpoint is marked as a major version.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MajorReason {
    /// Preserve the current document before applying a returning client's late changes.
    ReturningClientBefore,
    /// Preserve the result after applying a returning client's late changes.
    ReturningClientAfter,
}

/// Browser (or desktop editor) to server. Unknown fields are refused. The `Debug` form leaves
/// payloads out.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum LiveClientMsg {
    /// Enter the document's live session.
    LiveJoin {
        /// The client's identifier, kept across its reconnects.
        client: ClientId,
        /// The client's [`LIVE_PROTOCOL`].
        protocol: u32,
        /// The format of the changes the client sends and reads.
        format: String,
        /// The place the client held, when reconnecting.
        resume: Option<LiveResume>,
        /// The last log position the client applied, when reconnecting.
        seen: Option<Seq>,
        /// Whether this host can write checkpoint bytes now.
        can_checkpoint: bool,
        /// The kind of editor build running on this host.
        build: LiveBuild,
        /// The last local number in a returning client's saved batch, if any.
        returning_through: Option<LocalNo>,
    },
    /// Propose a change.
    Entry {
        /// The client's number for the change.
        local: LocalNo,
        /// The last log position the client had applied when it made the change.
        seen: Seq,
        /// The change's format.
        format: String,
        /// The change, opaque to the server.
        payload: String,
    },
    /// A sign of life, sent every [`HEARTBEAT_MS`].
    Heartbeat {
        /// The last log position the client applied.
        seen: Seq,
    },
    /// The entry at `seq` could not be applied by this client.
    Report {
        /// The entry's position.
        seq: Seq,
    },
    /// Report the digest of entered content at an exact agreed position.
    Fingerprint {
        /// The agreed position covered by the digest.
        at: Seq,
        /// A lowercase hexadecimal SHA-256 digest of entered content.
        entered: String,
    },
    /// Checkpoint bytes for a ticket are ready to upload.
    CheckpointReady {
        /// The one-use checkpoint ticket.
        ticket: Uuid,
        /// The exact agreed position written.
        at: Seq,
    },
    /// The client could not honour a checkpoint ticket.
    CheckpointFailed {
        /// The ticket that failed.
        ticket: Uuid,
    },
    /// Send the log again from the base.
    Resync,
    /// The person is leaving; their place ends now.
    Leave,
}

impl std::fmt::Debug for LiveClientMsg {
    /// Leaves the payload out.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LiveClientMsg::LiveJoin {
                client,
                protocol,
                format,
                resume,
                seen,
                can_checkpoint,
                build,
                returning_through,
            } => f
                .debug_struct("LiveJoin")
                .field("client", client)
                .field("protocol", protocol)
                .field("format", format)
                .field("resume", resume)
                .field("seen", seen)
                .field("can_checkpoint", can_checkpoint)
                .field("build", build)
                .field("returning_through", returning_through)
                .finish(),
            LiveClientMsg::Entry {
                local,
                seen,
                format,
                payload,
            } => f
                .debug_struct("Entry")
                .field("local", local)
                .field("seen", seen)
                .field("format", format)
                .field("payload", &Redacted(payload.len()))
                .finish(),
            LiveClientMsg::Heartbeat { seen } => {
                f.debug_struct("Heartbeat").field("seen", seen).finish()
            }
            LiveClientMsg::Report { seq } => f.debug_struct("Report").field("seq", seq).finish(),
            LiveClientMsg::Fingerprint { at, entered } => f
                .debug_struct("Fingerprint")
                .field("at", at)
                .field("entered", &Redacted(entered.len()))
                .finish(),
            LiveClientMsg::CheckpointReady { ticket, at } => f
                .debug_struct("CheckpointReady")
                .field("ticket", ticket)
                .field("at", at)
                .finish(),
            LiveClientMsg::CheckpointFailed { ticket } => f
                .debug_struct("CheckpointFailed")
                .field("ticket", ticket)
                .finish(),
            LiveClientMsg::Resync => f.write_str("Resync"),
            LiveClientMsg::Leave => f.write_str("Leave"),
        }
    }
}

/// How a join was placed. Written as an object tagged by `is`: `new`, `kept` or `returning`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "is", rename_all = "snake_case")]
pub enum Place {
    /// A new place in this session.
    New,
    /// The place named in `resume`, still held.
    Kept,
    /// The place named in `resume` had ended; the client has a new one.
    Returning {
        /// The participant number the client held before.
        previous: ParticipantNo,
    },
}

/// Server to browser (or desktop editor). The `Debug` form leaves payloads out, through
/// [`EntryBody`]'s.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LiveServerMsg {
    /// Join accepted.
    LiveJoined {
        /// The session joined.
        session: LiveSessionId,
        /// This client's place.
        participant: ParticipantNo,
        /// How the join was placed.
        place: Place,
        /// Whether this connection's person may make changes.
        can_edit: bool,
        /// How often to send a heartbeat, in milliseconds.
        heartbeat_ms: u64,
        /// Who holds a place.
        people: Vec<LivePerson>,
    },
    /// The entries `from + 1 ..= head` follow as `Entry` messages, applied on top of the
    /// stored revision at `base`.
    Catchup {
        /// The log position the stored revision covers.
        base: Seq,
        /// The stored revision, if any.
        revision: Option<String>,
        /// The last position before the entries that follow.
        from: Seq,
        /// The last position in the log.
        head: Seq,
    },
    /// An entry in the log.
    Entry {
        /// The entry.
        entry: LiveEntry,
    },
    /// A change was refused; it is not in the log.
    Refused {
        /// The client's number for the change.
        local: LocalNo,
        /// Why.
        reason: RefusedReason,
    },
    /// The document cannot be opened from the log: the entry at `seq` could not be applied.
    CannotOpen {
        /// The entry's position.
        seq: Seq,
    },
    /// Who holds a place changed.
    People {
        /// Who holds a place.
        people: Vec<LivePerson>,
    },
    /// This person's rights on the document changed while connected.
    Access {
        /// Whether they may now make changes.
        can_edit: bool,
    },
    /// Ask one capable editor to write or verify a checkpoint at an exact position.
    CheckpointTicket {
        /// The one-use checkpoint ticket.
        ticket: Uuid,
        /// Whether to write the checkpoint or independently verify it.
        purpose: TicketPurpose,
        /// The exact agreed position to write.
        at: Seq,
        /// The stored revision on which the checkpoint must be based.
        base: Option<String>,
        /// Why this checkpoint is a major version, when it is one.
        major: Option<MajorReason>,
    },
    /// New changes are paused because the unsaved log reached a hard limit.
    ChangesPaused {
        /// Retained entries in the unsaved log.
        entries: u64,
        /// Retained payload bytes in the unsaved log.
        bytes: u64,
    },
    /// New changes may be sent again.
    ChangesResumed,
    /// The checkpoint before a returning client's saved batch is durable.
    ReturnReady {
        /// The last local number in the saved batch that may now be sent.
        through: LocalNo,
    },
    /// The returning client's identities have expired; archive its shown copy instead.
    ArchiveReturn {
        /// The current stored revision under which the old shown copy is archived.
        current: Option<String>,
    },
    /// The last frame: the client's protocol or format does not match the session's.
    Incompatible {
        /// The server's protocol.
        protocol: u32,
        /// The session's format.
        format: String,
    },
    /// The last frame: the place ended after missed heartbeats. Join again.
    Expired,
    /// The last frame before the server closes the connection; do not reconnect.
    Removed {
        /// Why.
        reason: RemovedReason,
    },
}

/// One position in the log, stamped by the server.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveEntry {
    /// The entry's position.
    pub seq: Seq,
    /// When the server ordered it, in milliseconds since the Unix epoch.
    pub at_ms: i64,
    /// A number chosen by the server, the same for every editor that applies the entry.
    pub seed: u32,
    /// What the entry is.
    #[serde(flatten)]
    pub body: EntryBody,
}

/// What a log entry is. Written with a `kind` field: `change` or `cancel`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EntryBody {
    /// A person's change.
    Change {
        /// The sender's place.
        participant: ParticipantNo,
        /// The sender's client.
        client: ClientId,
        /// The sender's number for the change.
        local: LocalNo,
        /// The last log position the sender had applied when it made the change.
        seen: Seq,
        /// The change, opaque to the server; empty once cancelled.
        payload: String,
        /// Whether a later `Cancel` entry cancelled it.
        cancelled: bool,
    },
    /// Cancels the change at `cancels`, which editors reported they could not apply.
    Cancel {
        /// The cancelled change's position.
        cancels: Seq,
        /// How many places reported it.
        reports: u32,
    },
}

impl std::fmt::Debug for EntryBody {
    /// Leaves the payload out.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EntryBody::Change {
                participant,
                client,
                local,
                seen,
                payload,
                cancelled,
            } => f
                .debug_struct("Change")
                .field("participant", participant)
                .field("client", client)
                .field("local", local)
                .field("seen", seen)
                .field("payload", &Redacted(payload.len()))
                .field("cancelled", cancelled)
                .finish(),
            EntryBody::Cancel { cancels, reports } => f
                .debug_struct("Cancel")
                .field("cancels", cancels)
                .field("reports", reports)
                .finish(),
        }
    }
}

/// Shows a payload's length in place of its content.
struct Redacted(usize);

impl std::fmt::Debug for Redacted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<{} bytes>", self.0)
    }
}

/// Someone holding a place in the session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LivePerson {
    /// Their place.
    pub participant: ParticipantNo,
    /// Their client.
    pub client: ClientId,
    /// Their account.
    pub user: UserId,
    /// The account's name.
    pub name: String,
    /// Whether they may make changes.
    pub can_edit: bool,
    /// Whether their client is connected now; a place outlives a dropped connection until it
    /// expires.
    pub connected: bool,
}

/// Why a change was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusedReason {
    /// The person may only view the document.
    ReadOnly,
    /// The change's format is not the session's.
    WrongFormat,
    /// The change's content is over [`MAX_PAYLOAD_BYTES`].
    TooLarge,
    /// The client sent changes too fast.
    RateLimited,
    /// The change's `seen` is ahead of the log.
    BaseAhead,
    /// The change's `local` number is not the next one expected from this client.
    OutOfOrder,
    /// A reason this build does not know, sent by a newer server. Never sent; a client reads it
    /// as a refusal all the same.
    #[serde(other)]
    Unknown,
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

    fn tag_of(json: &str, field: &str) -> String {
        let value: serde_json::Value = serde_json::from_str(json).unwrap();
        value[field].as_str().unwrap().to_owned()
    }

    fn change(payload: &str) -> LiveEntry {
        LiveEntry {
            seq: Seq(5),
            at_ms: 1_763_290_000_000,
            seed: 42,
            body: EntryBody::Change {
                participant: ParticipantNo(2),
                client: ClientId(Uuid::from_u128(1)),
                local: LocalNo(7),
                seen: Seq(4),
                payload: payload.to_owned(),
                cancelled: false,
            },
        }
    }

    fn person() -> LivePerson {
        LivePerson {
            participant: ParticipantNo(1),
            client: ClientId(Uuid::from_u128(1)),
            user: UserId(Uuid::from_u128(2)),
            name: "Ann".into(),
            can_edit: true,
            connected: true,
        }
    }

    /// The `type` string of every message, which the page and the server both rely on. Each
    /// `match` is exhaustive, so adding a variant fails to compile until it is listed here.
    #[test]
    fn live_message_tags_are_pinned() {
        fn client_tag(msg: &LiveClientMsg) -> &'static str {
            match msg {
                LiveClientMsg::LiveJoin { .. } => "live_join",
                LiveClientMsg::Entry { .. } => "entry",
                LiveClientMsg::Heartbeat { .. } => "heartbeat",
                LiveClientMsg::Report { .. } => "report",
                LiveClientMsg::Fingerprint { .. } => "fingerprint",
                LiveClientMsg::CheckpointReady { .. } => "checkpoint_ready",
                LiveClientMsg::CheckpointFailed { .. } => "checkpoint_failed",
                LiveClientMsg::Resync => "resync",
                LiveClientMsg::Leave => "leave",
            }
        }
        fn server_tag(msg: &LiveServerMsg) -> &'static str {
            match msg {
                LiveServerMsg::LiveJoined { .. } => "live_joined",
                LiveServerMsg::Catchup { .. } => "catchup",
                LiveServerMsg::Entry { .. } => "entry",
                LiveServerMsg::Refused { .. } => "refused",
                LiveServerMsg::CannotOpen { .. } => "cannot_open",
                LiveServerMsg::People { .. } => "people",
                LiveServerMsg::Access { .. } => "access",
                LiveServerMsg::CheckpointTicket { .. } => "checkpoint_ticket",
                LiveServerMsg::ChangesPaused { .. } => "changes_paused",
                LiveServerMsg::ChangesResumed => "changes_resumed",
                LiveServerMsg::ReturnReady { .. } => "return_ready",
                LiveServerMsg::ArchiveReturn { .. } => "archive_return",
                LiveServerMsg::Incompatible { .. } => "incompatible",
                LiveServerMsg::Expired => "expired",
                LiveServerMsg::Removed { .. } => "removed",
            }
        }
        let client = vec![
            LiveClientMsg::LiveJoin {
                client: ClientId(Uuid::nil()),
                protocol: LIVE_PROTOCOL,
                format: "gridcraft/1".into(),
                resume: Some(LiveResume {
                    session: LiveSessionId(Uuid::from_u128(3)),
                    participant: ParticipantNo(2),
                }),
                seen: Some(Seq(9)),
                can_checkpoint: true,
                build: LiveBuild::Desktop,
                returning_through: Some(LocalNo(8)),
            },
            LiveClientMsg::Entry {
                local: LocalNo(1),
                seen: Seq(0),
                format: "gridcraft/1".into(),
                payload: "{}".into(),
            },
            LiveClientMsg::Heartbeat { seen: Seq(3) },
            LiveClientMsg::Report { seq: Seq(4) },
            LiveClientMsg::Fingerprint {
                at: Seq(4),
                entered: "0123456789abcdef".repeat(4),
            },
            LiveClientMsg::CheckpointReady {
                ticket: Uuid::from_u128(4),
                at: Seq(4),
            },
            LiveClientMsg::CheckpointFailed {
                ticket: Uuid::from_u128(4),
            },
            LiveClientMsg::Resync,
            LiveClientMsg::Leave,
        ];
        for msg in &client {
            let json = serde_json::to_string(msg).unwrap();
            assert_eq!(tag_of(&json, "type"), client_tag(msg));
            assert_eq!(serde_json::from_str::<LiveClientMsg>(&json).unwrap(), *msg);
        }
        let server = vec![
            LiveServerMsg::LiveJoined {
                session: LiveSessionId(Uuid::from_u128(3)),
                participant: ParticipantNo(1),
                place: Place::Returning {
                    previous: ParticipantNo(1),
                },
                can_edit: true,
                heartbeat_ms: HEARTBEAT_MS,
                people: vec![person()],
            },
            LiveServerMsg::Catchup {
                base: Seq(0),
                revision: None,
                from: Seq(0),
                head: Seq(2),
            },
            LiveServerMsg::Entry {
                entry: change("{}"),
            },
            LiveServerMsg::Refused {
                local: LocalNo(3),
                reason: RefusedReason::TooLarge,
            },
            LiveServerMsg::CannotOpen { seq: Seq(4) },
            LiveServerMsg::People {
                people: vec![person()],
            },
            LiveServerMsg::Access { can_edit: false },
            LiveServerMsg::CheckpointTicket {
                ticket: Uuid::from_u128(4),
                purpose: TicketPurpose::Write,
                at: Seq(4),
                base: Some("abc123".into()),
                major: Some(MajorReason::ReturningClientBefore),
            },
            LiveServerMsg::ChangesPaused {
                entries: 5_000,
                bytes: 64 * 1024 * 1024,
            },
            LiveServerMsg::ChangesResumed,
            LiveServerMsg::ReturnReady {
                through: LocalNo(8),
            },
            LiveServerMsg::ArchiveReturn {
                current: Some("abc123".into()),
            },
            LiveServerMsg::Incompatible {
                protocol: LIVE_PROTOCOL,
                format: "gridcraft/1".into(),
            },
            LiveServerMsg::Expired,
            LiveServerMsg::Removed {
                reason: RemovedReason::Deleted,
            },
        ];
        for msg in &server {
            let json = serde_json::to_string(msg).unwrap();
            assert_eq!(tag_of(&json, "type"), server_tag(msg));
            assert_eq!(serde_json::from_str::<LiveServerMsg>(&json).unwrap(), *msg);
        }
        // The strings are written out in the matches above; this checks they are the strings
        // on the wire, and that no sample was dropped from the lists.
        assert_eq!(client.len(), 9);
        assert_eq!(server.len(), 15);
    }

    #[test]
    fn checkpoint_wire_forms_are_pinned() {
        let join = LiveClientMsg::LiveJoin {
            client: ClientId(Uuid::from_u128(1)),
            protocol: LIVE_PROTOCOL,
            format: "gridcraft/1".into(),
            resume: None,
            seen: Some(Seq(12)),
            can_checkpoint: true,
            build: LiveBuild::Desktop,
            returning_through: None,
        };
        assert_eq!(
            serde_json::to_string(&join).unwrap(),
            "{\"type\":\"live_join\",\"client\":\"00000000-0000-0000-0000-000000000001\",\
             \"protocol\":1,\"format\":\"gridcraft/1\",\"resume\":null,\"seen\":12,\
             \"can_checkpoint\":true,\"build\":\"desktop\",\"returning_through\":null}"
        );

        let ticket = Uuid::from_u128(2);
        let ready = LiveClientMsg::CheckpointReady {
            ticket,
            at: Seq(12),
        };
        assert_eq!(
            serde_json::to_string(&ready).unwrap(),
            "{\"type\":\"checkpoint_ready\",\"ticket\":\"00000000-0000-0000-0000-000000000002\",\"at\":12}"
        );
        assert_eq!(
            serde_json::to_string(&LiveClientMsg::CheckpointFailed { ticket }).unwrap(),
            "{\"type\":\"checkpoint_failed\",\"ticket\":\"00000000-0000-0000-0000-000000000002\"}"
        );

        let request = LiveServerMsg::CheckpointTicket {
            ticket,
            purpose: TicketPurpose::Verify,
            at: Seq(12),
            base: Some("0123456789abcdef".into()),
            major: Some(MajorReason::ReturningClientAfter),
        };
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(
            json,
            "{\"type\":\"checkpoint_ticket\",\"ticket\":\"00000000-0000-0000-0000-000000000002\",\
             \"purpose\":\"verify\",\"at\":12,\"base\":\"0123456789abcdef\",\
             \"major\":\"returning_client_after\"}"
        );
        assert_eq!(
            serde_json::from_str::<LiveServerMsg>(&json).unwrap(),
            request
        );

        assert_eq!(
            serde_json::to_string(&LiveServerMsg::ChangesPaused {
                entries: 5_000,
                bytes: 67_108_864,
            })
            .unwrap(),
            "{\"type\":\"changes_paused\",\"entries\":5000,\"bytes\":67108864}"
        );
        assert_eq!(
            serde_json::to_string(&LiveServerMsg::ChangesResumed).unwrap(),
            "{\"type\":\"changes_resumed\"}"
        );
    }

    #[test]
    fn fingerprint_wire_form_is_entered_only() {
        let entered = "0123456789abcdef".repeat(4);
        let message = LiveClientMsg::Fingerprint {
            at: Seq(64),
            entered: entered.clone(),
        };
        let json = serde_json::to_string(&message).unwrap();
        assert_eq!(
            json,
            format!("{{\"type\":\"fingerprint\",\"at\":64,\"entered\":\"{entered}\"}}")
        );
        assert_eq!(
            serde_json::from_str::<LiveClientMsg>(&json).unwrap(),
            message
        );
        assert!(
            serde_json::from_str::<LiveClientMsg>(&format!(
                "{{\"type\":\"fingerprint\",\"at\":64,\"entered\":\"{entered}\",\"computed\":\"{entered}\"}}"
            ))
            .is_err()
        );
    }

    #[test]
    fn return_barrier_wire_forms_are_pinned() {
        let returning = LiveClientMsg::LiveJoin {
            client: ClientId(Uuid::from_u128(1)),
            protocol: LIVE_PROTOCOL,
            format: "gridcraft/1".into(),
            resume: Some(LiveResume {
                session: LiveSessionId(Uuid::from_u128(2)),
                participant: ParticipantNo(3),
            }),
            seen: Some(Seq(12)),
            can_checkpoint: false,
            build: LiveBuild::Browser,
            returning_through: Some(LocalNo(17)),
        };
        assert_eq!(
            serde_json::to_string(&returning).unwrap(),
            "{\"type\":\"live_join\",\"client\":\"00000000-0000-0000-0000-000000000001\",\
             \"protocol\":1,\"format\":\"gridcraft/1\",\"resume\":{\"session\":\
             \"00000000-0000-0000-0000-000000000002\",\"participant\":3},\"seen\":12,\
             \"can_checkpoint\":false,\"build\":\"browser\",\"returning_through\":17}"
        );

        let ready = LiveServerMsg::ReturnReady {
            through: LocalNo(17),
        };
        let json = serde_json::to_string(&ready).unwrap();
        assert_eq!(json, "{\"type\":\"return_ready\",\"through\":17}");
        assert_eq!(serde_json::from_str::<LiveServerMsg>(&json).unwrap(), ready);

        let archive = LiveServerMsg::ArchiveReturn {
            current: Some("fedcba9876543210".into()),
        };
        let json = serde_json::to_string(&archive).unwrap();
        assert_eq!(
            json,
            "{\"type\":\"archive_return\",\"current\":\"fedcba9876543210\"}"
        );
        assert_eq!(
            serde_json::from_str::<LiveServerMsg>(&json).unwrap(),
            archive
        );
    }

    #[test]
    fn entry_wire_form_is_pinned() {
        let change = change("{\"op\":\"x\"}");
        let json = serde_json::to_string(&change).unwrap();
        assert_eq!(
            json,
            "{\"seq\":5,\"at_ms\":1763290000000,\"seed\":42,\"kind\":\"change\",\
             \"participant\":2,\"client\":\"00000000-0000-0000-0000-000000000001\",\
             \"local\":7,\"seen\":4,\"payload\":\"{\\\"op\\\":\\\"x\\\"}\",\"cancelled\":false}"
        );
        assert_eq!(serde_json::from_str::<LiveEntry>(&json).unwrap(), change);

        let cancel = LiveEntry {
            seq: Seq(6),
            at_ms: 1,
            seed: 0,
            body: EntryBody::Cancel {
                cancels: Seq(5),
                reports: 2,
            },
        };
        let json = serde_json::to_string(&cancel).unwrap();
        assert_eq!(
            json,
            "{\"seq\":6,\"at_ms\":1,\"seed\":0,\"kind\":\"cancel\",\"cancels\":5,\"reports\":2}"
        );
        assert_eq!(serde_json::from_str::<LiveEntry>(&json).unwrap(), cancel);
    }

    #[test]
    fn place_wire_form_is_pinned() {
        assert_eq!(
            serde_json::to_string(&Place::New).unwrap(),
            "{\"is\":\"new\"}"
        );
        assert_eq!(
            serde_json::to_string(&Place::Kept).unwrap(),
            "{\"is\":\"kept\"}"
        );
        let returning = Place::Returning {
            previous: ParticipantNo(3),
        };
        let json = serde_json::to_string(&returning).unwrap();
        assert_eq!(json, "{\"is\":\"returning\",\"previous\":3}");
        assert_eq!(serde_json::from_str::<Place>(&json).unwrap(), returning);
    }

    /// Unknown fields are refused, so a text or file client's `join` is never taken for a
    /// live client's.
    #[test]
    fn client_messages_refuse_unknown_fields() {
        let good = "{\"type\":\"live_join\",\"client\":\"00000000-0000-0000-0000-000000000000\",\
                    \"protocol\":1,\"format\":\"gridcraft/1\",\"resume\":null,\"seen\":null,\
                    \"can_checkpoint\":false,\"build\":\"browser\",\"returning_through\":null}";
        assert!(serde_json::from_str::<LiveClientMsg>(good).is_ok());
        let extra = "{\"type\":\"live_join\",\"client\":\"00000000-0000-0000-0000-000000000000\",\
                     \"protocol\":1,\"format\":\"gridcraft/1\",\"resume\":null,\"seen\":null,\
                     \"can_checkpoint\":false,\"build\":\"browser\",\"returning_through\":null,\
                     \"extra\":true}";
        assert!(serde_json::from_str::<LiveClientMsg>(extra).is_err());
        let text = "{\"type\":\"join\",\"client\":\"00000000-0000-0000-0000-000000000000\",\
                    \"resume\":null}";
        assert!(serde_json::from_str::<LiveClientMsg>(text).is_err());
        let file = "{\"type\":\"join\",\"client\":\"00000000-0000-0000-0000-000000000000\"}";
        assert!(serde_json::from_str::<LiveClientMsg>(file).is_err());
        let entry = "{\"type\":\"entry\",\"local\":1,\"seen\":0,\"format\":\"gridcraft/1\",\
                     \"payload\":\"{}\",\"op_id\":\"x\"}";
        assert!(serde_json::from_str::<LiveClientMsg>(entry).is_err());
    }

    #[test]
    fn unknown_refused_reason_reads_as_unknown() {
        assert_eq!(
            serde_json::from_str::<RefusedReason>("\"made_up\"").unwrap(),
            RefusedReason::Unknown
        );
        assert_eq!(
            serde_json::from_str::<RefusedReason>("\"base_ahead\"").unwrap(),
            RefusedReason::BaseAhead
        );
        let msg: LiveServerMsg =
            serde_json::from_str("{\"type\":\"refused\",\"local\":4,\"reason\":\"made_up\"}")
                .unwrap();
        assert_eq!(
            msg,
            LiveServerMsg::Refused {
                local: LocalNo(4),
                reason: RefusedReason::Unknown
            }
        );
    }

    #[test]
    fn format_names_are_checked() {
        assert!(valid_format("gridcraft/1"));
        assert!(valid_format("a.b_c-d/0"));
        assert!(valid_format(&"a".repeat(MAX_FORMAT_LEN)));
        assert!(!valid_format(""));
        assert!(!valid_format(&"a".repeat(MAX_FORMAT_LEN + 1)));
        assert!(!valid_format("GridCraft/1"));
        assert!(!valid_format("grid craft"));
        assert!(!valid_format("grid\u{0}"));
        assert!(!valid_format("gridcraft/é"));
    }

    #[test]
    fn debug_leaves_payload_out() {
        let secret = "secret-cell-content-12345";
        let shown = format!(
            "{:?}",
            LiveServerMsg::Entry {
                entry: change(secret)
            }
        );
        assert!(!shown.contains(secret), "{shown}");
        assert!(shown.contains("Change"), "{shown}");
        let shown = format!(
            "{:?}",
            LiveClientMsg::Entry {
                local: LocalNo(1),
                seen: Seq(0),
                format: "gridcraft/1".into(),
                payload: secret.into(),
            }
        );
        assert!(!shown.contains(secret), "{shown}");
    }
}
