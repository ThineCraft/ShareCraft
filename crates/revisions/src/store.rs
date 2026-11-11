//! Git-backed storage of document checkpoints and of the content tree.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use collab_core::ids::DocId;
use gix::object::tree::EntryKind;
use gix::refs::Target;
use gix::refs::transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog};
use gix::{ObjectId, Repository};

use crate::meta::{Author, Meta};
use crate::{Checkpoint, CheckpointError, GitError, Outcome, RevisionId, RevisionInfo, Stored};

pub(crate) const MAIN: &str = "refs/heads/main";
const SCHEMA: u32 = 1;

/// Stores document checkpoints in one bare Git repository.
///
/// Layout: branch `refs/heads/main`; content at `docs/<doc_id>/content`; metadata at
/// `docs/<doc_id>/meta.json`; and `refs/sharecraft/docs/<doc_id>` pointing at the last commit
/// that changed that document. The content tree is recorded beside the documents, one file per
/// node at `tree/<node_id>.json`, with `refs/sharecraft/tree` pointing at the last commit that
/// changed `tree/`.
///
/// Checkpoints and tree changes are serialized through a mutex, so concurrent callers never race on `main`.
/// Each call opens the repository itself, so the store is `Send + Sync`.
#[derive(Debug)]
pub struct RevisionStore {
    path: PathBuf,
    write_lock: Mutex<()>,
}

const DOC_REFS: &str = "refs/sharecraft/docs/";
pub(crate) const TREE_REF: &str = "refs/sharecraft/tree";

fn doc_ref(doc: DocId) -> String {
    format!("{DOC_REFS}{}", doc.0)
}

fn valid_file_name(name: &str) -> bool {
    !name.is_empty()
        && !name
            .chars()
            .any(|c| c == '/' || c == '\\' || c.is_control())
}

