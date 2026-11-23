//! What the Album view of an image library needs to know of each picture before it is drawn:
//! its shape as shown (the width and height, turned as its EXIF orientation says) and, for a
//! photo whose camera recorded one, when it was taken.
//!
//! Only headers are read, never pixels: a JPEG is walked marker by marker up to its frame header,
//! reading only its EXIF segment, and PNG, GIF and WebP are read by their decoders up to the
//! image size. The answer is kept per content beside its thumbnails, at
//! `<data>/thumbs/<xx>/<sha256>.facts` (an empty object when nothing can be told), written to a
//! temporary file and renamed into place, and erased with the content (see
//! [`crate::previews::erase_thumbnail`]).

use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use image::metadata::Orientation;
use image::{ImageDecoder, ImageFormat, ImageReader, Limits};
use revisions::BlobStore;
use serde::{Deserialize, Serialize};

use crate::previews::{MAX_IMAGE_SIDE, thumb_dir_of};

/// The most JPEG segments walked before the frame header; a real file has a dozen or two.
const MAX_SEGMENTS: usize = 256;
/// The most entries read from one EXIF directory.
const MAX_IFD_ENTRIES: u16 = 512;

/// A picture's shape as shown and when it was taken.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhotoFacts {
    /// The width as shown, in pixels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    /// The height as shown, in pixels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// When it was taken, as the camera's clock said, `YYYY-MM-DDTHH:MM:SS` with no zone (EXIF
    /// records none).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub taken_at: Option<String>,
}

/// Where the facts of the content `sha256` of `store` are kept, whether or not they are there.
#[must_use]
pub fn facts_path(store: &BlobStore, sha256: &[u8; 32]) -> PathBuf {
    thumb_dir_of(store, sha256, "facts")
}

