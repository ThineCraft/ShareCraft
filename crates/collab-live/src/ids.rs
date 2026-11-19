//! Lasting identities for the rows (or other ordered items) of a live document.
//!
//! Every item has a [`LiveId`] that never changes while positions shift around it, so a change
//! can name "this row" and still mean it after other people's inserts, deletes and moves. The
//! ids of an untouched document are the base rows, `LiveId(0, 0)` upwards; each participant
//! makes new ids from its own counter with an [`IdMaker`]. [`OrderedIds`] keeps the order of
//! the live ids in runs of consecutive counters, so an untouched sheet of a million rows is a
//! single run, and remembers where deleted ids were.

use protocol::live::ParticipantNo;
use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

/// A lasting identity: (participant, counter). Participant 0 is the base rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct LiveId(pub u32, pub u32);

/// Why an identity operation was refused. A refused operation changes nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum IdError {
    /// The positions `at..at + count` do not fit a list of `len` items.
    #[error("positions {at}..+{count} are outside a list of {len}")]
    OutOfRange { at: u32, count: u32, len: u32 },
    /// The id is already live or was deleted; ids are never given out twice.
    #[error("id {0:?} is already taken")]
    Taken(LiveId),
    /// A participant's counter has run out.
    #[error("no more ids can be made")]
    Exhausted,
    /// The client holds no place yet, so it has no participant number to make ids under.
    #[error("no place in the session yet")]
    NoPlace,
    /// Stored identity data violates an invariant required by [`OrderedIds`].
    #[error("invalid ordered ids: {0}")]
    Invalid(&'static str),
}

/// Makes new ids for one participant, counting on from 0.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdMaker {
    participant: u32,
    /// The next counter to give out; `u32::MAX + 1` once all are used.
    next: u64,
}

impl IdMaker {
    /// A maker for `participant`, starting at counter 0.
    pub fn new(participant: ParticipantNo) -> IdMaker {
        IdMaker {
            participant: participant.0,
            next: 0,
        }
    }

    /// Takes `count` consecutive ids and returns the first of them.
    pub fn take(&mut self, count: u32) -> Result<LiveId, IdError> {
        let first = u32::try_from(self.next).map_err(|_| IdError::Exhausted)?;
        let end = self.next + u64::from(count);
        if end > u64::from(u32::MAX) + 1 {
            return Err(IdError::Exhausted);
        }
        self.next = end;
        Ok(LiveId(self.participant, first))
    }
}

/// Where an id is now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lookup {
    /// Live, at this position.
    At(u32),
    /// Deleted. `was_at` is the position it had when deleted; `after` is its nearest surviving
    /// predecessor (`None` when there is none), updated whenever an anchor is deleted.
    Deleted { was_at: u32, after: Option<LiveId> },
    /// Never seen.
    Unknown,
}

/// A run of `len` consecutive counters of one participant, starting at `start`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Run {
    participant: u32,
    start: u32,
    len: u32,
}

impl Run {
    /// The counter one past the run's last, as `u64` so it cannot overflow.
    fn end(&self) -> u64 {
        u64::from(self.start) + u64::from(self.len)
    }

    fn holds(&self, id: LiveId) -> bool {
        id.0 == self.participant && id.1 >= self.start && u64::from(id.1) < self.end()
    }

    /// Whether `next` carries on straight after this run.
    fn continued_by(&self, next: &Run) -> bool {
        self.participant == next.participant && self.end() == u64::from(next.start)
    }

    /// Whether this run shares any id with the counters `start..end` of `participant`.
    fn overlaps(&self, participant: u32, start: u64, end: u64) -> bool {
        self.participant == participant && u64::from(self.start) < end && start < self.end()
    }
}

/// A deleted block (or the part of one that was a single run).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct DeletedRun {
    run: Run,
    /// The position its first id had.
    was_at: u32,
    /// The live id just above the block when it was deleted.
    after: Option<LiveId>,
}

type IdentityInterval = (u32, u32, u64);
type DeletedInterval = (u32, u32, u64, usize);

/// The live ids in order, kept as runs, and the deleted ids with where they were.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OrderedIds {
    live: Vec<Run>,
    deleted: Vec<DeletedRun>,
}

