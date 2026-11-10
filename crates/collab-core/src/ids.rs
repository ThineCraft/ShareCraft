//! Identifiers shared by the collaboration spine.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Identifies one document.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DocId(pub Uuid);

/// Identifies one connection lifetime; chosen by the client.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ClientId(pub Uuid);

/// Identifies one change; chosen by the client per change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OpId(pub Uuid);

/// Position in a document's log. `Seq(0)` means nothing is committed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Seq(pub u64);

impl Seq {
    /// The next position in the log (saturates at `u64::MAX`).
    #[must_use]
    pub fn next(self) -> Seq {
        Seq(self.0.saturating_add(1))
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
    use uuid::Uuid;

    #[test]
    fn seq_next_increments() {
        assert_eq!(Seq(4).next(), Seq(5));
    }

    #[test]
    fn ids_serialize_as_plain_strings() {
        assert_eq!(
            serde_json::to_string(&DocId(Uuid::nil())).unwrap(),
            "\"00000000-0000-0000-0000-000000000000\""
        );
        assert_eq!(serde_json::to_string(&Seq(7)).unwrap(), "7");
    }
}
