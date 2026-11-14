//! The content store: file bytes outside Git, one file per distinct content, named by its
//! SHA-256.
//!
//! Layout: `<dir>/<first two hex digits>/<sha256>`, each file written once and never modified;
//! new content is written under `<dir>/tmp/`, flushed, then renamed into place. Git keeps, for
//! each version, a small pointer naming the hash and size (see [`BlobRef::to_pointer`]).

use std::fmt;
use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use sha2::{Digest, Sha256};

/// The first line of every pointer.
const POINTER_HEADER: &str = "sharecraft-blob 1\n";
/// How much is read and written at a time.
const CHUNK: usize = 64 * 1024;
/// The folder under the store that holds content still being written.
const TMP: &str = "tmp";

/// One stored content: its SHA-256 and its size in bytes.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlobRef {
    /// The SHA-256 of the bytes.
    pub sha256: [u8; 32],
    /// How many bytes there are.
    pub size: u64,
}

impl fmt::Debug for BlobRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BlobRef({}, {})", self.hex(), self.size)
    }
}

impl BlobRef {
    /// The hash and size of `bytes`, computed in memory.
    #[must_use]
    pub fn of(bytes: &[u8]) -> BlobRef {
        BlobRef {
            sha256: Sha256::digest(bytes).into(),
            size: bytes.len() as u64,
        }
    }

    /// The hash as 64 lowercase hexadecimal digits.
    #[must_use]
    pub fn hex(&self) -> String {
        hex(&self.sha256)
    }

    /// The pointer Git stores for this content, exactly
    /// `sharecraft-blob 1\nsha256 <64 lowercase hex>\nsize <decimal bytes>\n`.
    #[must_use]
    pub fn to_pointer(&self) -> Vec<u8> {
        format!(
            "{POINTER_HEADER}sha256 {}\nsize {}\n",
            self.hex(),
            self.size
        )
        .into_bytes()
    }

    /// The content `bytes` points at, if they are exactly a pointer as
    /// [`BlobRef::to_pointer`] writes it; `None` for anything else, which is inline content.
    #[must_use]
    pub fn from_pointer(bytes: &[u8]) -> Option<BlobRef> {
        let text = std::str::from_utf8(bytes).ok()?;
        let rest = text.strip_prefix(POINTER_HEADER)?;
        let rest = rest.strip_prefix("sha256 ")?;
        let (hash, rest) = rest.split_once('\n')?;
        let sha256 = parse_hex(hash)?;
        let size = rest.strip_prefix("size ")?.strip_suffix('\n')?;
        // Only the canonical decimal form, so a pointer reads back to the same bytes.
        let canonical = !size.is_empty()
            && size.bytes().all(|b| b.is_ascii_digit())
            && (size == "0" || !size.starts_with('0'));
        if !canonical {
            return None;
        }
        Some(BlobRef {
            sha256,
            size: size.parse().ok()?,
        })
    }
}

/// `bytes` as lowercase hexadecimal.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut text = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        // Writing to a String cannot fail.
        let _ = write!(text, "{b:02x}");
    }
    text
}

/// The 32 bytes written as `text`, 64 lowercase hexadecimal digits; `None` for anything else.
#[must_use]
pub fn parse_hex(text: &str) -> Option<[u8; 32]> {
    let digits = text.as_bytes();
    if digits.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    let (pairs, _) = digits.as_chunks::<2>();
    for (byte, [high, low]) in out.iter_mut().zip(pairs) {
        *byte = (hex_value(*high)? << 4) | hex_value(*low)?;
    }
    Some(out)
}

fn hex_value(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        _ => None,
    }
}

/// What a saved version holds: a pointer into the content store, or, for a version saved
/// before the store existed, the bytes themselves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Content {
    /// The bytes are in the content store.
    Blob(BlobRef),
    /// An old version whose bytes are stored inline in Git.
    Inline(Vec<u8>),
}

impl Content {
    /// The content as Git stores it: a pointer if `bytes` is exactly one, otherwise inline.
    #[must_use]
    pub fn from_git(bytes: Vec<u8>) -> Content {
        match BlobRef::from_pointer(&bytes) {
            Some(blob) => Content::Blob(blob),
            None => Content::Inline(bytes),
        }
    }

    /// The hash and size of the content; an inline version is hashed.
    #[must_use]
    pub fn blob_ref(&self) -> BlobRef {
        match self {
            Content::Blob(blob) => *blob,
            Content::Inline(bytes) => BlobRef::of(bytes),
        }
    }

    /// The content's size in bytes.
    #[must_use]
    pub fn size(&self) -> u64 {
        match self {
            Content::Blob(blob) => blob.size,
            Content::Inline(bytes) => bytes.len() as u64,
        }
    }
}

