//! Library records: each library's kind and settings, recorded as one JSON file beside its tree
//! record.
//!
//! A library's record lives at `libraries/<first two hex digits of the id>/<id>.json`. It is
//! written with the site records (see [`RevisionStore::write_sites`]), so a change of it moves
//! `refs/sharecraft/sites`, and creating a library writes it in the same commit as the
//! library's tree record. A library made before library records has no file: it reads as a
//! document library with the defaults (see [`LibraryFields::default`]) until its first change.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::CheckpointError;
use crate::sites::{parse, read_sharded};
use crate::store::{RevisionStore, main_tip, read_blob};
use crate::tree::shard;

/// The format version of a [`LibraryRecord`]; currently 1.
pub const LIBRARY_SCHEMA: u32 = 1;
/// The values a library's `kind` may have.
pub const LIBRARY_KINDS: [&str; 4] = ["document", "image", "template", "asset"];
/// The folder of the library records in a commit.
pub(crate) const LIBRARIES_DIR: &str = "libraries";

/// A library's kind and settings, without its identity: what a library record holds besides
/// `schema` and `id`, and what a site template carries for each library it makes. Field order
/// here is the order of the record's fields.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LibraryFields {
    /// `document`, `image`, `template` or `asset`.
    #[serde(default = "document_kind")]
    pub kind: String,
    /// What the library is for; 0 to 500 characters.
    #[serde(default)]
    pub description: String,
    /// Whether new folders may be made in it.
    #[serde(default = "folders_on")]
    pub folders: bool,
    /// The file types it accepts, as extensions (`".png"`); `None` for every type.
    #[serde(default)]
    pub allowed_types: Option<Vec<String>>,
    /// The file types it refuses, as extensions.
    #[serde(default)]
    pub blocked_types: Vec<String>,
    /// The largest file it takes, in bytes; `None` for no limit of its own.
    #[serde(default)]
    pub max_file_bytes: Option<u64>,
}

/// The kind of a library with no record, or of a template library that names none.
fn document_kind() -> String {
    LIBRARY_KINDS[0].to_owned()
}

/// Folders are on unless a library's settings turn them off.
fn folders_on() -> bool {
    true
}

impl Default for LibraryFields {
    /// A document library with no description, folders on, every type allowed, nothing
    /// blocked and no limit: what a library with no record reads as.
    fn default() -> Self {
        LibraryFields {
            kind: document_kind(),
            description: String::new(),
            folders: true,
            allowed_types: None,
            blocked_types: Vec::new(),
            max_file_bytes: None,
        }
    }
}

impl LibraryFields {
    /// Whether these are the defaults (see [`LibraryFields::default`]).
    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == LibraryFields::default()
    }
}

/// Contents of `libraries/<xx>/<id>.json`: a library's kind and settings beside its tree
/// record. Field order here is the file's field order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryRecord {
    /// Format version; currently 1.
    pub schema: u32,
    /// The library's node id.
    pub id: Uuid,
    /// `document`, `image`, `template` or `asset`.
    pub kind: String,
    /// What the library is for; 0 to 500 characters.
    pub description: String,
    /// Whether new folders may be made in it.
    pub folders: bool,
    /// The file types it accepts, as extensions; `None` for every type.
    pub allowed_types: Option<Vec<String>>,
    /// The file types it refuses, as extensions.
    pub blocked_types: Vec<String>,
    /// The largest file it takes, in bytes; `None` for no limit of its own.
    pub max_file_bytes: Option<u64>,
    /// The library's hero image: shown in place of the site header's banner image on the
    /// library's pages (only where the site's look has one). A copy of the image as chosen,
    /// pinned in the content store as a look's images are. Not among the fields a site
    /// template carries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hero_image: Option<crate::brand::ImageRef>,
}

impl LibraryRecord {
    /// The record of library `id` with `fields`.
    #[must_use]
    pub fn new(id: Uuid, fields: LibraryFields) -> LibraryRecord {
        LibraryRecord {
            schema: LIBRARY_SCHEMA,
            id,
            kind: fields.kind,
            description: fields.description,
            folders: fields.folders,
            allowed_types: fields.allowed_types,
            blocked_types: fields.blocked_types,
            max_file_bytes: fields.max_file_bytes,
            hero_image: None,
        }
    }

