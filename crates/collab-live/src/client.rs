//! The hooks an editor gives the library so it can show, go back and re-run changes.
//!
//! The library never looks inside a change: it hands each one to the editor as an
//! [`EntryView`] and the editor says what happened. Applying an entry may read only the entry,
//! its server stamp and its sender's participant number, never anything that belongs to one
//! person, so every device that applies the same entries in the same order ends the same.

use collab_core::ids::Seq;
use protocol::live::ParticipantNo;

/// What an editor does for the library.
pub trait LiveEditor {
    /// A copy of what is shown, to go back to.
    type Snapshot;

    /// Takes a copy of what is shown.
    fn snapshot(&self) -> Self::Snapshot;

    /// Goes back to a copy taken earlier.
    fn restore(&mut self, snapshot: &Self::Snapshot);

    /// Applies one change. `Err(Crash)` means the editor stopped in this entry.
    fn apply(&mut self, entry: &EntryView<'_>) -> Result<Outcome, Crash>;

    /// Keeps the crash note: the entry about to be applied (`Some`), or none once it is
    /// applied (`None`). The host stores it where it survives the editor stopping; the default
    /// keeps nothing.
    fn note_applying(&mut self, seq: Option<Seq>) {
        let _ = seq;
    }
}

/// The server's stamp on an entry: when it was put in order, and a seed for anything the
/// change needs to choose alike on every device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    pub at_ms: i64,
    pub seed: u32,
}

/// One change as the editor sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EntryView<'a> {
    /// Its position in the log; `None` while it is still waiting for the server.
    pub seq: Option<Seq>,
    /// Who sent it.
    pub participant: ParticipantNo,
    /// Whether this device sent it.
    pub own: bool,
    /// The server's stamp; `None` while it is still waiting for the server.
    pub stamp: Option<Stamp>,
    /// The change's content, exactly as sent.
    pub payload: &'a str,
}

/// What applying a change did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub effect: Effect,
    /// Whether the result depended on the stamp, so a change shown before the server stamped
    /// it must be applied again with the real stamp.
    pub read_stamp: bool,
}

/// Whether a change took effect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    Applied,
    /// The change did nothing, on every device alike; `reason` says why.
    NoEffect {
        reason: String,
    },
}

/// The editor stopped in this entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Crash;
