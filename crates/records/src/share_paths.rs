//! Approved file-share paths and the settings which bound their use.
//!
//! Parsing is deliberately lexical so that the same input is accepted or refused on every
//! operating system. File-system containment is checked later, where a share is opened.

use std::path::PathBuf;

use serde_json::Value;

use crate::sources::{DEFAULT_SOURCE_MAX_FILES, SOURCE_MAX_FILES};
use crate::{Db, DbError};

/// Why a path cannot be used as a file-share path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathRefusal {
    Empty,
    NotAbsolute,
    DevicePrefix,
    Traversal,
    EmptySegment,
    BadCharacter,
    DeviceName,
    Stream,
    TrailingDotOrSpace,
    TooLong,
    WholeDrive,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PathKind {
    Unc {
        server: String,
        share: String,
        folders: Vec<String>,
    },
    Drive {
        letter: char,
        folders: Vec<String>,
    },
    Posix {
        folders: Vec<String>,
    },
}

/// An absolute UNC, drive, or POSIX path, split into validated whole segments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SharePath(PathKind);

impl SharePath {
    /// Parses and validates an absolute path without consulting the file system.
    pub fn parse(text: &str) -> Result<SharePath, PathRefusal> {
        if text.is_empty() {
            return Err(PathRefusal::Empty);
        }
        if text.chars().count() > 1_000 {
            return Err(PathRefusal::TooLong);
        }
        let normal = text.replace('\\', "/");
        if normal.starts_with("//?/") || normal.starts_with("//./") {
            return Err(PathRefusal::DevicePrefix);
        }

        if let Some(rest) = normal.strip_prefix("//") {
            let segments = segments(rest)?;
            let mut parts = segments.into_iter();
            let server = parts.next().ok_or(PathRefusal::EmptySegment)?;
            let share = parts.next().ok_or(PathRefusal::EmptySegment)?;
            let folders = parts.collect();
            return Ok(SharePath(PathKind::Unc {
                server,
                share,
                folders,
            }));
        }

        let mut chars = normal.chars();
        if let (Some(letter), Some(':')) = (chars.next(), chars.next())
            && letter.is_ascii_alphabetic()
        {
            let Some(rest) = normal.get(2..) else {
                return Err(PathRefusal::NotAbsolute);
            };
            let Some(rest) = rest.strip_prefix('/') else {
                return Err(PathRefusal::NotAbsolute);
            };
            return Ok(SharePath(PathKind::Drive {
                letter: letter.to_ascii_uppercase(),
                folders: segments(rest)?,
            }));
        }

        if let Some(rest) = normal.strip_prefix('/') {
            return Ok(SharePath(PathKind::Posix {
                folders: segments(rest)?,
            }));
        }
        Err(PathRefusal::NotAbsolute)
    }

