//! The client half of the protocol: one change in flight, a queue of waiting changes, and the
//! adjustment of both when other people's changes are committed first.

use std::collections::VecDeque;

use crate::document::{Document, Entry};
use crate::ids::{ClientId, OpId, Seq};

/// A change to send to the server.
#[derive(Clone, Debug, PartialEq)]
pub struct Outgoing<Op> {
    /// The client's identifier for the change.
    pub op_id: OpId,
    /// The log position the change was made against.
    pub base: Seq,
    /// The change.
    pub op: Op,
}

/// What handling a committed entry asks of the caller.
#[derive(Clone, Debug, PartialEq)]
pub struct Applied<Op> {
    /// The change as it must be applied to the caller's own view; `None` when the entry was
    /// this client's own change being confirmed.
    pub remote: Option<Op>,
    /// The next waiting change, now in flight and ready to send.
    pub send_next: Option<Outgoing<Op>>,
}

/// Why the client refused a change or entry.
#[derive(Debug, thiserror::Error)]
pub enum ClientError<E: std::error::Error> {
    /// The document rejected a change or could not adjust it.
    #[error("document error: {0}")]
    Document(E),
    /// A committed entry arrived that is not the next one in the log.
    #[error("expected entry {expected:?}, got {got:?}")]
    OutOfOrder {
        /// The position the client needed next.
        expected: Seq,
        /// The position of the entry it was given.
        got: Seq,
    },
}

/// One client's view of a shared document.
#[derive(Clone, Debug)]
pub struct ClientState<D: Document> {
    client: ClientId,
    /// The document with every unconfirmed local change applied.
    doc: D,
    /// Last log position applied.
    seq: Seq,
    in_flight: Option<Outgoing<D::Op>>,
    /// Changes made after the one in flight, each based on the state before it.
    waiting: VecDeque<(OpId, D::Op)>,
}

impl<D: Document> ClientState<D> {
    /// Starts with `doc` as of log position `seq` and nothing unconfirmed.
    #[must_use]
    pub fn new(client: ClientId, doc: D, seq: Seq) -> Self {
        Self {
            client,
            doc,
            seq,
            in_flight: None,
            waiting: VecDeque::new(),
        }
    }

    /// This client's identifier.
    #[must_use]
    pub fn client(&self) -> ClientId {
        self.client
    }

    /// The document, including unconfirmed local changes.
    #[must_use]
    pub fn doc(&self) -> &D {
        &self.doc
    }

    /// The last log position applied.
    #[must_use]
    pub fn seq(&self) -> Seq {
        self.seq
    }

    /// How many local changes are unconfirmed (the one in flight plus those waiting).
    #[must_use]
    pub fn pending(&self) -> usize {
        usize::from(self.in_flight.is_some()) + self.waiting.len()
    }

    /// The change in flight, to resend after reconnecting.
    #[must_use]
    pub fn in_flight(&self) -> Option<Outgoing<D::Op>> {
        self.in_flight.clone()
    }

    /// Applies a local change. Returns what to send now, or `None` if a change is already in
    /// flight (this one waits for its confirmation).
    ///
    /// # Errors
    /// `Document` if the change does not fit the document; the state is left as it was.
    pub fn local(
        &mut self,
        op_id: OpId,
        op: D::Op,
    ) -> Result<Option<Outgoing<D::Op>>, ClientError<D::Error>> {
        let mut doc = self.doc.clone();
        doc.apply(&op).map_err(ClientError::Document)?;
        self.doc = doc;
        if self.in_flight.is_some() {
            self.waiting.push_back((op_id, op));
            return Ok(None);
        }
        let out = Outgoing {
            op_id,
            base: self.seq,
            op,
        };
        self.in_flight = Some(out.clone());
        Ok(Some(out))
    }

    /// Handles the next committed entry from the server. Our own in-flight change is
    /// confirmed; anything else is adjusted past the local unconfirmed changes and applied.
    ///
    /// # Errors
    /// `OutOfOrder` if `entry` is not the next position in the log, `Document` if it cannot
    /// be adjusted or applied. The state is left as it was.
    pub fn committed(
        &mut self,
        entry: &Entry<D::Op>,
    ) -> Result<Applied<D::Op>, ClientError<D::Error>> {
        let expected = self.seq.next();
        if entry.seq != expected {
            return Err(ClientError::OutOfOrder {
                expected,
                got: entry.seq,
            });
        }

        let confirms = self
            .in_flight
            .as_ref()
            .is_some_and(|f| entry.client == self.client && entry.op_id == f.op_id);
        if confirms {
            self.seq = entry.seq;
            self.in_flight = self.waiting.pop_front().map(|(op_id, op)| Outgoing {
                op_id,
                base: entry.seq,
                op,
            });
            return Ok(Applied {
                remote: None,
                send_next: self.in_flight.clone(),
            });
        }

        // The remote entry is `first`, as on the server. It moves past each unconfirmed
        // change in turn; each of those becomes its adjusted form, now based on this entry.
        let mut remote = entry.op.clone();
        let mut in_flight = self.in_flight.clone();
        if let Some(flight) = in_flight.as_mut() {
            let (r, l) = D::transform(&remote, &flight.op).map_err(ClientError::Document)?;
            remote = r;
            flight.op = l;
            flight.base = entry.seq;
        }
        let mut waiting = self.waiting.clone();
        for (_, op) in &mut waiting {
            let (r, l) = D::transform(&remote, op).map_err(ClientError::Document)?;
            remote = r;
            *op = l;
        }
        let mut doc = self.doc.clone();
        doc.apply(&remote).map_err(ClientError::Document)?;

        self.doc = doc;
        self.in_flight = in_flight;
        self.waiting = waiting;
        self.seq = entry.seq;
        Ok(Applied {
            remote: Some(remote),
            send_next: None,
        })
    }

