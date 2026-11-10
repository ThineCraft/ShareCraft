//! The metadata file stored beside each document's content.

use collab_core::ids::DocId;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// A person credited on a revision.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Author {
    /// Stable identifier; used for the commit email `<id>@sharecraft.invalid`.
    pub id: String,
    /// Display name.
    pub name: String,
}

/// Contents of `docs/<doc_id>/meta.json`. Field order here is the file's field order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    /// Format version; currently 1.
    pub schema: u32,
    /// The document.
    pub doc_id: DocId,
    /// Display file name.
    pub file_name: String,
    /// Free-form document fields.
    pub fields: Map<String, Value>,
    /// Everyone credited so far, in first-seen order.
    pub contributors: Vec<Author>,
}

impl Meta {
    /// Pretty-printed with two-space indent and a trailing newline.
    pub(crate) fn to_file_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        let mut bytes = serde_json::to_vec_pretty(self)?;
        bytes.push(b'\n');
        Ok(bytes)
    }
}