#[derive(Deserialize)]
struct RawOrderedIds {
    live: Vec<Run>,
    deleted: Vec<DeletedRun>,
}

impl<'de> Deserialize<'de> for OrderedIds {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = RawOrderedIds::deserialize(deserializer)?;
        let mut ids = OrderedIds {
            live: raw.live,
            deleted: raw.deleted,
        };
        ids.normalize_deleted_anchors()
            .map_err(serde::de::Error::custom)?;
        ids.validate().map_err(serde::de::Error::custom)?;
        Ok(ids)
    }
}

impl OrderedIds {
    /// The base rows `LiveId(0, 0)..LiveId(0, len - 1)`, as one run.
    pub fn base(len: u32) -> OrderedIds {
        let live = if len == 0 {
            Vec::new()
        } else {
            vec![Run {
                participant: 0,
                start: 0,
                len,
            }]
        };
        OrderedIds {
            live,
            deleted: Vec::new(),
        }
    }

    /// How many ids are live.
    pub fn len(&self) -> u32 {
        // Inserts refuse to grow past u32::MAX, so the sum always fits.
        self.live
            .iter()
            .fold(0u32, |sum, run| sum.saturating_add(run.len))
    }

    /// Whether no id is live.
    pub fn is_empty(&self) -> bool {
        self.live.is_empty()
    }

    /// How many runs the live ids are kept in.
    pub fn runs(&self) -> usize {
        self.live.len()
    }

    /// Checks that every stored identity is well formed, unique and internally consistent.
    pub fn validate(&self) -> Result<(), IdError> {
        let (_, mut live_intervals) = self.validated_intervals()?;
        live_intervals.sort_unstable();
        for gone in &self.deleted {
            if let Some(after) = gone.after
                && !Self::sorted_intervals_hold(&live_intervals, after)
            {
                return Err(IdError::Invalid("deleted after id is not live"));
            }
        }
        Ok(())
    }

    fn validated_intervals(
        &self,
    ) -> Result<(Vec<IdentityInterval>, Vec<IdentityInterval>), IdError> {
        let mut intervals = Vec::with_capacity(self.live.len() + self.deleted.len());
        let mut live_intervals = Vec::with_capacity(self.live.len());
        let mut live_len = 0u64;

        for run in &self.live {
            Self::validate_run(run)?;
            live_len += u64::from(run.len);
            if live_len > u64::from(u32::MAX) {
                return Err(IdError::Invalid("live length exceeds u32::MAX"));
            }
            let interval = (run.participant, run.start, run.end());
            intervals.push(interval);
            live_intervals.push(interval);
        }
        for gone in &self.deleted {
            Self::validate_run(&gone.run)?;
            if u64::from(gone.was_at) + u64::from(gone.run.len) > u64::from(u32::MAX) {
                return Err(IdError::Invalid(
                    "deleted historical position exceeds u32::MAX",
                ));
            }
            intervals.push((gone.run.participant, gone.run.start, gone.run.end()));
        }

        intervals.sort_unstable();
        let mut previous: Option<(u32, u64)> = None;
        for &(participant, start, end) in &intervals {
            if let Some((previous_participant, previous_end)) = previous
                && participant == previous_participant
                && u64::from(start) < previous_end
            {
                return Err(IdError::Taken(LiveId(participant, start)));
            }
            previous = Some((participant, end));
        }
        Ok((intervals, live_intervals))
    }

