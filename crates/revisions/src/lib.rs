#![forbid(unsafe_code)]
//! The revision store: the only code that writes to Git.
//!
//! A [`Checkpoint`] of one document becomes one Git commit holding its content and a JSON
//! metadata file. See [`RevisionStore`] for the repository layout.

mod meta;
mod store;

use std::fmt;
use std::str::FromStr;

use collab_core::ids::DocId;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub use meta::{Author, Meta};
pub use store::RevisionStore;

/// A Git commit identifier: 40 lowercase hexadecimal characters.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RevisionId(String);

/// The text was not 40 lowercase hexadecimal characters.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("a revision id is 40 lowercase hexadecimal characters")]
pub struct ParseRevisionIdError;

impl RevisionId {
    pub(crate) fn from_oid(oid: git2::Oid) -> Self {
        RevisionId(oid.to_string())
    }

    pub(crate) fn to_oid(&self) -> Result<git2::Oid, git2::Error> {
        git2::Oid::from_str(&self.0)
    }
}

impl FromStr for RevisionId {
    type Err = ParseRevisionIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let ok = s.len() == 40 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        if ok {
            Ok(RevisionId(s.to_owned()))
        } else {
            Err(ParseRevisionIdError)
        }
    }
}

impl TryFrom<String> for RevisionId {
    type Error = ParseRevisionIdError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<RevisionId> for String {
    fn from(id: RevisionId) -> String {
        id.0
    }
}

impl fmt::Display for RevisionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A request to store the current state of one document.
#[derive(Clone, Debug)]
pub struct Checkpoint<'a> {
    /// The document.
    pub doc: DocId,
    /// Display file name: non-empty, no `/`, `\` or control characters.
    pub file_name: &'a str,
    /// The document content.
    pub bytes: &'a [u8],
    /// The revision this state was derived from; `None` for a document with no revision yet.
    pub base: Option<&'a RevisionId>,
    /// Who contributed; the first is the commit author. Must not be empty.
    pub authors: &'a [Author],
    /// Free-form document fields stored in the metadata.
    pub fields: &'a Map<String, Value>,
}

/// What a checkpoint did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// A new commit was made.
    Written(RevisionId),
    /// Content, file name and fields equal the stored ones; nothing was written.
    Unchanged(RevisionId),
}

/// Why a checkpoint or read failed.
#[derive(Debug, thiserror::Error)]
pub enum CheckpointError {
    /// `base` is not the document's current revision.
    #[error("checkpoint is based on a stale revision")]
    Stale {
        /// The document's current revision, if it has one.
        current: Option<RevisionId>,
    },
    /// `authors` was empty.
    #[error("a checkpoint needs at least one author")]
    NoAuthors,
    /// `file_name` was empty or contained a separator or control character.
    #[error("invalid file name")]
    InvalidFileName,
    /// Git failed.
    #[error("git error: {0}")]
    Git(#[from] git2::Error),
    /// Metadata could not be encoded or decoded.
    #[error("metadata error: {0}")]
    Json(#[from] serde_json::Error),
}

/// A stored revision of a document.
#[derive(Clone, Debug, PartialEq)]
pub struct Stored {
    /// The revision read.
    pub revision: RevisionId,
    /// The content.
    pub bytes: Vec<u8>,
    /// The metadata.
    pub meta: Meta,
}
