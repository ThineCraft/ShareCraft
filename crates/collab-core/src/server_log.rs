//! The ordered change log a server keeps for one document.

use crate::document::{Document, Entry, SubmitError, Submitted};
use crate::ids::{ClientId, OpId, Seq};

/// The committed changes for one document, plus the document they produce.
///
/// Every incoming change is adjusted against the changes committed since the state it was
/// based on, so all clients converge on the same document.
#[derive(Clone, Debug)]
pub struct ServerLog<D: Document> {
    doc: D,
    /// Entries with `seq` in `(floor, head]`, in order.
    entries: Vec<Entry<D::Op>>,
    /// Sequence number of the newest forgotten entry (or the starting head); the oldest base
    /// that can still be adjusted from.
    floor: Seq,
    head: Seq,
}

impl<D: Document> ServerLog<D> {
    /// Starts an empty log whose head is `head`, holding `doc` as of that position.
    #[must_use]
    pub fn new(doc: D, head: Seq) -> Self {
        Self {
            doc,
            entries: Vec::new(),
            floor: head,
            head,
        }
    }

    /// The position of the newest committed change.
    #[must_use]
    pub fn head(&self) -> Seq {
        self.head
    }

    /// The document with every committed change applied.
    #[must_use]
    pub fn doc(&self) -> &D {
        &self.doc
    }

    /// Adjusts `op` (made against the state at `base`) to follow everything committed since,
    /// applies it and appends it.
    ///
    /// A repeat of a change still in the log returns `Duplicate` and changes nothing. On any
    /// error the log and document are left exactly as they were.
    ///
    /// # Errors
    /// `BaseAhead` or `BaseTooOld` if `base` is outside the kept history, `Invalid` if the
    /// change cannot be adjusted or applied.
    pub fn submit(
        &mut self,
        client: ClientId,
        op_id: OpId,
        base: Seq,
        op: D::Op,
    ) -> Result<Submitted<D::Op>, SubmitError> {
        if let Some(existing) = self
            .entries
            .iter()
            .find(|e| e.client == client && e.op_id == op_id)
        {
            return Ok(Submitted::Duplicate(existing.seq));
        }
        if base > self.head {
            return Err(SubmitError::BaseAhead {
                base,
                head: self.head,
            });
        }
        let Some(concurrent) = self.since(base) else {
            return Err(SubmitError::BaseTooOld {
                base,
                oldest: self.floor,
            });
        };

        let mut adjusted = op;
        for entry in concurrent {
            let (_, second) = D::transform(&entry.op, &adjusted)
                .map_err(|e| SubmitError::Invalid(e.to_string()))?;
            adjusted = second;
        }
        let mut doc = self.doc.clone();
        doc.apply(&adjusted)
            .map_err(|e| SubmitError::Invalid(e.to_string()))?;

        let entry = Entry {
            seq: self.head.next(),
            client,
            op_id,
            op: adjusted,
        };
        self.doc = doc;
        self.head = entry.seq;
        self.entries.push(entry.clone());
        Ok(Submitted::Committed(entry))
    }

    /// The entries committed after `seq`. `None` if `seq` is older than the oldest kept
    /// history or ahead of the head.
    #[must_use]
    pub fn since(&self, seq: Seq) -> Option<&[Entry<D::Op>]> {
        if seq < self.floor || seq > self.head {
            return None;
        }
        let skip = usize::try_from(seq.0 - self.floor.0).ok()?;
        self.entries.get(skip..)
    }

