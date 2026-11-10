#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use collab_core::ids::DocId;
use revisions::{Author, Checkpoint, CheckpointError, Outcome, RevisionId, RevisionStore};
use serde_json::{Map, Value};
use uuid::Uuid;

fn author(id: &str, name: &str) -> Author {
    Author {
        id: id.into(),
        name: name.into(),
    }
}

fn doc() -> DocId {
    DocId(Uuid::new_v4())
}

struct Fixture {
    _dir: tempfile::TempDir,
    path: std::path::PathBuf,
    store: RevisionStore,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.git");
    let store = RevisionStore::open(&path).unwrap();
    Fixture {
        _dir: dir,
        path,
        store,
    }
}

fn cp<'a>(
    doc: DocId,
    bytes: &'a [u8],
    base: Option<&'a RevisionId>,
    authors: &'a [Author],
    fields: &'a Map<String, Value>,
) -> Checkpoint<'a> {
    Checkpoint {
        doc,
        file_name: "notes.txt",
        bytes,
        base,
        authors,
        fields,
    }
}

fn written(o: Outcome) -> RevisionId {
    match o {
        Outcome::Written(r) => r,
        Outcome::Unchanged(r) => panic!("expected Written, got Unchanged({r})"),
    }
}

fn commit_count(path: &std::path::Path) -> usize {
    let repo = git2::Repository::open(path).unwrap();
    let mut walk = repo.revwalk().unwrap();
    walk.push_ref("refs/heads/main").unwrap();
    walk.count()
}

#[test]
fn first_checkpoint_writes_content_and_meta() {
    let f = fixture();
    let d = doc();
    let a = [author("u1", "Ann")];
    let fields = Map::new();
    let id = written(
        f.store
            .checkpoint(&cp(d, b"hello", None, &a, &fields))
            .unwrap(),
    );
    let stored = f.store.read(d, None).unwrap().unwrap();
    assert_eq!(stored.bytes, b"hello");
    assert_eq!(stored.revision, id);
    assert_eq!(stored.meta.schema, 1);
    assert_eq!(stored.meta.doc_id, d);
    assert_eq!(stored.meta.file_name, "notes.txt");
    assert_eq!(stored.meta.contributors, vec![author("u1", "Ann")]);
    assert_eq!(f.store.head(d).unwrap(), Some(id));
}

#[test]
fn meta_json_is_pretty_with_trailing_newline() {
    let f = fixture();
    let d = doc();
    let a = [author("u1", "Ann")];
    let mut fields = Map::new();
    fields.insert("title".into(), Value::String("T".into()));
    written(f.store.checkpoint(&cp(d, b"x", None, &a, &fields)).unwrap());
    let repo = git2::Repository::open(&f.path).unwrap();
    let tree = repo
        .revparse_single("refs/heads/main")
        .unwrap()
        .peel_to_tree()
        .unwrap();
    let entry = tree
        .get_path(std::path::Path::new(&format!("docs/{}/meta.json", d.0)))
        .unwrap();
    let blob = repo.find_blob(entry.id()).unwrap();
    let text = std::str::from_utf8(blob.content()).unwrap();
    let expected = format!(
        "{{\n  \"schema\": 1,\n  \"doc_id\": \"{}\",\n  \"file_name\": \"notes.txt\",\n  \"fields\": {{\n    \"title\": \"T\"\n  }},\n  \"contributors\": [\n    {{\n      \"id\": \"u1\",\n      \"name\": \"Ann\"\n    }}\n  ]\n}}\n",
        d.0
    );
    assert_eq!(text, expected);
}

#[test]
fn stale_base_is_refused() {
    let f = fixture();
    let d = doc();
    let a = [author("u1", "Ann")];
    let fields = Map::new();
    let first = written(
        f.store
            .checkpoint(&cp(d, b"one", None, &a, &fields))
            .unwrap(),
    );
    let err = f
        .store
        .checkpoint(&cp(d, b"two", None, &a, &fields))
        .unwrap_err();
    match err {
        CheckpointError::Stale { current } => assert_eq!(current, Some(first.clone())),
        other => panic!("expected Stale, got {other:?}"),
    }
    assert_eq!(f.store.head(d).unwrap(), Some(first));
}

#[test]
fn stale_when_base_given_but_nothing_stored() {
    let f = fixture();
    let a = [author("u1", "Ann")];
    let fields = Map::new();
    let bogus: RevisionId = "0123456789abcdef0123456789abcdef01234567".parse().unwrap();
    let err = f
        .store
        .checkpoint(&cp(doc(), b"one", Some(&bogus), &a, &fields))
        .unwrap_err();
    assert!(matches!(err, CheckpointError::Stale { current: None }));
}

#[test]
fn unchanged_content_writes_no_commit() {
    let f = fixture();
    let d = doc();
    let a = [author("u1", "Ann")];
    let fields = Map::new();
    let first = written(
        f.store
            .checkpoint(&cp(d, b"same", None, &a, &fields))
            .unwrap(),
    );
    let b = [author("u2", "Bob")];
    let out = f
        .store
        .checkpoint(&cp(d, b"same", Some(&first), &b, &fields))
        .unwrap();
    assert_eq!(out, Outcome::Unchanged(first));
    assert_eq!(commit_count(&f.path), 1);
    // Contributors alone do not make a revision.
    let stored = f.store.read(d, None).unwrap().unwrap();
    assert_eq!(stored.meta.contributors.len(), 1);
}