pub(crate) fn valid_author(a: &Author) -> bool {
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

pub(crate) fn email(author: &Author) -> String {
    format!("{}@sharecraft.invalid", author.id)
}

pub(crate) fn signature(author: &Author) -> gix::actor::Signature {
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

pub(crate) fn read_blob(tree: &gix::Tree<'_>, path: &str) -> Result<Option<Vec<u8>>, GitError> {
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
    match find_commit(repo, oid)? {
        Some(commit) => read_commit(doc, &commit),
        None => Ok(None),
    }
}

/// The commit `oid`, or `None` if the repository has no such object or it is not a commit (a
/// blob or tree id names no revision).
fn find_commit(repo: &Repository, oid: ObjectId) -> Result<Option<gix::Commit<'_>>, GitError> {
    match repo.try_find_object(oid)? {
        Some(object) => Ok(object.try_into_commit().ok()),
        None => Ok(None),
    }
}

pub(crate) fn corrupt(commit: &gix::Commit<'_>, reason: impl Into<String>) -> CheckpointError {
    CheckpointError::Corrupt {
        revision: RevisionId::from_oid(commit.id),
        reason: reason.into(),
    }
}

/// The document as `commit` stores it; `None` if the commit holds nothing of it, `Corrupt` if
/// it holds only part of it or metadata that cannot be read.
fn read_commit(doc: DocId, commit: &gix::Commit<'_>) -> Result<Option<Stored>, CheckpointError> {
    let tree = commit.tree()?;
    let dir = format!("docs/{}", doc.0);
    let content = read_blob(&tree, &format!("{dir}/content"))?;
    let meta = read_blob(&tree, &format!("{dir}/meta.json"))?;
    match (content, meta) {
        (None, None) => Ok(None),
        (Some(bytes), Some(meta)) => match serde_json::from_slice(&meta) {
            Ok(meta) => Ok(Some(Stored {
                revision: RevisionId::from_oid(commit.id),
                bytes,
                meta,
            })),
            Err(e) => Err(corrupt(
                commit,
                format!("its meta.json cannot be read: {e}"),
            )),
        },
        (Some(_), None) => Err(corrupt(commit, "it has content but no meta.json")),
        (None, Some(_)) => Err(corrupt(commit, "it has a meta.json but no content")),
    }
}

/// The id of the document's folder `dir` in `commit`'s tree, if the commit holds the document.
fn folder_id(commit: &gix::Commit<'_>, dir: &str) -> Result<Option<ObjectId>, GitError> {
    Ok(commit
        .tree()?
        .lookup_entry_by_path(dir)?
        .map(|entry| entry.object_id()))
}

/// The person a signature or trailer names: the id is the part of the email before `@`.
fn author_from(name: &[u8], email: &[u8]) -> Author {
    let email = String::from_utf8_lossy(email);
    let id = email.split('@').next().unwrap_or_default();
    Author {
        id: id.to_owned(),
        name: String::from_utf8_lossy(name).into_owned(),
    }
}

/// The author a `Co-authored-by: Name <email>` trailer line names, if it is one.
fn co_author(line: &str) -> Option<Author> {
    let rest = line.strip_prefix("Co-authored-by: ")?;
    let (name, email) = rest.trim_end().rsplit_once(" <")?;
    let email = email.strip_suffix('>')?;
    Some(author_from(name.as_bytes(), email.as_bytes()))
}

/// Describes `commit` as a revision of `doc`; `None` if it holds nothing of the document.
fn describe_commit(
    doc: DocId,
    commit: &gix::Commit<'_>,
) -> Result<Option<RevisionInfo>, CheckpointError> {
    let Some(stored) = read_commit(doc, commit)? else {
        return Ok(None);
    };
    let signature = commit.author()?;
    let mut authors = vec![author_from(signature.name, signature.email)];
    let message = String::from_utf8_lossy(commit.message_raw()?);
    authors.extend(message.lines().filter_map(co_author));
    Ok(Some(RevisionInfo {
        revision: stored.revision,
        doc,
        committed_at: commit.time()?.seconds,
        authors,
        file_name: stored.meta.file_name,
        fields: stored.meta.fields,
    }))
}

/// The commit `name` points at directly, or `None` if there is no such ref.
pub(crate) fn ref_target(repo: &Repository, name: &str) -> Result<Option<ObjectId>, GitError> {
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

/// Points `main` and the ref `other` at `oid` in one transaction, each allowed only while it
/// is still at the tip expected.
fn move_main_and(
    repo: &Repository,
    other: &str,
    oid: ObjectId,
    expected_main: Option<ObjectId>,
    expected_other: Option<ObjectId>,
    signature: &gix::actor::Signature,
) -> Result<(), CheckpointError> {
    let edits = [
        ref_edit(MAIN, oid, expected_main)?,
        ref_edit(other, oid, expected_other)?,
    ];
    let mut time = gix::date::parse::TimeBuf::default();
    repo.edit_references_as(edits, Some(signature.to_ref(&mut time)))?;
    Ok(())
}

/// What `main` is at the start of a write: its tip commit, if it has one.
pub(crate) fn main_tip(repo: &Repository) -> Result<Option<gix::Commit<'_>>, GitError> {
    Ok(match repo.try_find_reference(MAIN)? {
        Some(mut r) => Some(r.peel_to_commit()?),
        None => None,
    })
}

/// Makes one commit on top of `parent`: `edit` changes a copy of its tree, `message` is the
/// commit message and `signature` both its author and committer. Then points `main` and
/// `other` at it, each only if still at the tip expected. Returns the new commit.
pub(crate) fn commit_on_main(
    repo: &Repository,
    parent: Option<&gix::Commit<'_>>,
    signature: &gix::actor::Signature,
    message: String,
    other: &str,
    expected_other: Option<ObjectId>,
    edit: impl FnOnce(&mut gix::object::tree::Editor<'_>) -> Result<(), CheckpointError>,
) -> Result<ObjectId, CheckpointError> {
    let root = match parent {
        Some(commit) => commit.tree_id()?.detach(),
        None => ObjectId::empty_tree(repo.object_hash()),
    };
    let mut editor = repo.edit_tree(root)?;
    edit(&mut editor)?;
    let tree = editor.write()?.detach();

    let parent_id = parent.map(|p| p.id().detach());
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
    move_main_and(repo, other, oid, parent_id, expected_other, signature)?;
    Ok(oid)
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

    /// Takes the write lock, which serializes every commit on `main`.
    pub(crate) fn lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.write_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn repo(&self) -> Result<Repository, GitError> {
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
    /// `Corrupt` if the revision holds the document's content without readable metadata,
    /// otherwise the Git error.
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

    /// Describes the revision `revision` of `doc`. `None` if there is no such commit or it does
    /// not hold the document.
    ///
    /// # Errors
    /// `Corrupt` if the commit holds the document's content without readable metadata,
    /// otherwise the Git error.
    pub fn describe(
        &self,
        doc: DocId,
        revision: &RevisionId,
    ) -> Result<Option<RevisionInfo>, CheckpointError> {
        let repo = self.repo()?;
        match find_commit(&repo, revision.to_oid()?)? {
            Some(commit) => describe_commit(doc, &commit),
            None => Ok(None),
        }
    }

    /// Every revision of `doc`, oldest first: the commits reachable from the document's ref
    /// whose `docs/<doc>` folder differs from their parent's.
    ///
    /// # Errors
    /// `Corrupt` if any of them holds the document's content without readable metadata,
    /// otherwise the Git error.
    pub fn history(&self, doc: DocId) -> Result<Vec<RevisionInfo>, CheckpointError> {
        let repo = self.repo()?;
        let Some(head) = doc_head(&repo, doc)? else {
            return Ok(Vec::new());
        };
        let dir = format!("docs/{}", doc.0);
        let mut newest_first = Vec::new();
        let mut commit = repo.find_commit(head)?;
        let mut folder = folder_id(&commit, &dir)?;
        // A folder is never removed once written, so the walk can stop at the commit that
        // created it.
        while folder.is_some() {
            let parent = match commit.parent_ids().next() {
                Some(id) => Some(repo.find_commit(id.detach())?),
                None => None,
            };
            let parent_folder = match &parent {
                Some(p) => folder_id(p, &dir)?,
                None => None,
            };
            if folder != parent_folder
                && let Some(info) = describe_commit(doc, &commit)?
            {
                newest_first.push(info);
            }
            let Some(parent) = parent else {
                break;
            };
            commit = parent;
            folder = parent_folder;
        }
        newest_first.reverse();
        Ok(newest_first)
    }

    /// Every document with a revision, from the `refs/sharecraft/docs/<doc>` refs, in no
    /// particular order. A ref whose name is not a document id is skipped.
    ///
    /// # Errors
    /// Returns the Git error if the refs cannot be read.
    pub fn documents(&self) -> Result<Vec<DocId>, GitError> {
        let repo = self.repo()?;
        let refs = repo.references()?;
        let mut docs = Vec::new();
        for reference in refs.prefixed(DOC_REFS)? {
            let reference = reference?;
            let name = reference.name().as_bstr().to_string();
            if let Some(id) = name
                .strip_prefix(DOC_REFS)
                .and_then(|id| uuid::Uuid::parse_str(id).ok())
            {
                docs.push(DocId(id));
            }
        }
        Ok(docs)
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
        let _guard = self.lock();
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

        let parent = main_tip(&repo)?;

        // The document's folder is rebuilt from scratch: it holds these two files only.
        let dir = format!("docs/{}", c.doc.0);
        let signature = signature(first);
        let mut message = format!("Checkpoint {}\n", c.file_name);
        for (i, a) in credited.iter().skip(1).enumerate() {
            if i == 0 {
                message.push('\n');
            }
            // Writing to a String cannot fail.
            let _ = writeln!(message, "Co-authored-by: {} <{}>", a.name, email(a));
        }
        let oid = commit_on_main(
            &repo,
            parent.as_ref(),
            &signature,
            message,
            &doc_ref(c.doc),
            current,
            |editor| {
                let content = repo.write_blob(c.bytes)?.detach();
                let meta_blob = repo.write_blob(&meta_bytes)?.detach();
                editor.remove(dir.as_str())?;
                editor.upsert(format!("{dir}/content"), EntryKind::Blob, content)?;
                editor.upsert(format!("{dir}/meta.json"), EntryKind::Blob, meta_blob)?;
                Ok(())
            },
        )?;
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

    /// Points `main` and the document ref at `oid` in one transaction.
    fn move_refs(
        repo: &Repository,
        doc: DocId,
        oid: ObjectId,
        expected_main: Option<ObjectId>,
        expected_doc: Option<ObjectId>,
        signature: &gix::actor::Signature,
    ) -> Result<(), CheckpointError> {
        move_main_and(
            repo,
            &doc_ref(doc),
            oid,
            expected_main,
            expected_doc,
            signature,
        )
    }

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

    /// The id of a blob or a tree is no revision: reading or describing it finds nothing,
    /// rather than failing.
    #[test]
    fn object_that_is_not_a_commit_is_no_revision() {
        let dir = tempfile::tempdir().unwrap();
        let store = RevisionStore::open(&dir.path().join("lib.git")).unwrap();
        let doc = DocId(uuid::Uuid::new_v4());
        let authors = [Author {
            id: "u1".into(),
            name: "Ann".into(),
        }];
        let fields = Map::new();
        let Outcome::Written(first) = store
            .checkpoint(&Checkpoint {
                doc,
                file_name: "n.txt",
                bytes: b"x",
                base: None,
                authors: &authors,
                fields: &fields,
            })
            .unwrap()
        else {
            panic!("expected Written");
        };
        let repo = store.repo().unwrap();
        let commit = repo.find_commit(first.to_oid().unwrap()).unwrap();
        let tree = commit.tree_id().unwrap().detach();
        let blob = repo.write_blob(b"x").unwrap().detach();
        for oid in [tree, blob] {
            let id = RevisionId::from_oid(oid);
            assert!(store.read(doc, Some(&id)).unwrap().is_none(), "{id}");
            assert!(store.describe(doc, &id).unwrap().is_none(), "{id}");
        }
        assert!(store.read(doc, Some(&first)).unwrap().is_some());
    }

    #[test]
    fn revision_id_rejects_wrong_length_uppercase_and_non_hex() {
        let good = "0123456789abcdef0123456789abcdef01234567";
        assert!(good.parse::<RevisionId>().is_ok());
        for bad in [
            String::new(),
            good[..39].to_owned(),
            format!("{good}0"),
            good.to_uppercase(),
            "ABCDEF0123456789ABCDEF0123456789ABCDEF01".to_owned(),
            format!("g{}", &good[1..]),
            format!("{} ", &good[..39]),
        ] {
            assert!(bad.parse::<RevisionId>().is_err(), "{bad:?}");
            assert!(RevisionId::try_from(bad.clone()).is_err(), "{bad:?}");
            let json = serde_json::to_string(&bad).unwrap();
            assert!(
                serde_json::from_str::<RevisionId>(&json).is_err(),
                "{bad:?}"
            );
        }
    }
}
