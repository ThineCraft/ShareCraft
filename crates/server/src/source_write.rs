//! The types every write to an external library shares, and the names a written copy gets.
//!
//! Only the shared types and names are here for now. The store-specific code is in
//! `s3_write.rs`, the only module that changes a bucket.

use records::Tri;
use time::{OffsetDateTime, UtcOffset};

/// The longest name a conflict copy gets, in characters.
const NAME_MAX_CHARS: usize = 120;
/// The longest person name used in a conflict copy's name, in characters.
const PERSON_MAX_CHARS: usize = 40;
/// The longest extension kept apart from the stem, in characters (dot included).
const EXT_MAX_CHARS: usize = 20;

/// Why a write did not happen, in words the caller can turn into a plain sentence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteError {
    /// The store refuses this key the write.
    Denied,
    /// The store could not be reached now; trying again later may work.
    Offline,
    /// The name is taken (a file share's answer; S3 never gives it from `write_object`).
    NameTaken,
    /// The bucket or folder is gone.
    Gone,
    /// The store is full (never answered by S3 in slice 1).
    NoSpace,
    /// The file is larger than one write sends.
    TooLarge,
    /// Anything else.
    Failed,
}

/// What a write expects to find: the tag the file was last seen with (`None`: it must not
/// exist) and the hash of the content that was seen.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Base {
    pub tag: Option<String>,
    pub sha256: Option<[u8; 32]>,
}

/// What the store holds after a write, as the next listing will see it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seen {
    pub tag: String,
    pub size: u64,
    pub modified: OffsetDateTime,
}

/// The outcome of a write that was not refused or lost.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Written {
    Done(Seen),
    /// The file is not what the write was based on. Nothing was overwritten.
    Conflict,
}

/// What the write probe found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Probe {
    ReadWrite {
        conditional: Tri,
    },
    ReadOnly,
    /// The probe could not ask, for this reason (`keys`, `bucket`, `region`, `clock`,
    /// `address` or `offline`).
    Unreachable(&'static str),
}

/// Whether a character may stand in a name written for a person.
fn name_char(c: char) -> bool {
    !c.is_control() && !matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
}

/// `name` split into stem and extension (the extension keeps its dot). A leading dot is part
/// of the stem, and an extension longer than 20 characters is not one.
fn split_name(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(at) if at > 0 => {
            let ext = name.get(at..).unwrap_or("");
            if ext.chars().count() > EXT_MAX_CHARS {
                (name, "")
            } else {
                (name.get(..at).unwrap_or(name), ext)
            }
        }
        _ => (name, ""),
    }
}

/// `stem` cut to `max` characters, never inside one.
fn cut(stem: &str, max: usize) -> String {
    stem.chars().take(max).collect()
}

/// The name of the copy that keeps an edit made while the file changed outside:
/// `<stem> (edited by <person> <YYYY-MM-DD HH.MM>)<.ext>`, the time in UTC.
///
/// The person's name keeps only characters a name may hold and is cut to 40 characters; the
/// stem is shortened so the whole name fits 120 characters.
#[must_use]
pub fn sibling_name(name: &str, person: &str, at: OffsetDateTime) -> String {
    let cleaned: String = person.chars().filter(|c| name_char(*c)).collect();
    let cleaned = cut(cleaned.trim(), PERSON_MAX_CHARS);
    let cleaned = cleaned.trim_end();
    let cleaned = if cleaned.is_empty() {
        "someone"
    } else {
        cleaned
    };
    // A reserved device name ("con") would be refused by Windows; a prefix avoids it.
    let cleaned = if records::windows_safe_name(cleaned) {
        cleaned.to_owned()
    } else {
        format!("_{cleaned}")
    };
    let at = at.to_offset(UtcOffset::UTC);
    let stamp = format!(
        "{:04}-{:02}-{:02} {:02}.{:02}",
        at.year(),
        u8::from(at.month()),
        at.day(),
        at.hour(),
        at.minute()
    );
    let (stem, ext) = split_name(name);
    let tail = format!(" (edited by {cleaned} {stamp}){ext}");
    let room = NAME_MAX_CHARS.saturating_sub(tail.chars().count()).max(1);
    let stem = cut(stem, room);
    format!("{stem}{tail}")
}

/// `name` with its number before the extension: `numbered_name("a.txt", 2)` is `a (2).txt`.
#[must_use]
pub fn numbered_name(name: &str, n: u32) -> String {
    let (stem, ext) = split_name(name);
    format!("{stem} ({n}){ext}")
}
