//! Operational transform for text operations.

use crate::op::{Component, TextError, TextOp, TextOpBuilder};

/// Characters a retain or delete covers; zero for anything else.
fn span(c: &Component) -> usize {
    match c {
        Component::Retain(n) | Component::Delete(n) => *n,
        Component::Insert(_) => 0,
    }
}

/// What is left of a retain or delete after `used` characters were consumed.
fn remainder(c: Component, used: usize) -> Option<Component> {
    match c {
        Component::Retain(n) => n
            .checked_sub(used)
            .filter(|r| *r > 0)
            .map(Component::Retain),
        Component::Delete(n) => n
            .checked_sub(used)
            .filter(|r| *r > 0)
            .map(Component::Delete),
        Component::Insert(_) => None,
    }
}

/// Transforms two operations made against the same document. See
/// [`collab_core::document::Document::transform`]; `first` wins ties between inserts.
pub(crate) fn transform(first: &TextOp, second: &TextOp) -> Result<(TextOp, TextOp), TextError> {
    if first.base_len() != second.base_len() {
        return Err(TextError::BaseLengthsDiffer {
            first: first.base_len(),
            second: second.base_len(),
        });
    }
    let mut a_iter = first.components().iter().cloned();
    let mut b_iter = second.components().iter().cloned();
    let mut a_out = TextOpBuilder::default();
    let mut b_out = TextOpBuilder::default();
    let mut a_cur = a_iter.next();
    let mut b_cur = b_iter.next();

    loop {
        match (a_cur.take(), b_cur.take()) {
            (None, None) => break,
            (Some(Component::Insert(s)), b_rest) => {
                // first's insert comes before anything second inserts here.
                b_out = b_out.retain(s.chars().count());
                a_out = a_out.insert(s);
                a_cur = a_iter.next();
                b_cur = b_rest;
            }
            (a_rest, Some(Component::Insert(s))) => {
                a_out = a_out.retain(s.chars().count());
                b_out = b_out.insert(s);
                a_cur = a_rest;
                b_cur = b_iter.next();
            }
            (Some(a), Some(b)) => {
                let n = span(&a).min(span(&b));
                match (&a, &b) {
                    (Component::Retain(_), Component::Retain(_)) => {
                        a_out = a_out.retain(n);
                        b_out = b_out.retain(n);
                    }
                    (Component::Retain(_), Component::Delete(_)) => b_out = b_out.delete(n),
                    (Component::Delete(_), Component::Retain(_)) => a_out = a_out.delete(n),
                    // Both delete the same characters: nothing left for either to do.
                    (Component::Delete(_), Component::Delete(_)) => {}
                    _ => return Err(TextError::OutOfRange),
                }
                a_cur = remainder(a, n).or_else(|| a_iter.next());
                b_cur = remainder(b, n).or_else(|| b_iter.next());
            }
            // One side ran out while the other still has characters to cover.
            _ => return Err(TextError::OutOfRange),
        }
    }
    Ok((a_out.build(), b_out.build()))
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use crate::{TextDoc, TextError, TextOp};
    use collab_core::document::Document;

    fn both_orders(start: &str, a: &TextOp, b: &TextOp) -> (String, String) {
        let (a2, b2) = TextDoc::transform(a, b).unwrap();
        let mut left = TextDoc::new(start);
        left.apply(a).unwrap();
        left.apply(&b2).unwrap();
        let mut right = TextDoc::new(start);
        right.apply(b).unwrap();
        right.apply(&a2).unwrap();
        (left.as_str().to_owned(), right.as_str().to_owned())
    }

    #[test]
    fn same_position_inserts_first_wins_place() {
        let first = TextOp::replace(2, 1, 0, "X").unwrap();
        let second = TextOp::replace(2, 1, 0, "Y").unwrap();
        assert_eq!(
            both_orders("ab", &first, &second),
            ("aXYb".into(), "aXYb".into())
        );
    }

    #[test]
    fn overlapping_deletes_remove_each_char_once() {
        let first = TextOp::replace(6, 1, 3, "").unwrap();
        let second = TextOp::replace(6, 2, 3, "").unwrap();
        assert_eq!(
            both_orders("abcdef", &first, &second),
            ("af".into(), "af".into())
        );
    }

    #[test]
    fn insert_inside_deleted_range_survives() {
        let del = TextOp::replace(6, 1, 4, "").unwrap();
        let ins = TextOp::replace(6, 3, 0, "Z").unwrap();
        assert_eq!(
            both_orders("abcdef", &del, &ins),
            ("aZf".into(), "aZf".into())
        );
        assert_eq!(
            both_orders("abcdef", &ins, &del),
            ("aZf".into(), "aZf".into())
        );
    }

    #[test]
    fn transform_rejects_different_base_len() {
        let a = TextOp::builder().retain(3).build();
        let b = TextOp::builder().retain(4).build();
        let err = TextDoc::transform(&a, &b).unwrap_err();
        assert_eq!(
            err,
            TextError::BaseLengthsDiffer {
                first: 3,
                second: 4
            }
        );
        // It does not claim to know the document's length.
        assert_eq!(
            err.to_string(),
            "the two operations expect different document lengths (3 and 4 characters)"
        );
    }
}
