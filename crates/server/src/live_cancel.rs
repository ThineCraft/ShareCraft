//! The in-memory witness rule for entries that stop an editor.

use std::collections::{BTreeMap, BTreeSet};

use collab_core::ids::Seq;

#[derive(Default)]
pub(crate) struct CancelBook {
    reports: BTreeMap<Seq, Report>,
    gone: BTreeSet<u32>,
}

struct Report {
    witnesses: BTreeSet<u32>,
    reporters: BTreeSet<u32>,
    stands: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    Cancel { seq: Seq, reporters: Vec<u32> },
    Stands { seq: Seq, reporters: Vec<u32> },
}

impl CancelBook {
    pub(crate) fn report(
        &mut self,
        seq: Seq,
        witnesses: &[u32],
        reporter: u32,
        seen_of: &dyn Fn(u32) -> Seq,
    ) -> Option<Verdict> {
        let report = self.reports.entry(seq).or_insert_with(|| Report {
            witnesses: witnesses
                .iter()
                .copied()
                .filter(|witness| !self.gone.contains(witness))
                .collect(),
            reporters: BTreeSet::new(),
            stands: false,
        });
        if !report.reporters.insert(reporter) {
            return None;
        }
        if report.stands
            || report
                .witnesses
                .iter()
                .any(|witness| seen_of(*witness) >= seq)
        {
            report.stands = true;
            return Some(Verdict::Stands {
                seq,
                reporters: report.reporters.iter().copied().collect(),
            });
        }
        if report
            .witnesses
            .iter()
            .all(|witness| report.reporters.contains(witness))
        {
            let reporters = report.reporters.iter().copied().collect();
            self.reports.remove(&seq);
            return Some(Verdict::Cancel { seq, reporters });
        }
        None
    }

    pub(crate) fn seen(&mut self, participant: u32, seen: Seq) -> Vec<Verdict> {
        let mut verdicts = Vec::new();
        for (seq, report) in &mut self.reports {
            if !report.stands && report.witnesses.contains(&participant) && seen >= *seq {
                report.stands = true;
                verdicts.push(Verdict::Stands {
                    seq: *seq,
                    reporters: report.reporters.iter().copied().collect(),
                });
            }
        }
        verdicts
    }

    pub(crate) fn gone(&mut self, participant: u32) -> Vec<Verdict> {
        self.gone.insert(participant);
        let mut cancelled = Vec::new();
        self.reports.retain(|seq, report| {
            report.witnesses.remove(&participant);
            let cancel = !report.stands
                && !report.reporters.is_empty()
                && report
                    .witnesses
                    .iter()
                    .all(|witness| report.reporters.contains(witness));
            if cancel {
                cancelled.push(Verdict::Cancel {
                    seq: *seq,
                    reporters: report.reporters.iter().copied().collect(),
                });
            }
            !cancel
        });
        cancelled
    }

    pub(crate) fn is_standing(&self, seq: Seq) -> bool {
        self.reports.get(&seq).is_some_and(|report| report.stands)
    }
}

#[cfg(test)]
mod tests {
    use collab_core::ids::Seq;

    use super::{CancelBook, Verdict};

    #[test]
    fn all_witnesses_report_cancels() {
        let mut book = CancelBook::default();
        let seen = |_| Seq(0);

        assert_eq!(book.report(Seq(7), &[1, 2], 1, &seen), None);
        assert_eq!(
            book.report(Seq(7), &[1, 2], 2, &seen),
            Some(Verdict::Cancel {
                seq: Seq(7),
                reporters: vec![1, 2],
            })
        );
    }

    #[test]
    fn one_witness_moved_past_stands() {
        let mut book = CancelBook::default();
        let seen = |participant| {
            if participant == 2 { Seq(7) } else { Seq(0) }
        };

        assert_eq!(
            book.report(Seq(7), &[1, 2], 1, &seen),
            Some(Verdict::Stands {
                seq: Seq(7),
                reporters: vec![1],
            })
        );
    }

    #[test]
    fn expired_witness_stops_counting() {
        let mut book = CancelBook::default();
        let seen = |_| Seq(0);

        assert_eq!(book.report(Seq(7), &[1, 2], 1, &seen), None);
        assert_eq!(
            book.gone(2),
            vec![Verdict::Cancel {
                seq: Seq(7),
                reporters: vec![1],
            }]
        );
    }

    #[test]
    fn one_person_cannot_cancel_alone() {
        let mut book = CancelBook::default();
        let seen = |_| Seq(0);

        assert_eq!(book.report(Seq(7), &[1, 2], 1, &seen), None);
    }

    #[test]
    fn reports_survive_the_reporter_leaving() {
        let mut book = CancelBook::default();
        let seen = |_| Seq(0);

        assert_eq!(book.report(Seq(7), &[1, 2], 1, &seen), None);
        assert!(book.gone(1).is_empty());
        assert_eq!(
            book.report(Seq(7), &[1, 2], 2, &seen),
            Some(Verdict::Cancel {
                seq: Seq(7),
                reporters: vec![1, 2],
            })
        );
    }

    #[test]
    fn late_joiner_report() {
        let mut book = CancelBook::default();
        let seen = |_| Seq(0);

        assert!(book.gone(1).is_empty());
        assert_eq!(
            book.report(Seq(7), &[1], 3, &seen),
            Some(Verdict::Cancel {
                seq: Seq(7),
                reporters: vec![3],
            })
        );
    }
}
