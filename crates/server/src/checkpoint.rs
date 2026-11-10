//! Where rooms load documents from and write their checkpoints to, and when they write them.

use std::path::{Path, PathBuf};
use std::time::Duration;

use collab_core::ids::{DocId, Seq};
use revisions::{
    Author, Checkpoint, CheckpointError, GitError, Outcome, RevisionId, RevisionInfo,
    RevisionStore, Stored,
};
use serde_json::{Map, Value};
use tokio::time::Instant;

/// The single library every document lives in until libraries can be chosen.
const LIBRARY: &str = "00000000-0000-0000-0000-000000000001";
/// How many times in all an empty room tries its final checkpoint before giving up.
const FINAL_ATTEMPTS: u32 = 10;
/// Longest wait between attempts at an empty room's final checkpoint.
const MAX_FINAL_DELAY: Duration = Duration::from_secs(30);

/// Stores and reads document revisions on behalf of rooms. Calls block, so rooms make them
/// from a blocking task.
pub trait Checkpointer: Send + Sync + 'static {
    /// Stores one checkpoint.
    ///
    /// # Errors
    /// Whatever the store reports, including `Stale` when `c.base` is not the current revision.
    fn write(&self, c: &Checkpoint<'_>) -> Result<Outcome, CheckpointError>;

    /// The document's current revision, or `None` if it has never been saved.
    ///
    /// # Errors
    /// Whatever the store reports when it cannot be read.
    fn load(&self, doc: DocId) -> Result<Option<Stored>, CheckpointError>;

    /// The content of `doc` as it was at `revision`; `None` if `revision` is not one of the
    /// document's. Implementations that can read history must override this; the default
    /// refuses.
    ///
    /// # Errors
    /// Whatever the store reports when it cannot be read.
    fn load_at(
        &self,
        doc: DocId,
        revision: &RevisionId,
    ) -> Result<Option<Stored>, CheckpointError> {
        let _ = (doc, revision);
        Err(CheckpointError::Git(GitError::new(
            "this store cannot read an older revision",
        )))
    }

    /// Describes the revision `revision` of `doc`; `None` if it is not one.
    ///
    /// # Errors
    /// Whatever the store reports, including `Corrupt` for unreadable metadata.
    fn describe(
        &self,
        doc: DocId,
        revision: &RevisionId,
    ) -> Result<Option<RevisionInfo>, CheckpointError>;

    /// Every revision of `doc`, oldest first.
    ///
    /// # Errors
    /// Whatever the store reports, including `Corrupt` for unreadable metadata.
    fn history(&self, doc: DocId) -> Result<Vec<RevisionInfo>, CheckpointError>;

    /// Every document with a revision.
    ///
    /// # Errors
    /// Whatever the store reports when it cannot be read.
    fn documents(&self) -> Result<Vec<DocId>, GitError>;
}

impl Checkpointer for RevisionStore {
    fn write(&self, c: &Checkpoint<'_>) -> Result<Outcome, CheckpointError> {
        self.checkpoint(c)
    }

    fn load(&self, doc: DocId) -> Result<Option<Stored>, CheckpointError> {
        self.read(doc, None)
    }

    fn load_at(
        &self,
        doc: DocId,
        revision: &RevisionId,
    ) -> Result<Option<Stored>, CheckpointError> {
        self.read(doc, Some(revision))
    }

    fn describe(
        &self,
        doc: DocId,
        revision: &RevisionId,
    ) -> Result<Option<RevisionInfo>, CheckpointError> {
        RevisionStore::describe(self, doc, revision)
    }

    fn history(&self, doc: DocId) -> Result<Vec<RevisionInfo>, CheckpointError> {
        RevisionStore::history(self, doc)
    }

    fn documents(&self) -> Result<Vec<DocId>, GitError> {
        RevisionStore::documents(self)
    }
}

/// The library repository under `data_dir`.
#[must_use]
pub fn library_path(data_dir: &Path) -> PathBuf {
    data_dir.join("libraries").join(format!("{LIBRARY}.git"))
}

/// When checkpoints are written.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Timing {
    /// Written once no change has arrived for this long.
    pub idle: Duration,
    /// Written at the latest this long after the first unsaved change.
    pub max: Duration,
}

