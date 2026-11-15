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
//! the decoded pixels need, the size of the file, for a GIF the number of its frames and for
//! a JPEG the number of its scans (each counted by walking the file without decoding it), so
//! the work of a decode is bounded too. At most [`THUMBNAIL_TURNS`] are made at once, each off
//! the async workers; a request waits at most [`THUMBNAIL_WAIT`] for a turn (then answers busy)
//! and a decode has [`THUMBNAIL_TIME`] (see `Thumbnailer`). A thumbnail is
//! written to a temporary file and renamed into place, so a failure leaves nothing behind. Content
//! that fails, or takes too long, is marked and never decoded again. Erasing a content erases
//! its thumbnail and its mark (see [`erase_thumbnail`]).

use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError};
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
/// The most scans a JPEG may have to get a thumbnail: decoding walks every pixel once per
/// scan, and a progressive JPEG can hold thousands in a small file.
pub const MAX_JPEG_SCANS: u64 = 64;
/// The most pixels times scans a JPEG may need decoded to get a thumbnail.
pub const MAX_JPEG_SCAN_PIXELS: u64 = 1_500_000_000;
/// How many thumbnails are made at once.
pub const THUMBNAIL_TURNS: usize = 2;
/// The longest a thumbnail request waits for a turn to make one; it then answers busy, and
/// nothing is remembered.
pub const THUMBNAIL_WAIT: Duration = Duration::from_secs(10);
/// The longest a thumbnail may take to make once it has its turn; content that takes longer is
/// remembered as having none.
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

/// Where the mark that no thumbnail can be made of the content `sha256` of `store` is kept,
/// whether or not it is there: `<data>/thumbs/<xx>/<sha256>.none`, an empty file.
#[must_use]
pub fn no_thumb_path(store: &BlobStore, sha256: &[u8; 32]) -> PathBuf {
    thumb_path(store, sha256).with_extension("none")
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
    /// The image is larger than the limits allow, or would take too much work to decode.
    #[error("the image is larger than a thumbnail is made from")]
    TooLarge,
    /// The GIF has more than [`MAX_GIF_FRAMES`] frames.
    #[error("the GIF has too many frames")]
    TooManyFrames,
    /// The JPEG has more than [`MAX_JPEG_SCANS`] scans.
    #[error("the JPEG has too many scans")]
    TooManyScans,
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
    /// No turn came free within [`THUMBNAIL_WAIT`]; nothing was tried, and asking again later
    /// may work.
    #[error("too many thumbnails are being made")]
    Busy,
    /// Its decode took longer than [`THUMBNAIL_TIME`].
    #[error("making the thumbnail took too long")]
    TimedOut,
    /// The task making it failed.
    #[error("the thumbnail task failed: {0}")]
    Task(String),
    /// An earlier attempt failed for good or took too long; the content is not decoded again.
    #[error("no thumbnail can be made of this content")]
    Remembered,
}

impl ThumbError {
    /// Whether the failure belongs to the content, so trying again would fail again: such
    /// content is marked and never decoded again. A missing file, the store, the file system
    /// or the task failing is not remembered.
    #[must_use]
    pub fn is_lasting(&self) -> bool {
        matches!(
            self,
            ThumbError::NotAnImage
                | ThumbError::TooLarge
                | ThumbError::TooManyFrames
                | ThumbError::TooManyScans
                | ThumbError::Truncated
                | ThumbError::Decode(_)
                | ThumbError::TimedOut
        )
    }
}

/// The thumbnail of the content `sha256` of `store`, made and kept if it is not kept yet: a
/// PNG at most [`THUMBNAIL_SIDE`] pixels on its longer side, never enlarged. Blocks; the server
/// calls it through a `Thumbnailer`. A failure that belongs to the content (see
/// [`ThumbError::is_lasting`]) is remembered with a mark beside the thumbnails (see
/// [`no_thumb_path`]), and from then on the content is not decoded again.
///
/// # Errors
/// [`ThumbError`] when no thumbnail can be made: not a PNG, JPEG, GIF or WebP whatever its
/// name, over the limits (see the module documentation), cut short or broken, the store or
/// the file system failing, or [`ThumbError::Remembered`] once one of those was recorded.
/// No partial thumbnail is ever left behind.
pub fn thumbnail(store: &BlobStore, sha256: &[u8; 32]) -> Result<PathBuf, ThumbError> {
    let path = thumb_path(store, sha256);
    if path.is_file() {
        return Ok(path);
    }
    if no_thumb_path(store, sha256).is_file() {
        return Err(ThumbError::Remembered);
    }
    match make_thumbnail(store, sha256, &path) {
        Ok(()) => Ok(path),
        Err(e) => {
            if e.is_lasting()
                && let Err(failed) = remember_failure(store, sha256)
            {
                eprintln!(
                    "sharecraft-server: cannot mark content {} as having no thumbnail: {failed}",
                    hex(sha256)
                );
            }
            Err(e)
        }
    }
}