    fn normalize_deleted_anchors(&mut self) -> Result<(), IdError> {
        let (_, mut live_intervals) = self.validated_intervals()?;
        live_intervals.sort_unstable();

        let mut deleted_intervals: Vec<_> = self
            .deleted
            .iter()
            .enumerate()
            .map(|(index, gone)| (gone.run.participant, gone.run.start, gone.run.end(), index))
            .collect();
        deleted_intervals.sort_unstable();

        let anchors: Vec<_> = self.deleted.iter().map(|gone| gone.after).collect();
        let mut states = vec![0u8; self.deleted.len()];
        let mut normalized = vec![None; self.deleted.len()];

        for root in 0..self.deleted.len() {
            if states.get(root).copied() == Some(2) {
                continue;
            }
            let mut path = Vec::new();
            let mut current = root;
            let terminal = loop {
                match states
                    .get(current)
                    .copied()
                    .ok_or(IdError::Invalid("deleted anchor index is out of range"))?
                {
                    1 => return Err(IdError::Invalid("deleted after anchors form a cycle")),
                    2 => {
                        break normalized
                            .get(current)
                            .copied()
                            .ok_or(IdError::Invalid("deleted anchor index is out of range"))?;
                    }
                    _ => {}
                }
                *states
                    .get_mut(current)
                    .ok_or(IdError::Invalid("deleted anchor index is out of range"))? = 1;
                path.push(current);

                let Some(anchor) = anchors
                    .get(current)
                    .copied()
                    .ok_or(IdError::Invalid("deleted anchor index is out of range"))?
                else {
                    break None;
                };
                if Self::sorted_intervals_hold(&live_intervals, anchor) {
                    break Some(anchor);
                }
                current = Self::sorted_deleted_intervals_find(&deleted_intervals, anchor)
                    .ok_or(IdError::Invalid("deleted after id does not exist"))?;
            };

            for index in path.into_iter().rev() {
                *normalized
                    .get_mut(index)
                    .ok_or(IdError::Invalid("deleted anchor index is out of range"))? = terminal;
                *states
                    .get_mut(index)
                    .ok_or(IdError::Invalid("deleted anchor index is out of range"))? = 2;
            }
        }

        for (gone, after) in self.deleted.iter_mut().zip(normalized) {
            gone.after = after;
        }
        Ok(())
    }

    fn sorted_intervals_hold(intervals: &[IdentityInterval], id: LiveId) -> bool {
        let index = intervals.partition_point(|&(participant, start, _)| {
            participant < id.0 || (participant == id.0 && start <= id.1)
        });
        index
            .checked_sub(1)
            .and_then(|index| intervals.get(index))
            .is_some_and(|&(participant, _, end)| participant == id.0 && u64::from(id.1) < end)
    }

    fn sorted_deleted_intervals_find(intervals: &[DeletedInterval], id: LiveId) -> Option<usize> {
        let index = intervals.partition_point(|&(participant, start, _, _)| {
            participant < id.0 || (participant == id.0 && start <= id.1)
        });
        index
            .checked_sub(1)
            .and_then(|index| intervals.get(index))
            .filter(|&&(participant, _, end, _)| participant == id.0 && u64::from(id.1) < end)
            .map(|&(_, _, _, deleted_index)| deleted_index)
    }

    fn validate_run(run: &Run) -> Result<(), IdError> {
        if run.len == 0 {
            return Err(IdError::Invalid("run length is zero"));
        }
        if run.end() > u64::from(u32::MAX) + 1 {
            return Err(IdError::Invalid("run counter exceeds u32::MAX"));
        }
        Ok(())
    }

    /// The id at `pos`, if there is one.
    pub fn id_at(&self, pos: u32) -> Option<LiveId> {
        let mut first = 0u32;
        for run in &self.live {
            let offset = pos.checked_sub(first)?;
            if offset < run.len {
                return Some(LiveId(run.participant, run.start + offset));
            }
            first += run.len;
        }
        None
    }

    /// Where `id` is now.
    pub fn find(&self, id: LiveId) -> Lookup {
        let mut first = 0u32;
        for run in &self.live {
            if run.holds(id) {
                return Lookup::At(first + (id.1 - run.start));
            }
            first += run.len;
        }
        for gone in &self.deleted {
            if gone.run.holds(id) {
                return Lookup::Deleted {
                    was_at: gone.was_at.saturating_add(id.1 - gone.run.start),
                    after: gone.after,
                };
            }
        }
        Lookup::Unknown
    }

    /// Inserts the `count` ids starting at `first` so the first of them is at position `at`.
    pub fn insert(&mut self, at: u32, first: LiveId, count: u32) -> Result<(), IdError> {
        let len = self.len();
        if at > len || len.checked_add(count).is_none() {
            return Err(IdError::OutOfRange { at, count, len });
        }
        if count == 0 {
            debug_assert!(self.validate().is_ok());
            return Ok(());
        }
        let start = u64::from(first.1);
        let end = start + u64::from(count);
        if end > u64::from(u32::MAX) + 1 {
            return Err(IdError::Exhausted);
        }
        if let Some(taken) = self.first_taken(first.0, start, end) {
            return Err(IdError::Taken(taken));
        }
        let index = self.split_at(at);
        let run = Run {
            participant: first.0,
            start: first.1,
            len: count,
        };
        self.live.insert(index, run);
        self.merge();
        debug_assert!(self.validate().is_ok());
        Ok(())
    }

