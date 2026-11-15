//! Safe previews and image thumbnails.
//!
//! A preview hands a file's bytes to the browser to show, so only a short list of types a
//! browser shows without running anything has one, and the type it is served as is fixed by
//! the name's extension, never read from the bytes (see [`preview_type`]). Web pages are only
//! ever shown as text; SVG only as an image, behind the sandbox every answer carries.
//!
//! A thumbnail is made on the server the first time it is asked for, from PNG, JPEG, GIF (its
//! first frame) and WebP, and kept at `<data>/thumbs/<xx>/<sha256>.png` (see [`thumbnail`]).
//! Decoding is limited before anything is allocated: the size the header gives, the memory
//! the decoded pixels need, the size of the file, and for a GIF the number of its frames,
//! counted without decoding them. At most [`THUMBNAIL_TURNS`] are made at once, each off the
//! async workers and within [`THUMBNAIL_TIME`] (see `Thumbnailer`). A thumbnail is written to
//! a temporary file and renamed into place, so a failure leaves nothing behind; erasing a
//! content erases its thumbnail (see [`erase_thumbnail`]).

use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits};
use revisions::{BlobStore, hex};
use tokio::sync::Semaphore;

/// The longer side of a thumbnail, at most.
pub const THUMBNAIL_SIDE: u32 = 256;
/// The widest and tallest image a thumbnail is made from.
pub const MAX_IMAGE_SIDE: u32 = 12_000;
/// The most memory one thumbnail may use: the file read plus its decoded pixels.
pub const MAX_DECODE_BYTES: u64 = 256 << 20;
/// The largest image file a thumbnail is made from.
pub const MAX_IMAGE_FILE_BYTES: u64 = 64 << 20;
/// The most frames a GIF may have to get a thumbnail. Only the first is decoded.
pub const MAX_GIF_FRAMES: usize = 1_000;
/// How many thumbnails are made at once.
pub const THUMBNAIL_TURNS: usize = 2;
/// The longest a thumbnail request waits for one to be made, its turn included.
pub const THUMBNAIL_TIME: Duration = Duration::from_secs(10);
/// How much of a text file a preview shows, at most.
pub const TEXT_PREVIEW_BYTES: u64 = 1 << 20;

/// What a preview shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreviewKind {
    /// An image, shown whole.
    Image,
    /// Sound, which a player may ask for in ranges.
    Audio,
    /// Video, which a player may ask for in ranges.
    Video,
    /// Text: the first [`TEXT_PREVIEW_BYTES`], if they are UTF-8.
    Text,
}

/// How a file is previewed: what it shows and the `Content-Type` it is always served as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PreviewType {
    /// What the preview shows.
    pub kind: PreviewKind,
    /// The type it is served as, fixed by the extension.
    pub content_type: &'static str,
}

impl PreviewType {
    /// Whether a preview of this type is served in byte ranges when asked (audio and video).
    #[must_use]
    pub fn ranges(self) -> bool {
        matches!(self.kind, PreviewKind::Audio | PreviewKind::Video)
    }
}

/// The plain-text type every text preview is served as.
const TEXT_PLAIN: &str = "text/plain; charset=utf-8";

/// Extensions with a preview other than text, and the type each is served as.
const MEDIA: &[(&str, PreviewKind, &str)] = &[
    ("png", PreviewKind::Image, "image/png"),
    ("jpg", PreviewKind::Image, "image/jpeg"),
    ("jpeg", PreviewKind::Image, "image/jpeg"),
    ("gif", PreviewKind::Image, "image/gif"),
    ("webp", PreviewKind::Image, "image/webp"),
    ("avif", PreviewKind::Image, "image/avif"),
    ("bmp", PreviewKind::Image, "image/bmp"),
    ("svg", PreviewKind::Image, "image/svg+xml"),
    ("mp3", PreviewKind::Audio, "audio/mpeg"),
    ("ogg", PreviewKind::Audio, "audio/ogg"),
    ("wav", PreviewKind::Audio, "audio/wav"),
    ("m4a", PreviewKind::Audio, "audio/mp4"),
    ("flac", PreviewKind::Audio, "audio/flac"),
    ("mp4", PreviewKind::Video, "video/mp4"),
    ("webm", PreviewKind::Video, "video/webm"),
    // A QuickTime file is MPEG-4 to a browser.
    ("mov", PreviewKind::Video, "video/mp4"),
];

