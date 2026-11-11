//! The content tree of sites, libraries, folders and files, recorded one JSON file per node.

use gix::object::tree::EntryKind;
use serde::{Deserialize, Serialize};
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

/// When something happened and who did it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Stamp {
    /// The moment, as an RFC 3339 timestamp.
    pub at: String,
    /// The person.
    pub by: Author,
}

/// Contents of `tree/<id>.json`: one node of the content tree. Field order here is the file's
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

fn record_path(id: Uuid) -> String {
    format!("{TREE_DIR}/{id}.json")
}

/// Parses the record stored at `path`, which must name the node the file is called after.
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
    if record_path(record.id) != path {
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
    /// session's next checkpoint is never refused because of it.
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
            files.push((record_path(record.id), record.to_file_bytes()?));
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
                for (path, bytes) in files {
                    let blob = repo.write_blob(&bytes)?.detach();
                    editor.upsert(path, EntryKind::Blob, blob)?;
                }
                Ok(())
            },
        )?;
        Ok(RevisionId::from_oid(oid))
    }

    /// Every record of the content tree on `main`, in no particular order; none if `main` does
    /// not exist or holds no tree.
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
        let mut records = Vec::new();
        for entry in dir.iter() {
            let entry = entry.map_err(GitError::new)?;
            if !entry.mode().is_blob() {
                continue;
            }
            let path = format!("{TREE_DIR}/{}", entry.filename());
            let object = repo.find_object(entry.oid()).map_err(GitError::new)?;
            records.push(parse_record(&commit, &path, &object.data)?);
        }
        Ok(records)
    }

    /// The record of node `id` on `main`; `None` if there is none.
    ///
    /// # Errors
    /// `Corrupt` if the record cannot be read, otherwise the Git error.
    pub fn read_tree_record(&self, id: Uuid) -> Result<Option<TreeRecord>, CheckpointError> {
        let repo = self.repo()?;
        let Some(commit) = main_tip(&repo)? else {
            return Ok(None);
        };
        let path = record_path(id);
        match read_blob(&commit.tree()?, &path)? {
            Some(bytes) => Ok(Some(parse_record(&commit, &path, &bytes)?)),
            None => Ok(None),
        }
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
                bytes,
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
        assert_eq!(store.read(doc, None).unwrap().unwrap().bytes, b"two");
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
}
