//! The content tree of sites, libraries, folders and files, recorded one JSON file per node.
//!
//! A record lives at `tree/<first two hex digits of the id>/<id>.json`, so that a change to one
//! node rewrites a folder of a few hundred entries, never one of every node. Records written
//! before that lived at `tree/<id>.json`; they are read from either place until
//! [`RevisionStore::move_tree_records`] moves them, once, into their sub-folders.

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};

use gix::object::tree::EntryKind;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::meta::Author;
use crate::store::{
    RevisionStore, TREE_REF, commit_on_main, corrupt, main_tip, read_blob, ref_target, signature,
    valid_author,
};
use crate::{CheckpointError, GitError, RevisionId};

/// The format version of a [`TreeRecord`]; currently 1.
const TREE_SCHEMA: u32 = 1;
/// The folder of the tree records in a commit.
const TREE_DIR: &str = "tree";
/// The values a record's `kind` may have.
const KINDS: [&str; 4] = ["site", "library", "folder", "document"];
/// The message of the commit that moves the records into their sub-folders.
pub const TREE_MOVE_MESSAGE: &str = "tree: move records into sub-folders";

/// When something happened and who did it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Stamp {
    /// The moment, as an RFC 3339 timestamp.
    pub at: String,
    /// The person.
    pub by: Author,
}

/// Contents of `tree/<xx>/<id>.json`, where `xx` are the id's first two hex digits: one node of the content tree. Field order here is the file's
/// field order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TreeRecord {
    /// Format version; currently 1.
    pub schema: u32,
    /// The node's lasting identifier; a file's is its document id.
    pub id: Uuid,
    /// One of `site`, `library`, `folder` or `document`.
    pub kind: String,
    /// The node it is in; `None` for a site.
    pub parent: Option<Uuid>,
    /// The site it belongs to; a site's own id for the site itself.
    pub site: Uuid,
    /// The display name.
    pub name: String,
    /// When and by whom it was created.
    pub created: Stamp,
    /// When and by whom it was deleted, if it was.
    pub deleted: Option<Stamp>,
}

impl TreeRecord {
    /// Pretty-printed with two-space indent and a trailing newline.
    fn to_file_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        let mut bytes = serde_json::to_vec_pretty(self)?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    /// Why the record cannot be stored or was stored wrongly, if so.
    fn problem(&self) -> Option<String> {
        if self.schema != TREE_SCHEMA {
            return Some(format!("unknown schema {}", self.schema));
        }
        if !KINDS.contains(&self.kind.as_str()) {
            return Some(format!("unknown kind {:?}", self.kind));
        }
        None
    }
}

/// One change to the tree: every record it writes, stored as one commit.
#[derive(Clone, Debug)]
pub struct TreeChange<'a> {
    /// Who made the change; the commit author.
    pub author: &'a Author,
    /// The commit message.
    pub message: &'a str,
    /// The records to write, each replacing any stored under the same id. Must not be empty.
    pub records: &'a [TreeRecord],
}

/// What [`RevisionStore::move_tree_records`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TreeMove {
    /// Records moved from `tree/` into their sub-folders.
    pub moved: usize,
    /// Records found at both places; the one with the later stamp was kept (the sub-folder's
    /// on equal stamps), the other dropped.
    pub duplicates: usize,
}

/// The latest moment `record` speaks of: when it was deleted, if it was, or created. `None` if
/// neither can be read as RFC 3339.
fn latest(record: &TreeRecord) -> Option<OffsetDateTime> {
    let at = |stamp: &Stamp| OffsetDateTime::parse(&stamp.at, &Rfc3339).ok();
    at(&record.created).max(record.deleted.as_ref().and_then(at))
}

/// Of the two records of one node, `flat` at `tree/<id>.json` and `sharded` in its sub-folder,
/// whether to keep `flat`: only when its latest stamp (see [`latest`]) is later. On equal
/// stamps, or stamps that cannot be read, the sub-folder's copy wins.
fn keeps_flat(flat: &TreeRecord, sharded: &TreeRecord) -> bool {
    latest(flat) > latest(sharded)
}

/// The sub-folder of `tree/` that holds the record of `id`: its first two hex digits.
fn shard(id: Uuid) -> String {
    id.simple().to_string().chars().take(2).collect()
}

/// Where the record of `id` is written: `tree/<xx>/<id>.json`.
fn record_path(id: Uuid) -> String {
    format!("{TREE_DIR}/{}/{id}.json", shard(id))
}

/// Where the record of `id` was written before the sub-folders: `tree/<id>.json`.
fn flat_path(id: Uuid) -> String {
    format!("{TREE_DIR}/{id}.json")
}