    /// Deletes the `count` ids from position `at`, remembering where they were.
    pub fn delete(&mut self, at: u32, count: u32) -> Result<(), IdError> {
        let len = self.len();
        if at.checked_add(count).is_none_or(|end| end > len) {
            return Err(IdError::OutOfRange { at, count, len });
        }
        if count == 0 {
            debug_assert!(self.validate().is_ok());
            return Ok(());
        }
        let after = at.checked_sub(1).and_then(|above| self.id_at(above));
        let block = self.take_out(at, count);
        let mut removed_intervals: Vec<_> = block
            .iter()
            .map(|run| (run.participant, run.start, run.end()))
            .collect();
        removed_intervals.sort_unstable();
        for gone in &mut self.deleted {
            if gone
                .after
                .is_some_and(|id| Self::sorted_intervals_hold(&removed_intervals, id))
            {
                gone.after = after;
            }
        }
        let mut was_at = at;
        for run in block {
            self.deleted.push(DeletedRun { run, was_at, after });
            was_at += run.len;
        }
        debug_assert!(self.validate().is_ok());
        Ok(())
    }

    /// Moves the `count` ids from position `from` so the first of them is at `to`, where `to`
    /// is a position in the list after the block is taken out.
    pub fn move_block(&mut self, from: u32, count: u32, to: u32) -> Result<(), IdError> {
        let len = self.len();
        if from.checked_add(count).is_none_or(|end| end > len) {
            return Err(IdError::OutOfRange {
                at: from,
                count,
                len,
            });
        }
        let rest = len - count;
        if to > rest {
            return Err(IdError::OutOfRange {
                at: to,
                count,
                len: rest,
            });
        }
        if count == 0 {
            debug_assert!(self.validate().is_ok());
            return Ok(());
        }
        let block = self.take_out(from, count);
        let index = self.split_at(to);
        self.live.splice(index..index, block);
        self.merge();
        debug_assert!(self.validate().is_ok());
        Ok(())
    }

    /// The first id among `participant`'s counters `start..end` that is live or deleted.
    fn first_taken(&self, participant: u32, start: u64, end: u64) -> Option<LiveId> {
        self.live
            .iter()
            .chain(self.deleted.iter().map(|gone| &gone.run))
            .filter(|run| run.overlaps(participant, start, end))
            .map(|run| run.start.max(u32::try_from(start).unwrap_or(u32::MAX)))
            .min()
            .map(|counter| LiveId(participant, counter))
    }

    /// Takes the live ids at positions `at..at + count` out of the list, as runs. The range
    /// must be in bounds.
    fn take_out(&mut self, at: u32, count: u32) -> Vec<Run> {
        let first = self.split_at(at);
        let last = self.split_at(at + count);
        let block: Vec<Run> = self.live.drain(first..last).collect();
        self.merge();
        block
    }

    /// Splits the run holding `pos` so a run starts there, and returns that run's index (the
    /// number of runs when `pos` is the end). `pos` must be at most `len()`.
    fn split_at(&mut self, pos: u32) -> usize {
        let mut first = 0u32;
        for index in 0..self.live.len() {
            let Some(run) = self.live.get(index).copied() else {
                break;
            };
            if pos == first {
                return index;
            }
            let offset = pos - first;
            if offset < run.len {
                let upper = Run {
                    participant: run.participant,
                    start: run.start + offset,
                    len: run.len - offset,
                };
                if let Some(lower) = self.live.get_mut(index) {
                    lower.len = offset;
                }
                self.live.insert(index + 1, upper);
                return index + 1;
            }
            first += run.len;
        }
        self.live.len()
    }