/// The facts of the content `sha256` of `store`, read once and kept. Content that is not a PNG,
/// JPEG, GIF or WebP, or whose headers cannot be read, has empty facts. Blocks.
///
/// # Errors
/// The I/O error when the content or the kept facts cannot be read, or the facts not kept.
pub fn photo_facts(store: &BlobStore, sha256: &[u8; 32]) -> std::io::Result<PhotoFacts> {
    let path = facts_path(store, sha256);
    match std::fs::read(&path) {
        Ok(kept) => return Ok(serde_json::from_slice(&kept).unwrap_or_default()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let file = match store.open_blob(sha256) {
        Ok(Some(file)) => file,
        Ok(None) => return Ok(PhotoFacts::default()),
        Err(e) => return Err(std::io::Error::other(e.to_string())),
    };
    let facts = read_facts(&mut BufReader::new(file)).unwrap_or_default();
    keep(store, sha256, &path, &facts)?;
    Ok(facts)
}

/// Writes `facts` to a temporary file and renames it to `path`; erased again if the content was
/// erased meanwhile (as a thumbnail is).
fn keep(
    store: &BlobStore,
    sha256: &[u8; 32],
    path: &Path,
    facts: &PhotoFacts,
) -> std::io::Result<()> {
    let json = serde_json::to_vec(facts).map_err(std::io::Error::other)?;
    crate::previews::write_kept(store, path, &json)?;
    if !store.contains(sha256) {
        crate::previews::erase_thumbnail(store, sha256)?;
    }
    Ok(())
}

/// The facts of the picture `reader` holds, from its headers only; `None` when it is not a
/// PNG, JPEG, GIF or WebP or its headers cannot be read.
pub fn read_facts<R: BufRead + Seek>(reader: &mut R) -> Option<PhotoFacts> {
    let mut start = [0u8; 16];
    let got = read_up_to(reader, &mut start).ok()?;
    let format = image::guess_format(start.get(..got)?).ok()?;
    reader.seek(SeekFrom::Start(0)).ok()?;
    let (width, height, exif) = match format {
        ImageFormat::Jpeg => jpeg_header(reader)?,
        ImageFormat::Png | ImageFormat::Gif | ImageFormat::WebP => {
            let mut limits = Limits::default();
            limits.max_image_width = Some(MAX_IMAGE_SIDE);
            limits.max_image_height = Some(MAX_IMAGE_SIDE);
            limits.max_alloc = Some(16 << 20);
            let mut image = ImageReader::with_format(&mut *reader, format);
            image.limits(limits);
            let mut decoder = image.into_decoder().ok()?;
            let (width, height) = decoder.dimensions();
            // Only PNG and WebP carry EXIF; a GIF has none.
            let exif = if format == ImageFormat::Gif {
                None
            } else {
                decoder.exif_metadata().ok().flatten()
            };
            (width, height, exif)
        }
        _ => return None,
    };
    if width == 0 || height == 0 {
        return None;
    }
    let orientation = exif
        .as_deref()
        .and_then(Orientation::from_exif_chunk)
        .unwrap_or(Orientation::NoTransforms);
    let (width, height) = if turns_sideways(orientation) {
        (height, width)
    } else {
        (width, height)
    };
    Some(PhotoFacts {
        width: Some(width),
        height: Some(height),
        taken_at: exif.as_deref().and_then(taken_at),
    })
}

/// Whether showing a picture as `orientation` says swaps its width and height.
#[must_use]
pub fn turns_sideways(orientation: Orientation) -> bool {
    matches!(
        orientation,
        Orientation::Rotate90
            | Orientation::Rotate270
            | Orientation::Rotate90FlipH
            | Orientation::Rotate270FlipH
    )
}

fn read_up_to<R: Read>(reader: &mut R, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut got = 0;
    while got < buf.len() {
        let Some(rest) = buf.get_mut(got..) else {
            break;
        };
        match reader.read(rest)? {
            0 => break,
            n => got += n,
        }
    }
    Ok(got)
}

/// A JPEG's width and height from its frame header, and its EXIF (the TIFF data after
/// `Exif\0\0`) if an APP1 segment before the frame header holds one. Walks the segments,
/// seeking over each; `None` when the file ends, a scan starts, or more than
/// [`MAX_SEGMENTS`] go by before a frame header.
fn jpeg_header<R: Read + Seek>(reader: &mut R) -> Option<(u32, u32, Option<Vec<u8>>)> {
    let mut two = [0u8; 2];
    reader.read_exact(&mut two).ok()?;
    if two != [0xFF, 0xD8] {
        return None;
    }
    let mut exif = None;
    for _ in 0..MAX_SEGMENTS {
        let mut byte = [0u8; 1];
        reader.read_exact(&mut byte).ok()?;
        if byte[0] != 0xFF {
            return None;
        }
        // Fill bytes before a marker.
        let mut marker = 0xFF;
        while marker == 0xFF {
            reader.read_exact(&mut byte).ok()?;
            marker = byte[0];
        }
        match marker {
            // Markers with no length.
            0x01 | 0xD0..=0xD7 => continue,
            // End of image or start of scan before any frame header.
            0xD9 | 0xDA => return None,
            _ => {}
        }
        reader.read_exact(&mut two).ok()?;
        let length = u16::from_be_bytes(two);
        if length < 2 {
            return None;
        }
        let body = u64::from(length - 2);
        let is_frame = matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC);
        if is_frame {
            let mut frame = [0u8; 5];
            reader.read_exact(&mut frame).ok()?;
            let height = u32::from(u16::from_be_bytes([frame[1], frame[2]]));
            let width = u32::from(u16::from_be_bytes([frame[3], frame[4]]));
            return Some((width, height, exif));
        }
        if marker == 0xE1 && exif.is_none() {
            let mut data = vec![0u8; usize::from(length - 2)];
            reader.read_exact(&mut data).ok()?;
            if let Some(tiff) = data.strip_prefix(b"Exif\0\0") {
                exif = Some(tiff.to_vec());
            }
            continue;
        }
        reader
            .seek(SeekFrom::Current(i64::try_from(body).ok()?))
            .ok()?;
    }
    None
}

/// When a photo was taken, from its EXIF (TIFF data): DateTimeOriginal in the EXIF directory,
/// else DateTime in the first. `None` when neither is there or neither is a real date.
#[must_use]
pub fn taken_at(tiff: &[u8]) -> Option<String> {
    let big = match tiff.get(..4)? {
        [0x49, 0x49, 42, 0] => false,
        [0x4D, 0x4D, 0, 42] => true,
        _ => return None,
    };
    let u16_at = |at: usize| -> Option<u16> {
        let b: [u8; 2] = tiff.get(at..at.checked_add(2)?)?.try_into().ok()?;
        Some(if big {
            u16::from_be_bytes(b)
        } else {
            u16::from_le_bytes(b)
        })
    };
    let u32_at = |at: usize| -> Option<u32> {
        let b: [u8; 4] = tiff.get(at..at.checked_add(4)?)?.try_into().ok()?;
        Some(if big {
            u32::from_be_bytes(b)
        } else {
            u32::from_le_bytes(b)
        })
    };
    // The entry `tag` of the directory at `ifd`: its type, count and value or offset.
    let entry = |ifd: usize, tag: u16| -> Option<(u16, u32, usize)> {
        let count = u16_at(ifd)?.min(MAX_IFD_ENTRIES);
        for n in 0..usize::from(count) {
            let at = ifd.checked_add(2 + n * 12)?;
            if u16_at(at)? == tag {
                return Some((u16_at(at + 2)?, u32_at(at + 4)?, at + 8));
            }
        }
        None
    };
    let text = |ifd: usize, tag: u16| -> Option<String> {
        let (kind, count, value) = entry(ifd, tag)?;
        // ASCII, "YYYY:MM:DD HH:MM:SS" and its NUL: always 20, so stored at an offset.
        if kind != 2 || count < 19 {
            return None;
        }
        let at = usize::try_from(u32_at(value)?).ok()?;
        let raw = tiff.get(at..at.checked_add(19)?)?;
        exif_date(raw)
    };
    let first = usize::try_from(u32_at(4)?).ok()?;
    let original = entry(first, 0x8769)
        .and_then(|(_, _, value)| u32_at(value))
        .and_then(|offset| usize::try_from(offset).ok())
        .and_then(|exif_ifd| text(exif_ifd, 0x9003));
    original.or_else(|| text(first, 0x0132))
}

/// `YYYY:MM:DD HH:MM:SS` as `YYYY-MM-DDTHH:MM:SS`, when it is a real date and time.
fn exif_date(raw: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(raw).ok()?;
    let digits = |range: std::ops::Range<usize>| -> Option<u32> {
        let part = text.get(range)?;
        if part.bytes().all(|b| b.is_ascii_digit()) {
            part.parse().ok()
        } else {
            None
        }
    };
    let bytes = text.as_bytes();
    if bytes.get(4) != Some(&b':')
        || bytes.get(7) != Some(&b':')
        || bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
    {
        return None;
    }
    let (year, month, day) = (digits(0..4)?, digits(5..7)?, digits(8..10)?);
    let (hour, minute, second) = (digits(11..13)?, digits(14..16)?, digits(17..19)?);
    if !(1800..=9999).contains(&year)
        || !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}"
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// TIFF data, little-endian, with an Orientation in the first directory and a
    /// DateTimeOriginal in the EXIF directory.
    pub(crate) fn tiff(orientation: u16, date: &str) -> Vec<u8> {
        let mut t = b"II\x2a\x00\x08\x00\x00\x00".to_vec();
        // IFD0 at 8: two entries.
        t.extend_from_slice(&2u16.to_le_bytes());
        t.extend_from_slice(&0x0112u16.to_le_bytes());
        t.extend_from_slice(&3u16.to_le_bytes());
        t.extend_from_slice(&1u32.to_le_bytes());
        t.extend_from_slice(&orientation.to_le_bytes());
        t.extend_from_slice(&[0, 0]);
        t.extend_from_slice(&0x8769u16.to_le_bytes());
        t.extend_from_slice(&4u16.to_le_bytes());
        t.extend_from_slice(&1u32.to_le_bytes());
        let exif_ifd = 8 + 2 + 24 + 4;
        t.extend_from_slice(&u32::try_from(exif_ifd).unwrap().to_le_bytes());
        t.extend_from_slice(&0u32.to_le_bytes());
        // EXIF IFD: one entry, the date at the end.
        let date_at = exif_ifd + 2 + 12 + 4;
        t.extend_from_slice(&1u16.to_le_bytes());
        t.extend_from_slice(&0x9003u16.to_le_bytes());
        t.extend_from_slice(&2u16.to_le_bytes());
        t.extend_from_slice(&20u32.to_le_bytes());
        t.extend_from_slice(&u32::try_from(date_at).unwrap().to_le_bytes());
        t.extend_from_slice(&0u32.to_le_bytes());
        t.extend_from_slice(date.as_bytes());
        t.push(0);
        t
    }

    /// A JPEG header: SOI, an APP1 with `exif`, a frame header of `width` × `height`.
    fn jpeg(width: u16, height: u16, exif: Option<&[u8]>) -> Vec<u8> {
        let mut j = vec![0xFF, 0xD8];
        // A JFIF APP0 to seek over.
        j.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x10]);
        j.extend_from_slice(b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0");
        if let Some(exif) = exif {
            let length = u16::try_from(exif.len() + 8).unwrap();
            j.extend_from_slice(&[0xFF, 0xE1]);
            j.extend_from_slice(&length.to_be_bytes());
            j.extend_from_slice(b"Exif\0\0");
            j.extend_from_slice(exif);
        }
        j.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08]);
        j.extend_from_slice(&height.to_be_bytes());
        j.extend_from_slice(&width.to_be_bytes());
        j.extend_from_slice(&[3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
        j.extend_from_slice(&[0xFF, 0xDA]);
        j
    }

    #[test]
    fn jpeg_shape_turned_and_date_taken() {
        let plain = read_facts(&mut Cursor::new(jpeg(4000, 3000, None))).unwrap();
        assert_eq!(
            (plain.width, plain.height, plain.taken_at),
            (Some(4000), Some(3000), None)
        );

        let exif = tiff(6, "2024:07:14 18:05:09");
        let turned = read_facts(&mut Cursor::new(jpeg(4000, 3000, Some(&exif)))).unwrap();
        assert_eq!((turned.width, turned.height), (Some(3000), Some(4000)));
        assert_eq!(turned.taken_at.as_deref(), Some("2024-07-14T18:05:09"));

        let upright = read_facts(&mut Cursor::new(jpeg(
            4000,
            3000,
            Some(&tiff(3, "2024:07:14 18:05:09")),
        )))
        .unwrap();
        assert_eq!((upright.width, upright.height), (Some(4000), Some(3000)));
    }

    #[test]
    fn broken_and_odd_files_tell_nothing() {
        assert_eq!(
            read_facts(&mut Cursor::new(b"not a picture".to_vec())),
            None
        );
        // Cut short before the frame header.
        let mut cut = jpeg(10, 10, None);
        cut.truncate(12);
        assert_eq!(read_facts(&mut Cursor::new(cut)), None);
        // A zero size.
        assert_eq!(read_facts(&mut Cursor::new(jpeg(0, 10, None))), None);
        // Endless restart markers stop at the segment limit.
        let mut endless = vec![0xFF, 0xD8];
        for _ in 0..1000 {
            endless.extend_from_slice(&[0xFF, 0xD0]);
        }
        assert_eq!(read_facts(&mut Cursor::new(endless)), None);
    }

    #[test]
    fn dates_must_be_real() {
        assert_eq!(taken_at(&tiff(1, "0000:00:00 00:00:00")), None);
        assert_eq!(taken_at(&tiff(1, "2024:13:01 00:00:00")), None);
        assert_eq!(taken_at(&tiff(1, "    :  :     :  :  ")), None);
        assert_eq!(
            taken_at(&tiff(1, "1999:12:31 23:59:59")).as_deref(),
            Some("1999-12-31T23:59:59")
        );
        // Offsets past the end, and garbage.
        let mut short = tiff(1, "2024:01:01 00:00:00");
        short.truncate(40);
        assert_eq!(taken_at(&short), None);
        assert_eq!(taken_at(b"MM\0\x2a\xff\xff\xff\xff"), None);
        assert_eq!(taken_at(b""), None);
    }

    #[test]
    fn png_size_from_its_header() {
        let mut png = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::new(30, 20))
            .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
            .unwrap();
        let facts = read_facts(&mut Cursor::new(png)).unwrap();
        assert_eq!(
            (facts.width, facts.height, facts.taken_at),
            (Some(30), Some(20), None)
        );
    }
}
