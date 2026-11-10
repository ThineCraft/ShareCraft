//! Git-backed storage of document checkpoints.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use collab_core::ids::DocId;
use gix::object::tree::EntryKind;
use gix::refs::Target;
use gix::refs::transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog};
use gix::{ObjectId, Repository};

use crate::meta::{Author, Meta};
use crate::{Checkpoint, CheckpointError, GitError, Outcome, RevisionId, Stored};

const MAIN: &str = "refs/heads/main";
const SCHEMA: u32 = 1;

/// Stores document checkpoints in one bare Git repository.
///
/// Layout: branch `refs/heads/main`; content at `docs/<doc_id>/content`; metadata at
/// `docs/<doc_id>/meta.json`; and `refs/sharecraft/docs/<doc_id>` pointing at the last commit
/// that changed that document.
///
/// Checkpoints are serialized through a mutex, so concurrent callers never race on `main`.
/// Each call opens the repository itself, so the store is `Send + Sync`.
#[derive(Debug)]
pub struct RevisionStore {
    path: PathBuf,
    write_lock: Mutex<()>,
}

fn doc_ref(doc: DocId) -> String {
    format!("refs/sharecraft/docs/{}", doc.0)
}

fn valid_file_name(name: &str) -> bool {
    !name.is_empty()
        && !name
            .chars()
            .any(|c| c == '/' || c == '\\' || c.is_control())
}

fn valid_author(a: &Author) -> bool {
    let id_ok = !a.id.is_empty()
        && !a
            .id
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '<' | '>' | '@'));
    id_ok && is_valid_author_name(&a.name)
}

/// Whether `name` can be stored as an author name: no control characters, `<` or `>`, and
/// something left after trimming what Git trims from both ends of a signature name.
#[must_use]
pub fn is_valid_author_name(name: &str) -> bool {
    !name.trim_matches(trimmed_from_signature).is_empty()
        && !name
            .chars()
            .any(|c| c.is_control() || matches!(c, '<' | '>'))
}

/// Characters trimmed from the ends of a signature name before it is stored: bytes up to
/// space and `, : ; < > " \ '`, plus any other whitespace. Git tools clean names the same
/// way, so a stored name reads back unchanged whichever tool reads it.
fn trimmed_from_signature(c: char) -> bool {
    c <= ' ' || c.is_whitespace() || matches!(c, ',' | ':' | ';' | '<' | '>' | '"' | '\\' | '\'')
}

fn email(author: &Author) -> String {
    format!("{}@sharecraft.invalid", author.id)
}

fn signature(author: &Author) -> gix::actor::Signature {
    gix::actor::Signature {
        name: author.name.trim_matches(trimmed_from_signature).into(),
        email: email(author).into(),
        time: gix::date::Time::now_local_or_utc(),
    }
}

/// Options that ignore the machine's and the user's Git configuration, so the store behaves
/// the same wherever the server runs.
fn open_options() -> gix::open::Options {
    gix::open::Options::isolated()
}

fn read_blob(tree: &gix::Tree<'_>, path: &str) -> Result<Option<Vec<u8>>, GitError> {
    match tree.lookup_entry_by_path(path)? {
        Some(entry) => Ok(Some(entry.object()?.detach().data)),
        None => Ok(None),
    }
}

fn read_at(
    repo: &Repository,
    doc: DocId,
    oid: ObjectId,
) -> Result<Option<Stored>, CheckpointError> {
    let Some(object) = repo.try_find_object(oid)? else {
        return Ok(None);
    };
    let tree = object.try_into_commit().map_err(GitError::new)?.tree()?;
    let dir = format!("docs/{}", doc.0);
    let content = read_blob(&tree, &format!("{dir}/content"))?;
    let meta = read_blob(&tree, &format!("{dir}/meta.json"))?;
    match (content, meta) {
        (Some(bytes), Some(meta)) => Ok(Some(Stored {
            revision: RevisionId::from_oid(oid),
            bytes,
            meta: serde_json::from_slice(&meta)?,
        })),
        _ => Ok(None),
    }
}

/// The commit `name` points at directly, or `None` if there is no such ref.
fn ref_target(repo: &Repository, name: &str) -> Result<Option<ObjectId>, GitError> {
    Ok(repo
        .try_find_reference(name)?
        .and_then(|r| r.try_id().map(gix::Id::detach)))
}

fn doc_head(repo: &Repository, doc: DocId) -> Result<Option<ObjectId>, GitError> {
    ref_target(repo, &doc_ref(doc))
}