#[test]
fn metadata_only_change_is_a_revision() {
    let f = fixture();
    let d = doc();
    let a = [author("u1", "Ann")];
    let empty = Map::new();
    let first = written(
        f.store
            .checkpoint(&cp(d, b"same", None, &a, &empty))
            .unwrap(),
    );
    let mut fields = Map::new();
    fields.insert("title".into(), Value::String("New".into()));
    let second = written(
        f.store
            .checkpoint(&cp(d, b"same", Some(&first), &a, &fields))
            .unwrap(),
    );
    assert_ne!(first, second);
    assert_eq!(commit_count(&f.path), 2);
}

#[test]
fn several_authors_are_credited() {
    let f = fixture();
    let d = doc();
    let a = [author("u1", "Ann"), author("u2", "Bob")];
    let fields = Map::new();
    let id = written(f.store.checkpoint(&cp(d, b"x", None, &a, &fields)).unwrap());
    let repo = git2::Repository::open(&f.path).unwrap();
    let commit = repo
        .find_commit(git2::Oid::from_str(&id.to_string()).unwrap())
        .unwrap();
    let msg = commit.message().unwrap();
    assert_eq!(
        msg,
        "Checkpoint notes.txt\n\nCo-authored-by: Bob <u2@sharecraft.invalid>\n"
    );
    let sig = commit.author();
    assert_eq!(sig.email().unwrap(), "u1@sharecraft.invalid");
    let committer = commit.committer();
    assert_eq!(committer.name().unwrap(), "Ann");
    let stored = f.store.read(d, None).unwrap().unwrap();
    assert_eq!(stored.meta.contributors, a.to_vec());
}

#[test]
fn contributors_accumulate_in_first_seen_order() {
    let f = fixture();
    let d = doc();
    let fields = Map::new();
    let a1 = [author("u1", "Ann")];
    let first = written(
        f.store
            .checkpoint(&cp(d, b"1", None, &a1, &fields))
            .unwrap(),
    );
    let a2 = [author("u2", "Bob"), author("u1", "Ann")];
    written(
        f.store
            .checkpoint(&cp(d, b"2", Some(&first), &a2, &fields))
            .unwrap(),
    );
    let stored = f.store.read(d, None).unwrap().unwrap();
    assert_eq!(
        stored.meta.contributors,
        vec![author("u1", "Ann"), author("u2", "Bob")]
    );
}

#[test]
fn documents_do_not_block_each_other() {
    let f = fixture();
    let (da, db) = (doc(), doc());
    let a = [author("u1", "Ann")];
    let fields = Map::new();
    let a1 = written(
        f.store
            .checkpoint(&cp(da, b"a1", None, &a, &fields))
            .unwrap(),
    );
    written(
        f.store
            .checkpoint(&cp(db, b"b1", None, &a, &fields))
            .unwrap(),
    );
    let a2 = f
        .store
        .checkpoint(&cp(da, b"a2", Some(&a1), &a, &fields))
        .unwrap();
    assert!(matches!(a2, Outcome::Written(_)));
    // Both documents' content survives on main.
    assert_eq!(f.store.read(db, None).unwrap().unwrap().bytes, b"b1");
    assert_eq!(f.store.read(da, None).unwrap().unwrap().bytes, b"a2");
}

#[test]
fn read_at_earlier_revision_returns_earlier_bytes() {
    let f = fixture();
    let d = doc();
    let a = [author("u1", "Ann")];
    let fields = Map::new();
    let first = written(
        f.store
            .checkpoint(&cp(d, b"old", None, &a, &fields))
            .unwrap(),
    );
    written(
        f.store
            .checkpoint(&cp(d, b"new", Some(&first), &a, &fields))
            .unwrap(),
    );
    let old = f.store.read(d, Some(&first)).unwrap().unwrap();
    assert_eq!(old.bytes, b"old");
    assert_eq!(old.revision, first);
    assert_eq!(f.store.read(d, None).unwrap().unwrap().bytes, b"new");
}

#[test]
fn read_unknown_document_is_none() {
    let f = fixture();
    assert!(f.store.read(doc(), None).unwrap().is_none());
    assert_eq!(f.store.head(doc()).unwrap(), None);
}

#[test]
fn rejects_file_name_with_separator() {
    let f = fixture();
    let a = [author("u1", "Ann")];
    let fields = Map::new();
    for bad in ["a/b", "a\\b", "", "a\nb", "a\u{0}b"] {
        let mut c = cp(doc(), b"x", None, &a, &fields);
        c.file_name = bad;
        assert!(
            matches!(
                f.store.checkpoint(&c),
                Err(CheckpointError::InvalidFileName)
            ),
            "{bad:?}"
        );
    }
}