/// One checkpoint to write, taken from the room when it starts.
pub(crate) struct SaveJob {
    text: String,
    base: Option<RevisionId>,
    authors: Vec<Author>,
    /// The document fields, carried forward unchanged from the revision the room loaded.
    fields: Map<String, Value>,
}

impl SaveJob {
    /// The revision this checkpoint is based on.
    pub(crate) fn base(&self) -> Option<&RevisionId> {
        self.base.as_ref()
    }

    /// The ids of the people credited with this checkpoint, in first-seen order.
    pub(crate) fn author_ids(&self) -> Vec<String> {
        self.authors.iter().map(|a| a.id.clone()).collect()
    }

    /// Writes the checkpoint of `doc`, stored under the document's name `file_name`. Blocks.
    pub(crate) fn write(
        &self,
        doc: DocId,
        file_name: &str,
        store: &dyn Checkpointer,
    ) -> Result<Outcome, CheckpointError> {
        store.write(&Checkpoint {
            doc,
            file_name,
            bytes: self.text.as_bytes(),
            base: self.base.as_ref(),
            authors: &self.authors,
            fields: &self.fields,
        })
    }
}

/// Why a checkpoint stored nothing.
#[derive(Debug)]
pub(crate) enum WriteError {
    /// The document has a revision the checkpoint was not based on: someone else saved it.
    Stale {
        /// The document's current revision, if it has one.
        current: Option<RevisionId>,
    },
    /// Anything else; worth trying again.
    Failed(String),
}

impl From<CheckpointError> for WriteError {
    fn from(e: CheckpointError) -> Self {
        match e {
            CheckpointError::Stale { current } => WriteError::Stale { current },
            other => WriteError::Failed(other.to_string()),
        }
    }
}

/// How a checkpoint ended.
#[derive(Debug)]
pub(crate) enum Finished {
    /// Everything through `through` is stored as `revision`.
    Saved { revision: RevisionId, through: Seq },
    /// Nothing was stored; it is retried after the idle time, or in an empty room after a wait
    /// that doubles with each failure (see [`final_delay`]).
    Failed {
        through: Seq,
        error: String,
        /// The room is empty and has used up its final attempts.
        gave_up: bool,
    },
    /// Nothing was stored because the document was saved from elsewhere. No checkpoint is
    /// tried again: the room is in conflict for the rest of its life.
    Conflict {
        through: Seq,
        /// The document's current revision, if it has one.
        current: Option<RevisionId>,
    },
}

struct Contributor {
    author: Author,
    /// The last log position this author changed.
    last: Seq,
}

/// Decides when a room writes a checkpoint and keeps what the next one needs: the revision it
/// is based on and who has contributed since the last one.
pub(crate) struct Schedule {
    timing: Timing,
    /// The revision last loaded or written.
    revision: Option<RevisionId>,
    /// The log position the stored revision corresponds to.
    saved_through: Seq,
    /// In first-seen order.
    contributors: Vec<Contributor>,
    first_unsaved: Option<Instant>,
    last_change: Option<Instant>,
    /// The log position the running checkpoint covers.
    running: Option<Seq>,
    /// The first change that arrived while the running checkpoint was being written.
    first_during_run: Option<Instant>,
    /// No checkpoint starts before this, after a failure.
    retry_at: Option<Instant>,
    /// Failed attempts since the room became empty.
    final_failures: u32,
    gave_up: bool,
    /// A checkpoint was refused as stale; nothing is saved any more.
    conflict: bool,
    /// The document fields every checkpoint stores: those of the revision the room loaded.
    fields: Map<String, Value>,
}

impl Schedule {
    /// A schedule for a room whose log starts at `head`, holding `revision`.
    pub(crate) fn new(revision: Option<RevisionId>, head: Seq, timing: Timing) -> Self {
        Self {
            timing,
            revision,
            saved_through: head,
            contributors: Vec::new(),
            first_unsaved: None,
            last_change: None,
            running: None,
            first_during_run: None,
            retry_at: None,
            final_failures: 0,
            gave_up: false,
            conflict: false,
            fields: Map::new(),
        }
    }

