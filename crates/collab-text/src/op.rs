//! Text operations and the document they apply to.

use collab_core::document::Document;
use serde::{Deserialize, Serialize};

use crate::transform;

/// A plain-text document.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TextDoc(String);

impl TextDoc {
    /// A document holding `s`.
    #[must_use]
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// The text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The length in Unicode scalar values.
    #[must_use]
    pub fn len_chars(&self) -> usize {
        self.0.chars().count()
    }
}

/// One step of a [`TextOp`]. Counts are Unicode scalar values.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Component {
    /// Keep this many characters.
    Retain(usize),
    /// Insert this text.
    Insert(String),
    /// Remove this many characters.
    Delete(usize),
}

/// A change to a whole document: a normalized list of [`Component`]s.
///
/// Normalized means no empty components, adjacent components of one kind merged, and an insert
/// next to a delete placed first.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "Vec<Component>", into = "Vec<Component>")]
pub struct TextOp(Vec<Component>);

impl From<Vec<Component>> for TextOp {
    fn from(components: Vec<Component>) -> Self {
        let mut op = TextOp(Vec::with_capacity(components.len()));
        for component in components {
            op.push(component);
        }
        op
    }
}

impl From<TextOp> for Vec<Component> {
    fn from(op: TextOp) -> Self {
        op.0
    }
}

/// Why a text change could not be built or applied.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TextError {
    /// The change was made for a document of a different length than the one it is applied to.
    #[error("operation expects {expected} characters but the document has {actual}")]
    LengthMismatch {
        /// Characters the change expects.
        expected: usize,
        /// Characters the document has.
        actual: usize,
    },
    /// Two changes that should start from the same document expect different lengths, so they
    /// cannot be transformed against each other.
    #[error(
        "the two operations expect different document lengths ({first} and {second} characters)"
    )]
    BaseLengthsDiffer {
        /// Characters the first change expects.
        first: usize,
        /// Characters the second change expects.
        second: usize,
    },
    /// A range reaches past the end of the document.
    #[error("range is outside the document")]
    OutOfRange,
}

impl TextOp {
    /// Starts building an operation.
    #[must_use]
    pub fn builder() -> TextOpBuilder {
        TextOpBuilder(TextOp(Vec::new()))
    }

    /// Replaces `remove` characters at `at` with `insert`, keeping the rest of a document of
    /// `doc_len` characters.
    ///
    /// # Errors
    /// `OutOfRange` if `at + remove` is beyond `doc_len`.
    pub fn replace(
        doc_len: usize,
        at: usize,
        remove: usize,
        insert: &str,
    ) -> Result<TextOp, TextError> {
        let end = at.checked_add(remove).ok_or(TextError::OutOfRange)?;
        let tail = doc_len.checked_sub(end).ok_or(TextError::OutOfRange)?;
        Ok(TextOp::builder()
            .retain(at)
            .delete(remove)
            .insert(insert)
            .retain(tail)
            .build())
    }

    /// Characters the operation expects the document to have: retains plus deletes.
    #[must_use]
    pub fn base_len(&self) -> usize {
        self.0.iter().fold(0usize, |acc, c| match c {
            Component::Retain(n) | Component::Delete(n) => acc.saturating_add(*n),
            Component::Insert(_) => acc,
        })
    }

    /// Characters the document has afterwards: retains plus inserted characters.
    #[must_use]
    pub fn target_len(&self) -> usize {
        self.0.iter().fold(0usize, |acc, c| match c {
            Component::Retain(n) => acc.saturating_add(*n),
            Component::Insert(s) => acc.saturating_add(s.chars().count()),
            Component::Delete(_) => acc,
        })
    }

    /// The normalized components.
    #[must_use]
    pub fn components(&self) -> &[Component] {
        &self.0
    }

    /// Appends a component, keeping the list normalized.
    fn push(&mut self, component: Component) {
        match component {
            Component::Retain(0) | Component::Delete(0) => {}
            Component::Insert(s) if s.is_empty() => {}
            Component::Retain(n) => {
                if let Some(Component::Retain(m)) = self.0.last_mut() {
                    *m = m.saturating_add(n);
                } else {
                    self.0.push(Component::Retain(n));
                }
            }
            Component::Delete(n) => {
                if let Some(Component::Delete(m)) = self.0.last_mut() {
                    *m = m.saturating_add(n);
                } else {
                    self.0.push(Component::Delete(n));
                }
            }
            Component::Insert(s) => self.push_insert(s),
        }
    }

    fn push_insert(&mut self, s: String) {
        let len = self.0.len();
        if matches!(self.0.last(), Some(Component::Delete(_))) {
            // An insert goes before the delete it follows, joining an insert already there.
            let before = len.checked_sub(2);
            if let Some(Component::Insert(t)) = before.and_then(|i| self.0.get_mut(i)) {
                t.push_str(&s);
            } else {
                self.0.insert(len.saturating_sub(1), Component::Insert(s));
            }
        } else if let Some(Component::Insert(t)) = self.0.last_mut() {
            t.push_str(&s);
        } else {
            self.0.push(Component::Insert(s));
        }
    }