/// Whether `name` is the name of a sub-folder of `tree/`: two lowercase hex digits.
fn is_shard_name(name: &[u8]) -> bool {
    name.len() == 2 && name.iter().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The node a file named `<id>.json` is called after, if its name has that form.
fn named_id(file_name: &[u8]) -> Option<Uuid> {
    let text = std::str::from_utf8(file_name).ok()?;
    let id = Uuid::parse_str(text.strip_suffix(".json")?).ok()?;
    (format!("{id}.json") == text).then_some(id)
}

/// Parses the record stored at `path`, which must name the node the file is called after, at
/// either of its places.
fn parse_record(
    commit: &gix::Commit<'_>,
    path: &str,
    bytes: &[u8],
) -> Result<TreeRecord, CheckpointError> {
    let record: TreeRecord = serde_json::from_slice(bytes)
        .map_err(|e| corrupt(commit, format!("{path} cannot be read: {e}")))?;
    if let Some(problem) = record.problem() {
        return Err(corrupt(commit, format!("{path}: {problem}")));
    }
    if record_path(record.id) != path && flat_path(record.id) != path {
        return Err(corrupt(
            commit,
            format!("{path} holds the node {}", record.id),
        ));
    }
    Ok(record)
}

impl RevisionStore {
    /// Records one change to the content tree as one commit on `main`, credited to the change's
    /// author, and points `refs/sharecraft/tree` at it. No document ref moves, so a live
    /// session's next checkpoint is never refused because of it. Each record is written into
    /// its sub-folder; a copy at its place before the sub-folders is removed.
    ///
    /// # Errors
    /// `InvalidTreeChange` if there are no records or one is not valid, `InvalidAuthor` for a
    /// bad author, otherwise Git or metadata errors.
    pub fn write_tree(&self, change: &TreeChange<'_>) -> Result<RevisionId, CheckpointError> {
        if change.records.is_empty() {
            return Err(CheckpointError::InvalidTreeChange(
                "a tree change needs at least one record".into(),
            ));
        }
        if !valid_author(change.author) {
            return Err(CheckpointError::InvalidAuthor);
        }
        let mut files = Vec::with_capacity(change.records.len());
        for record in change.records {
            if let Some(problem) = record.problem() {
                return Err(CheckpointError::InvalidTreeChange(problem));
            }
            files.push((record.id, record.to_file_bytes()?));
        }

        let _guard = self.lock();
        let repo = self.repo()?;
        let parent = main_tip(&repo)?;
        let mut message = change.message.to_owned();
        if !message.ends_with('\n') {
            message.push('\n');
        }
        let oid = commit_on_main(
            &repo,
            parent.as_ref(),
            &signature(change.author),
            message,
            TREE_REF,
            ref_target(&repo, TREE_REF)?,
            |editor| {
                for (id, bytes) in files {
                    let blob = repo.write_blob(&bytes)?.detach();
                    editor.upsert(record_path(id), EntryKind::Blob, blob)?;
                    editor.remove(flat_path(id).as_str())?;
                }
                Ok(())
            },
        )?;
        Ok(RevisionId::from_oid(oid))
    }

    /// Every record of the content tree on `main`, in no particular order; none if `main` does
    /// not exist or holds no tree. Records are read from their sub-folders and from `tree/`
    /// itself, where they were before the sub-folders; of a node recorded at both places, the
    /// record with the later stamp is read, the sub-folder's on equal stamps.
    ///
    /// # Errors
    /// `Corrupt`, naming `main`'s commit and the file, if a record cannot be read, has an
    /// unknown schema or kind, or is stored under another node's name; otherwise the Git error.
    pub fn read_tree(&self) -> Result<Vec<TreeRecord>, CheckpointError> {
        let repo = self.repo()?;
        let Some(commit) = main_tip(&repo)? else {
            return Ok(Vec::new());
        };
        let root = commit.tree()?;
        let Some(entry) = root.lookup_entry_by_path(TREE_DIR)? else {
            return Ok(Vec::new());
        };
        let dir = entry.object().map_err(GitError::new)?.into_tree();
        let mut flat = Vec::new();
        let mut sharded = HashMap::new();
        for entry in dir.iter() {
            let entry = entry.map_err(GitError::new)?;
            let name = entry.filename();
            if entry.mode().is_blob() {
                let path = format!("{TREE_DIR}/{name}");
                let object = repo.find_object(entry.oid()).map_err(GitError::new)?;
                flat.push(parse_record(&commit, &path, &object.data)?);
            } else if entry.mode().is_tree() && is_shard_name(name) {
                let sub = repo
                    .find_object(entry.oid())
                    .map_err(GitError::new)?
                    .into_tree();
                for file in sub.iter() {
                    let file = file.map_err(GitError::new)?;
                    if !file.mode().is_blob() {
                        continue;
                    }
                    let path = format!("{TREE_DIR}/{name}/{}", file.filename());
                    let object = repo.find_object(file.oid()).map_err(GitError::new)?;
                    let record = parse_record(&commit, &path, &object.data)?;
                    sharded.insert(record.id, record);
                }
            }
        }
        for record in flat {
            match sharded.entry(record.id) {
                Entry::Vacant(place) => {
                    place.insert(record);
                }
                Entry::Occupied(mut place) => {
                    if keeps_flat(&record, place.get()) {
                        place.insert(record);
                    }
                }
            }
        }
        Ok(sharded.into_values().collect())
    }

    /// The record of node `id` on `main`, from its sub-folder or its place before the
    /// sub-folders; from both, the one with the later stamp, the sub-folder's on equal stamps.
    /// `None` if there is none.
    ///
    /// # Errors
    /// `Corrupt` if the record cannot be read, otherwise the Git error.
    pub fn read_tree_record(&self, id: Uuid) -> Result<Option<TreeRecord>, CheckpointError> {
        let repo = self.repo()?;
        let Some(commit) = main_tip(&repo)? else {
            return Ok(None);
        };
        let root = commit.tree()?;
        let read = |path: String| -> Result<Option<TreeRecord>, CheckpointError> {
            read_blob(&root, &path)?
                .map(|bytes| parse_record(&commit, &path, &bytes))
                .transpose()
        };
        let sharded = read(record_path(id))?;
        let flat = read(flat_path(id))?;
        Ok(match (sharded, flat) {
            (Some(sharded), Some(flat)) if keeps_flat(&flat, &sharded) => Some(flat),
            (Some(sharded), _) => Some(sharded),
            (None, flat) => flat,
        })
    }

    /// Moves every record still at `tree/<id>.json`, where records were written before the
    /// sub-folders, to `tree/<xx>/<id>.json`, in one commit on `main` credited to `author`
    /// with the message [`TREE_MOVE_MESSAGE`]; only `refs/sharecraft/tree` moves with `main`.
    /// Of a record found at both places the copy with the later stamp is kept, the sub-folder's
    /// on equal stamps (only such pairs are read). Nothing is written when no record is at the
    /// old place, so a second call does nothing. The files move as they are.
    ///
    /// The move is one way: a server from before the sub-folders would find no tree.
    ///
    /// # Errors
    /// `InvalidAuthor` for a bad author, otherwise the Git error.
    pub fn move_tree_records(&self, author: &Author) -> Result<TreeMove, CheckpointError> {
        if !valid_author(author) {
            return Err(CheckpointError::InvalidAuthor);
        }
        let _guard = self.lock();
        let repo = self.repo()?;
        let Some(parent) = main_tip(&repo)? else {
            return Ok(TreeMove::default());
        };
        let Some(entry) = parent.tree()?.lookup_entry_by_path(TREE_DIR)? else {
            return Ok(TreeMove::default());
        };
        let dir = decoded_tree(&repo, entry.object_id())?;

        let mut kept = Vec::new();
        let mut shards: BTreeMap<String, gix::objs::Tree> = BTreeMap::new();
        let mut old = Vec::new();
        for entry in dir.entries {
            if entry.mode.is_blob()
                && let Some(id) = named_id(&entry.filename)
            {
                old.push((id, entry));
            } else if entry.mode.is_tree() && is_shard_name(&entry.filename) {
                shards.insert(entry.filename.to_string(), decoded_tree(&repo, entry.oid)?);
            } else {
                kept.push(entry);
            }
        }
        if old.is_empty() {
            return Ok(TreeMove::default());
        }

        let mut done = TreeMove::default();
        for (id, entry) in old {
            let sub = shards.entry(shard(id)).or_default();
            if let Some(there) = sub
                .entries
                .iter_mut()
                .find(|e| e.filename == entry.filename)
            {
                let flat_path = flat_path(id);
                let flat = repo.find_object(entry.oid).map_err(GitError::new)?;
                let flat = parse_record(&parent, &flat_path, &flat.data)?;
                let sharded_path = record_path(id);
                let sharded = repo.find_object(there.oid).map_err(GitError::new)?;
                let sharded = parse_record(&parent, &sharded_path, &sharded.data)?;
                if keeps_flat(&flat, &sharded) {
                    there.oid = entry.oid;
                    there.mode = entry.mode;
                }
                done.duplicates = done.duplicates.saturating_add(1);
            } else {
                sub.entries.push(entry);
                done.moved = done.moved.saturating_add(1);
            }
        }
        for (name, mut sub) in shards {
            sub.entries.sort();
            let oid = repo.write_object(&sub).map_err(GitError::new)?.detach();
            kept.push(gix::objs::tree::Entry {
                mode: EntryKind::Tree.into(),
                filename: name.into(),
                oid,
            });
        }
        kept.sort();
        let new_dir = repo
            .write_object(&gix::objs::Tree { entries: kept })
            .map_err(GitError::new)?
            .detach();
        commit_on_main(
            &repo,
            Some(&parent),
            &signature(author),
            format!("{TREE_MOVE_MESSAGE}\n"),
            TREE_REF,
            ref_target(&repo, TREE_REF)?,
            |editor| {
                editor.upsert(TREE_DIR, EntryKind::Tree, new_dir)?;
                Ok(())
            },
        )?;
        Ok(done)
    }
}

/// The tree `id`, decoded into entries that can be changed and written again.
fn decoded_tree(repo: &gix::Repository, id: gix::ObjectId) -> Result<gix::objs::Tree, GitError> {
    let tree = repo.find_tree(id).map_err(GitError::new)?;
    let decoded = tree.decode().map_err(GitError::new)?;
    Ok(decoded.into())
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
    use crate::store::MAIN;
    use crate::{Checkpoint, Outcome};
    use collab_core::ids::DocId;
    use serde_json::Map;

    fn ann() -> Author {
        Author {
            id: "u1".into(),
            name: "Ann".into(),
        }
    }

    fn bob() -> Author {
        Author {
            id: "u2".into(),
            name: "Bob".into(),
        }
    }

    fn stamp(by: Author) -> Stamp {
        Stamp {
            at: "2026-11-11T09:30:00Z".into(),
            by,
        }
    }

    fn record(id: Uuid, kind: &str, parent: Option<Uuid>, site: Uuid, name: &str) -> TreeRecord {
        TreeRecord {
            schema: 1,
            id,
            kind: kind.into(),
            parent,
            site,
            name: name.into(),
            created: stamp(ann()),
            deleted: None,
        }
    }

    fn store() -> (tempfile::TempDir, RevisionStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = RevisionStore::open(&dir.path().join("lib.git")).unwrap();
        (dir, store)
    }

    fn checkpoint(
        store: &RevisionStore,
        doc: DocId,
        base: Option<&RevisionId>,
        bytes: &[u8],
    ) -> Outcome {
        let authors = [ann()];
        let fields = Map::new();
        store
            .checkpoint(&Checkpoint {
                doc,
                file_name: "n.txt",
                blob: crate::BlobRef::of(bytes),
                base,
                authors: &authors,
                fields: &fields,
            })
            .unwrap()
    }

    #[test]
    fn write_then_read_round_trips() {
        let (_dir, store) = store();
        assert!(store.read_tree().unwrap().is_empty());
        let site = Uuid::new_v4();
        let lib = Uuid::new_v4();
        let mut folder = record(Uuid::new_v4(), "folder", Some(lib), site, "Plans");
        folder.deleted = Some(stamp(bob()));
        let records = vec![
            record(site, "site", None, site, "Head office"),
            record(lib, "library", Some(site), site, "Documents"),
            folder,
        ];
        let author = ann();
        store
            .write_tree(&TreeChange {
                author: &author,
                message: "Create site",
                records: &records,
            })
            .unwrap();
        let mut read = store.read_tree().unwrap();
        let mut expected = records.clone();
        read.sort_by_key(|r| r.id);
        expected.sort_by_key(|r| r.id);
        assert_eq!(read, expected);
        for r in &records {
            assert_eq!(store.read_tree_record(r.id).unwrap().as_ref(), Some(r));
        }
        assert_eq!(store.read_tree_record(Uuid::new_v4()).unwrap(), None);

        // A later change replaces the record stored under the same id.
        let mut renamed = records[1].clone();
        renamed.name = "Files".into();
        store
            .write_tree(&TreeChange {
                author: &author,
                message: "Rename",
                records: std::slice::from_ref(&renamed),
            })
            .unwrap();
        assert_eq!(store.read_tree_record(lib).unwrap(), Some(renamed));
        assert_eq!(store.read_tree().unwrap().len(), 3);
    }

    #[test]
    fn an_empty_change_writes_nothing() {
        let (_dir, store) = store();
        let author = ann();
        let err = store
            .write_tree(&TreeChange {
                author: &author,
                message: "nothing",
                records: &[],
            })
            .unwrap_err();
        assert!(matches!(err, CheckpointError::InvalidTreeChange(_)));
        let repo = store.repo().unwrap();
        assert!(ref_target(&repo, MAIN).unwrap().is_none());
        assert!(ref_target(&repo, TREE_REF).unwrap().is_none());
    }

    #[test]
    fn tree_commit_keeps_document_refs() {
        let (_dir, store) = store();
        let doc = DocId(Uuid::new_v4());
        let Outcome::Written(first) = checkpoint(&store, doc, None, b"one") else {
            panic!("expected Written");
        };
        let site = Uuid::new_v4();
        let author = bob();
        let tree_rev = store
            .write_tree(&TreeChange {
                author: &author,
                message: "Create site",
                records: &[record(site, "site", None, site, "Head office")],
            })
            .unwrap();
        assert_eq!(store.head(doc).unwrap(), Some(first.clone()));
        assert_ne!(tree_rev, first);
        let repo = store.repo().unwrap();
        assert_eq!(
            ref_target(&repo, TREE_REF).unwrap(),
            Some(tree_rev.to_oid().unwrap())
        );
        assert_eq!(
            ref_target(&repo, MAIN).unwrap(),
            Some(tree_rev.to_oid().unwrap())
        );
        // A checkpoint based on the unchanged head still succeeds.
        let Outcome::Written(second) = checkpoint(&store, doc, Some(&first), b"two") else {
            panic!("expected Written");
        };
        assert_eq!(store.head(doc).unwrap(), Some(second.clone()));
        assert_eq!(
            store.read(doc, None).unwrap().unwrap().content,
            crate::Content::Blob(crate::BlobRef::of(b"two"))
        );
        assert_eq!(store.history(doc).unwrap().len(), 2);
    }

    #[test]
    fn checkpoint_keeps_tree_records() {
        let (_dir, store) = store();
        let site = Uuid::new_v4();
        let author = ann();
        let rec = record(site, "site", None, site, "Head office");
        store
            .write_tree(&TreeChange {
                author: &author,
                message: "Create site",
                records: std::slice::from_ref(&rec),
            })
            .unwrap();
        let tree_ref = store.repo().unwrap();
        let tree_rev = ref_target(&tree_ref, TREE_REF).unwrap();
        let doc = DocId(Uuid::new_v4());
        checkpoint(&store, doc, None, b"one");
        assert_eq!(store.read_tree().unwrap(), vec![rec]);
        // The tree ref names the last commit that changed `tree/`, not the checkpoint.
        assert_eq!(
            ref_target(&store.repo().unwrap(), TREE_REF).unwrap(),
            tree_rev
        );
    }

    #[test]
    fn one_commit_per_change() {
        let (_dir, store) = store();
        let site = Uuid::new_v4();
        let lib = Uuid::new_v4();
        let records = [
            record(site, "site", None, site, "Head office"),
            record(lib, "library", Some(site), site, "Documents"),
        ];
        let first = ann();
        let rev1 = store
            .write_tree(&TreeChange {
                author: &first,
                message: "Create site Head office",
                records: &records,
            })
            .unwrap();
        let second = bob();
        let rev2 = store
            .write_tree(&TreeChange {
                author: &second,
                message: "Rename library",
                records: &records[1..],
            })
            .unwrap();
        let repo = store.repo().unwrap();
        let c2 = repo.find_commit(rev2.to_oid().unwrap()).unwrap();
        let c1 = repo.find_commit(rev1.to_oid().unwrap()).unwrap();
        assert_eq!(
            c2.parent_ids().map(gix::Id::detach).collect::<Vec<_>>(),
            vec![c1.id]
        );
        assert_eq!(c1.parent_ids().count(), 0);
        let sig = c2.author().unwrap();
        assert_eq!(sig.name, "Bob");
        assert_eq!(sig.email, "u2@sharecraft.invalid");
        assert_eq!(
            String::from_utf8_lossy(c2.message_raw().unwrap()),
            "Rename library\n"
        );
        assert_eq!(
            String::from_utf8_lossy(c1.message_raw().unwrap()),
            "Create site Head office\n"
        );
        assert_eq!(c1.author().unwrap().name, "Ann");
        assert_eq!(
            ref_target(&repo, TREE_REF).unwrap(),
            Some(rev2.to_oid().unwrap())
        );
    }

    #[test]
    fn record_bytes_are_stable() {
        let id = Uuid::parse_str("11111111-1111-4111-8111-111111111111").unwrap();
        let parent = Uuid::parse_str("22222222-2222-4222-8222-222222222222").unwrap();
        let site = Uuid::parse_str("33333333-3333-4333-8333-333333333333").unwrap();
        let rec = record(id, "folder", Some(parent), site, "Plans");
        let text = String::from_utf8(rec.to_file_bytes().unwrap()).unwrap();
        let expected = r#"{
  "schema": 1,
  "id": "11111111-1111-4111-8111-111111111111",
  "kind": "folder",
  "parent": "22222222-2222-4222-8222-222222222222",
  "site": "33333333-3333-4333-8333-333333333333",
  "name": "Plans",
  "created": {
    "at": "2026-11-11T09:30:00Z",
    "by": {
      "id": "u1",
      "name": "Ann"
    }
  },
  "deleted": null
}
"#;
        assert_eq!(text, expected);
    }

    #[test]
    fn unreadable_record_is_reported() {
        let (_dir, store) = store();
        let site = Uuid::new_v4();
        let author = ann();
        let good = record(site, "site", None, site, "Head office");
        store
            .write_tree(&TreeChange {
                author: &author,
                message: "Create site",
                records: std::slice::from_ref(&good),
            })
            .unwrap();
        // Put bad records into main by hand, as a damaged repository would hold them.
        let bad_json = Uuid::new_v4();
        let bad_kind = Uuid::new_v4();
        let bad_name = Uuid::new_v4();
        let mut kind_record = record(bad_kind, "shortcut", None, site, "X");
        kind_record.kind = "shortcut".into();
        let cases: Vec<(Uuid, Vec<u8>, &str)> = vec![
            (bad_json, b"{ not json".to_vec(), "cannot be read"),
            (
                bad_kind,
                kind_record.to_file_bytes().unwrap(),
                "unknown kind",
            ),
            (
                bad_name,
                record(Uuid::new_v4(), "site", None, site, "Y")
                    .to_file_bytes()
                    .unwrap(),
                "holds the node",
            ),
        ];
        for (id, bytes, reason) in cases {
            let repo = store.repo().unwrap();
            let parent = main_tip(&repo).unwrap().unwrap();
            let blob = repo.write_blob(&bytes).unwrap().detach();
            let sig = signature(&author);
            let oid = commit_on_main(
                &repo,
                Some(&parent),
                &sig,
                "damage\n".into(),
                TREE_REF,
                ref_target(&repo, TREE_REF).unwrap(),
                |editor| {
                    editor.upsert(record_path(id), EntryKind::Blob, blob)?;
                    Ok(())
                },
            )
            .unwrap();
            for result in [
                store.read_tree().map(|_| ()),
                store.read_tree_record(id).map(|_| ()),
            ] {
                match result.unwrap_err() {
                    CheckpointError::Corrupt {
                        revision,
                        reason: r,
                    } => {
                        assert_eq!(revision, RevisionId::from_oid(oid));
                        assert!(r.contains(&record_path(id)), "{r}");
                        assert!(r.contains(reason), "{r}");
                    }
                    other => panic!("expected Corrupt, got {other:?}"),
                }
            }
            // Take the damage out again so the next case is read on its own.
            let repo = store.repo().unwrap();
            let tip = main_tip(&repo).unwrap().unwrap();
            commit_on_main(
                &repo,
                Some(&tip),
                &sig,
                "repair\n".into(),
                TREE_REF,
                ref_target(&repo, TREE_REF).unwrap(),
                |editor| {
                    editor.remove(record_path(id).as_str())?;
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!(store.read_tree().unwrap(), vec![good.clone()]);
        }
    }

    /// The entries of the folder `path` in `main`'s tree: (name, whether it is a folder).
    fn folder_entries(store: &RevisionStore, path: &str) -> Vec<(String, bool)> {
        let repo = store.repo().unwrap();
        let tip = main_tip(&repo).unwrap().unwrap();
        let entry = tip.tree().unwrap().lookup_entry_by_path(path).unwrap();
        let Some(entry) = entry else {
            return Vec::new();
        };
        let dir = entry.object().unwrap().into_tree();
        dir.iter()
            .map(|e| {
                let e = e.unwrap();
                (e.filename().to_string(), e.mode().is_tree())
            })
            .collect()
    }

    /// The id of the object at `path` in `main`'s tree, if any.
    fn object_at(store: &RevisionStore, path: &str) -> Option<gix::ObjectId> {
        let repo = store.repo().unwrap();
        let tip = main_tip(&repo).unwrap().unwrap();
        tip.tree()
            .unwrap()
            .lookup_entry_by_path(path)
            .unwrap()
            .map(|e| e.object_id())
    }

    /// Writes `records` at their places from before the sub-folders, `tree/<id>.json`, as a
    /// server of that time did: one commit moving `main` and the tree ref.
    fn write_flat(store: &RevisionStore, records: &[TreeRecord]) {
        let repo = store.repo().unwrap();
        let parent = main_tip(&repo).unwrap();
        commit_on_main(
            &repo,
            parent.as_ref(),
            &signature(&ann()),
            "Old server\n".into(),
            TREE_REF,
            ref_target(&repo, TREE_REF).unwrap(),
            |editor| {
                for record in records {
                    let blob = repo.write_blob(record.to_file_bytes()?)?.detach();
                    editor.upsert(flat_path(record.id), EntryKind::Blob, blob)?;
                }
                Ok(())
            },
        )
        .unwrap();
    }

    fn sorted(mut records: Vec<TreeRecord>) -> Vec<TreeRecord> {
        records.sort_by_key(|r| r.id);
        records
    }

    #[test]
    fn records_live_in_sub_folders() {
        let (_dir, store) = store();
        let site = Uuid::parse_str("ab000000-0000-4000-8000-000000000001").unwrap();
        let lib = Uuid::parse_str("ab000000-0000-4000-8000-000000000002").unwrap();
        let folder = Uuid::parse_str("3f000000-0000-4000-8000-000000000003").unwrap();
        let records = vec![
            record(site, "site", None, site, "Head office"),
            record(lib, "library", Some(site), site, "Documents"),
            record(folder, "folder", Some(lib), site, "Plans"),
        ];
        let author = ann();
        store
            .write_tree(&TreeChange {
                author: &author,
                message: "Create site",
                records: &records,
            })
            .unwrap();
        assert_eq!(
            folder_entries(&store, "tree"),
            [("3f".to_owned(), true), ("ab".to_owned(), true)]
        );
        assert_eq!(
            folder_entries(&store, "tree/ab"),
            [
                (format!("{site}.json"), false),
                (format!("{lib}.json"), false)
            ]
        );
        assert_eq!(
            folder_entries(&store, "tree/3f"),
            [(format!("{folder}.json"), false)]
        );
        assert_eq!(record_path(folder), format!("tree/3f/{folder}.json"));
        assert_eq!(sorted(store.read_tree().unwrap()), sorted(records.clone()));
        assert_eq!(
            store.read_tree_record(lib).unwrap(),
            Some(records[1].clone())
        );

        // A change to one node rewrites its own sub-folder only.
        let other = object_at(&store, "tree/ab").unwrap();
        let mut renamed = records[2].clone();
        renamed.name = "Drawings".into();
        store
            .write_tree(&TreeChange {
                author: &author,
                message: "Rename",
                records: std::slice::from_ref(&renamed),
            })
            .unwrap();
        assert_eq!(object_at(&store, "tree/ab"), Some(other));
        assert_eq!(store.read_tree_record(folder).unwrap(), Some(renamed));

        // A record still at its old place is read from there, and written to its sub-folder,
        // leaving nothing behind.
        let old = Uuid::parse_str("c0000000-0000-4000-8000-000000000004").unwrap();
        let old_record = record(old, "folder", Some(lib), site, "Old");
        write_flat(&store, std::slice::from_ref(&old_record));
        assert_eq!(
            store.read_tree_record(old).unwrap(),
            Some(old_record.clone())
        );
        assert_eq!(store.read_tree().unwrap().len(), 4);
        let mut changed = old_record.clone();
        changed.name = "Older".into();
        store
            .write_tree(&TreeChange {
                author: &author,
                message: "Rename",
                records: std::slice::from_ref(&changed),
            })
            .unwrap();
        assert_eq!(object_at(&store, &flat_path(old)), None);
        assert!(object_at(&store, &record_path(old)).is_some());
        assert_eq!(store.read_tree_record(old).unwrap(), Some(changed));
        assert_eq!(store.read_tree().unwrap().len(), 4);
    }

    #[test]
    fn old_layout_moves_in_one_commit() {
        let (_dir, store) = store();
        let doc = DocId(Uuid::new_v4());
        checkpoint(&store, doc, None, b"one");
        let site = Uuid::new_v4();
        let lib = Uuid::new_v4();
        let both = Uuid::new_v4();
        let flat = vec![
            record(site, "site", None, site, "Head office"),
            record(lib, "library", Some(site), site, "Documents"),
            record(both, "folder", Some(lib), site, "Stale copy"),
        ];
        write_flat(&store, &flat);
        // A node recorded at both places, which should not happen: its sub-folder copy wins.
        let current = record(both, "folder", Some(lib), site, "Current");
        let repo = store.repo().unwrap();
        let tip = main_tip(&repo).unwrap().unwrap();
        commit_on_main(
            &repo,
            Some(&tip),
            &signature(&ann()),
            "Damage\n".into(),
            TREE_REF,
            ref_target(&repo, TREE_REF).unwrap(),
            |editor| {
                let blob = repo.write_blob(current.to_file_bytes()?)?.detach();
                editor.upsert(record_path(both), EntryKind::Blob, blob)?;
                Ok(())
            },
        )
        .unwrap();
        let expected = sorted(vec![flat[0].clone(), flat[1].clone(), current.clone()]);
        assert_eq!(sorted(store.read_tree().unwrap()), expected);
        assert_eq!(store.read_tree_record(both).unwrap(), Some(current.clone()));
        assert_eq!(store.read_tree_record(site).unwrap(), Some(flat[0].clone()));

        let repo = store.repo().unwrap();
        let before = ref_target(&repo, MAIN).unwrap().unwrap();
        let doc_ref = format!("refs/sharecraft/docs/{}", doc.0);
        let doc_head = ref_target(&repo, &doc_ref).unwrap();
        let server = Author {
            id: "sharecraft".into(),
            name: "ShareCraft".into(),
        };
        let done = store.move_tree_records(&server).unwrap();
        assert_eq!(
            done,
            TreeMove {
                moved: 2,
                duplicates: 1
            }
        );

        let repo = store.repo().unwrap();
        let after = ref_target(&repo, MAIN).unwrap().unwrap();
        let commit = repo.find_commit(after).unwrap();
        assert_eq!(
            commit.parent_ids().map(gix::Id::detach).collect::<Vec<_>>(),
            vec![before],
            "one commit"
        );
        assert_eq!(
            String::from_utf8_lossy(commit.message_raw().unwrap()),
            "tree: move records into sub-folders\n"
        );
        let sig = commit.author().unwrap();
        assert_eq!(
            (sig.name.to_string(), sig.email.to_string()),
            (
                "ShareCraft".to_owned(),
                "sharecraft@sharecraft.invalid".to_owned()
            )
        );
        assert_eq!(ref_target(&repo, TREE_REF).unwrap(), Some(after));
        assert_eq!(
            ref_target(&repo, &doc_ref).unwrap(),
            doc_head,
            "no document ref moves"
        );
        assert!(
            folder_entries(&store, "tree").iter().all(|(_, dir)| *dir),
            "nothing left at the old places"
        );
        for r in &expected {
            assert!(object_at(&store, &record_path(r.id)).is_some());
            assert_eq!(object_at(&store, &flat_path(r.id)), None);
        }
        assert_eq!(sorted(store.read_tree().unwrap()), expected);
        assert_eq!(
            store.read(doc, None).unwrap().unwrap().content,
            crate::Content::Blob(crate::BlobRef::of(b"one"))
        );

        // A second move finds nothing to move and writes nothing.
        assert_eq!(
            store.move_tree_records(&server).unwrap(),
            TreeMove::default()
        );
        assert_eq!(
            ref_target(&store.repo().unwrap(), MAIN).unwrap(),
            Some(after)
        );
    }

    /// Of a node recorded at both places, the record with the later stamp is read and kept,
    /// wherever it is; on equal stamps the sub-folder's copy.
    #[test]
    fn duplicate_with_the_later_stamp_wins() {
        let (_dir, store) = store();
        let site = Uuid::new_v4();
        let lib = Uuid::new_v4();
        let flat_newer = Uuid::new_v4();
        let sub_newer = Uuid::new_v4();
        // Deleted after it was copied into its sub-folder: the old place is newer.
        let mut deleted = record(flat_newer, "folder", Some(lib), site, "Deleted later");
        deleted.deleted = Some(Stamp {
            at: "2026-11-11T11:00:00.5+01:00".into(),
            by: ann(),
        });
        let mut renamed = record(sub_newer, "folder", Some(lib), site, "Renamed later");
        renamed.created.at = "2026-11-11T09:30:00.25Z".into();
        let flat = vec![
            record(site, "site", None, site, "Head office"),
            record(lib, "library", Some(site), site, "Documents"),
            deleted.clone(),
            record(sub_newer, "folder", Some(lib), site, "Older"),
        ];
        write_flat(&store, &flat);
        let stale = record(flat_newer, "folder", Some(lib), site, "Not yet deleted");
        let repo = store.repo().unwrap();
        let tip = main_tip(&repo).unwrap().unwrap();
        commit_on_main(
            &repo,
            Some(&tip),
            &signature(&ann()),
            "Damage
"
            .into(),
            TREE_REF,
            ref_target(&repo, TREE_REF).unwrap(),
            |editor| {
                for r in [&stale, &renamed] {
                    let blob = repo.write_blob(r.to_file_bytes()?)?.detach();
                    editor.upsert(record_path(r.id), EntryKind::Blob, blob)?;
                }
                Ok(())
            },
        )
        .unwrap();
        let expected = sorted(vec![
            flat[0].clone(),
            flat[1].clone(),
            deleted.clone(),
            renamed.clone(),
        ]);
        assert_eq!(sorted(store.read_tree().unwrap()), expected);
        assert_eq!(
            store.read_tree_record(flat_newer).unwrap(),
            Some(deleted.clone())
        );
        assert_eq!(
            store.read_tree_record(sub_newer).unwrap(),
            Some(renamed.clone())
        );

        let done = store.move_tree_records(&ann()).unwrap();
        assert_eq!(
            done,
            TreeMove {
                moved: 2,
                duplicates: 2
            }
        );
        assert_eq!(sorted(store.read_tree().unwrap()), expected);
        assert_eq!(store.read_tree_record(flat_newer).unwrap(), Some(deleted));
        assert_eq!(store.read_tree_record(sub_newer).unwrap(), Some(renamed));
        assert_eq!(object_at(&store, &flat_path(flat_newer)), None);
    }

    #[test]
    fn nothing_to_move_writes_nothing() {
        let (_dir, store) = store();
        let server = ann();
        assert_eq!(
            store.move_tree_records(&server).unwrap(),
            TreeMove::default()
        );
        let site = Uuid::new_v4();
        store
            .write_tree(&TreeChange {
                author: &server,
                message: "Create site",
                records: &[record(site, "site", None, site, "Head office")],
            })
            .unwrap();
        let repo = store.repo().unwrap();
        let tip = ref_target(&repo, MAIN).unwrap();
        assert_eq!(
            store.move_tree_records(&server).unwrap(),
            TreeMove::default()
        );
        assert_eq!(ref_target(&store.repo().unwrap(), MAIN).unwrap(), tip);
    }
}