    /// Drops unconfirmed changes and adopts a server snapshot. Returns how many were dropped.
    pub fn reset(&mut self, doc: D, seq: Seq) -> usize {
        let dropped = self.pending();
        self.doc = doc;
        self.seq = seq;
        self.in_flight = None;
        self.waiting.clear();
        dropped
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
    use crate::document::Submitted;
    use crate::server_log::ServerLog;
    use serde::{Deserialize, Serialize};
    use uuid::Uuid;

    #[derive(Clone, Debug, PartialEq)]
    struct Chars(Vec<char>);

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    enum CharOp {
        InsertAt(usize, char),
    }

    #[derive(Debug, PartialEq, thiserror::Error)]
    #[error("position out of range")]
    struct OutOfRange;

    impl Document for Chars {
        type Op = CharOp;
        type Error = OutOfRange;

        fn apply(&mut self, op: &CharOp) -> Result<(), OutOfRange> {
            let CharOp::InsertAt(at, c) = op;
            if *at > self.0.len() {
                return Err(OutOfRange);
            }
            self.0.insert(*at, *c);
            Ok(())
        }

        fn transform(first: &CharOp, second: &CharOp) -> Result<(CharOp, CharOp), OutOfRange> {
            let (CharOp::InsertAt(a, ca), CharOp::InsertAt(b, cb)) = (first, second);
            // On a tie `first` keeps the earlier place.
            let first2 = if b < a { a + 1 } else { *a };
            let second2 = if a <= b { b + 1 } else { *b };
            Ok((
                CharOp::InsertAt(first2, *ca),
                CharOp::InsertAt(second2, *cb),
            ))
        }
    }

    fn text(doc: &Chars) -> String {
        doc.0.iter().collect()
    }
    fn id(n: u128) -> ClientId {
        ClientId(Uuid::from_u128(n))
    }
    fn op_id(n: u128) -> OpId {
        OpId(Uuid::from_u128(n))
    }
    fn ins(at: usize, c: char) -> CharOp {
        CharOp::InsertAt(at, c)
    }
    fn client(n: u128) -> ClientState<Chars> {
        ClientState::new(id(n), Chars(vec![]), Seq(0))
    }
    fn entry(seq: u64, by: u128, op: u128, change: CharOp) -> Entry<CharOp> {
        Entry {
            seq: Seq(seq),
            client: id(by),
            op_id: op_id(op),
            op: change,
        }
    }

    #[test]
    fn first_local_change_is_sent_with_current_seq() {
        let mut c = ClientState::new(id(1), Chars(vec![]), Seq(4));
        let out = c.local(op_id(1), ins(0, 'a')).unwrap().unwrap();
        assert_eq!(
            out,
            Outgoing {
                op_id: op_id(1),
                base: Seq(4),
                op: ins(0, 'a')
            }
        );
        assert_eq!(c.in_flight(), Some(out));
        assert_eq!(c.pending(), 1);
        assert_eq!(text(c.doc()), "a");
    }

    #[test]
    fn second_local_change_waits_until_confirmation() {
        let mut c = client(1);
        c.local(op_id(1), ins(0, 'a')).unwrap().unwrap();
        assert_eq!(c.local(op_id(2), ins(1, 'b')).unwrap(), None);
        assert_eq!(c.pending(), 2);

        let applied = c.committed(&entry(1, 1, 1, ins(0, 'a'))).unwrap();
        assert_eq!(applied.remote, None);
        assert_eq!(
            applied.send_next,
            Some(Outgoing {
                op_id: op_id(2),
                base: Seq(1),
                op: ins(1, 'b')
            })
        );
        assert_eq!(c.pending(), 1);
        assert_eq!(c.seq(), Seq(1));
        assert_eq!(text(c.doc()), "ab");
    }

    #[test]
    fn remote_change_is_adjusted_past_pending() {
        let mut c = client(1);
        c.local(op_id(1), ins(0, 'x')).unwrap();
        let applied = c.committed(&entry(1, 2, 9, ins(0, 'y'))).unwrap();
        assert_eq!(applied.remote, Some(ins(0, 'y')));
        assert_eq!(applied.send_next, None);
        assert_eq!(text(c.doc()), "yx");
        // The in-flight change now follows the remote one.
        assert_eq!(
            c.in_flight(),
            Some(Outgoing {
                op_id: op_id(1),
                base: Seq(1),
                op: ins(1, 'x')
            })
        );
    }

    #[test]
    fn own_entry_confirms_and_returns_no_remote() {
        let mut c = client(1);
        c.local(op_id(1), ins(0, 'a')).unwrap();
        let applied = c.committed(&entry(1, 1, 1, ins(0, 'a'))).unwrap();
        assert_eq!(
            applied,
            Applied {
                remote: None,
                send_next: None
            }
        );
        assert_eq!(c.pending(), 0);
        assert_eq!(c.in_flight(), None);
        assert_eq!(c.seq(), Seq(1));
    }

    #[test]
    fn out_of_order_entry_is_refused() {
        let mut c = ClientState::new(id(1), Chars(vec![]), Seq(1));
        c.local(op_id(1), ins(0, 'a')).unwrap();
        let before = c.clone();
        let err = c.committed(&entry(3, 2, 9, ins(0, 'z'))).unwrap_err();
        assert!(matches!(
            err,
            ClientError::OutOfOrder {
                expected: Seq(2),
                got: Seq(3)
            }
        ));
        assert_eq!(c.seq(), before.seq());
        assert_eq!(c.doc(), before.doc());
        assert_eq!(c.in_flight(), before.in_flight());
    }

    #[test]
    fn failed_local_change_leaves_state_unchanged() {
        let mut c = client(1);
        let err = c.local(op_id(1), ins(5, 'a')).unwrap_err();
        assert!(matches!(err, ClientError::Document(OutOfRange)));
        assert_eq!(c.pending(), 0);
        assert_eq!(c.in_flight(), None);
        assert_eq!(text(c.doc()), "");
    }

    #[test]
    fn failed_committed_entry_leaves_state_unchanged() {
        let mut c = client(1);
        c.local(op_id(1), ins(0, 'a')).unwrap();
        c.local(op_id(2), ins(1, 'b')).unwrap();
        let before = c.clone();
        // Position 9 cannot be applied to a two-character document.
        let err = c.committed(&entry(1, 2, 9, ins(9, 'z'))).unwrap_err();
        assert!(matches!(err, ClientError::Document(OutOfRange)));
        assert_eq!(c.seq(), before.seq());
        assert_eq!(c.doc(), before.doc());
        assert_eq!(c.in_flight(), before.in_flight());
        assert_eq!(c.pending(), 2);
    }

    #[test]
    fn reset_reports_dropped_changes() {
        let mut c = client(1);
        c.local(op_id(1), ins(0, 'a')).unwrap();
        c.local(op_id(2), ins(1, 'b')).unwrap();
        let dropped = c.reset(Chars(vec!['q']), Seq(7));
        assert_eq!(dropped, 2);
        assert_eq!(c.pending(), 0);
        assert_eq!(c.seq(), Seq(7));
        assert_eq!(text(c.doc()), "q");
        assert_eq!(c.in_flight(), None);
    }

    fn submit(log: &mut ServerLog<Chars>, who: u128, out: &Outgoing<CharOp>) {
        let result = log
            .submit(id(who), out.op_id, out.base, out.op.clone())
            .unwrap();
        assert!(matches!(result, Submitted::Committed(_)));
    }

    #[test]
    fn two_clients_and_server_converge() {
        let mut log = ServerLog::new(Chars(vec![]), Seq(0));
        let mut a = client(1);
        let mut b = client(2);

        let a1 = a.local(op_id(1), ins(0, 'a')).unwrap().unwrap();
        a.local(op_id(2), ins(1, 'b')).unwrap();
        let b1 = b.local(op_id(3), ins(0, 'x')).unwrap().unwrap();
        b.local(op_id(4), ins(1, 'y')).unwrap();
        submit(&mut log, 1, &a1);
        submit(&mut log, 2, &b1);

        let mut sent = 2;
        while a.seq() < log.head() || b.seq() < log.head() {
            for (who, c) in [(1u128, &mut a), (2u128, &mut b)] {
                let next = log.since(c.seq()).unwrap().first().cloned();
                if let Some(e) = next {
                    let applied = c.committed(&e).unwrap();
                    if let Some(out) = applied.send_next {
                        submit(&mut log, who, &out);
                        sent += 1;
                    }
                }
            }
        }
        assert_eq!(sent, 4);
        assert_eq!(a.pending() + b.pending(), 0);
        assert_eq!(a.doc(), log.doc());
        assert_eq!(b.doc(), log.doc());
        assert_eq!(log.doc().0.len(), 4);
    }
}