/// Why the content store failed.
#[derive(Debug, thiserror::Error)]
pub enum BlobError {
    /// More bytes arrived than the limit allows; nothing was stored.
    #[error("the content is larger than {limit} bytes")]
    TooLarge {
        /// The limit passed.
        limit: u64,
    },
    /// A part file does not hold what it was expected to.
    #[error("the part file does not match its expected size or hash")]
    Mismatch,
    /// A pointer names content the store does not have.
    #[error("content {0} is missing from the content store")]
    Missing(String),
    /// Reading or writing failed.
    #[error("content store: {0}")]
    Io(#[from] std::io::Error),
}

/// The content store in one folder. Every call works on the file system directly, so the store
/// is `Send + Sync` and may be shared.
#[derive(Debug)]
pub struct BlobStore {
    dir: PathBuf,
}

impl BlobStore {
    /// Opens the store at `dir`, creating it and its `tmp` folder if absent.
    ///
    /// # Errors
    /// The I/O error if a folder cannot be created.
    pub fn open(dir: &Path) -> Result<BlobStore, BlobError> {
        std::fs::create_dir_all(dir.join(TMP))?;
        Ok(BlobStore {
            dir: dir.to_path_buf(),
        })
    }

    /// The folder the store lives in.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The folder for content still being written, on the same volume as the store, so a
    /// finished file is renamed into place.
    #[must_use]
    pub fn tmp_dir(&self) -> PathBuf {
        self.dir.join(TMP)
    }

    /// Where the content with `sha256` is, whether or not it is there.
    #[must_use]
    pub fn path_of(&self, sha256: &[u8; 32]) -> PathBuf {
        let name = hex(sha256);
        let prefix = name.get(..2).unwrap_or_default().to_owned();
        self.dir.join(prefix).join(name)
    }

    /// Whether the content with `sha256` is stored.
    #[must_use]
    pub fn contains(&self, sha256: &[u8; 32]) -> bool {
        self.path_of(sha256).is_file()
    }

    /// Reads everything `reader` gives into the store, hashing it on the way, and answers what
    /// it stored. At most `limit` bytes are taken: one more fails with `TooLarge` and stores
    /// nothing. Content already stored is not written again. Never holds more than one chunk.
    ///
    /// # Errors
    /// `TooLarge`, or the I/O error of the reader or the file system; nothing is stored then.
    pub fn write_from(&self, reader: impl Read, limit: u64) -> Result<BlobRef, BlobError> {
        let (tmp, blob) = self.write_tmp(reader, limit)?;
        self.place(&tmp, blob)?;
        Ok(blob)
    }

    /// The first half of [`BlobStore::write_from`]: reads everything `reader` gives into a new
    /// file under `tmp`, hashing it on the way, flushes it, and answers where it is and what it
    /// holds. [`BlobStore::place`] then puts it in place. At most `limit` bytes are taken.
    ///
    /// # Errors
    /// `TooLarge`, or the I/O error of the reader or the file system; nothing is kept then.
    pub fn write_tmp(
        &self,
        mut reader: impl Read,
        limit: u64,
    ) -> Result<(PathBuf, BlobRef), BlobError> {
        let (path, mut file) = self.new_tmp()?;
        let result = (|| {
            let mut hasher = Sha256::new();
            let mut size: u64 = 0;
            let mut buf = vec![0u8; CHUNK];
            loop {
                let n = match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                    Err(e) => return Err(BlobError::Io(e)),
                };
                let chunk = buf.get(..n).unwrap_or_default();
                size = size.saturating_add(n as u64);
                if size > limit {
                    return Err(BlobError::TooLarge { limit });
                }
                hasher.update(chunk);
                file.write_all(chunk)?;
            }
            file.sync_all()?;
            Ok(BlobRef {
                sha256: hasher.finalize().into(),
                size,
            })
        })();
        drop(file);
        match result {
            Ok(blob) => Ok((path, blob)),
            Err(e) => {
                let _ = std::fs::remove_file(&path);
                Err(e)
            }
        }
    }

    /// The second half of [`BlobStore::write_from`]: moves `tmp`, written by
    /// [`BlobStore::write_tmp`] to hold `blob`, into place, or removes it if that content is
    /// stored already.
    ///
    /// # Errors
    /// The I/O error; `tmp` is removed then.
    pub fn place(&self, tmp: &Path, blob: BlobRef) -> Result<(), BlobError> {
        self.put_in_place(tmp, &blob)
    }

    /// Renames `part`, an upload's part file under [`BlobStore::tmp_dir`] already checked to
    /// hold `expected`, into place; if that content is stored already, `part` is removed. The
    /// size is checked again here; the hash is the caller's to have checked.
    ///
    /// # Errors
    /// `Mismatch` if the part file's size differs (it is left where it is), or the I/O error.
    pub fn adopt_part(&self, part: &Path, expected: BlobRef) -> Result<(), BlobError> {
        if std::fs::metadata(part)?.len() != expected.size {
            return Err(BlobError::Mismatch);
        }
        // Flushing needs write access on Windows.
        File::options().write(true).open(part)?.sync_all()?;
        self.put_in_place(part, &expected)
    }