    /// Writes the canonical separator form for this kind of path.
    pub fn display(&self) -> String {
        match &self.0 {
            PathKind::Unc {
                server,
                share,
                folders,
            } => {
                let mut result = format!(r"\\{server}\{share}");
                append(&mut result, folders, '\\');
                result
            }
            PathKind::Drive { letter, folders } => {
                let mut result = format!(r"{letter}:\");
                result.push_str(&folders.join(r"\"));
                result
            }
            PathKind::Posix { folders } => {
                let mut result = String::from("/");
                result.push_str(&folders.join("/"));
                result
            }
        }
    }

    /// Converts the canonical display form to an operating-system path.
    pub fn to_path_buf(&self) -> PathBuf {
        PathBuf::from(self.display())
    }

    /// Whether this is a UNC path whose share name ends in `$`.
    pub fn is_admin_share(&self) -> bool {
        matches!(&self.0, PathKind::Unc { share, .. } if share.ends_with('$'))
    }

    /// Whether this path equals `root` or is below it by whole segments.
    pub fn is_within(&self, root: &SharePath) -> bool {
        match (&self.0, &root.0) {
            (
                PathKind::Unc {
                    server,
                    share,
                    folders,
                },
                PathKind::Unc {
                    server: root_server,
                    share: root_share,
                    folders: root_folders,
                },
            ) => {
                equal_windows(server, root_server)
                    && equal_windows(share, root_share)
                    && prefix(folders, root_folders, equal_windows)
            }
            (
                PathKind::Drive { letter, folders },
                PathKind::Drive {
                    letter: root_letter,
                    folders: root_folders,
                },
            ) => {
                letter.eq_ignore_ascii_case(root_letter)
                    && prefix(folders, root_folders, equal_windows)
            }
            (
                PathKind::Posix { folders },
                PathKind::Posix {
                    folders: root_folders,
                },
            ) => prefix(folders, root_folders, |left, right| left == right),
            _ => false,
        }
    }

    /// Appends a `/`-separated relative path, refusing absolute or climbing input.
    pub fn join(&self, relative: &str) -> Result<SharePath, PathRefusal> {
        if relative.is_empty() {
            return Err(PathRefusal::Empty);
        }
        if relative.chars().count() > 1_000 {
            return Err(PathRefusal::TooLong);
        }
        let normal = relative.replace('\\', "/");
        if normal.starts_with('/') || looks_like_drive(&normal) {
            return Err(PathRefusal::NotAbsolute);
        }
        let joined = segments(&normal)?;
        let mut path = self.clone();
        match &mut path.0 {
            PathKind::Unc { folders, .. }
            | PathKind::Drive { folders, .. }
            | PathKind::Posix { folders } => folders.extend(joined),
        }
        if path.display().chars().count() > 1_000 {
            return Err(PathRefusal::TooLong);
        }
        Ok(path)
    }
}

/// Returns the first approved root containing `path`.
pub fn approved_root<'a>(path: &SharePath, roots: &'a [SharePath]) -> Option<&'a SharePath> {
    roots.iter().find(|root| path.is_within(root))
}

/// Refuses approvals which would grant an entire drive or a bare administrative share.
pub fn valid_root(root: &SharePath) -> Result<(), PathRefusal> {
    match &root.0 {
        PathKind::Drive { folders, .. } if folders.is_empty() => Err(PathRefusal::WholeDrive),
        PathKind::Unc { share, folders, .. } if share.ends_with('$') && folders.is_empty() => {
            Err(PathRefusal::WholeDrive)
        }
        _ => Ok(()),
    }
}

/// Whether `name` is one Windows-safe path segment.
pub fn windows_safe_name(name: &str) -> bool {
    !name.is_empty() && !name.contains(['/', '\\', ':']) && validate_segment(name).is_ok()
}

/// Whether common operating-system housekeeping should be hidden from a listing by default.
pub fn hidden_by_default(name: &str) -> bool {
    name.eq_ignore_ascii_case("Thumbs.db")
        || name.eq_ignore_ascii_case("desktop.ini")
        || name.eq_ignore_ascii_case(".DS_Store")
        || name.starts_with("~$")
}

/// Whether `name` is owned by ShareCraft and must never be exposed as library content.
pub fn reserved_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower == ".sharecraft-bin" || lower.starts_with(".sharecraft-tmp-")
}

/// The setting containing a JSON array of approved absolute roots.
pub const SHARE_ROOTS: &str = "share_roots";
/// The maximum number of external bytes held in the local cache.
pub const SHARE_CACHE_MAX_BYTES: &str = "share_cache_max_bytes";
/// The maximum duration of one share listing pass, in seconds.
pub const SHARE_LIST_SECONDS: &str = "share_list_seconds";

const DEFAULT_CACHE_MAX_BYTES: u64 = 10 * 1024 * 1024 * 1024;
const MIN_CACHE_MAX_BYTES: u64 = 256 * 1024 * 1024;
const DEFAULT_LIST_SECONDS: u64 = 600;
const MIN_LIST_SECONDS: u64 = 30;
const MAX_LIST_SECONDS: u64 = 3_600;
const MAX_ROOTS: usize = 20;

/// File-share settings read as one consistent value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShareSettings {
    pub roots: Vec<SharePath>,
    pub cache_max_bytes: u64,
    pub list_seconds: u64,
    pub max_files: usize,
}

/// Whether a setting value is a valid approved-root list.
pub(crate) fn valid_roots_setting(value: &Value) -> bool {
    let Some(values) = value.as_array() else {
        return false;
    };
    values.len() <= MAX_ROOTS
        && values.iter().all(|value| {
            value
                .as_str()
                .and_then(|text| SharePath::parse(text).ok())
                .is_some_and(|path| valid_root(&path).is_ok())
        })
}

/// Whether a setting value is a permitted cache size.
pub(crate) fn valid_cache_setting(value: &Value) -> bool {
    value
        .as_u64()
        .is_some_and(|bytes| bytes >= MIN_CACHE_MAX_BYTES)
}

/// Whether a setting value is a permitted listing duration.
pub(crate) fn valid_list_setting(value: &Value) -> bool {
    value
        .as_u64()
        .is_some_and(|seconds| (MIN_LIST_SECONDS..=MAX_LIST_SECONDS).contains(&seconds))
}