    /// Joins neighbouring runs that carry on from each other.
    fn merge(&mut self) {
        let mut merged: Vec<Run> = Vec::with_capacity(self.live.len());
        for run in self.live.drain(..) {
            match merged.last_mut() {
                Some(last) if last.continued_by(&run) => last.len += run.len,
                _ => merged.push(run),
            }
        }
        self.live = merged;
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
    use proptest::prelude::*;
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    #[test]
    fn untouched_sheet_is_one_run() {
        let ids = OrderedIds::base(1_048_576);
        assert_eq!(ids.runs(), 1);
        assert_eq!(ids.len(), 1_048_576);
        assert_eq!(ids.id_at(5), Some(LiveId(0, 5)));
        assert_eq!(ids.id_at(1_048_576), None);
        assert_eq!(ids.find(LiveId(0, 1_048_575)), Lookup::At(1_048_575));
        assert_eq!(ids.find(LiveId(0, 1_048_576)), Lookup::Unknown);
        assert_eq!(OrderedIds::base(0).runs(), 0);
    }

    #[test]
    fn insert_then_find() {
        let mut ids = OrderedIds::base(10);
        ids.insert(5, LiveId(2, 0), 3).unwrap();
        assert_eq!(ids.find(LiveId(2, 1)), Lookup::At(6));
        assert_eq!(ids.find(LiveId(0, 5)), Lookup::At(8));
        assert_eq!(ids.find(LiveId(0, 4)), Lookup::At(4));
        assert_eq!(ids.len(), 13);
        assert_eq!(ids.runs(), 3);
    }

    #[test]
    fn deleted_ids_remember_where_they_were() {
        let mut ids = OrderedIds::base(10);
        ids.delete(3, 2).unwrap();
        assert_eq!(
            ids.find(LiveId(0, 4)),
            Lookup::Deleted {
                was_at: 4,
                after: Some(LiveId(0, 2))
            }
        );
        assert_eq!(
            ids.find(LiveId(0, 3)),
            Lookup::Deleted {
                was_at: 3,
                after: Some(LiveId(0, 2))
            }
        );
        assert_eq!(ids.find(LiveId(0, 5)), Lookup::At(3));
        assert_eq!(ids.len(), 8);

        let mut top = OrderedIds::base(10);
        top.delete(0, 2).unwrap();
        assert_eq!(
            top.find(LiveId(0, 1)),
            Lookup::Deleted {
                was_at: 1,
                after: None
            }
        );
    }

    #[test]
    fn deleting_an_anchor_reanchors_deleted_ids() {
        let mut ids = OrderedIds::base(6);
        ids.delete(3, 1).unwrap();
        ids.delete(2, 1).unwrap();

        assert_eq!(
            ids.find(LiveId(0, 3)),
            Lookup::Deleted {
                was_at: 3,
                after: Some(LiveId(0, 1))
            }
        );
    }

    #[test]
    fn reusing_an_id_is_refused() {
        let mut ids = OrderedIds::base(10);
        ids.delete(3, 2).unwrap();
        ids.insert(0, LiveId(5, 3), 2).unwrap();
        let before = ids.clone();
        assert_eq!(
            ids.insert(0, LiveId(0, 7), 1),
            Err(IdError::Taken(LiveId(0, 7)))
        );
        assert_eq!(
            ids.insert(0, LiveId(0, 4), 1),
            Err(IdError::Taken(LiveId(0, 4)))
        );
        // A block that only reaches a taken id at its end is refused too.
        assert_eq!(
            ids.insert(0, LiveId(5, 0), 4),
            Err(IdError::Taken(LiveId(5, 3)))
        );
        assert_eq!(ids, before);
    }

    #[test]
    fn out_of_range_is_refused() {
        let mut ids = OrderedIds::base(10);
        let before = ids.clone();
        assert_eq!(
            ids.insert(11, LiveId(1, 0), 1),
            Err(IdError::OutOfRange {
                at: 11,
                count: 1,
                len: 10
            })
        );
        assert_eq!(
            ids.delete(8, 3),
            Err(IdError::OutOfRange {
                at: 8,
                count: 3,
                len: 10
            })
        );
        assert_eq!(
            ids.move_block(9, 2, 0),
            Err(IdError::OutOfRange {
                at: 9,
                count: 2,
                len: 10
            })
        );
        assert_eq!(
            ids.move_block(0, 2, 9),
            Err(IdError::OutOfRange {
                at: 9,
                count: 2,
                len: 8
            })
        );
        assert_eq!(ids, before);
        // Inserting at the very end is allowed.
        ids.insert(10, LiveId(1, 0), 1).unwrap();
        assert_eq!(ids.id_at(10), Some(LiveId(1, 0)));
    }

    #[test]
    fn move_block_lands_after_removal() {
        let mut ids = OrderedIds::base(6);
        // Take out rows 1 and 2, leaving 0, 3, 4, 5; put them at position 3, before row 5.
        ids.move_block(1, 2, 3).unwrap();
        let order: Vec<_> = (0..6).filter_map(|p| ids.id_at(p)).map(|id| id.1).collect();
        assert_eq!(order, vec![0, 3, 4, 1, 2, 5]);
        // Moving them back joins the runs into one again.
        ids.move_block(3, 2, 1).unwrap();
        assert_eq!(ids, OrderedIds::base(6));
    }

    #[test]
    fn id_maker_counts_on() {
        let mut maker = IdMaker::new(ParticipantNo(4));
        assert_eq!(maker.take(3), Ok(LiveId(4, 0)));
        assert_eq!(maker.take(1), Ok(LiveId(4, 3)));

        let mut full = IdMaker::new(ParticipantNo(4));
        assert_eq!(full.take(u32::MAX), Ok(LiveId(4, 0)));
        assert_eq!(full.take(2), Err(IdError::Exhausted));
        assert_eq!(full.take(1), Ok(LiveId(4, u32::MAX)));
        assert_eq!(full.take(1), Err(IdError::Exhausted));
    }

    #[test]
    fn ordered_ids_round_trip_as_json() {
        let mut ids = OrderedIds::base(10);
        ids.insert(5, LiveId(2, 0), 3).unwrap();
        ids.delete(1, 2).unwrap();
        ids.move_block(0, 2, 6).unwrap();
        let json = serde_json::to_string(&ids).unwrap();
        let back: OrderedIds = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ids);

        let mut maker = IdMaker::new(ParticipantNo(3));
        maker.take(7).unwrap();
        let json = serde_json::to_string(&maker).unwrap();
        let mut back: IdMaker = serde_json::from_str(&json).unwrap();
        assert_eq!(back.take(1), Ok(LiveId(3, 7)));
    }