    /// Applies the operation to `text`.
    fn apply_to(&self, text: &str) -> Result<String, TextError> {
        let actual = text.chars().count();
        let expected = self.base_len();
        if expected != actual {
            return Err(TextError::LengthMismatch { expected, actual });
        }
        let mut chars = text.chars();
        let mut out = String::with_capacity(text.len());
        for component in &self.0 {
            match component {
                Component::Retain(n) => {
                    let taken = chars.by_ref().take(*n).inspect(|c| out.push(*c)).count();
                    if taken < *n {
                        return Err(TextError::OutOfRange);
                    }
                }
                Component::Insert(s) => out.push_str(s),
                Component::Delete(n) => {
                    if chars.by_ref().take(*n).count() < *n {
                        return Err(TextError::OutOfRange);
                    }
                }
            }
        }
        Ok(out)
    }
}

/// Builds a [`TextOp`], normalizing as components are added.
#[derive(Clone, Debug, Default)]
pub struct TextOpBuilder(TextOp);

impl TextOpBuilder {
    /// Keeps `n` characters.
    #[must_use]
    pub fn retain(mut self, n: usize) -> Self {
        self.0.push(Component::Retain(n));
        self
    }

    /// Inserts `s`.
    #[must_use]
    pub fn insert(mut self, s: impl Into<String>) -> Self {
        self.0.push(Component::Insert(s.into()));
        self
    }

    /// Removes `n` characters.
    #[must_use]
    pub fn delete(mut self, n: usize) -> Self {
        self.0.push(Component::Delete(n));
        self
    }

    /// Finishes the operation.
    #[must_use]
    pub fn build(self) -> TextOp {
        self.0
    }
}

impl Document for TextDoc {
    type Op = TextOp;
    type Error = TextError;

    fn apply(&mut self, op: &TextOp) -> Result<(), TextError> {
        self.0 = op.apply_to(&self.0)?;
        Ok(())
    }

    fn transform(first: &TextOp, second: &TextOp) -> Result<(TextOp, TextOp), TextError> {
        transform::transform(first, second)
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
    use collab_core::document::Document;

    #[test]
    fn apply_inserts_and_deletes() {
        let mut doc = TextDoc::new("hello world");
        let op = TextOp::builder()
            .retain(6)
            .delete(5)
            .insert("there")
            .build();
        doc.apply(&op).unwrap();
        assert_eq!(doc.as_str(), "hello there");
    }

    #[test]
    fn apply_rejects_length_mismatch() {
        let mut doc = TextDoc::new("hello");
        let op = TextOp::builder().retain(3).build();
        let err = doc.apply(&op).unwrap_err();
        assert_eq!(
            err,
            TextError::LengthMismatch {
                expected: 3,
                actual: 5
            }
        );
        assert_eq!(doc.as_str(), "hello");
        assert_eq!(
            err.to_string(),
            "operation expects 3 characters but the document has 5"
        );
    }

    #[test]
    fn replace_rejects_out_of_range() {
        assert_eq!(TextOp::replace(5, 4, 3, ""), Err(TextError::OutOfRange));
        assert_eq!(
            TextOp::replace(5, usize::MAX, 2, ""),
            Err(TextError::OutOfRange)
        );
    }

    #[test]
    fn builder_normalizes() {
        let op = TextOp::builder()
            .retain(0)
            .insert("a")
            .insert("b")
            .retain(2)
            .build();
        assert_eq!(
            op.components(),
            &[Component::Insert("ab".into()), Component::Retain(2)]
        );
    }

    #[test]
    fn builder_puts_insert_before_adjacent_delete() {
        let op = TextOp::builder()
            .delete(2)
            .insert("x")
            .delete(1)
            .insert("y")
            .build();
        assert_eq!(
            op.components(),
            &[Component::Insert("xy".into()), Component::Delete(3)]
        );
    }

    #[test]
    fn counts_chars_not_bytes() {
        let mut doc = TextDoc::new("a😀b");
        assert_eq!(doc.len_chars(), 3);
        let op = TextOp::replace(3, 2, 0, "é").unwrap();
        doc.apply(&op).unwrap();
        assert_eq!(doc.as_str(), "a😀éb");
    }

    #[test]
    fn lengths() {
        let op = TextOp::builder().retain(2).delete(3).insert("é😀").build();
        assert_eq!(op.base_len(), 5);
        assert_eq!(op.target_len(), 4);
    }

    #[test]
    fn serde_shape_and_normalizing_deserialize() {
        let op = TextOp::builder().retain(3).insert("x").delete(2).build();
        let json = serde_json::to_string(&op).unwrap();
        assert_eq!(json, r#"[{"retain":3},{"insert":"x"},{"delete":2}]"#);
        let spelled: TextOp = serde_json::from_str(
            r#"[{"retain":1},{"retain":2},{"delete":2},{"insert":"x"},{"retain":0}]"#,
        )
        .unwrap();
        assert_eq!(spelled, op);
    }
}
