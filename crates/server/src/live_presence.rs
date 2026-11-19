//! Presence notes in a live room: where each person is in the document, opaque to the server.
//!
//! Notes live in memory only. The room keeps the latest note of each held place, relays it to
//! every other connected member and hands a joiner the current notes after its catch-up. A note
//! is never stored, never put in an event and never logged, and it does not count towards the
//! unsaved-log limits. A note over [`MAX_PRESENCE_BYTES`], or beyond [`PRESENCE_PER_SECOND`]
//! a second (bursts up to [`PRESENCE_BURST`]) from one place, is dropped without an answer.

use protocol::live::{LiveServerMsg, MAX_PRESENCE_BYTES, ParticipantNo};
use std::collections::BTreeMap;
use tokio::time::Instant;

/// Presence notes a place may send each second on average.
pub(crate) const PRESENCE_PER_SECOND: u32 = 10;
/// Presence notes a place may send at once.
pub(crate) const PRESENCE_BURST: u32 = 20;

/// The latest note of each place, and each place's allowance.
#[derive(Default)]
pub(crate) struct PresenceBook {
    notes: BTreeMap<ParticipantNo, String>,
    rates: BTreeMap<ParticipantNo, Allowance>,
}

impl PresenceBook {
    /// Takes `participant`'s note. Returns the message to relay to the others, or `None` when
    /// the note is dropped (too large or too fast).
    pub(crate) fn note(
        &mut self,
        participant: ParticipantNo,
        payload: String,
        now: Instant,
    ) -> Option<LiveServerMsg> {
        if payload.len() > MAX_PRESENCE_BYTES {
            return None;
        }
        if !self
            .rates
            .entry(participant)
            .or_insert_with(|| Allowance::full(now))
            .take(now)
        {
            return None;
        }
        self.notes.insert(participant, payload.clone());
        Some(LiveServerMsg::Presence {
            participant,
            payload: Some(payload),
        })
    }

    /// The current notes of everyone but `except`, for a joiner.
    pub(crate) fn current(&self, except: ParticipantNo) -> Vec<LiveServerMsg> {
        self.notes
            .iter()
            .filter(|(participant, _)| **participant != except)
            .map(|(participant, payload)| LiveServerMsg::Presence {
                participant: *participant,
                payload: Some(payload.clone()),
            })
            .collect()
    }

    /// Forgets the notes and allowances of every place `present` no longer accepts, and
    /// returns a clearing message for each note forgotten.
    pub(crate) fn keep_only(
        &mut self,
        present: impl Fn(ParticipantNo) -> bool,
    ) -> Vec<LiveServerMsg> {
        self.rates.retain(|participant, _| present(*participant));
        let gone: Vec<ParticipantNo> = self
            .notes
            .keys()
            .copied()
            .filter(|participant| !present(*participant))
            .collect();
        gone.into_iter()
            .filter_map(|participant| {
                self.notes
                    .remove(&participant)
                    .map(|_| LiveServerMsg::Presence {
                        participant,
                        payload: None,
                    })
            })
            .collect()
    }
}

/// A token bucket of [`PRESENCE_BURST`] notes refilled at [`PRESENCE_PER_SECOND`].
struct Allowance {
    tokens: u32,
    at: Instant,
    /// Refill credit below one note, in note-nanoseconds.
    remainder: u128,
}

impl Allowance {
    fn full(now: Instant) -> Self {
        Self {
            tokens: PRESENCE_BURST,
            at: now,
            remainder: 0,
        }
    }

    fn take(&mut self, now: Instant) -> bool {
        const NANOS_PER_SECOND: u128 = 1_000_000_000;
        let elapsed = now.saturating_duration_since(self.at);
        self.at = self.at.max(now);
        let credit = elapsed
            .as_nanos()
            .saturating_mul(u128::from(PRESENCE_PER_SECOND))
            .saturating_add(self.remainder);
        self.remainder = credit % NANOS_PER_SECOND;
        let added = u32::try_from(credit / NANOS_PER_SECOND).unwrap_or(u32::MAX);
        self.tokens = self.tokens.saturating_add(added).min(PRESENCE_BURST);
        if self.tokens == 0 {
            false
        } else {
            self.tokens -= 1;
            true
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn notes_are_latest_wins_and_bounded() {
        let now = Instant::now();
        let mut book = PresenceBook::default();
        assert!(book.note(ParticipantNo(1), "a".into(), now).is_some());
        assert!(book.note(ParticipantNo(1), "b".into(), now).is_some());
        assert!(
            book.note(ParticipantNo(2), "x".repeat(MAX_PRESENCE_BYTES + 1), now)
                .is_none()
        );
        assert!(
            book.note(ParticipantNo(2), "x".repeat(MAX_PRESENCE_BYTES), now)
                .is_some()
        );
        let current = book.current(ParticipantNo(2));
        assert_eq!(
            current,
            vec![LiveServerMsg::Presence {
                participant: ParticipantNo(1),
                payload: Some("b".into())
            }]
        );
        let cleared = book.keep_only(|participant| participant == ParticipantNo(1));
        assert_eq!(
            cleared,
            vec![LiveServerMsg::Presence {
                participant: ParticipantNo(2),
                payload: None
            }]
        );
        // Clearing twice says nothing more.
        assert!(
            book.keep_only(|participant| participant == ParticipantNo(1))
                .is_empty()
        );
    }

    #[test]
    fn allowance_is_a_burst_of_twenty_then_ten_a_second() {
        let start = Instant::now();
        let mut book = PresenceBook::default();
        let taken = (0..40)
            .filter(|n| book.note(ParticipantNo(1), format!("{n}"), start).is_some())
            .count();
        assert_eq!(taken, PRESENCE_BURST as usize);
        let later = start + Duration::from_millis(500);
        let taken = (0..40)
            .filter(|n| book.note(ParticipantNo(1), format!("{n}"), later).is_some())
            .count();
        assert_eq!(taken, 5);
        // Another place has its own allowance.
        assert!(book.note(ParticipantNo(2), "x".into(), later).is_some());
        // An ended place starts afresh when it comes back.
        book.keep_only(|_| false);
        assert!(book.note(ParticipantNo(1), "x".into(), later).is_some());
    }
}