    #[test]
    fn ordered_ids_loads_g2_deleted_anchor_chain() {
        let g2_json = serde_json::json!({
            "live": [
                { "participant": 0, "start": 0, "len": 2 },
                { "participant": 0, "start": 4, "len": 2 }
            ],
            "deleted": [
                {
                    "run": { "participant": 0, "start": 3, "len": 1 },
                    "was_at": 3,
                    "after": [0, 2]
                },
                {
                    "run": { "participant": 0, "start": 2, "len": 1 },
                    "was_at": 2,
                    "after": [0, 1]
                }
            ]
        });
        let loaded: OrderedIds = serde_json::from_value(g2_json).unwrap();

        let mut expected = OrderedIds::base(6);
        expected.delete(3, 1).unwrap();
        expected.delete(2, 1).unwrap();

        assert_eq!(loaded, expected);
    }

    #[test]
    fn deleting_fragmented_runs_reanchors_without_quadratic_work() {
        const RUNS: u32 = 20_000;
        let mut ids = OrderedIds {
            live: (0..RUNS)
                .map(|start| Run {
                    participant: 1,
                    start: start * 2,
                    len: 1,
                })
                .collect(),
            deleted: (0..RUNS)
                .map(|start| DeletedRun {
                    run: Run {
                        participant: 2,
                        start,
                        len: 1,
                    },
                    was_at: start,
                    after: Some(LiveId(1, start * 2)),
                })
                .collect(),
        };

        let started = Instant::now();
        ids.delete(0, RUNS).unwrap();

        assert!(
            started.elapsed() < Duration::from_millis(500),
            "fragmented deletion took {:?}",
            started.elapsed()
        );
        assert!(ids.deleted.iter().all(|gone| gone.after.is_none()));
    }

    fn assert_ordered_ids_json_is_invalid(json: serde_json::Value) {
        let result = serde_json::from_value::<OrderedIds>(json);
        assert!(
            result.is_err(),
            "invalid OrderedIds deserialized: {result:?}"
        );
    }