/// Extensions previewed as plain text. Web pages are among them: only ever shown as text.
const TEXT: &[&str] = &[
    "txt", "md", "csv", "tsv", "log", "json", "xml", "yaml", "yml", "toml", "ini", "rs", "js",
    "ts", "py", "java", "c", "h", "cpp", "cs", "go", "rb", "php", "sh", "ps1", "sql", "css",
    "html", "htm",
];

/// Extensions a thumbnail is made for.
const THUMBNAILED: &[&str] = &["png", "jpg", "jpeg", "gif", "webp"];

/// The extension of `name`, lower case, without its dot: what follows the last dot, if
/// anything does and something precedes it.
fn extension(name: &str) -> Option<String> {
    let (stem, ext) = name.rsplit_once('.')?;
    (!stem.is_empty() && !ext.is_empty()).then(|| ext.to_ascii_lowercase())
}

/// How a file named `name` is previewed, chosen by its extension alone; `None` when it has no
/// preview (PDFs open in their editor; everything else is downloaded).
#[must_use]
pub fn preview_type(name: &str) -> Option<PreviewType> {
    let ext = extension(name)?;
    if let Some((_, kind, content_type)) = MEDIA.iter().find(|(e, _, _)| *e == ext) {
        return Some(PreviewType {
            kind: *kind,
            content_type,
        });
    }
    TEXT.contains(&ext.as_str()).then_some(PreviewType {
        kind: PreviewKind::Text,
        content_type: TEXT_PLAIN,
    })
}

/// Whether a file named `name` is of a type a thumbnail is made for. Whether one can be made
/// depends on its bytes.
#[must_use]
pub fn has_thumbnail(name: &str) -> bool {
    extension(name).is_some_and(|ext| THUMBNAILED.contains(&ext.as_str()))
}

/// The text a preview shows of `bytes`, the start of a file, `cut` when the file goes on
/// beyond them: the bytes themselves if they are UTF-8, less a character the cut split;
/// `None` if they are not UTF-8.
#[must_use]
pub fn text_preview(mut bytes: Vec<u8>, cut: bool) -> Option<Vec<u8>> {
    match std::str::from_utf8(&bytes) {
        Ok(_) => Some(bytes),
        // Only an incomplete character at the very end, where the file was cut.
        Err(e) if cut && e.error_len().is_none() => {
            bytes.truncate(e.valid_up_to());
            Some(bytes)
        }
        Err(_) => None,
    }
}

/// Where thumbnails are kept under `data_dir`.
#[must_use]
pub fn thumbs_path(data_dir: &Path) -> PathBuf {
    data_dir.join("thumbs")
}

/// Where the thumbnails of the content store `store` are kept: `thumbs` beside it.
#[must_use]
pub fn thumbs_dir(store: &BlobStore) -> PathBuf {
    match store.dir().parent() {
        Some(data_dir) => thumbs_path(data_dir),
        None => store.dir().join("thumbs"),
    }
}

/// Where the thumbnail of the content `sha256` of `store` is kept, whether or not it is there:
/// `<data>/thumbs/<xx>/<sha256>.png`.
#[must_use]
pub fn thumb_path(store: &BlobStore, sha256: &[u8; 32]) -> PathBuf {
    let name = hex(sha256);
    let prefix = name.get(..2).unwrap_or_default().to_owned();
    thumbs_dir(store).join(prefix).join(format!("{name}.png"))
}