    /// The record of library `id` with the defaults: what a library with no record reads as.
    #[must_use]
    pub fn defaults(id: Uuid) -> LibraryRecord {
        LibraryRecord::new(id, LibraryFields::default())
    }

    /// The kind and settings, without the identity (and without the hero image).
    #[must_use]
    pub fn fields(&self) -> LibraryFields {
        LibraryFields {
            kind: self.kind.clone(),
            description: self.description.clone(),
            folders: self.folders,
            allowed_types: self.allowed_types.clone(),
            blocked_types: self.blocked_types.clone(),
            max_file_bytes: self.max_file_bytes,
        }
    }

    /// Why the record cannot be stored or was stored wrongly, if so. The finer rules of the
    /// settings are the database's to check (`records::validate_library_settings`).
    pub(crate) fn problem(&self) -> Option<String> {
        if self.schema != LIBRARY_SCHEMA {
            return Some(format!("unknown schema {}", self.schema));
        }
        if !LIBRARY_KINDS.contains(&self.kind.as_str()) {
            return Some(format!("unknown library kind {:?}", self.kind));
        }
        if self.description.chars().count() > crate::MAX_DESCRIPTION_CHARS {
            return Some("a description longer than 500 characters".to_owned());
        }
        None
    }
}

/// Where the record of library `id` is written: `libraries/<xx>/<id>.json`.
pub(crate) fn library_path(id: Uuid) -> String {
    format!("{LIBRARIES_DIR}/{}/{id}.json", shard(id))
}

/// Parses the library record at `path`, which must name the library the file is called after.
pub(crate) fn parse_library(
    commit: &gix::Commit<'_>,
    path: &str,
    bytes: &[u8],
) -> Result<LibraryRecord, CheckpointError> {
    let record: LibraryRecord = parse(commit, path, bytes)?;
    if let Some(problem) = record.problem() {
        return Err(crate::store::corrupt(commit, format!("{path}: {problem}")));
    }
    if library_path(record.id) != path {
        return Err(crate::store::corrupt(
            commit,
            format!("{path} holds the library {}", record.id),
        ));
    }
    Ok(record)
}

impl RevisionStore {
    /// Every library record on `main`, in no particular order; none if there are none.
    ///
    /// # Errors
    /// `Corrupt`, naming `main`'s commit and the file, if a record cannot be read, has an
    /// unknown schema or kind, or is stored under another library's name; otherwise the Git
    /// error.
    pub fn read_libraries(&self) -> Result<Vec<LibraryRecord>, CheckpointError> {
        let repo = self.repo()?;
        let Some(commit) = main_tip(&repo)? else {
            return Ok(Vec::new());
        };
        read_sharded(&repo, &commit, LIBRARIES_DIR, parse_library)
    }