/// An edit that points `name` at `new`, allowed only while the ref is still at `expected`.
fn ref_edit(name: &str, new: ObjectId, expected: Option<ObjectId>) -> Result<RefEdit, GitError> {
    Ok(RefEdit {
        change: Change::Update {
            log: LogChange {
                mode: RefLog::AndReference,
                force_create_reflog: false,
                message: "checkpoint".into(),
            },
            expected: match expected {
                Some(id) => PreviousValue::MustExistAndMatch(Target::Object(id)),
                None => PreviousValue::MustNotExist,
            },
            new: Target::Object(new),
        },
        name: name.try_into().map_err(GitError::new)?,
        deref: false,
    })
}

/// Points `main` and the document ref at `oid` in one transaction.
///
/// Each edit names the tip seen at the start of the checkpoint. Both refs are locked and
/// compared with those tips before either is written; on any difference nothing is set.
fn move_refs(
    repo: &Repository,
    doc: DocId,
    oid: ObjectId,
    expected_main: Option<ObjectId>,
    expected_doc: Option<ObjectId>,
    signature: &gix::actor::Signature,
) -> Result<(), CheckpointError> {
    let edits = [
        ref_edit(MAIN, oid, expected_main)?,
        ref_edit(&doc_ref(doc), oid, expected_doc)?,
    ];
    let mut time = gix::date::parse::TimeBuf::default();
    repo.edit_references_as(edits, Some(signature.to_ref(&mut time)))?;
    Ok(())
}

impl RevisionStore {
    /// Opens the repository at `repo_path`, creating a bare one if absent.
    ///
    /// # Errors
    /// Returns the Git error if the repository cannot be opened or created.
    pub fn open(repo_path: &Path) -> Result<Self, GitError> {
        if repo_path.join("HEAD").is_file() {
            gix::open_opts(repo_path, open_options())?;
        } else {
            std::fs::create_dir_all(repo_path).map_err(GitError::new)?;
            gix::ThreadSafeRepository::init_opts(
                repo_path,
                gix::create::Kind::Bare,
                gix::create::Options::default(),
                open_options(),
            )?;
        }
        Ok(RevisionStore {
            path: repo_path.to_path_buf(),
            write_lock: Mutex::new(()),
        })
    }

    fn repo(&self) -> Result<Repository, GitError> {
        Ok(gix::open_opts(self.path.as_path(), open_options())?)
    }

    /// The document's current revision, if it has one.
    ///
    /// # Errors
    /// Returns the Git error if the repository cannot be read.
    pub fn head(&self, doc: DocId) -> Result<Option<RevisionId>, GitError> {
        let repo = self.repo()?;
        Ok(doc_head(&repo, doc)?.map(RevisionId::from_oid))
    }

    /// Reads a document at `at`, or at its current revision when `at` is `None`.
    /// Returns `None` when the document or revision does not exist.
    ///
    /// # Errors
    /// Returns an error if Git fails or the stored metadata is malformed.
    pub fn read(
        &self,
        doc: DocId,
        at: Option<&RevisionId>,
    ) -> Result<Option<Stored>, CheckpointError> {
        let repo = self.repo()?;
        let oid = match at {
            Some(rev) => Some(rev.to_oid()?),
            None => doc_head(&repo, doc)?,
        };
        match oid {
            Some(oid) => read_at(&repo, doc, oid),
            None => Ok(None),
        }
    }