/// Why no thumbnail was made.
#[derive(Debug, thiserror::Error)]
pub enum ThumbError {
    /// The content is not in the store (any more).
    #[error("the content is not in the content store")]
    Missing,
    /// The file is not a PNG, JPEG, GIF or WebP image.
    #[error("not an image a thumbnail is made from")]
    NotAnImage,
    /// The image is larger than the limits allow.
    #[error("the image is larger than a thumbnail is made from")]
    TooLarge,
    /// The GIF has more than [`MAX_GIF_FRAMES`] frames.
    #[error("the GIF has too many frames")]
    TooManyFrames,
    /// The file ends before the image does.
    #[error("the image is cut short")]
    Truncated,
    /// The image could not be decoded.
    #[error("the image cannot be decoded: {0}")]
    Decode(#[from] image::ImageError),
    /// The file system failed.
    #[error("{0}")]
    Io(#[from] std::io::Error),
    /// The content store failed.
    #[error("{0}")]
    Store(#[from] revisions::BlobError),
    /// It took longer than [`THUMBNAIL_TIME`].
    #[error("making the thumbnail took too long")]
    TimedOut,
    /// The task making it failed.
    #[error("the thumbnail task failed: {0}")]
    Task(String),
}

/// The thumbnail of the content `sha256` of `store`, made and kept if it is not kept yet: a
/// PNG at most [`THUMBNAIL_SIDE`] pixels on its longer side, never enlarged. Blocks; the server
/// calls it through a `Thumbnailer`.
///
/// # Errors
/// [`ThumbError`] when no thumbnail can be made: not a PNG, JPEG, GIF or WebP whatever its
/// name, over the limits (see the module documentation), cut short or broken, or the store or
/// the file system failing. Nothing is left behind then.
pub fn thumbnail(store: &BlobStore, sha256: &[u8; 32]) -> Result<PathBuf, ThumbError> {
    let path = thumb_path(store, sha256);
    if path.is_file() {
        return Ok(path);
    }
    let file = store.open_blob(sha256)?.ok_or(ThumbError::Missing)?;
    let size = file.metadata()?.len();
    if size > MAX_IMAGE_FILE_BYTES {
        return Err(ThumbError::TooLarge);
    }
    let mut bytes = Vec::new();
    file.take(MAX_IMAGE_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    let size = bytes.len() as u64;
    let format = image::guess_format(&bytes).map_err(|_| ThumbError::NotAnImage)?;
    match format {
        ImageFormat::Png | ImageFormat::WebP => {}
        ImageFormat::Jpeg if jpeg_complete(&bytes) => {}
        ImageFormat::Jpeg => return Err(ThumbError::Truncated),
        ImageFormat::Gif => {
            gif_frames(&bytes)?;
        }
        _ => return Err(ThumbError::NotAnImage),
    }
    let budget = MAX_DECODE_BYTES.saturating_sub(size);
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_SIDE);
    limits.max_image_height = Some(MAX_IMAGE_SIDE);
    limits.max_alloc = Some(budget);
    let mut reader = ImageReader::with_format(Cursor::new(bytes.as_slice()), format);
    reader.limits(limits);
    // The decoder reads the header only; the limits are checked against it here.
    let decoder = reader.into_decoder().map_err(|e| match e {
        image::ImageError::Limits(_) => ThumbError::TooLarge,
        e => ThumbError::Decode(e),
    })?;
    let (width, height) = decoder.dimensions();
    if width == 0 || height == 0 || width > MAX_IMAGE_SIDE || height > MAX_IMAGE_SIDE {
        return Err(ThumbError::TooLarge);
    }
    if decoder.total_bytes() > budget {
        return Err(ThumbError::TooLarge);
    }
    // A GIF's decoder decodes its first frame only.
    let image = DynamicImage::from_decoder(decoder)?;
    drop(bytes);
    let small = if width > THUMBNAIL_SIDE || height > THUMBNAIL_SIDE {
        image.thumbnail(THUMBNAIL_SIDE, THUMBNAIL_SIDE)
    } else {
        image
    };
    let small = DynamicImage::ImageRgba8(small.to_rgba8());
    let mut png = Vec::new();
    small.write_to(&mut Cursor::new(&mut png), ImageFormat::Png)?;
    keep(store, sha256, &path, &png)?;
    Ok(path)
}

/// Writes `png` to a temporary file beside the content store and renames it to `path`. If the
/// content was erased meanwhile, the thumbnail is erased too (an erasure deletes the content
/// first, then its thumbnail; whichever looks last finds the other's work).
fn keep(store: &BlobStore, sha256: &[u8; 32], path: &Path, png: &[u8]) -> Result<(), ThumbError> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = store
        .tmp_dir()
        .join(format!("thumb-{}.png", uuid::Uuid::new_v4().simple()));
    let written = (|| {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(png)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        // Another request may have put the same thumbnail in place first.
        if path.is_file() {
            return Ok(());
        }
        return Err(e.into());
    }
    if !store.contains(sha256) {
        let _ = erase_thumbnail(store, sha256);
        return Err(ThumbError::Missing);
    }
    Ok(())
}

/// Erases the thumbnail of the content `sha256` of `store`, if there is one; whether there was.
///
/// # Errors
/// The I/O error for anything but a missing file.
pub fn erase_thumbnail(store: &BlobStore, sha256: &[u8; 32]) -> std::io::Result<bool> {
    match std::fs::remove_file(thumb_path(store, sha256)) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Whether a JPEG's last scan is followed by its end-of-image marker: a file cut short is not
/// decoded (the decoder would fill what is missing).
fn jpeg_complete(bytes: &[u8]) -> bool {
    let marker = |second: u8| move |pair: &[u8]| pair == [0xFF, second];
    let Some(scan) = bytes.windows(2).rposition(marker(0xDA)) else {
        return false;
    };
    bytes
        .get(scan..)
        .is_some_and(|rest| rest.windows(2).any(marker(0xD9)))
}

/// The number of frames of the GIF `bytes`, counted by walking its blocks without decoding
/// any; stops counting past [`MAX_GIF_FRAMES`].
fn gif_frames(bytes: &[u8]) -> Result<usize, ThumbError> {
    let byte = |at: usize| bytes.get(at).copied().ok_or(ThumbError::Truncated);
    // Skips a run of data sub-blocks starting at `at`; where the next block starts.
    let sub_blocks = |mut at: usize| -> Result<usize, ThumbError> {
        loop {
            let len = usize::from(byte(at)?);
            at += 1;
            if len == 0 {
                return Ok(at);
            }
            at += len;
        }
    };
    let colour_table = |packed: u8| -> usize {
        if packed & 0x80 == 0 {
            0
        } else {
            3 << ((packed & 0x07) + 1)
        }
    };
    if !(bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) {
        return Err(ThumbError::NotAnImage);
    }
    // The header and the logical screen descriptor, then the global colour table.
    let mut at = 13 + colour_table(byte(10)?);
    let mut frames = 0;
    loop {
        match byte(at)? {
            // An extension: its label, then its data.
            0x21 => at = sub_blocks(at + 2)?,
            // An image: its descriptor, its own colour table, the code size, its data.
            0x2C => {
                frames += 1;
                if frames > MAX_GIF_FRAMES {
                    return Err(ThumbError::TooManyFrames);
                }
                let packed = byte(at + 9)?;
                at = sub_blocks(at + 10 + colour_table(packed) + 1)?;
            }
            0x3B if frames > 0 => return Ok(frames),
            _ => return Err(ThumbError::Truncated),
        }
    }
}

/// Makes thumbnails for the server: [`THUMBNAIL_TURNS`] at once, each in `spawn_blocking`.
pub(crate) struct Thumbnailer {
    turns: Arc<Semaphore>,
}

impl Default for Thumbnailer {
    fn default() -> Self {
        Self {
            turns: Arc::new(Semaphore::new(THUMBNAIL_TURNS)),
        }
    }
}

impl Thumbnailer {
    /// The thumbnail of the content `sha256` of `blobs`, made if needed (see [`thumbnail`]),
    /// within [`THUMBNAIL_TIME`] counting the wait for a turn. A thumbnail still being made
    /// when the time is up keeps its turn until it is done, so slow images never make more
    /// than [`THUMBNAIL_TURNS`] at once; what it makes is kept for the next request.
    pub(crate) async fn make(
        &self,
        blobs: Arc<crate::blobs::Blobs>,
        sha256: [u8; 32],
    ) -> Result<PathBuf, ThumbError> {
        let path = thumb_path(blobs.store(), &sha256);
        if tokio::fs::metadata(&path)
            .await
            .is_ok_and(|meta| meta.is_file())
        {
            return Ok(path);
        }
        let turns = Arc::clone(&self.turns);
        let work = async move {
            let turn = turns
                .acquire_owned()
                .await
                .map_err(|e| ThumbError::Task(e.to_string()))?;
            tokio::task::spawn_blocking(move || {
                let _turn = turn;
                thumbnail(blobs.store(), &sha256)
            })
            .await
            .map_err(|e| ThumbError::Task(e.to_string()))?
        };
        tokio::time::timeout(THUMBNAIL_TIME, work)
            .await
            .map_err(|_| ThumbError::TimedOut)?
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn extensions_are_the_last_suffix_only() {
        assert_eq!(extension("a.tar.gz").as_deref(), Some("gz"));
        assert_eq!(extension("Photo.JPG").as_deref(), Some("jpg"));
        assert_eq!(extension(".png"), None);
        assert_eq!(extension("png"), None);
        assert_eq!(extension("a."), None);
        assert!(preview_type("page.html.png").is_some_and(|t| t.content_type == "image/png"));
        assert!(preview_type("image.png.html").is_some_and(|t| t.content_type == TEXT_PLAIN));
        assert!(has_thumbnail("a.JPEG"));
        assert!(!has_thumbnail("a.svg"));
    }

    #[test]
    fn text_is_cut_only_at_a_character() {
        assert_eq!(
            text_preview(b"plain".to_vec(), false),
            Some(b"plain".to_vec())
        );
        let split = "ab\u{e9}".as_bytes()[..3].to_vec();
        assert_eq!(text_preview(split.clone(), true), Some(b"ab".to_vec()));
        assert_eq!(text_preview(split, false), None);
        assert_eq!(text_preview(b"a\xffb".to_vec(), true), None);
    }

    #[test]
    fn gif_frames_are_counted_without_decoding() {
        let gif = |frames: usize, end: bool| {
            let mut out = b"GIF89a".to_vec();
            out.extend([1, 0, 1, 0, 0x80, 0, 0, 0, 0, 0, 255, 255, 255]);
            out.extend([0x21, 0xF9, 4, 0, 0, 0, 0, 0]);
            for _ in 0..frames {
                out.extend([0x2C, 0, 0, 0, 0, 1, 0, 1, 0, 0, 2, 2, 0x44, 1, 0]);
            }
            if end {
                out.push(0x3B);
            }
            out
        };
        assert_eq!(gif_frames(&gif(3, true)).unwrap(), 3);
        assert!(matches!(
            gif_frames(&gif(MAX_GIF_FRAMES + 1, true)),
            Err(ThumbError::TooManyFrames)
        ));
        assert!(matches!(
            gif_frames(&gif(2, false)),
            Err(ThumbError::Truncated)
        ));
        assert!(matches!(
            gif_frames(&gif(0, true)),
            Err(ThumbError::Truncated)
        ));
        assert!(matches!(gif_frames(b"GIF89"), Err(ThumbError::NotAnImage)));
    }

    #[test]
    fn jpeg_needs_its_end_after_the_last_scan() {
        assert!(jpeg_complete(&[0xFF, 0xD8, 0xFF, 0xDA, 1, 2, 0xFF, 0xD9]));
        assert!(!jpeg_complete(&[0xFF, 0xD8, 0xFF, 0xDA, 1, 2]));
        assert!(!jpeg_complete(&[0xFF, 0xD8, 0xFF, 0xD9]));
    }
}