    /// The record of library `id` on `main`; `None` if it has none (a library made before
    /// library records, or no such library).
    ///
    /// # Errors
    /// `Corrupt` if the record cannot be read, otherwise the Git error.
    pub fn read_library_record(&self, id: Uuid) -> Result<Option<LibraryRecord>, CheckpointError> {
        let repo = self.repo()?;
        let Some(commit) = main_tip(&repo)? else {
            return Ok(None);
        };
        let path = library_path(id);
        read_blob(&commit.tree()?, &path)?
            .map(|bytes| parse_library(&commit, &path, &bytes))
            .transpose()
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
    use crate::meta::Author;
    use crate::sites::{SitesChange, file_bytes};
    use crate::store::{MAIN, SITES_REF, TREE_REF, ref_target};

    fn ann() -> Author {
        Author {
            id: "u1".into(),
            name: "Ann".into(),
        }
    }

    fn store() -> (tempfile::TempDir, RevisionStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = RevisionStore::open(&dir.path().join("lib.git")).unwrap();
        (dir, store)
    }

    #[test]
    fn library_record_round_trip_exact_bytes() {
        let id = Uuid::parse_str("cd111111-1111-4111-8111-111111111111").unwrap();
        let record = LibraryRecord {
            schema: 1,
            id,
            kind: "image".into(),
            description: "Photos of the site".into(),
            folders: false,
            allowed_types: Some(vec![".png".into(), ".jpg".into()]),
            blocked_types: vec![".tar.gz".into()],
            max_file_bytes: Some(10_485_760),
            hero_image: None,
        };
        let text = String::from_utf8(file_bytes(&record).unwrap()).unwrap();
        let expected = r#"{
  "schema": 1,
  "id": "cd111111-1111-4111-8111-111111111111",
  "kind": "image",
  "description": "Photos of the site",
  "folders": false,
  "allowed_types": [
    ".png",
    ".jpg"
  ],
  "blocked_types": [
    ".tar.gz"
  ],
  "max_file_bytes": 10485760
}
"#;
        assert_eq!(text, expected);
        assert_eq!(
            serde_json::from_str::<LibraryRecord>(&text).unwrap(),
            record
        );

        // The defaults, as a library with no record reads.
        let defaults =
            String::from_utf8(file_bytes(&LibraryRecord::defaults(id)).unwrap()).unwrap();
        assert_eq!(
            defaults,
            r#"{
  "schema": 1,
  "id": "cd111111-1111-4111-8111-111111111111",
  "kind": "document",
  "description": "",
  "folders": true,
  "allowed_types": null,
  "blocked_types": [],
  "max_file_bytes": null
}
"#
        );

        // Stored and read back, byte for byte, at its place; the commit moves the sites ref
        // and not the tree ref.
        let (_dir, store) = store();
        let author = ann();
        let mut change = SitesChange::new(&author, "Change library");
        let records = [record.clone()];
        change.libraries = &records;
        let rev = store.write_sites(&change).unwrap();
        assert_eq!(store.read_library_record(id).unwrap(), Some(record.clone()));
        assert_eq!(store.read_libraries().unwrap(), vec![record]);
        let repo = store.repo().unwrap();
        let oid = rev.to_oid().unwrap();
        assert_eq!(ref_target(&repo, MAIN).unwrap(), Some(oid));
        assert_eq!(ref_target(&repo, SITES_REF).unwrap(), Some(oid));
        assert_eq!(ref_target(&repo, TREE_REF).unwrap(), None);
        let tip = main_tip(&repo).unwrap().unwrap();
        let bytes = read_blob(&tip.tree().unwrap(), &format!("libraries/cd/{id}.json"))
            .unwrap()
            .unwrap();
        assert_eq!(String::from_utf8(bytes).unwrap(), expected);

        // A removal takes the record away.
        let removed = [id];
        let mut change = SitesChange::new(&author, "Undo: Change library");
        change.removed_libraries = &removed;
        store.write_sites(&change).unwrap();
        assert_eq!(store.read_library_record(id).unwrap(), None);
        assert!(store.read_libraries().unwrap().is_empty());
    }

    #[test]
    fn bad_record_refused_and_template_fields_default() {
        let (_dir, store) = store();
        let author = ann();
        let mut bad = LibraryRecord::defaults(Uuid::new_v4());
        bad.kind = "wiki".into();
        let bad = [bad];
        let mut change = SitesChange::new(&author, "Bad");
        change.libraries = &bad;
        assert!(matches!(
            store.write_sites(&change),
            Err(CheckpointError::InvalidTreeChange(_))
        ));
        assert!(ref_target(&store.repo().unwrap(), MAIN).unwrap().is_none());

        // Fields given in part take the defaults for the rest; unknown ones are refused.
        let fields: LibraryFields = serde_json::from_str(r#"{"kind": "template"}"#).unwrap();
        assert_eq!(
            fields,
            LibraryFields {
                kind: "template".into(),
                ..LibraryFields::default()
            }
        );
        assert!(serde_json::from_str::<LibraryFields>(r#"{"colour": "red"}"#).is_err());
    }
}