/// Decodes the content `sha256` of `store` within the limits and keeps its thumbnail at `path`.
fn make_thumbnail(store: &BlobStore, sha256: &[u8; 32], path: &Path) -> Result<(), ThumbError> {
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
        ImageFormat::Jpeg => check_jpeg(&bytes)?,
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
    keep(store, sha256, path, &png)
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

/// Marks the content `sha256` of `store` as having no thumbnail, so it is not decoded again
/// (see [`no_thumb_path`]). Erased with the content, as its thumbnail is; a mark made after the
/// content was erased is removed again.
///
/// # Errors
/// The I/O error.
pub fn remember_failure(store: &BlobStore, sha256: &[u8; 32]) -> std::io::Result<()> {
    let mark = no_thumb_path(store, sha256);
    if let Some(dir) = mark.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::File::create(&mark)?;
    if !store.contains(sha256) {
        erase_thumbnail(store, sha256)?;
    }
    Ok(())
}

/// Erases the thumbnail of the content `sha256` of `store`, and its mark of having none; whether
/// there was either.
///
/// # Errors
/// The I/O error for anything but a missing file.
pub fn erase_thumbnail(store: &BlobStore, sha256: &[u8; 32]) -> std::io::Result<bool> {
    let mut erased = false;
    for path in [thumb_path(store, sha256), no_thumb_path(store, sha256)] {
        match std::fs::remove_file(path) {
            Ok(()) => erased = true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(erased)
}

/// Checks a JPEG's structure without decoding it, by walking its segments and skipping over
/// each scan's coded data: refused when it ends before its end-of-image marker (the decoder
/// would fill what is missing), when it has more than [`MAX_JPEG_SCANS`] scans, or when its
/// pixels times its scans pass [`MAX_JPEG_SCAN_PIXELS`]. Decoding walks every pixel once per
/// scan, so these bound its work: a progressive JPEG can hold any number of scans in a small
/// file.
fn check_jpeg(bytes: &[u8]) -> Result<(), ThumbError> {
    let byte = |at: usize| bytes.get(at).copied().ok_or(ThumbError::Truncated);
    // After the start-of-image marker.
    let mut at = 2;
    let mut scans: u64 = 0;
    let mut pixels: u64 = 0;
    loop {
        if byte(at)? != 0xFF {
            return Err(ThumbError::Truncated);
        }
        // Fill bytes may come before a marker.
        while byte(at + 1)? == 0xFF {
            at += 1;
        }
        let marker = byte(at + 1)?;
        at += 2;
        match marker {
            0xD9 if scans > 0 => return Ok(()),
            0xD9 => return Err(ThumbError::Truncated),
            // Markers with no segment.
            0x01 | 0xD0..=0xD7 => {}
            _ => {
                let len = usize::from(byte(at)?) << 8 | usize::from(byte(at + 1)?);
                if len < 2 {
                    return Err(ThumbError::Truncated);
                }
                // A start of frame (not a table): its height and width.
                if matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
                    let height = u64::from(u16::from_be_bytes([byte(at + 3)?, byte(at + 4)?]));
                    let width = u64::from(u16::from_be_bytes([byte(at + 5)?, byte(at + 6)?]));
                    pixels = width * height;
                }
                at += len;
                if marker == 0xDA {
                    scans += 1;
                    if scans > MAX_JPEG_SCANS {
                        return Err(ThumbError::TooManyScans);
                    }
                    if pixels.saturating_mul(scans) > MAX_JPEG_SCAN_PIXELS {
                        return Err(ThumbError::TooLarge);
                    }
                    // The coded data, up to the next marker that is not a restart.
                    loop {
                        if byte(at)? == 0xFF {
                            let next = byte(at + 1)?;
                            if next != 0x00 && !(0xD0..=0xD7).contains(&next) {
                                break;
                            }
                            at += 2;
                        } else {
                            at += 1;
                        }
                    }
                }
            }
        }
    }
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
    /// How long a request waits for a turn ([`THUMBNAIL_WAIT`]; tests shorten it).
    wait: std::sync::Mutex<Duration>,
}

impl Default for Thumbnailer {
    fn default() -> Self {
        Self {
            turns: Arc::new(Semaphore::new(THUMBNAIL_TURNS)),
            wait: std::sync::Mutex::new(THUMBNAIL_WAIT),
        }
    }
}

impl Thumbnailer {
    /// The thumbnail of the content `sha256` of `blobs`, made if needed (see [`thumbnail`]);
    /// content marked as having none answers [`ThumbError::Remembered`] at once.
    ///
    /// A request waits at most [`THUMBNAIL_WAIT`] for a turn and then answers
    /// [`ThumbError::Busy`], leaving no mark: the image was never looked at, and the page may
    /// ask again. Once it has its turn, the decode has [`THUMBNAIL_TIME`]; content whose decode
    /// takes longer is marked, so it is never decoded again. The decode still running then
    /// keeps its turn until it really ends, so never more than [`THUMBNAIL_TURNS`] run at once;
    /// the limits checked before decoding (see [`thumbnail`]) bound how long that can be. What
    /// it makes is kept and served from then on.
    pub(crate) async fn make(
        &self,
        blobs: Arc<crate::blobs::Blobs>,
        sha256: [u8; 32],
    ) -> Result<PathBuf, ThumbError> {
        let path = thumb_path(blobs.store(), &sha256);
        let mark = no_thumb_path(blobs.store(), &sha256);
        if is_file(&path).await {
            return Ok(path);
        }
        if is_file(&mark).await {
            return Err(ThumbError::Remembered);
        }
        let wait = *self.wait.lock().unwrap_or_else(PoisonError::into_inner);
        let turn = match tokio::time::timeout(wait, Arc::clone(&self.turns).acquire_owned()).await {
            Ok(Ok(turn)) => turn,
            Ok(Err(e)) => return Err(ThumbError::Task(e.to_string())),
            Err(_) => return Err(ThumbError::Busy),
        };
        // Made, or found unmakeable, by another request while this one waited.
        if is_file(&path).await {
            return Ok(path);
        }
        if is_file(&mark).await {
            return Err(ThumbError::Remembered);
        }
        let maker = Arc::clone(&blobs);
        let job = tokio::task::spawn_blocking(move || {
            let _turn = turn;
            thumbnail(maker.store(), &sha256)
        });
        match tokio::time::timeout(THUMBNAIL_TIME, job).await {
            Ok(made) => made.map_err(|e| ThumbError::Task(e.to_string()))?,
            Err(_) => {
                let marked =
                    tokio::task::spawn_blocking(move || remember_failure(blobs.store(), &sha256))
                        .await;
                if !matches!(marked, Ok(Ok(()))) {
                    eprintln!(
                        "sharecraft-server: cannot mark content {} as having no thumbnail",
                        hex(&sha256)
                    );
                }
                Err(ThumbError::TimedOut)
            }
        }
    }

    /// Takes every turn, so no thumbnail is made until the answer is dropped. For tests.
    pub(crate) async fn hold_turns(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        let turns = u32::try_from(THUMBNAIL_TURNS).ok()?;
        Arc::clone(&self.turns).acquire_many_owned(turns).await.ok()
    }

    /// Sets how long a request waits for a turn. For tests.
    pub(crate) fn set_wait(&self, wait: Duration) {
        *self.wait.lock().unwrap_or_else(PoisonError::into_inner) = wait;
    }
}

/// Whether `path` is a file, without blocking the async workers.
async fn is_file(path: &Path) -> bool {
    tokio::fs::metadata(path)
        .await
        .is_ok_and(|meta| meta.is_file())
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

    /// A JPEG of `scans` scans of a `width` × `height` frame, with `end` its end marker.
    fn jpeg(width: u16, height: u16, scans: usize, end: bool) -> Vec<u8> {
        let mut out = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0xFF, 0xD9];
        out.extend([0xFF, 0xC2, 0x00, 0x0B, 0x08]);
        out.extend(height.to_be_bytes());
        out.extend(width.to_be_bytes());
        out.extend([0x01, 0x01, 0x11, 0x00]);
        for _ in 0..scans {
            out.extend([0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3F, 0x00]);
            // Coded data with a stuffed byte and a restart marker in it.
            out.extend([0x12, 0xFF, 0x00, 0x34, 0xFF, 0xD3, 0x56]);
        }
        if end {
            out.extend([0xFF, 0xD9]);
        }
        out
    }

    #[test]
    fn jpeg_scans_are_counted_without_decoding() {
        assert!(check_jpeg(&jpeg(100, 100, 3, true)).is_ok());
        // The end of an embedded segment does not count; the file's end does.
        assert!(matches!(
            check_jpeg(&jpeg(100, 100, 3, false)),
            Err(ThumbError::Truncated)
        ));
        assert!(matches!(
            check_jpeg(&jpeg(100, 100, 0, true)),
            Err(ThumbError::Truncated)
        ));
        let limit = usize::try_from(MAX_JPEG_SCANS).unwrap();
        assert!(check_jpeg(&jpeg(10, 10, limit, true)).is_ok());
        assert!(matches!(
            check_jpeg(&jpeg(10, 10, limit + 1, true)),
            Err(ThumbError::TooManyScans)
        ));
        // 12,000 x 12,000 pixels: ten scans pass, twenty-five do not.
        assert!(check_jpeg(&jpeg(12_000, 12_000, 10, true)).is_ok());
        assert!(matches!(
            check_jpeg(&jpeg(12_000, 12_000, 25, true)),
            Err(ThumbError::TooLarge)
        ));
    }
}