    /// Stores the document state as one commit, unless it equals what is stored.
    ///
    /// # Errors
    /// `Stale` if `base` is not the document's current revision, `NoAuthors` or
    /// `InvalidFileName` for bad input, otherwise Git or metadata errors.
    pub fn checkpoint(&self, c: &Checkpoint<'_>) -> Result<Outcome, CheckpointError> {
        let Some(first) = c.authors.first() else {
            return Err(CheckpointError::NoAuthors);
        };
        if !c.authors.iter().all(valid_author) {
            return Err(CheckpointError::InvalidAuthor);
        }
        if !valid_file_name(c.file_name) {
            return Err(CheckpointError::InvalidFileName);
        }
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let repo = self.repo()?;

        let current = doc_head(&repo, c.doc)?;
        let current_id = current.map(RevisionId::from_oid);
        if c.base != current_id.as_ref() {
            return Err(CheckpointError::Stale {
                current: current_id,
            });
        }
        let stored = match current {
            Some(oid) => read_at(&repo, c.doc, oid)?,
            None => None,
        };

        if let Some(s) = &stored
            && s.bytes == c.bytes
            && s.meta.file_name == c.file_name
            && s.meta.fields == *c.fields
        {
            return Ok(Outcome::Unchanged(s.revision.clone()));
        }
        let mut contributors = stored.map(|s| s.meta.contributors).unwrap_or_default();
        let mut credited: Vec<&Author> = Vec::new();
        for a in c.authors {
            if !credited.iter().any(|x| x.id == a.id) {
                credited.push(a);
            }
            if !contributors.iter().any(|x| x.id == a.id) {
                contributors.push(a.clone());
            }
        }

        let meta = Meta {
            schema: SCHEMA,
            doc_id: c.doc,
            file_name: c.file_name.to_owned(),
            fields: c.fields.clone(),
            contributors,
        };
        let meta_bytes = meta.to_file_bytes()?;

        let parent = match repo.try_find_reference(MAIN)? {
            Some(mut r) => Some(r.peel_to_commit()?),
            None => None,
        };
        let root = match &parent {
            Some(commit) => commit.tree_id()?.detach(),
            None => ObjectId::empty_tree(repo.object_hash()),
        };

        // The document's folder is rebuilt from scratch: it holds these two files only.
        let dir = format!("docs/{}", c.doc.0);
        let content = repo.write_blob(c.bytes)?.detach();
        let meta_blob = repo.write_blob(&meta_bytes)?.detach();
        let mut editor = repo.edit_tree(root)?;
        editor.remove(dir.as_str())?;
        editor.upsert(format!("{dir}/content"), EntryKind::Blob, content)?;
        editor.upsert(format!("{dir}/meta.json"), EntryKind::Blob, meta_blob)?;
        let tree = editor.write()?.detach();

        let signature = signature(first);
        let mut message = format!("Checkpoint {}\n", c.file_name);
        for (i, a) in credited.iter().skip(1).enumerate() {
            if i == 0 {
                message.push('\n');
            }
            // Writing to a String cannot fail.
            let _ = writeln!(message, "Co-authored-by: {} <{}>", a.name, email(a));
        }

        let parent_id = parent.as_ref().map(|p| p.id().detach());
        let commit = gix::objs::Commit {
            tree,
            parents: parent_id.into_iter().collect(),
            author: signature.clone(),
            committer: signature.clone(),
            encoding: None,
            message: message.into(),
            extra_headers: Vec::new(),
        };
        let oid = repo.write_object(&commit)?.detach();
        move_refs(&repo, c.doc, oid, parent_id, current, &signature)?;
        Ok(Outcome::Written(RevisionId::from_oid(oid)))
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
    use crate::{Author, Checkpoint};
    use serde_json::Map;

    #[test]
    fn moved_main_aborts_without_touching_document_ref() {
        let dir = tempfile::tempdir().unwrap();
        let store = RevisionStore::open(&dir.path().join("lib.git")).unwrap();
        let doc = DocId(uuid::Uuid::new_v4());
        let authors = [Author {
            id: "u1".into(),
            name: "Ann".into(),
        }];
        let fields = Map::new();
        let cp = Checkpoint {
            doc,
            file_name: "n.txt",
            bytes: b"x",
            base: None,
            authors: &authors,
            fields: &fields,
        };
        let Outcome::Written(first) = store.checkpoint(&cp).unwrap() else {
            panic!("expected Written");
        };
        let first_oid = first.to_oid().unwrap();

        let repo = store.repo().unwrap();
        let sig = signature(&authors[0]);
        let tree = repo
            .find_commit(first_oid)
            .unwrap()
            .tree_id()
            .unwrap()
            .detach();
        let commit = |message: &str| {
            let commit = gix::objs::Commit {
                tree,
                parents: Default::default(),
                author: sig.clone(),
                committer: sig.clone(),
                encoding: None,
                message: message.into(),
                extra_headers: Vec::new(),
            };
            repo.write_object(&commit).unwrap().detach()
        };
        let other = commit("other");
        let new = commit("new");
        let tips = |repo: &Repository| {
            (
                ref_target(repo, MAIN).unwrap().unwrap(),
                doc_head(repo, doc).unwrap().unwrap(),
            )
        };
        let force = |name: &str, to: ObjectId| {
            repo.reference(name, to, PreviousValue::Any, "test")
                .unwrap();
        };

        // Another writer moved `main` after we read `first` as its tip.
        force(MAIN, other);
        let err = move_refs(&repo, doc, new, Some(first_oid), Some(first_oid), &sig);
        assert!(err.is_err());
        assert_eq!(tips(&repo), (other, first_oid), "both refs untouched");

        // The document ref moved instead.
        force(MAIN, first_oid);
        force(&doc_ref(doc), other);
        assert!(move_refs(&repo, doc, new, Some(first_oid), Some(first_oid), &sig).is_err());
        assert_eq!(tips(&repo), (first_oid, other), "both refs untouched");

        // A ref expected to be absent that exists is also a difference.
        assert!(move_refs(&repo, doc, new, None, Some(other), &sig).is_err());
        assert!(move_refs(&repo, doc, new, Some(first_oid), None, &sig).is_err());
        assert_eq!(tips(&repo), (first_oid, other), "both refs untouched");

        // Matching tips: both move together.
        move_refs(&repo, doc, new, Some(first_oid), Some(other), &sig).unwrap();
        assert_eq!(tips(&repo), (new, new));
    }
}