    /// Moves the finished file `tmp` to where `blob` belongs, or removes it if `blob` is there.
    fn put_in_place(&self, tmp: &Path, blob: &BlobRef) -> Result<(), BlobError> {
        let target = self.path_of(&blob.sha256);
        if target.is_file() {
            std::fs::remove_file(tmp)?;
            return Ok(());
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if let Err(e) = std::fs::rename(tmp, &target) {
            // Another writer put the same content in place meanwhile.
            if target.is_file() {
                std::fs::remove_file(tmp)?;
                return Ok(());
            }
            let _ = std::fs::remove_file(tmp);
            return Err(e.into());
        }
        Ok(())
    }

    /// A new empty file under `tmp`, with a name nothing else uses.
    fn new_tmp(&self) -> Result<(PathBuf, File), BlobError> {
        let dir = self.tmp_dir();
        std::fs::create_dir_all(&dir)?;
        loop {
            let path = dir.join(format!("{}.tmp", uuid::Uuid::new_v4().simple()));
            match File::options().write(true).create_new(true).open(&path) {
                Ok(file) => return Ok((path, file)),
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// The stored content with `sha256`, opened for reading; `None` if it is not stored.
    ///
    /// # Errors
    /// The I/O error for anything but a missing file.
    pub fn open_blob(&self, sha256: &[u8; 32]) -> Result<Option<File>, BlobError> {
        match File::open(self.path_of(sha256)) {
            Ok(file) => Ok(Some(file)),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// The whole of `content` in memory: inline bytes as they are, a pointer's from the store.
    /// For content known to be small, such as a text document.
    ///
    /// # Errors
    /// `Missing` if the pointer names content not stored, or the I/O error.
    pub fn read_all(&self, content: &Content) -> Result<Vec<u8>, BlobError> {
        match content {
            Content::Inline(bytes) => Ok(bytes.clone()),
            Content::Blob(blob) => {
                let mut file = self
                    .open_blob(&blob.sha256)?
                    .ok_or_else(|| BlobError::Missing(blob.hex()))?;
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes)?;
                Ok(bytes)
            }
        }
    }

    /// Deletes the content with `sha256`; whether there was any. Only ever called once nothing
    /// names it (see the blob-safety rows in the `records` crate).
    ///
    /// # Errors
    /// The I/O error for anything but a missing file.
    pub fn erase(&self, sha256: &[u8; 32]) -> Result<bool, BlobError> {
        match std::fs::remove_file(self.path_of(sha256)) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// Every stored content, with when its file was last modified, in no particular order.
    /// Files whose names are not hashes are skipped.
    ///
    /// # Errors
    /// The I/O error if a folder cannot be read.
    pub fn list(&self) -> Result<Vec<([u8; 32], SystemTime)>, BlobError> {
        let mut found = Vec::new();
        for prefix in std::fs::read_dir(&self.dir)? {
            let prefix = prefix?;
            let name = prefix.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name.len() != 2 || !prefix.file_type()?.is_dir() {
                continue;
            }
            for entry in std::fs::read_dir(prefix.path())? {
                let entry = entry?;
                let Some(sha256) = entry.file_name().to_str().and_then(parse_hex) else {
                    continue;
                };
                if !hex(&sha256).starts_with(name) {
                    continue;
                }
                found.push((sha256, entry.metadata()?.modified()?));
            }
        }
        Ok(found)
    }

    /// Removes the files under `tmp` last modified before `before`; how many it removed. A
    /// file still being written is modified as it grows, so only abandoned ones are old.
    ///
    /// # Errors
    /// The I/O error if the folder cannot be read.
    pub fn clear_tmp(&self, before: SystemTime) -> Result<usize, BlobError> {
        let mut removed = 0;
        for entry in std::fs::read_dir(self.tmp_dir())? {
            let entry = entry?;
            let metadata = entry.metadata()?;
            if metadata.is_file()
                && metadata.modified()? < before
                && std::fs::remove_file(entry.path()).is_ok()
            {
                removed += 1;
            }
        }
        Ok(removed)
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

    #[test]
    fn hex_round_trips_and_refuses_other_text() {
        let blob = BlobRef::of(b"abc");
        assert_eq!(parse_hex(&blob.hex()), Some(blob.sha256));
        assert_eq!(
            blob.hex(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(parse_hex(&blob.hex().to_uppercase()).is_none());
        assert!(parse_hex(&blob.hex()[1..]).is_none());
    }

    #[test]
    fn non_canonical_pointers_are_inline_content() {
        let blob = BlobRef::of(b"abc");
        let good = String::from_utf8(blob.to_pointer()).unwrap();
        for bad in [
            good.replace("size 3", "size 03"),
            good.replace("size 3", "size +3"),
            good.replace("size 3\n", "size 3"),
            format!("{good}\n"),
            good.replace("sharecraft-blob 1", "sharecraft-blob 2"),
            good.to_uppercase(),
        ] {
            assert_eq!(BlobRef::from_pointer(bad.as_bytes()), None, "{bad:?}");
            assert!(matches!(
                Content::from_git(bad.into_bytes()),
                Content::Inline(_)
            ));
        }
    }
}
