//! A small stand-in document for testing live sessions without a real editor: a list of text
//! lines, each with a lasting id.
//!
//! Its changes follow the conflict principles of C8 in miniature: a change that names a line
//! that is gone, reuses an id, carries a text that is too long or does not parse does nothing,
//! with a reason, on every device alike.

use serde::{Deserialize, Serialize};

use crate::client::{Crash, Effect, EntryView, LiveEditor, Outcome};
use crate::ids::{LiveId, Lookup, OrderedIds};

/// The longest text a line may hold, in characters.
const MAX_TEXT: usize = 1_000;

/// A list of text lines with lasting ids, on one named device.
#[derive(Clone, Debug, PartialEq)]
pub struct StandIn {
    /// The device's name, which `CrashIf` matches.
    device: String,
    /// The lines in order, each with its id.
    lines: Vec<(LiveId, String)>,
    /// The same ids in order, and the deleted ones.
    ids: OrderedIds,
}

/// One change to a [`StandIn`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum StandInOp {
    /// Inserts a line before `before`, or at the end when it is `None`.
    Insert {
        before: Option<LiveId>,
        id: LiveId,
        text: String,
    },
    /// Deletes a line.
    Delete { id: LiveId },
    /// Replaces a line's text.
    Set { id: LiveId, text: String },
    /// Sets the line to `"{at_ms}/{seed}"` from the server stamp, or `"local"` while waiting.
    Stamp { id: LiveId },
    /// Crashes on the device with this name; does nothing elsewhere.
    CrashIf { device: String },
}

impl StandIn {
    /// An empty document on `device`: the base in G2.
    pub fn new(device: &str) -> StandIn {
        StandIn {
            device: device.to_owned(),
            lines: Vec::new(),
            ids: OrderedIds::base(0),
        }
    }

    /// The lines' texts, in order.
    pub fn lines(&self) -> Vec<String> {
        self.lines.iter().map(|(_, text)| text.clone()).collect()
    }

    /// The payload text for `op`.
    pub fn op(op: &StandInOp) -> String {
        // Serialising this enum cannot fail: every field is a string, a number or an option.
        serde_json::to_string(op).unwrap_or_default()
    }

    /// Where the live line `id` is, or why there is none.
    fn position(&self, id: LiveId) -> Result<usize, String> {
        match self.ids.find(id) {
            Lookup::At(pos) => usize::try_from(pos).map_err(|_| format!("line {id:?} is too far")),
            Lookup::Deleted { .. } => Err(format!("line {id:?} was deleted")),
            Lookup::Unknown => Err(format!("line {id:?} does not exist")),
        }
    }

    fn insert(&mut self, before: Option<LiveId>, id: LiveId, text: &str) -> Result<(), String> {
        check_text(text)?;
        let at = match before {
            Some(before) => self.position(before)?,
            None => self.lines.len(),
        };
        let at32 = u32::try_from(at).map_err(|_| "the document is full".to_owned())?;
        self.ids
            .insert(at32, id, 1)
            .map_err(|error| format!("cannot insert {id:?}: {error}"))?;
        self.lines.insert(at, (id, text.to_owned()));
        Ok(())
    }

    fn delete(&mut self, id: LiveId) -> Result<(), String> {
        let at = self.position(id)?;
        let at32 = u32::try_from(at).map_err(|_| format!("line {id:?} is too far"))?;
        self.ids
            .delete(at32, 1)
            .map_err(|error| format!("cannot delete {id:?}: {error}"))?;
        self.lines.remove(at);
        Ok(())
    }

    fn set(&mut self, id: LiveId, text: &str) -> Result<(), String> {
        check_text(text)?;
        let at = self.position(id)?;
        let line = self
            .lines
            .get_mut(at)
            .ok_or_else(|| format!("line {id:?} is missing"))?;
        line.1 = text.to_owned();
        Ok(())
    }
}

/// Refuses a text over [`MAX_TEXT`] characters.
fn check_text(text: &str) -> Result<(), String> {
    let chars = text.chars().count();
    if chars > MAX_TEXT {
        Err(format!("the text has {chars} characters, over {MAX_TEXT}"))
    } else {
        Ok(())
    }
}

impl LiveEditor for StandIn {
    type Snapshot = StandIn;

    fn snapshot(&self) -> StandIn {
        self.clone()
    }