    /// The same schedule, storing `fields` with every checkpoint instead of none.
    pub(crate) fn with_fields(mut self, fields: Map<String, Value>) -> Self {
        self.fields = fields;
        self
    }

    /// Whether a checkpoint was refused because the document was saved from elsewhere.
    pub(crate) fn in_conflict(&self) -> bool {
        self.conflict
    }

    /// The revision last loaded or written.
    pub(crate) fn revision(&self) -> Option<&RevisionId> {
        self.revision.as_ref()
    }

    /// The log position the stored revision covers.
    pub(crate) fn saved_through(&self) -> Seq {
        self.saved_through
    }

    fn unsaved(&self, head: Seq) -> bool {
        head > self.saved_through
    }

    /// Records a committed change at `seq` by `author`.
    pub(crate) fn changed(&mut self, author: Author, seq: Seq) {
        let now = Instant::now();
        self.last_change = Some(now);
        self.first_unsaved.get_or_insert(now);
        if self.running.is_some() {
            self.first_during_run.get_or_insert(now);
        }
        match self
            .contributors
            .iter_mut()
            .find(|c| c.author.id == author.id)
        {
            Some(c) => {
                c.author.name = author.name;
                c.last = seq;
            }
            None => self.contributors.push(Contributor { author, last: seq }),
        }
    }

    /// Someone joined: an empty room that was giving up on saving tries again.
    pub(crate) fn joined(&mut self) {
        self.final_failures = 0;
        self.gave_up = false;
    }

    /// When the next checkpoint should start, if one is needed and none is running. An empty
    /// room saves straight away.
    pub(crate) fn due(&self, head: Seq, empty: bool) -> Option<Instant> {
        if self.running.is_some() || self.gave_up || self.conflict || !self.unsaved(head) {
            return None;
        }
        let now = Instant::now();
        let due = if empty {
            now
        } else {
            let last = self.last_change.unwrap_or(now);
            let first = self.first_unsaved.unwrap_or(now);
            let idle = last.checked_add(self.timing.idle).unwrap_or(last);
            let max = first.checked_add(self.timing.max).unwrap_or(first);
            idle.min(max)
        };
        Some(self.retry_at.map_or(due, |retry| due.max(retry)))
    }

    /// Starts a checkpoint of `text`, the document at `head`, unless one is running, nothing
    /// is unsaved or the room is in conflict.
    pub(crate) fn start(&mut self, head: Seq, text: String) -> Option<SaveJob> {
        if self.running.is_some() || self.conflict || !self.unsaved(head) {
            return None;
        }
        self.running = Some(head);
        self.first_during_run = None;
        Some(SaveJob {
            text,
            base: self.revision.clone(),
            authors: self.contributors.iter().map(|c| c.author.clone()).collect(),
            fields: self.fields.clone(),
        })
    }

    /// Records how the running checkpoint ended; `head` and `empty` describe the room now.
    /// `None` if no checkpoint was running.
    pub(crate) fn finish(
        &mut self,
        result: Result<Outcome, WriteError>,
        head: Seq,
        empty: bool,
    ) -> Option<Finished> {
        let through = self.running.take()?;
        let first_during_run = self.first_during_run.take();
        match result {
            Ok(Outcome::Written(revision) | Outcome::Unchanged(revision)) => {
                self.revision = Some(revision.clone());
                self.saved_through = through;
                self.contributors.retain(|c| c.last > through);
                self.first_unsaved = if self.unsaved(head) {
                    first_during_run.or_else(|| Some(Instant::now()))
                } else {
                    None
                };
                self.retry_at = None;
                self.final_failures = 0;
                Some(Finished::Saved { revision, through })
            }
            Err(WriteError::Stale { current }) => {
                self.conflict = true;
                Some(Finished::Conflict { through, current })
            }
            Err(WriteError::Failed(error)) => {
                let delay = if empty {
                    self.final_failures = self.final_failures.saturating_add(1);
                    self.gave_up = self.final_failures >= FINAL_ATTEMPTS;
                    final_delay(self.timing.idle, self.final_failures)
                } else {
                    self.timing.idle
                };
                let now = Instant::now();
                self.retry_at = Some(now.checked_add(delay).unwrap_or(now));
                Some(Finished::Failed {
                    through,
                    error,
                    gave_up: self.gave_up,
                })
            }
        }
    }