    #[test]
    fn ordered_ids_reject_invalid_serialized_runs() {
        for invalid in [
            serde_json::json!({
                "live": [{ "participant": 1, "start": 0, "len": 0 }],
                "deleted": []
            }),
            serde_json::json!({
                "live": [],
                "deleted": [{
                    "run": { "participant": 1, "start": 0, "len": 0 },
                    "was_at": 0,
                    "after": null
                }]
            }),
            serde_json::json!({
                "live": [{ "participant": 1, "start": u32::MAX, "len": 2 }],
                "deleted": []
            }),
            serde_json::json!({
                "live": [
                    { "participant": 1, "start": 0, "len": u32::MAX },
                    { "participant": 2, "start": 0, "len": 1 }
                ],
                "deleted": []
            }),
            serde_json::json!({
                "live": [
                    { "participant": 1, "start": 0, "len": 2 },
                    { "participant": 1, "start": 1, "len": 2 }
                ],
                "deleted": []
            }),
            serde_json::json!({
                "live": [{ "participant": 1, "start": 0, "len": 1 }],
                "deleted": [{
                    "run": { "participant": 2, "start": 0, "len": 1 },
                    "was_at": 0,
                    "after": [9, 9]
                }]
            }),
            serde_json::json!({
                "live": [],
                "deleted": [{
                    "run": { "participant": 1, "start": 0, "len": 2 },
                    "was_at": u32::MAX,
                    "after": null
                }]
            }),
            serde_json::json!({
                "live": [],
                "deleted": [
                    {
                        "run": { "participant": 0, "start": 0, "len": 1 },
                        "was_at": 0,
                        "after": [0, 1]
                    },
                    {
                        "run": { "participant": 0, "start": 1, "len": 1 },
                        "was_at": 1,
                        "after": [0, 0]
                    }
                ]
            }),
        ] {
            assert_ordered_ids_json_is_invalid(invalid);
        }
    }

    #[test]
    fn ordered_ids_reject_live_deleted_overlap() {
        assert_ordered_ids_json_is_invalid(serde_json::json!({
            "live": [{ "participant": 1, "start": 0, "len": 3 }],
            "deleted": [{
                "run": { "participant": 1, "start": 2, "len": 2 },
                "was_at": 2,
                "after": [1, 0]
            }]
        }));
    }

    #[test]
    fn ordered_ids_reject_duplicate_deleted_ids() {
        assert_ordered_ids_json_is_invalid(serde_json::json!({
            "live": [],
            "deleted": [
                {
                    "run": { "participant": 1, "start": 0, "len": 2 },
                    "was_at": 0,
                    "after": null
                },
                {
                    "run": { "participant": 1, "start": 1, "len": 2 },
                    "was_at": 1,
                    "after": null
                }
            ]
        }));
    }

