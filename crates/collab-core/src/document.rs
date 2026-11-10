//! The contract a document type fulfils so the server log can merge changes to it.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::ids::{ClientId, OpId, Seq};

/// A document that changes can be applied to and reconciled against each other.
pub trait Document: Clone {
    /// One change to the document.
    type Op: Clone + PartialEq + std::fmt::Debug + Serialize + DeserializeOwned;
    /// Why a change could not be applied or transformed.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Applies `op` to the document.
    ///
    /// # Errors
    /// Returns an error if the change does not fit the document.
    fn apply(&mut self, op: &Self::Op) -> Result<(), Self::Error>;

    /// `first` and `second` were both made against the same state. Returns `(first2, second2)`
    /// such that applying `first` then `second2` gives the same state as applying `second`
    /// then `first2`. Where the two compete for the same place, `first` takes it.
    ///
    /// # Errors
    /// Returns an error if the two changes cannot be reconciled.
    fn transform(first: &Self::Op, second: &Self::Op) -> Result<(Self::Op, Self::Op), Self::Error>;
}

/// One committed change in a document's log.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry<Op> {
    /// Position of this change in the log.
    pub seq: Seq,
    /// The connection that submitted it.
    pub client: ClientId,
    /// The client's identifier for the change.
    pub op_id: OpId,
    /// The change, already adjusted to follow every earlier entry.
    pub op: Op,
}

/// The outcome of a successful submit.
#[derive(Clone, Debug, PartialEq)]
pub enum Submitted<Op> {
    /// The change was adjusted, applied and appended.
    Committed(Entry<Op>),
    /// The same client and change id was already committed at this position; nothing changed.
    Duplicate(Seq),
}

/// Why a submit was refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SubmitError {
    /// The change claims to be based on a state the server has not reached.
    #[error("base {base:?} is ahead of head {head:?}")]
    BaseAhead {
        /// The base the client named.
        base: Seq,
        /// The server's current head.
        head: Seq,
    },
    /// The history needed to adjust the change has been forgotten.
    #[error("base {base:?} is older than the oldest kept history {oldest:?}")]
    BaseTooOld {
        /// The base the client named.
        base: Seq,
        /// The oldest base the server can still accept.
        oldest: Seq,
    },
    /// The change could not be adjusted or applied.
    #[error("invalid change: {0}")]
    Invalid(String),
}
