//! The types every write to an external library shares, the guard that says where a change
//! goes, and the names a written copy gets.
//!
//! The store-specific code is in `s3_write.rs`, the only module that changes a bucket.

use collab_core::ids::{NodeId, UserId};
use records::{Db, SourceKind, SourceMode, Tri, WriteState};
use time::{OffsetDateTime, UtcOffset};

use crate::api::ApiError;

/// The store an external library's changes are written to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Store {
    S3(S3WriteTarget),
}

/// A read-and-write S3 library, as a write to it needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct S3WriteTarget {
    pub library: NodeId,
    /// Whether the store honours conditional writes, as the write probe last found.
    pub conditional: Tri,
}

/// Where a change to a node goes. Not `crate::room::Place`, the room registry's entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Place {
    /// Outside any connected library: the ordinary tree and content store.
    Ordinary,
    /// In a read-and-write library of an outside store: the change is staged and written there.
    External(Store),
}

/// Where a change to `node` by `user` goes, read afresh (never cached).
///
/// Outside a connected library: [`Place::Ordinary`], whoever asks (the handler judges access).
/// In one, someone who cannot see `node` is told nothing: `404`, as the other routes answer (a
/// recycle-bin entry, which nobody has access to as such, is judged by its library). Someone
/// who can: `409 source_read_only` for a GitHub, file-share or read-only S3 library; `409
/// source_write_blocked` for a read-and-write S3 library whose writes are blocked; otherwise
/// [`Place::External`] with what the probe learnt about conditional writes. Whether the
/// endpoint is still approved is not read here: a write to one no longer approved fails in the
/// worker and stays queued.
///
/// The guard belongs in the HTTP handlers, never in `tree::apply` or the upload functions,
/// which the sync engine calls directly.
pub(crate) async fn writable_place(db: &Db, user: UserId, node: NodeId) -> Result<Place, ApiError> {
    let unavailable = |e: records::DbError| ApiError::unavailable(&e);
    let Some(library) = db.source_library_of(node).await.map_err(unavailable)? else {
        return Ok(Place::Ordinary);
    };
    let visible = if db
        .effective_access(user, node)
        .await
        .map_err(unavailable)?
        .is_some()
    {
        true
    } else {
        let in_bin = db
            .node(node)
            .await
            .map_err(unavailable)?
            .is_some_and(|found| found.deleted);
        in_bin
            && db
                .effective_access(user, library)
                .await
                .map_err(unavailable)?
                .is_some()
    };
    if !visible {
        return Err(ApiError::NotFound);
    }
    let Some((config, _)) = db.source_of(library).await.map_err(unavailable)? else {
        // Disconnected a moment ago: judged as it now is.
        return Ok(Place::Ordinary);
    };
    if config.kind != SourceKind::S3 || config.mode != SourceMode::ReadWrite {
        return Err(ApiError::SourceReadOnly);
    }
    if let Some((WriteState::Blocked, _)) = db.write_state(library).await.map_err(unavailable)? {
        return Err(ApiError::SourceWriteBlocked);
    }
    let conditional = db
        .s3_place(library)
        .await
        .map_err(unavailable)?
        .map_or(Tri::Unknown, |place| place.conditional);
    Ok(Place::External(Store::S3(S3WriteTarget {
        library,
        conditional,
    })))
}

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