    #[test]
    fn ordered_ids_valid_round_trip_still_works() {
        let mut ids = OrderedIds::base(8);
        ids.insert(3, LiveId(2, 10), 2).unwrap();
        ids.delete(4, 2).unwrap();
        ids.move_block(0, 2, 4).unwrap();

        let json = serde_json::to_string(&ids).unwrap();
        let deserialized: OrderedIds = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized, ids);
    }

    #[test]
    fn ordered_ids_validate_many_fragmented_anchors_without_quadratic_work() {
        const RUNS: u32 = 50_000;
        let after = Some(LiveId(1, (RUNS - 1) * 2));
        let ids = OrderedIds {
            live: (0..RUNS)
                .map(|start| Run {
                    participant: 1,
                    start: start * 2,
                    len: 1,
                })
                .collect(),
            deleted: (0..RUNS)
                .map(|start| DeletedRun {
                    run: Run {
                        participant: 2,
                        start: start * 2,
                        len: 1,
                    },
                    was_at: start,
                    after,
                })
                .collect(),
        };
        let json = serde_json::to_vec(&ids).unwrap();
        assert!(json.len() < 8 * 1024 * 1024);

        let deserialized: OrderedIds = serde_json::from_slice(&json).unwrap();

        assert_eq!(deserialized, ids);
    }

    #[derive(Clone, Debug)]
    enum Op {
        Insert {
            at: u32,
            participant: u32,
            count: u32,
        },
        Delete {
            at: u32,
            count: u32,
        },
        Move {
            from: u32,
            count: u32,
            to: u32,
        },
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            (0..60u32, 1..4u32, 1..6u32).prop_map(|(at, participant, count)| Op::Insert {
                at,
                participant,
                count
            }),
            (0..60u32, 1..6u32).prop_map(|(at, count)| Op::Delete { at, count }),
            (0..60u32, 1..6u32, 0..60u32).prop_map(|(from, count, to)| Op::Move {
                from,
                count,
                to
            }),
        ]
    }

    /// How many neighbouring pairs in `list` do not carry on from each other.
    fn breaks(list: &[LiveId]) -> usize {
        list.windows(2)
            .filter(|w| !(w[0].0 == w[1].0 && w[0].1.checked_add(1) == Some(w[1].1)))
            .count()
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]
        #[test]
        fn ordered_ids_match_a_plain_list(
            base in 0..=50u32,
            ops in proptest::collection::vec(op(), 0..=200),
        ) {
            let mut ids = OrderedIds::base(base);
            let mut list: Vec<LiveId> = (0..base).map(|c| LiveId(0, c)).collect();
            let mut deleted: HashMap<LiveId, (u32, Option<LiveId>)> = HashMap::new();
            let mut makers: HashMap<u32, IdMaker> = HashMap::new();
            for op in ops {
                let len = list.len() as u32;
                match op {
                    Op::Insert { at, participant, count } => {
                        let maker = makers
                            .entry(participant)
                            .or_insert_with(|| IdMaker::new(ParticipantNo(participant)));
                        let first = maker.take(count).unwrap();
                        let result = ids.insert(at, first, count);
                        if at <= len {
                            prop_assert_eq!(result, Ok(()));
                            for i in 0..count {
                                list.insert((at + i) as usize, LiveId(first.0, first.1 + i));
                            }
                        } else {
                            prop_assert_eq!(result, Err(IdError::OutOfRange { at, count, len }));
                        }
                    }
                    Op::Delete { at, count } => {
                        let result = ids.delete(at, count);
                        if at + count <= len {
                            prop_assert_eq!(result, Ok(()));
                            let after = at.checked_sub(1).map(|p| list[p as usize]);
                            let gone: Vec<_> =
                                list.drain(at as usize..(at + count) as usize).collect();
                            for (_, deleted_after) in deleted.values_mut() {
                                if deleted_after.is_some_and(|id| gone.contains(&id)) {
                                    *deleted_after = after;
                                }
                            }
                            for (i, id) in gone.into_iter().enumerate() {
                                deleted.insert(id, (at + i as u32, after));
                            }
                        } else {
                            prop_assert_eq!(result, Err(IdError::OutOfRange { at, count, len }));
                        }
                    }
                    Op::Move { from, count, to } => {
                        let before = ids.clone();
                        let result = ids.move_block(from, count, to);
                        if from + count <= len && to <= len - count {
                            prop_assert_eq!(result, Ok(()));
                            let block: Vec<_> =
                                list.drain(from as usize..(from + count) as usize).collect();
                            for (i, id) in block.into_iter().enumerate() {
                                list.insert(to as usize + i, id);
                            }
                        } else {
                            prop_assert!(
                                matches!(result, Err(IdError::OutOfRange { .. })),
                                "unexpected {:?}",
                                result
                            );
                            prop_assert_eq!(&ids, &before);
                        }
                    }
                }
                prop_assert_eq!(ids.len(), list.len() as u32);
                prop_assert_eq!(ids.id_at(list.len() as u32), None);
                for (pos, id) in list.iter().enumerate() {
                    prop_assert_eq!(ids.id_at(pos as u32), Some(*id));
                    prop_assert_eq!(ids.find(*id), Lookup::At(pos as u32));
                }
                for (id, (was_at, after)) in &deleted {
                    prop_assert_eq!(
                        ids.find(*id),
                        Lookup::Deleted { was_at: *was_at, after: *after }
                    );
                }
                let expected_runs = if list.is_empty() { 0 } else { breaks(&list) + 1 };
                prop_assert!(ids.runs() <= expected_runs);
            }
        }
    }
}
