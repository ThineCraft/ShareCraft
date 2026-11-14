#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use collab_core::ids::DocId;
use revisions::{
    Author, BlobRef, Checkpoint, CheckpointError, Content, Outcome, RevisionId, RevisionStore,
};
use serde_json::{Map, Value};
use std::str::FromStr;
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
        blob: BlobRef::of(bytes),
        base,
        authors,
        fields,
    }
}

/// What a version saved with `bytes` stores: a pointer to them.
fn pointer(bytes: &[u8]) -> Content {
    Content::Blob(BlobRef::of(bytes))
}

fn written(o: Outcome) -> RevisionId {
    match o {
        Outcome::Written(r) => r,
        Outcome::Unchanged(r) => panic!("expected Written, got Unchanged({r})"),
    }
}

fn commit_count(path: &std::path::Path) -> usize {
    let repo = gix::open(path).unwrap();
    let main = repo.find_reference("refs/heads/main").unwrap().id();
    repo.rev_walk([main]).all().unwrap().count()
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
    assert_eq!(stored.content, pointer(b"hello"));
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
    let repo = gix::open(&f.path).unwrap();
    let tree = repo
        .find_reference("refs/heads/main")
        .unwrap()
        .peel_to_tree()
        .unwrap();
    let entry = tree
        .lookup_entry_by_path(format!("docs/{}/meta.json", d.0))
        .unwrap()
        .unwrap();
    let blob = entry.object().unwrap();
    let text = std::str::from_utf8(&blob.data).unwrap();
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
    let repo = gix::open(&f.path).unwrap();
    let commit = repo
        .find_commit(gix::ObjectId::from_hex(id.to_string().as_bytes()).unwrap())
        .unwrap();
    let msg = commit.message_raw().unwrap();
    assert_eq!(
        msg,
        "Checkpoint notes.txt\n\nCo-authored-by: Bob <u2@sharecraft.invalid>\n"
    );
    let sig = commit.author().unwrap();
    assert_eq!(sig.email, "u1@sharecraft.invalid");
    let committer = commit.committer().unwrap();
    assert_eq!(committer.name, "Ann");
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
    assert_eq!(
        f.store.read(db, None).unwrap().unwrap().content,
        pointer(b"b1")
    );
    assert_eq!(
        f.store.read(da, None).unwrap().unwrap().content,
        pointer(b"a2")
    );
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
    assert_eq!(old.content, pointer(b"old"));
    assert_eq!(old.revision, first);
    assert_eq!(
        f.store.read(d, None).unwrap().unwrap().content,
        pointer(b"new")
    );
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
fn rejects_author_name_git_would_empty() {
    // Git trims these from both ends of a signature name, which would leave nothing.
    for name in [";", "'", "\"", ",", ":", "\\", " ;: ", "'\"'"] {
        assert_invalid_author_writes_nothing(author("u1", name));
        assert!(!revisions::is_valid_author_name(name), "{name:?}");
    }
    // Not trimmed to nothing, so these are kept.
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
    let repo = gix::open(&f.path).unwrap();
    let main = repo.find_reference("refs/heads/main").unwrap().id();
    let docref = repo
        .find_reference(&format!("refs/sharecraft/docs/{}", d.0))
        .unwrap()
        .id();
    assert_eq!(main.to_string(), id.to_string());
    assert_eq!(docref.to_string(), id.to_string());
}

fn oid(id: &RevisionId) -> gix::ObjectId {
    gix::ObjectId::from_hex(id.to_string().as_bytes()).unwrap()
}

#[test]
fn history_lists_only_this_documents_revisions_oldest_first() {
    let f = fixture();
    let (da, db) = (doc(), doc());
    let a = [author("u1", "Ann")];
    let fields = Map::new();
    let a1 = written(
        f.store
            .checkpoint(&cp(da, b"a1", None, &a, &fields))
            .unwrap(),
    );
    let b1 = written(
        f.store
            .checkpoint(&cp(db, b"b1", None, &a, &fields))
            .unwrap(),
    );
    let a2 = written(
        f.store
            .checkpoint(&cp(da, b"a2", Some(&a1), &a, &fields))
            .unwrap(),
    );
    let b2 = written(
        f.store
            .checkpoint(&cp(db, b"b2", Some(&b1), &a, &fields))
            .unwrap(),
    );
    let a3 = written(
        f.store
            .checkpoint(&cp(da, b"a3", Some(&a2), &a, &fields))
            .unwrap(),
    );

    let history = f.store.history(da).unwrap();
    let ids: Vec<_> = history.iter().map(|r| r.revision.clone()).collect();
    assert_eq!(ids, vec![a1, a2, a3]);
    assert!(
        history
            .iter()
            .all(|r| r.doc == da && r.file_name == "notes.txt")
    );
    let ids: Vec<_> = f
        .store
        .history(db)
        .unwrap()
        .into_iter()
        .map(|r| r.revision)
        .collect();
    assert_eq!(ids, vec![b1, b2]);
    assert!(f.store.history(doc()).unwrap().is_empty());
}

#[test]
fn describe_reports_all_credited_authors() {
    let f = fixture();
    let d = doc();
    let a = [author("u1", "Ann Lee"), author("u2", "O'Brien, Pat")];
    let mut fields = Map::new();
    fields.insert("title".into(), Value::String("Plan".into()));
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let id = written(f.store.checkpoint(&cp(d, b"x", None, &a, &fields)).unwrap());
    let info = f.store.describe(d, &id).unwrap().unwrap();
    assert_eq!(info.revision, id);
    assert_eq!(info.doc, d);
    assert_eq!(info.authors, a.to_vec());
    assert_eq!(info.file_name, "notes.txt");
    assert_eq!(info.fields, fields);
    let committed = u64::try_from(info.committed_at).unwrap();
    assert!(
        committed + 1 >= before && committed <= before + 60,
        "{committed}"
    );
    // A revision of another document, or one that does not exist, describes nothing.
    assert_eq!(f.store.describe(doc(), &id).unwrap(), None);
    let bogus: RevisionId = "0123456789abcdef0123456789abcdef01234567".parse().unwrap();
    assert_eq!(f.store.describe(d, &bogus).unwrap(), None);
}

#[test]
fn documents_lists_every_saved_document() {
    let f = fixture();
    assert!(f.store.documents().unwrap().is_empty());
    let a = [author("u1", "Ann")];
    let fields = Map::new();
    let mut docs: Vec<DocId> = (0..3).map(|_| doc()).collect();
    for &d in &docs {
        written(f.store.checkpoint(&cp(d, b"x", None, &a, &fields)).unwrap());
    }
    let mut listed = f.store.documents().unwrap();
    listed.sort();
    docs.sort();
    assert_eq!(listed, docs);
}

/// Replaces `doc`'s folder at its head with one holding `content` and, if given, `meta`, as a
/// new commit both refs point at.
fn tamper(f: &Fixture, d: DocId, meta: Option<&[u8]>) -> RevisionId {
    let repo = gix::open(&f.path).unwrap();
    let head = f.store.head(d).unwrap().unwrap();
    let tip = repo.find_commit(oid(&head)).unwrap();
    let dir = format!("docs/{}", d.0);
    let mut editor = repo.edit_tree(tip.tree_id().unwrap()).unwrap();
    editor.remove(dir.as_str()).unwrap();
    let content = repo.write_blob(b"content").unwrap().detach();
    editor
        .upsert(
            format!("{dir}/content"),
            gix::object::tree::EntryKind::Blob,
            content,
        )
        .unwrap();
    if let Some(meta) = meta {
        let blob = repo.write_blob(meta).unwrap().detach();
        editor
            .upsert(
                format!("{dir}/meta.json"),
                gix::object::tree::EntryKind::Blob,
                blob,
            )
            .unwrap();
    }
    let tree = editor.write().unwrap().detach();
    let sig = gix::actor::Signature {
        name: "Ann".into(),
        email: "u1@sharecraft.invalid".into(),
        time: gix::date::Time::now_utc(),
    };
    let commit = gix::objs::Commit {
        tree,
        parents: [tip.id].into_iter().collect(),
        author: sig.clone(),
        committer: sig,
        encoding: None,
        message: "Checkpoint notes.txt\n".into(),
        extra_headers: Vec::new(),
    };
    let new = repo.write_object(&commit).unwrap().detach();
    for name in [
        "refs/heads/main".to_owned(),
        format!("refs/sharecraft/docs/{}", d.0),
    ] {
        repo.reference(
            name.as_str(),
            new,
            gix::refs::transaction::PreviousValue::Any,
            "test",
        )
        .unwrap();
    }
    RevisionId::from_str(&new.to_string()).unwrap()
}

fn assert_corrupt<T: std::fmt::Debug>(result: Result<T, CheckpointError>, at: &RevisionId) {
    match result {
        Err(CheckpointError::Corrupt { revision, reason }) => {
            assert_eq!(&revision, at);
            assert!(!reason.is_empty());
        }
        other => panic!("expected Corrupt at {at}, got {other:?}"),
    }
}

#[test]
fn content_without_metadata_is_corrupt() {
    for meta in [None, Some(&b"{ not json"[..])] {
        let f = fixture();
        let d = doc();
        let a = [author("u1", "Ann")];
        let fields = Map::new();
        written(f.store.checkpoint(&cp(d, b"x", None, &a, &fields)).unwrap());
        let bad = tamper(&f, d, meta);
        assert_corrupt(f.store.read(d, None), &bad);
        assert_corrupt(f.store.read(d, Some(&bad)), &bad);
        assert_corrupt(f.store.describe(d, &bad), &bad);
        assert_corrupt(f.store.history(d), &bad);
    }
}

/// A document saved before the content store keeps its bytes inline. Saving the same bytes
/// again, now as a pointer, is unchanged (compared by hash and size) and writes nothing; other
/// bytes are written as a pointer, and the old version still reads inline.
#[test]
fn unchanged_old_document_writes_nothing() {
    let f = fixture();
    let d = doc();
    let a = [author("u1", "Ann")];
    let fields = Map::new();
    let old = written(
        f.store
            .checkpoint_inline(&cp(d, b"", None, &a, &fields), b"from before")
            .unwrap(),
    );
    let stored = f.store.read(d, None).unwrap().unwrap();
    assert_eq!(stored.content, Content::Inline(b"from before".to_vec()));
    let commits = commit_count(&f.path);

    let again = f
        .store
        .checkpoint(&cp(d, b"from before", Some(&old), &a, &fields))
        .unwrap();
    assert_eq!(again, Outcome::Unchanged(old.clone()));
    assert_eq!(commit_count(&f.path), commits);

    let new = written(
        f.store
            .checkpoint(&cp(d, b"changed", Some(&old), &a, &fields))
            .unwrap(),
    );
    assert_eq!(commit_count(&f.path), commits + 1);
    assert_eq!(
        f.store.read(d, None).unwrap().unwrap().content,
        pointer(b"changed")
    );
    let first = f.store.read(d, Some(&old)).unwrap().unwrap();
    assert_eq!(first.content, Content::Inline(b"from before".to_vec()));

    // The history describes both: the old one by its inline size, the new one by its pointer.
    let history = f.store.history(d).unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!((history[0].blob, history[0].size), (None, 11));
    assert_eq!(
        (
            history[1].revision.clone(),
            history[1].blob,
            history[1].size
        ),
        (new, Some(BlobRef::of(b"changed")), 7)
    );
}