    fn restore(&mut self, snapshot: &StandIn) {
        self.clone_from(snapshot);
    }

    fn apply(&mut self, entry: &EntryView<'_>) -> Result<Outcome, Crash> {
        let op = match serde_json::from_str::<StandInOp>(entry.payload) {
            Ok(op) => op,
            Err(error) => {
                return Ok(Outcome {
                    effect: Effect::NoEffect {
                        reason: format!("not a stand-in change: {error}"),
                    },
                    read_stamp: false,
                });
            }
        };
        let read_stamp = matches!(op, StandInOp::Stamp { .. });
        let result = match op {
            StandInOp::Insert { before, id, text } => self.insert(before, id, &text),
            StandInOp::Delete { id } => self.delete(id),
            StandInOp::Set { id, text } => self.set(id, &text),
            StandInOp::Stamp { id } => {
                let text = match entry.stamp {
                    Some(stamp) => format!("{}/{}", stamp.at_ms, stamp.seed),
                    None => "local".to_owned(),
                };
                self.set(id, &text)
            }
            StandInOp::CrashIf { device } => {
                if device == self.device {
                    return Err(Crash);
                }
                Ok(())
            }
        };
        let effect = match result {
            Ok(()) => Effect::Applied,
            Err(reason) => Effect::NoEffect { reason },
        };
        Ok(Outcome { effect, read_stamp })
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
    use crate::client::Stamp;
    use collab_core::ids::Seq;
    use protocol::live::ParticipantNo;

    fn entry<'a>(payload: &'a str, stamp: Option<Stamp>) -> EntryView<'a> {
        EntryView {
            seq: stamp.map(|_| Seq(1)),
            participant: ParticipantNo(1),
            own: false,
            stamp,
            payload,
        }
    }

    fn run(doc: &mut StandIn, op: &StandInOp) -> Result<Outcome, Crash> {
        let payload = StandIn::op(op);
        doc.apply(&entry(&payload, None))
    }

    fn applied(doc: &mut StandIn, op: &StandInOp) {
        let outcome = run(doc, op).unwrap();
        assert_eq!(outcome.effect, Effect::Applied, "{op:?}");
    }

    fn no_effect(outcome: Result<Outcome, Crash>) {
        match outcome.unwrap().effect {
            Effect::NoEffect { reason } => assert!(!reason.is_empty()),
            Effect::Applied => panic!("expected no effect"),
        }
    }

    fn insert_end(id: LiveId, text: &str) -> StandInOp {
        StandInOp::Insert {
            before: None,
            id,
            text: text.to_owned(),
        }
    }

    const A: LiveId = LiveId(1, 0);
    const B: LiveId = LiveId(1, 1);
    const C: LiveId = LiveId(2, 0);

    #[test]
    fn insert_set_delete() {
        let mut doc = StandIn::new("a");
        applied(&mut doc, &insert_end(A, "one"));
        applied(&mut doc, &insert_end(B, "two"));
        applied(&mut doc, &insert_end(C, "three"));
        applied(
            &mut doc,
            &StandInOp::Set {
                id: B,
                text: "TWO".to_owned(),
            },
        );
        applied(&mut doc, &StandInOp::Delete { id: A });
        assert_eq!(doc.lines(), vec!["TWO", "three"]);
    }

    #[test]
    fn insert_before_names_a_line() {
        let mut doc = StandIn::new("a");
        applied(&mut doc, &insert_end(A, "one"));
        applied(&mut doc, &insert_end(B, "two"));
        applied(
            &mut doc,
            &StandInOp::Insert {
                before: Some(A),
                id: C,
                text: "zero".to_owned(),
            },
        );
        assert_eq!(doc.lines(), vec!["zero", "one", "two"]);
    }

    #[test]
    fn edits_to_a_deleted_line_do_nothing() {
        let mut doc = StandIn::new("a");
        applied(&mut doc, &insert_end(A, "one"));
        applied(&mut doc, &insert_end(B, "two"));
        applied(&mut doc, &StandInOp::Delete { id: A });
        let before = doc.lines();
        no_effect(run(
            &mut doc,
            &StandInOp::Set {
                id: A,
                text: "x".to_owned(),
            },
        ));
        no_effect(run(&mut doc, &StandInOp::Delete { id: A }));
        no_effect(run(
            &mut doc,
            &StandInOp::Insert {
                before: Some(A),
                id: C,
                text: "x".to_owned(),
            },
        ));
        // Reusing a deleted id, or a live one, does nothing too.
        no_effect(run(&mut doc, &insert_end(A, "again")));
        no_effect(run(&mut doc, &insert_end(B, "again")));
        // So does naming a line that never was.
        no_effect(run(&mut doc, &StandInOp::Delete { id: LiveId(9, 9) }));
        assert_eq!(doc.lines(), before);
    }

    #[test]
    fn malformed_payload_has_no_effect() {
        let mut doc = StandIn::new("a");
        applied(&mut doc, &insert_end(A, "one"));
        let before = doc.clone();
        let long = "x".repeat(1_001);
        let payloads = [
            "not json".to_owned(),
            "{\"op\":\"unknown\"}".to_owned(),
            "{\"type\":\"joined\"}".to_owned(),
            StandIn::op(&insert_end(B, &long)),
            StandIn::op(&StandInOp::Set { id: A, text: long }),
        ];
        for payload in &payloads {
            let outcome = doc.apply(&entry(payload, None)).unwrap();
            assert!(!outcome.read_stamp, "{payload}");
            no_effect(Ok(outcome));
            assert_eq!(doc, before, "{payload}");
        }
        // Exactly 1,000 characters is allowed.
        applied(&mut doc, &insert_end(B, &"y".repeat(1_000)));
    }

    #[test]
    fn stamp_uses_the_server_stamp() {
        let mut doc = StandIn::new("a");
        applied(&mut doc, &insert_end(A, "one"));
        let payload = StandIn::op(&StandInOp::Stamp { id: A });
        let stamped = doc
            .apply(&entry(
                &payload,
                Some(Stamp {
                    at_ms: 1000,
                    seed: 7,
                }),
            ))
            .unwrap();
        assert_eq!(stamped.effect, Effect::Applied);
        assert!(stamped.read_stamp);
        assert_eq!(doc.lines(), vec!["1000/7"]);

        let local = doc.apply(&entry(&payload, None)).unwrap();
        assert_eq!(local.effect, Effect::Applied);
        assert!(local.read_stamp);
        assert_eq!(doc.lines(), vec!["local"]);

        // No other change reads the stamp.
        let set = run(
            &mut doc,
            &StandInOp::Set {
                id: A,
                text: "x".to_owned(),
            },
        )
        .unwrap();
        assert!(!set.read_stamp);
    }

    #[test]
    fn crash_only_on_the_named_device() {
        let op = StandInOp::CrashIf {
            device: "b".to_owned(),
        };
        let mut b = StandIn::new("b");
        assert_eq!(run(&mut b, &op), Err(Crash));

        let mut a = StandIn::new("a");
        applied(&mut a, &insert_end(A, "one"));
        let before = a.clone();
        let outcome = run(&mut a, &op).unwrap();
        assert_eq!(outcome.effect, Effect::Applied);
        assert!(!outcome.read_stamp);
        assert_eq!(a, before);
    }

    #[test]
    fn restore_goes_back() {
        let mut doc = StandIn::new("a");
        applied(&mut doc, &insert_end(A, "one"));
        let snapshot = doc.snapshot();
        applied(&mut doc, &insert_end(B, "two"));
        applied(&mut doc, &StandInOp::Delete { id: A });
        assert_ne!(doc.lines(), snapshot.lines());
        doc.restore(&snapshot);
        assert_eq!(doc.lines(), snapshot.lines());
        assert_eq!(doc, snapshot);
        // The restored document carries on from the snapshot: B is free again, A is live.
        applied(&mut doc, &insert_end(B, "two again"));
        applied(&mut doc, &StandInOp::Delete { id: A });
        assert_eq!(doc.lines(), vec!["two again"]);
    }

    #[test]
    fn payload_is_tagged_json() {
        let payload = StandIn::op(&StandInOp::Delete { id: LiveId(3, 4) });
        assert_eq!(payload, r#"{"op":"delete","id":[3,4]}"#);
        let crash = StandIn::op(&StandInOp::CrashIf {
            device: "b".to_owned(),
        });
        assert_eq!(crash, r#"{"op":"crash_if","device":"b"}"#);
    }
}
