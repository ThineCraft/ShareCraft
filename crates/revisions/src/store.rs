//! Git-backed storage of document checkpoints.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use collab_core::ids::DocId;
use git2::{ObjectType, Oid, Repository, RepositoryInitOptions, Signature, Tree};

use crate::meta::{Author, Meta};
use crate::{Checkpoint, CheckpointError, Outcome, RevisionId, Stored};

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
    let name_ok = !a.name.trim().is_empty()
        && !a
            .name
            .chars()
            .any(|c| c.is_control() || matches!(c, '<' | '>'));
    id_ok && name_ok
}

fn email(author: &Author) -> String {
    format!("{}@sharecraft.invalid", author.id)
}

fn read_blob(
    repo: &Repository,
    tree: &Tree<'_>,
    path: &str,
) -> Result<Option<Vec<u8>>, git2::Error> {
    match tree.get_path(Path::new(path)) {
        Ok(entry) => Ok(Some(repo.find_blob(entry.id())?.content().to_vec())),
        Err(e) if e.code() == git2::ErrorCode::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn read_at(repo: &Repository, doc: DocId, oid: Oid) -> Result<Option<Stored>, CheckpointError> {
    let commit = match repo.find_commit(oid) {
        Ok(c) => c,
        Err(e) if e.code() == git2::ErrorCode::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let tree = commit.tree()?;
    let dir = format!("docs/{}", doc.0);
    let content = read_blob(repo, &tree, &format!("{dir}/content"))?;
    let meta = read_blob(repo, &tree, &format!("{dir}/meta.json"))?;
    match (content, meta) {
        (Some(bytes), Some(meta)) => Ok(Some(Stored {
            revision: RevisionId::from_oid(oid),
            bytes,
            meta: serde_json::from_slice(&meta)?,
        })),
        _ => Ok(None),
    }
}

fn doc_head(repo: &Repository, doc: DocId) -> Result<Option<Oid>, git2::Error> {
    match repo.find_reference(&doc_ref(doc)) {
        Ok(r) => Ok(r.target()),
        Err(e) if e.code() == git2::ErrorCode::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

impl RevisionStore {
    /// Opens the repository at `repo_path`, creating a bare one if absent.
    ///
    /// # Errors
    /// Returns the Git error if the repository cannot be opened or created.
    pub fn open(repo_path: &Path) -> Result<Self, git2::Error> {
        let mut opts = RepositoryInitOptions::new();
        opts.bare(true).initial_head("main").mkpath(true);
        Repository::init_opts(repo_path, &opts)?;
        Ok(RevisionStore {
            path: repo_path.to_path_buf(),
            write_lock: Mutex::new(()),
        })
    }

    fn repo(&self) -> Result<Repository, git2::Error> {
        Repository::open_bare(&self.path)
    }

    /// The document's current revision, if it has one.
    ///
    /// # Errors
    /// Returns the Git error if the repository cannot be read.
    pub fn head(&self, doc: DocId) -> Result<Option<RevisionId>, git2::Error> {
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

        let parent = match repo.find_reference(MAIN) {
            Ok(r) => Some(r.peel_to_commit()?),
            Err(e) if e.code() == git2::ErrorCode::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        let root = parent.as_ref().map(git2::Commit::tree).transpose()?;
        let docs = match root.as_ref().and_then(|t| t.get_name("docs")) {
            Some(e) if e.kind() == Some(ObjectType::Tree) => Some(repo.find_tree(e.id())?),
            _ => None,
        };

        let mut doc_tree = repo.treebuilder(None)?;
        doc_tree.insert("content", repo.blob(c.bytes)?, 0o100_644)?;
        doc_tree.insert("meta.json", repo.blob(&meta_bytes)?, 0o100_644)?;
        let doc_tree_id = doc_tree.write()?;
        let mut docs_builder = repo.treebuilder(docs.as_ref())?;
        docs_builder.insert(c.doc.0.to_string(), doc_tree_id, 0o040_000)?;
        let docs_id = docs_builder.write()?;
        let mut root_builder = repo.treebuilder(root.as_ref())?;
        root_builder.insert("docs", docs_id, 0o040_000)?;
        let tree = repo.find_tree(root_builder.write()?)?;

        let signature = Signature::now(&first.name, &email(first))?;
        let mut message = format!("Checkpoint {}\n", c.file_name);
        for (i, a) in credited.iter().skip(1).enumerate() {
            if i == 0 {
                message.push('\n');
            }
            // Writing to a String cannot fail.
            let _ = writeln!(message, "Co-authored-by: {} <{}>", a.name, email(a));
        }

        let parents: Vec<&git2::Commit<'_>> = parent.iter().collect();
        // Fails if `main` moved since we read it, rather than dropping a concurrent commit.
        let oid = repo.commit(
            Some(MAIN),
            &signature,
            &signature,
            &message,
            &tree,
            &parents,
        )?;
        repo.reference(&doc_ref(c.doc), oid, true, "checkpoint")?;
        Ok(Outcome::Written(RevisionId::from_oid(oid)))
    }
}