    /// Whether an empty room may end: nothing is running and everything is saved, or it gave
    /// up trying, or it is in conflict.
    pub(crate) fn may_end(&self, head: Seq) -> bool {
        self.running.is_none() && (self.gave_up || self.conflict || !self.unsaved(head))
    }
}

/// The wait after an empty room's `failures`-th failed final checkpoint: `idle`, doubled for
/// each earlier failure, at most [`MAX_FINAL_DELAY`].
fn final_delay(idle: Duration, failures: u32) -> Duration {
    let factor = 1u32
        .checked_shl(failures.saturating_sub(1))
        .unwrap_or(u32::MAX);
    idle.checked_mul(factor)
        .unwrap_or(MAX_FINAL_DELAY)
        .min(MAX_FINAL_DELAY)
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

    fn timing() -> Timing {
        Timing {
            idle: Duration::from_millis(100),
            max: Duration::from_secs(30),
        }
    }

    fn author(id: &str) -> Author {
        Author {
            id: id.into(),
            name: id.into(),
        }
    }

    fn rev(n: u8) -> RevisionId {
        format!("{n:040x}").parse().unwrap()
    }

    #[test]
    fn nothing_is_due_until_something_changes() {
        let s = Schedule::new(None, Seq(0), timing());
        assert!(s.due(Seq(0), false).is_none());
        assert!(s.due(Seq(0), true).is_none());
        assert!(s.may_end(Seq(0)));
    }

    #[test]
    fn contributors_after_the_saved_point_are_kept() {
        let mut s = Schedule::new(None, Seq(0), timing());
        s.changed(author("a"), Seq(1));
        let job = s.start(Seq(1), "x".into()).unwrap();
        assert_eq!(job.authors, vec![author("a")]);
        assert!(s.start(Seq(1), "x".into()).is_none(), "one at a time");
        s.changed(author("b"), Seq(2));
        let done = s.finish(Ok(Outcome::Written(rev(1))), Seq(2), false);
        assert!(matches!(
            done,
            Some(Finished::Saved {
                through: Seq(1),
                ..
            })
        ));
        assert_eq!(s.revision(), Some(&rev(1)));
        assert_eq!(s.saved_through(), Seq(1));
        assert!(s.due(Seq(2), false).is_some());
        let job = s.start(Seq(2), "xy".into()).unwrap();
        assert_eq!(job.authors, vec![author("b")]);
        assert_eq!(job.base, Some(rev(1)));
    }

    #[test]
    fn one_author_is_credited_once_under_their_latest_name() {
        let mut s = Schedule::new(None, Seq(0), timing());
        s.changed(author("a"), Seq(1));
        s.changed(author("b"), Seq(2));
        s.changed(
            Author {
                id: "a".into(),
                name: "A2".into(),
            },
            Seq(3),
        );
        let job = s.start(Seq(3), "x".into()).unwrap();
        assert_eq!(
            job.authors,
            vec![
                Author {
                    id: "a".into(),
                    name: "A2".into()
                },
                author("b")
            ]
        );
    }

    #[test]
    fn empty_room_gives_up_after_its_final_attempts() {
        let mut s = Schedule::new(None, Seq(0), timing());
        s.changed(author("a"), Seq(1));
        for attempt in 1..=FINAL_ATTEMPTS {
            assert!(!s.may_end(Seq(1)));
            s.start(Seq(1), "x".into()).unwrap();
            let before = Instant::now();
            let done = s
                .finish(Err(WriteError::Failed("no".into())), Seq(1), true)
                .unwrap();
            let Finished::Failed { gave_up, .. } = done else {
                panic!("expected failure");
            };
            assert_eq!(gave_up, attempt == FINAL_ATTEMPTS);
            if !gave_up {
                // The next attempt waits for the delay, which doubles with each failure.
                let due = s.due(Seq(1), true).unwrap();
                assert!(due >= before + final_delay(timing().idle, attempt));
            }
        }
        assert!(s.may_end(Seq(1)));
        assert!(s.due(Seq(1), true).is_none());
        s.joined();
        assert!(!s.may_end(Seq(1)));
        assert!(s.due(Seq(1), false).is_some());
    }

    #[test]
    fn final_delay_doubles_from_idle_up_to_thirty_seconds() {
        let idle = Duration::from_secs(2);
        let delays: Vec<u64> = (1..=FINAL_ATTEMPTS)
            .map(|n| final_delay(idle, n).as_secs())
            .collect();
        assert_eq!(delays, vec![2, 4, 8, 16, 30, 30, 30, 30, 30, 30]);
        let quick = Duration::from_millis(100);
        assert_eq!(final_delay(quick, 1), quick);
        assert_eq!(final_delay(quick, 3), Duration::from_millis(400));
        assert_eq!(final_delay(quick, 40), MAX_FINAL_DELAY);
    }

    #[test]
    fn every_checkpoint_keeps_the_loaded_fields() {
        let mut fields = Map::new();
        fields.insert("status".into(), Value::String("draft".into()));
        let mut s = Schedule::new(Some(rev(1)), Seq(0), timing()).with_fields(fields.clone());
        s.changed(author("a"), Seq(1));
        assert_eq!(s.start(Seq(1), "x".into()).unwrap().fields, fields);
        s.finish(Ok(Outcome::Written(rev(2))), Seq(2), false);
        s.changed(author("a"), Seq(2));
        assert_eq!(s.start(Seq(2), "xy".into()).unwrap().fields, fields);
    }

    #[test]
    fn stale_checkpoint_stops_saving_for_good() {
        let mut s = Schedule::new(Some(rev(1)), Seq(0), timing());
        s.changed(author("a"), Seq(1));
        s.start(Seq(1), "x".into()).unwrap();
        let done = s.finish(Err(WriteError::Stale { current: None }), Seq(1), false);
        assert!(matches!(
            done,
            Some(Finished::Conflict {
                through: Seq(1),
                ..
            })
        ));
        assert!(s.in_conflict());
        assert_eq!(s.revision(), Some(&rev(1)), "nothing was saved");
        s.changed(author("a"), Seq(2));
        s.joined();
        assert!(s.in_conflict(), "joining does not end a conflict");
        assert!(s.due(Seq(2), false).is_none());
        assert!(s.due(Seq(2), true).is_none());
        assert!(s.start(Seq(2), "xy".into()).is_none());
        assert!(s.may_end(Seq(2)), "unsaved changes do not keep it open");
    }

    #[test]
    fn failure_delays_the_next_attempt() {
        let mut s = Schedule::new(None, Seq(0), timing());
        s.changed(author("a"), Seq(1));
        s.start(Seq(1), "x".into()).unwrap();
        let before = Instant::now();
        s.finish(Err(WriteError::Failed("no".into())), Seq(1), true);
        let due = s.due(Seq(1), true).unwrap();
        assert!(due >= before + timing().idle);
    }

    /// Typing that never pauses keeps pushing the idle deadline back, but the checkpoint is
    /// still due `max` after the first unsaved change.
    #[tokio::test(start_paused = true)]
    async fn checkpoint_is_due_at_max_while_typing_continues() {
        let t = Timing {
            idle: Duration::from_millis(100),
            max: Duration::from_secs(1),
        };
        let mut s = Schedule::new(None, Seq(0), t);
        let first = Instant::now();
        s.changed(author("a"), Seq(1));
        let mut seq = 1;
        // A change every 50 ms, never leaving the 100 ms idle gap.
        while Instant::now() < first + Duration::from_millis(950) {
            tokio::time::advance(Duration::from_millis(50)).await;
            seq += 1;
            s.changed(author("a"), Seq(seq));
            let due = s.due(Seq(seq), false).unwrap();
            assert!(due <= first + t.max, "due after max");
            // While the idle gap ends before `max` it is the one in force.
            assert_eq!(due, (Instant::now() + t.idle).min(first + t.max));
        }
        // The idle deadline is now past `max`: the checkpoint is due exactly at max.
        assert!(Instant::now() + t.idle > first + t.max);
        assert_eq!(s.due(Seq(seq), false), Some(first + t.max));
        tokio::time::advance(first + t.max - Instant::now()).await;
        assert!(s.due(Seq(seq), false).unwrap() <= Instant::now());
    }
}
