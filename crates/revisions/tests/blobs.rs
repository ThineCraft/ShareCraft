#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::io::Read;

use revisions::{BlobError, BlobRef, BlobStore, Content};

fn store() -> (tempfile::TempDir, BlobStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = BlobStore::open(&dir.path().join("blobs")).unwrap();
    (dir, store)
}

/// Every file under the store's folder, `tmp` included, relative to it.
fn files(store: &BlobStore) -> Vec<String> {
    fn walk(dir: &std::path::Path, root: &std::path::Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, root, out);
            } else {
                let relative = path.strip_prefix(root).unwrap();
                out.push(relative.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    let mut out = Vec::new();
    walk(store.dir(), store.dir(), &mut out);
    out.sort();
    out
}

#[test]
fn blob_written_once_and_named_by_hash() {
    let (_dir, store) = store();
    let bytes = b"the same bytes, twice".repeat(10_000);
    let first = store.write_from(&bytes[..], u64::MAX).unwrap();
    assert_eq!(first, BlobRef::of(&bytes));
    assert_eq!(first.size, bytes.len() as u64);
    let hex = first.hex();
    assert_eq!(files(&store), vec![format!("{}/{hex}", &hex[..2])]);
    let path = store.path_of(&first.sha256);
    let written_at = std::fs::metadata(&path).unwrap().modified().unwrap();

    // The same content again is not rewritten: one file, untouched, and nothing left in tmp.
    std::thread::sleep(std::time::Duration::from_millis(20));
    let second = store.write_from(&bytes[..], u64::MAX).unwrap();
    assert_eq!(second, first);
    assert_eq!(files(&store), vec![format!("{}/{hex}", &hex[..2])]);
    assert_eq!(
        std::fs::metadata(&path).unwrap().modified().unwrap(),
        written_at
    );

    let mut read = Vec::new();
    store
        .open_blob(&first.sha256)
        .unwrap()
        .unwrap()
        .read_to_end(&mut read)
        .unwrap();
    assert_eq!(read, bytes);
    assert_eq!(store.read_all(&Content::Blob(first)).unwrap(), bytes);

    // Other content is another file.
    let other = store.write_from(&b"other"[..], u64::MAX).unwrap();
    assert_ne!(other, first);
    assert_eq!(files(&store).len(), 2);
    assert_eq!(store.list().unwrap().len(), 2);

    // Erasing removes the one file; content not stored is reported missing.
    assert!(store.erase(&first.sha256).unwrap());
    assert!(!store.erase(&first.sha256).unwrap());
    assert!(store.open_blob(&first.sha256).unwrap().is_none());
    assert!(matches!(
        store.read_all(&Content::Blob(first)),
        Err(BlobError::Missing(_))
    ));
}

#[test]
fn pointer_round_trip_exact_bytes() {
    for bytes in [&b""[..], b"x", &[0u8; 100_000][..]] {
        let blob = BlobRef::of(bytes);
        let pointer = blob.to_pointer();
        assert_eq!(
            pointer,
            format!(
                "sharecraft-blob 1\nsha256 {}\nsize {}\n",
                blob.hex(),
                bytes.len()
            )
            .into_bytes()
        );
        assert_eq!(BlobRef::from_pointer(&pointer), Some(blob));
        assert_eq!(
            BlobRef::from_pointer(&pointer).unwrap().to_pointer(),
            pointer
        );
        assert_eq!(Content::from_git(pointer), Content::Blob(blob));
    }
    // The empty content's well-known hash.
    assert_eq!(
        String::from_utf8(BlobRef::of(b"").to_pointer()).unwrap(),
        "sharecraft-blob 1\n\
         sha256 e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\n\
         size 0\n"
    );
    // Anything that does not start with the header is an old version's inline bytes.
    let old = b"plain old text".to_vec();
    assert_eq!(Content::from_git(old.clone()), Content::Inline(old.clone()));
    assert_eq!(Content::Inline(old.clone()).blob_ref(), BlobRef::of(&old));
}

#[test]
fn too_large_writes_nothing() {
    let (_dir, store) = store();
    let bytes = vec![7u8; 200_000];
    let err = store.write_from(&bytes[..], 199_999).unwrap_err();
    assert!(matches!(err, BlobError::TooLarge { limit: 199_999 }));
    assert!(files(&store).is_empty(), "{:?}", files(&store));
    assert!(!store.contains(&BlobRef::of(&bytes).sha256));

    // Exactly the limit is allowed.
    let blob = store.write_from(&bytes[..], 200_000).unwrap();
    assert!(store.contains(&blob.sha256));

    // A reader that fails leaves nothing either.
    struct Broken(usize);
    impl Read for Broken {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.0 == 0 {
                return Err(std::io::Error::other("connection reset"));
            }
            self.0 -= 1;
            buf[0] = 1;
            Ok(1)
        }
    }
    assert!(store.write_from(Broken(10), u64::MAX).is_err());
    assert_eq!(files(&store).len(), 1);
}

#[test]
fn part_file_is_adopted_only_at_its_size() {
    let (_dir, store) = store();
    let part = store.tmp_dir().join("upload.part");
    std::fs::write(&part, b"uploaded bytes").unwrap();
    let blob = BlobRef::of(b"uploaded bytes");
    let wrong = BlobRef {
        size: blob.size + 1,
        ..blob
    };
    assert!(matches!(
        store.adopt_part(&part, wrong),
        Err(BlobError::Mismatch)
    ));
    assert!(part.is_file(), "a mismatched part stays where it is");
    store.adopt_part(&part, blob).unwrap();
    assert!(!part.exists());
    assert_eq!(
        store.read_all(&Content::Blob(blob)).unwrap(),
        b"uploaded bytes"
    );
}
