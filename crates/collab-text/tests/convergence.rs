#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use collab_core::document::{Document, Submitted};
use collab_core::ids::{ClientId, OpId, Seq};
use collab_core::server_log::ServerLog;
use collab_text::{TextDoc, TextOp};
use proptest::prelude::*;
use uuid::Uuid;

const MIXED: &[&str] = &["a", "b", "Z", "q", "é", "😀", "👩\u{200D}👧", "\u{0301}"];
const NON_ASCII: &[&str] = &["é", "😀", "👩\u{200D}👧", "\u{0301}"];

fn text(alphabet: &'static [&'static str], max: usize) -> impl Strategy<Value = String> {
    prop::collection::vec(prop::sample::select(alphabet), 0..=max).prop_map(|v| v.concat())
}

/// A valid op for a string of `len` chars: replace a random range, and sometimes also insert
/// a second piece later in the tail.
fn op_for(len: usize, alphabet: &'static [&'static str]) -> impl Strategy<Value = TextOp> {
    (0..=len, 0..=len, text(alphabet, 4), any::<bool>()).prop_map(move |(a, b, ins, two)| {
        let (at, end) = (a.min(b), a.max(b));
        if two && end < len {
            let tail = len - end;
            TextOp::builder()
                .retain(at)
                .delete(end - at)
                .insert(ins)
                .retain(tail / 2)
                .insert("é")
                .retain(tail - tail / 2)
                .build()
        } else {
            TextOp::replace(len, at, end - at, &ins).unwrap()
        }
    })
}

fn scenario(alphabet: &'static [&'static str]) -> impl Strategy<Value = (String, TextOp, TextOp)> {
    text(alphabet, 12).prop_flat_map(move |start| {
        let len = start.chars().count();
        (Just(start), op_for(len, alphabet), op_for(len, alphabet))
    })
}

/// Applies both orders and asserts they agree; returns the shared result.
fn converge(start: &str, a: &TextOp, b: &TextOp) -> (TextDoc, TextOp, TextOp) {
    let (a2, b2) = TextDoc::transform(a, b).unwrap();
    let mut left = TextDoc::new(start);
    left.apply(a).unwrap();
    left.apply(&b2).unwrap();
    let mut right = TextDoc::new(start);
    right.apply(b).unwrap();
    right.apply(&a2).unwrap();
    assert_eq!(left, right);
    (left, a2, b2)
}

proptest! {
    #[test]
    fn transform_converges((start, a, b) in scenario(MIXED)) {
        converge(&start, &a, &b);
    }

    #[test]
    fn converges_with_astral_and_combining_chars((start, a, b) in scenario(NON_ASCII)) {
        let (doc, a2, b2) = converge(&start, &a, &b);
        // The result is a valid string (iterating by char) whose length is what the ops promise.
        prop_assert_eq!(doc.as_str().chars().count(), a2.target_len());
        prop_assert_eq!(doc.as_str().chars().count(), b2.target_len());
        prop_assert_eq!(doc.len_chars(), a2.target_len());
    }

    #[test]
    fn three_clients_converge_through_server_log(
        (start, a, b) in scenario(MIXED),
        c_seed in any::<u8>(),
        order in prop::sample::select(vec![
            [0usize, 1, 2], [0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0],
        ]),
    ) {
        let len = start.chars().count();
        let at = usize::from(c_seed) % (len + 1);
        let c = TextOp::replace(len, at, 0, "C").unwrap();
        let ops = [a, b, c];

        let mut log = ServerLog::new(TextDoc::new(start.clone()), Seq(0));
        for (n, i) in order.iter().enumerate() {
            let out = log
                .submit(
                    ClientId(Uuid::from_u128((*i as u128) + 1)),
                    OpId(Uuid::from_u128((n as u128) + 100)),
                    Seq(0),
                    ops[*i].clone(),
                )
                .unwrap();
            prop_assert!(matches!(out, Submitted::Committed(_)));
        }

        let mut replay = TextDoc::new(start);
        for entry in log.since(Seq(0)).unwrap() {
            replay.apply(&entry.op).unwrap();
        }
        prop_assert_eq!(&replay, log.doc());
    }
}