    /// Forgets entries with `seq` at or below the given one. The head is unchanged.
    pub fn truncate_through(&mut self, seq: Seq) {
        let through = seq.min(self.head);
        if through <= self.floor {
            return;
        }
        let drop = usize::try_from(through.0 - self.floor.0).unwrap_or(self.entries.len());
        self.entries.drain(..drop.min(self.entries.len()));
        self.floor = through;
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
    use crate::document::{Document, SubmitError, Submitted};
    use crate::ids::{ClientId, OpId, Seq};
    use serde::{Deserialize, Serialize};
    use uuid::Uuid;

    #[derive(Clone, Debug, PartialEq)]
    struct Counter(Vec<u32>);

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    enum CounterOp {
        Push(u32),
    }

    #[derive(Debug, PartialEq, thiserror::Error)]
    #[error("zero is not allowed")]
    struct ZeroPush;

    impl Document for Counter {
        type Op = CounterOp;
        type Error = ZeroPush;

        fn apply(&mut self, op: &CounterOp) -> Result<(), ZeroPush> {
            let CounterOp::Push(n) = op;
            if *n == 0 {
                return Err(ZeroPush);
            }
            self.0.push(*n);
            Ok(())
        }

        fn transform(a: &CounterOp, b: &CounterOp) -> Result<(CounterOp, CounterOp), ZeroPush> {
            Ok((a.clone(), b.clone()))
        }
    }

    fn client(n: u128) -> ClientId {
        ClientId(Uuid::from_u128(n))
    }
    fn op_id(n: u128) -> OpId {
        OpId(Uuid::from_u128(n))
    }
    fn log() -> ServerLog<Counter> {
        ServerLog::new(Counter(vec![]), Seq(0))
    }
    fn push(log: &mut ServerLog<Counter>, n: u32) -> Submitted<CounterOp> {
        let base = log.head();
        log.submit(client(1), op_id(u128::from(n)), base, CounterOp::Push(n))
            .unwrap()
    }

    #[test]
    fn commits_in_arrival_order() {
        let mut log = log();
        let a = log
            .submit(client(1), op_id(1), Seq(0), CounterOp::Push(10))
            .unwrap();
        let b = log
            .submit(client(2), op_id(2), Seq(0), CounterOp::Push(20))
            .unwrap();
        assert!(matches!(a, Submitted::Committed(ref e) if e.seq == Seq(1)));
        assert!(matches!(b, Submitted::Committed(ref e) if e.seq == Seq(2)));
        assert_eq!(log.doc(), &Counter(vec![10, 20]));
        assert_eq!(log.head(), Seq(2));
    }

    #[test]
    fn duplicate_op_id_is_not_applied_twice() {
        let mut log = log();
        let first = log
            .submit(client(1), op_id(1), Seq(0), CounterOp::Push(10))
            .unwrap();
        assert!(matches!(first, Submitted::Committed(_)));
        let again = log
            .submit(client(1), op_id(1), Seq(0), CounterOp::Push(10))
            .unwrap();
        assert_eq!(again, Submitted::Duplicate(Seq(1)));
        assert_eq!(log.doc(), &Counter(vec![10]));
        assert_eq!(log.head(), Seq(1));
    }

    #[test]
    fn rejects_base_ahead_of_log() {
        let mut log = log();
        let err = log
            .submit(client(1), op_id(1), Seq(5), CounterOp::Push(1))
            .unwrap_err();
        assert_eq!(
            err,
            SubmitError::BaseAhead {
                base: Seq(5),
                head: Seq(0)
            }
        );
    }

    #[test]
    fn rejects_base_older_than_kept_history() {
        let mut log = log();
        for n in 1..=3 {
            push(&mut log, n);
        }
        log.truncate_through(Seq(2));
        let err = log
            .submit(client(2), op_id(100), Seq(1), CounterOp::Push(9))
            .unwrap_err();
        assert_eq!(
            err,
            SubmitError::BaseTooOld {
                base: Seq(1),
                oldest: Seq(2)
            }
        );
        assert!(
            log.submit(client(2), op_id(101), Seq(2), CounterOp::Push(9))
                .is_ok()
        );
    }

    #[test]
    fn invalid_op_leaves_state_untouched() {
        let mut log = log();
        push(&mut log, 5);
        let err = log
            .submit(client(2), op_id(100), Seq(1), CounterOp::Push(0))
            .unwrap_err();
        assert!(matches!(err, SubmitError::Invalid(_)));
        assert_eq!(log.head(), Seq(1));
        assert_eq!(log.doc(), &Counter(vec![5]));
        assert_eq!(log.since(Seq(0)).unwrap().len(), 1);
    }

    #[test]
    fn since_returns_entries_after_seq() {
        let mut log = log();
        for n in 1..=3 {
            push(&mut log, n);
        }
        assert_eq!(log.since(Seq(1)).unwrap().len(), 2);
        assert_eq!(log.since(Seq(3)).unwrap().len(), 0);
        assert!(log.since(Seq(9)).is_none());
    }

    #[test]
    fn since_is_none_for_forgotten_history() {
        let mut log = log();
        for n in 1..=3 {
            push(&mut log, n);
        }
        log.truncate_through(Seq(2));
        assert!(log.since(Seq(1)).is_none());
        assert_eq!(log.since(Seq(2)).unwrap().len(), 1);
    }
}