#[test]
fn rejects_empty_authors() {
    let f = fixture();
    let fields = Map::new();
    let err = f
        .store
        .checkpoint(&cp(doc(), b"x", None, &[], &fields))
        .unwrap_err();
    assert!(matches!(err, CheckpointError::NoAuthors));
}

#[test]
fn reopening_keeps_history() {
    let f = fixture();
    let d = doc();
    let a = [author("u1", "Ann")];
    let fields = Map::new();
    let first = written(f.store.checkpoint(&cp(d, b"x", None, &a, &fields)).unwrap());
    let again = RevisionStore::open(&f.path).unwrap();
    assert_eq!(again.head(d).unwrap(), Some(first));
}

#[test]
fn concurrent_checkpoints_of_different_documents_all_land() {
    let f = fixture();
    let store = std::sync::Arc::new(f.store);
    let docs: Vec<DocId> = (0..8).map(|_| doc()).collect();
    let handles: Vec<_> = docs
        .iter()
        .map(|&d| {
            let s = store.clone();
            std::thread::spawn(move || {
                let a = [author("u1", "Ann")];
                let fields = Map::new();
                s.checkpoint(&cp(d, b"x", None, &a, &fields)).unwrap();
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    for d in docs {
        assert!(store.read(d, None).unwrap().is_some());
    }
    assert_eq!(commit_count(&f.path), 8);
}

fn assert_invalid_author_writes_nothing(a: Author) {
    let f = fixture();
    let d = doc();
    let fields = Map::new();
    let authors = [a];
    let err = f
        .store
        .checkpoint(&cp(d, b"x", None, &authors, &fields))
        .unwrap_err();
    assert!(matches!(err, CheckpointError::InvalidAuthor), "{err:?}");
    assert_eq!(f.store.head(d).unwrap(), None);
}

#[test]
fn rejects_author_name_with_newline() {
    assert_invalid_author_writes_nothing(author("u1", "Ann\nCo-authored-by: Evil <e@x>"));
}

#[test]
fn rejects_author_name_with_angle_brackets() {
    assert_invalid_author_writes_nothing(author("u1", "Ann <a>"));
    assert_invalid_author_writes_nothing(author("u1", "Ann>"));
}

#[test]
fn rejects_author_id_with_at_or_space() {
    assert_invalid_author_writes_nothing(author("u@1", "Ann"));
    assert_invalid_author_writes_nothing(author("u 1", "Ann"));
    assert_invalid_author_writes_nothing(author("", "Ann"));
}

#[test]
fn rejects_blank_author_name() {
    assert_invalid_author_writes_nothing(author("u1", "   "));
    assert_invalid_author_writes_nothing(author("u1", ""));
}

#[test]
fn rejects_author_name_libgit2_would_empty() {
    // libgit2 trims these from both ends of a signature name and refuses an empty result.
    for name in [";", "'", "\"", ",", ":", "\\", " ;: ", "'\"'"] {
        assert_invalid_author_writes_nothing(author("u1", name));
        assert!(!revisions::is_valid_author_name(name), "{name:?}");
    }
    // Trimmed by libgit2 but not to nothing, so these are kept.
    for name in [".", "...", ":)"] {
        assert!(revisions::is_valid_author_name(name), "{name:?}");
        let f = fixture();
        let d = doc();
        let fields = Map::new();
        let authors = [author("u1", name)];
        written(
            f.store
                .checkpoint(&cp(d, b"x", None, &authors, &fields))
                .unwrap(),
        );
    }
}

#[test]
fn rejects_invalid_further_author() {
    let f = fixture();
    let d = doc();
    let fields = Map::new();
    let authors = [author("u1", "Ann"), author("u2", "Bob\n")];
    let err = f
        .store
        .checkpoint(&cp(d, b"x", None, &authors, &fields))
        .unwrap_err();
    assert!(matches!(err, CheckpointError::InvalidAuthor));
    assert_eq!(f.store.head(d).unwrap(), None);
}

#[test]
fn ordinary_names_are_accepted_and_kept_verbatim() {
    let f = fixture();
    let d = doc();
    let fields = Map::new();
    let authors = [
        author("u1", "Zoë Müller"),
        author("u2", "O'Brien, Pat (Ops)"),
    ];
    written(
        f.store
            .checkpoint(&cp(d, b"x", None, &authors, &fields))
            .unwrap(),
    );
    let stored = f.store.read(d, None).unwrap().unwrap();
    assert_eq!(stored.meta.contributors, authors.to_vec());
}

#[test]
fn both_refs_point_at_the_new_commit_after_checkpoint() {
    let f = fixture();
    let d = doc();
    let a = [author("u1", "Ann")];
    let fields = Map::new();
    let id = written(f.store.checkpoint(&cp(d, b"x", None, &a, &fields)).unwrap());
    let repo = git2::Repository::open(&f.path).unwrap();
    let main = repo.refname_to_id("refs/heads/main").unwrap();
    let docref = repo
        .refname_to_id(&format!("refs/sharecraft/docs/{}", d.0))
        .unwrap();
    assert_eq!(main.to_string(), id.to_string());
    assert_eq!(docref.to_string(), id.to_string());
}