impl Db {
    /// Reads all settings used by file shares, applying safe defaults to missing values.
    pub async fn share_settings(&self) -> Result<ShareSettings, DbError> {
        let rows: Vec<(String, Value)> =
            sqlx::query_as("select key, value from settings where key = any($1) order by key")
                .bind(vec![
                    SHARE_ROOTS,
                    SHARE_CACHE_MAX_BYTES,
                    SHARE_LIST_SECONDS,
                    SOURCE_MAX_FILES,
                ])
                .fetch_all(self.pool())
                .await?;

        let mut settings = ShareSettings {
            roots: Vec::new(),
            cache_max_bytes: DEFAULT_CACHE_MAX_BYTES,
            list_seconds: DEFAULT_LIST_SECONDS,
            max_files: usize::try_from(DEFAULT_SOURCE_MAX_FILES).unwrap_or(50_000),
        };
        for (key, value) in rows {
            match key.as_str() {
                SHARE_ROOTS => {
                    settings.roots = value
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .filter_map(|text| match SharePath::parse(text) {
                            Ok(path) if valid_root(&path).is_ok() => Some(path),
                            _ => {
                                eprintln!("ignored an invalid stored approved share root");
                                None
                            }
                        })
                        .collect();
                }
                SHARE_CACHE_MAX_BYTES => {
                    if let Some(bytes) =
                        value.as_u64().filter(|bytes| *bytes >= MIN_CACHE_MAX_BYTES)
                    {
                        settings.cache_max_bytes = bytes;
                    }
                }
                SHARE_LIST_SECONDS => {
                    if let Some(seconds) = value
                        .as_u64()
                        .filter(|seconds| (MIN_LIST_SECONDS..=MAX_LIST_SECONDS).contains(seconds))
                    {
                        settings.list_seconds = seconds;
                    }
                }
                SOURCE_MAX_FILES => {
                    if let Some(files) = value.as_u64().and_then(|n| usize::try_from(n).ok()) {
                        settings.max_files = files;
                    }
                }
                _ => {}
            }
        }
        Ok(settings)
    }
}

fn segments(text: &str) -> Result<Vec<String>, PathRefusal> {
    let text = text.trim_end_matches('/');
    if text.is_empty() {
        return Ok(Vec::new());
    }
    text.split('/')
        .map(|segment| {
            validate_segment(segment)?;
            Ok(segment.to_string())
        })
        .collect()
}

fn validate_segment(segment: &str) -> Result<(), PathRefusal> {
    if segment.is_empty() {
        return Err(PathRefusal::EmptySegment);
    }
    if segment == "." || segment == ".." {
        return Err(PathRefusal::Traversal);
    }
    if segment.contains(':') {
        return Err(PathRefusal::Stream);
    }
    if segment
        .chars()
        .any(|ch| ch.is_control() || matches!(ch, '"' | '*' | '<' | '>' | '?' | '|'))
        || reserved_name(segment)
    {
        return Err(PathRefusal::BadCharacter);
    }
    if is_device_name(segment) {
        return Err(PathRefusal::DeviceName);
    }
    if segment.ends_with(['.', ' ']) {
        return Err(PathRefusal::TrailingDotOrSpace);
    }
    Ok(())
}

fn is_device_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or_default();
    if stem.eq_ignore_ascii_case("CON")
        || stem.eq_ignore_ascii_case("PRN")
        || stem.eq_ignore_ascii_case("AUX")
        || stem.eq_ignore_ascii_case("NUL")
    {
        return true;
    }
    let upper = stem.to_ascii_uppercase();
    let Some(number) = upper
        .strip_prefix("COM")
        .or_else(|| upper.strip_prefix("LPT"))
    else {
        return false;
    };
    matches!(number, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
}

fn equal_windows(left: &str, right: &str) -> bool {
    // NTFS compares names through its single-code-point $UpCase table. Fold each character
    // independently (so sigma and final sigma both become capital sigma), and leave a character
    // literal when Unicode defines only a multi-character uppercase mapping. Callers apply this
    // to individual path segments, never to a whole path string.
    left.chars()
        .map(simple_uppercase)
        .eq(right.chars().map(simple_uppercase))
}

fn simple_uppercase(character: char) -> char {
    let mut uppercase = character.to_uppercase();
    let Some(first) = uppercase.next() else {
        return character;
    };
    if uppercase.next().is_none() {
        first
    } else {
        character
    }
}

fn prefix(path: &[String], root: &[String], equal: impl Fn(&str, &str) -> bool) -> bool {
    path.len() >= root.len()
        && path
            .iter()
            .zip(root)
            .all(|(segment, root_segment)| equal(segment, root_segment))
}

fn append(result: &mut String, folders: &[String], separator: char) {
    for folder in folders {
        result.push(separator);
        result.push_str(folder);
    }
}

fn looks_like_drive(text: &str) -> bool {
    let mut chars = text.chars();
    matches!((chars.next(), chars.next()), (Some(letter), Some(':')) if letter.is_ascii_alphabetic())
}
