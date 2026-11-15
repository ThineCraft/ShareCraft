//! File policies: which file names and sizes a library takes. A name is refused when its
//! extension is on the organisation's block list (the `blocked_file_types` setting, empty by
//! default, set by administrators), or its library does not allow it (its `blocked_types`, its
//! `allowed_types`, or for an image library the image types); a size over the library's
//! `max_file_bytes` is refused. A file's extension is the longest suffix of its name that is in
//! the list being checked, matched without regard to case, so `.tar.gz` matches before `.gz`.

use std::collections::HashSet;
use std::fmt;

use collab_core::ids::{DocId, UserId};
use serde::{Serialize, Serializer};
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::libraries::{LibrarySettings, MAX_LIBRARY_TYPES, is_extension};
use crate::{AccessError, Db, DbError};

/// The setting holding the organisation's block list: a list of extensions.
pub const BLOCKED_FILE_TYPES: &str = "blocked_file_types";

/// The longest extension a policy names, its dot included.
const MAX_TYPE_BYTES: usize = 20;

/// A file type as a refusal names it: an extension, lower case with its leading dot
/// (`".exe"`, `".tar.gz"`), or empty for a name with no extension a list could name. At most
/// 20 characters, so it is kept inline and copied freely.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileType {
    bytes: [u8; MAX_TYPE_BYTES],
    len: u8,
}

impl FileType {
    /// No extension.
    pub const NONE: FileType = FileType {
        bytes: [0; MAX_TYPE_BYTES],
        len: 0,
    };

    /// `text` as a file type, lower-cased; `NONE` unless it is then an extension (see
    /// [`is_extension`]).
    #[must_use]
    pub fn new(text: &str) -> FileType {
        let lower = text.to_ascii_lowercase();
        if !is_extension(&lower) {
            return FileType::NONE;
        }
        let mut bytes = [0; MAX_TYPE_BYTES];
        for (to, from) in bytes.iter_mut().zip(lower.bytes()) {
            *to = from;
        }
        FileType {
            bytes,
            // An extension is at most 20 ASCII characters.
            len: u8::try_from(lower.len()).unwrap_or(0),
        }
    }

    /// The extension, or `""` for none.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.bytes
            .get(..usize::from(self.len))
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .unwrap_or_default()
    }
}

impl fmt::Debug for FileType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl fmt::Display for FileType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for FileType {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// The organisation's own file policy.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OrgPolicy {
    /// The extensions refused in every library.
    pub blocked_file_types: Vec<String>,
}

/// Why a file policy refuses a name or a size.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PolicyError {
    /// The name's type is blocked by the organisation, or not allowed by the library.
    #[error("files of type {0:?} are not allowed here")]
    BlockedFileType(FileType),
    /// The file is larger than the library takes; the library's limit in bytes.
    #[error("that file is larger than the {0} bytes allowed here")]
    FileTooLarge(u64),
}

impl From<PolicyError> for AccessError {
    fn from(e: PolicyError) -> Self {
        match e {
            PolicyError::BlockedFileType(file_type) => AccessError::BlockedFileType(file_type),
            PolicyError::FileTooLarge(limit) => AccessError::FileTooLarge(limit),
        }
    }
}

/// The longest entry of `list` that ends `name`, ignoring ASCII case.
#[must_use]
pub fn matching_extension<'a>(name: &str, list: &'a [String]) -> Option<&'a str> {
    list.iter()
        .map(String::as_str)
        .filter(|ext| ends_with(name, ext))
        .max_by_key(|ext| ext.len())
}

/// The type a refusal names for `name` when no list entry matches it: what follows its last
/// dot (none for a leading dot), when that is an extension.
#[must_use]
pub fn simple_extension(name: &str) -> FileType {
    match name.rfind('.') {
        Some(dot) if dot > 0 => name.get(dot..).map_or(FileType::NONE, FileType::new),
        _ => FileType::NONE,
    }
}

/// Whether `name` ends with `suffix`, ignoring ASCII case.
fn ends_with(name: &str, suffix: &str) -> bool {
    name.len()
        .checked_sub(suffix.len())
        .and_then(|start| name.get(start..))
        .is_some_and(|tail| tail.eq_ignore_ascii_case(suffix))
}

/// Checks a file's name against the policies of the library it is going into: refused as
/// `BlockedFileType` (naming the extension that matched, or for a type the library does not
/// allow, the name's own) when its extension is on the organisation's block list or the
/// library's, or the library allows a list of types (an image library, the image types at
/// most) that does not hold it.
pub fn check_name(
    name: &str,
    library: &LibrarySettings,
    org: &OrgPolicy,
) -> Result<(), PolicyError> {
    for blocked in [&org.blocked_file_types, &library.blocked_types] {
        if let Some(ext) = matching_extension(name, blocked) {
            return Err(PolicyError::BlockedFileType(FileType::new(ext)));
        }
    }
    if let Some(allowed) = library.effective_allowed()
        && matching_extension(name, &allowed).is_none()
    {
        return Err(PolicyError::BlockedFileType(simple_extension(name)));
    }
    Ok(())
}

/// Checks a file's size against the largest file its library takes: `FileTooLarge` with the
/// limit when it is larger.
pub fn check_size(bytes: u64, library: &LibrarySettings) -> Result<(), PolicyError> {
    match library.max_file_bytes {
        Some(limit) if bytes > limit => Err(PolicyError::FileTooLarge(limit)),
        _ => Ok(()),
    }
}

/// The organisation's block list as a request gives it, made acceptable: each entry trimmed
/// and lower-cased. `InvalidSetting` unless it is then a list of at most 100 extensions (see
/// [`is_extension`]), none twice.
pub fn clean_blocked_types(value: &Value) -> Result<Vec<String>, AccessError> {
    let list = value.as_array().ok_or(AccessError::InvalidSetting)?;
    if list.len() > MAX_LIBRARY_TYPES {
        return Err(AccessError::InvalidSetting);
    }
    let mut seen = HashSet::new();
    let mut types = Vec::with_capacity(list.len());
    for each in list {
        let ext = each
            .as_str()
            .ok_or(AccessError::InvalidSetting)?
            .trim()
            .to_lowercase();
        if !is_extension(&ext) || !seen.insert(ext.clone()) {
            return Err(AccessError::InvalidSetting);
        }
        types.push(ext);
    }
    Ok(types)
}

/// Whether `value` is acceptable as the organisation's block list as stored: already clean.
pub(crate) fn valid_blocked_types(value: &Value) -> bool {
    clean_blocked_types(value).is_ok_and(|types| {
        value
            .as_array()
            .is_some_and(|list| list.iter().zip(&types).all(|(a, b)| a.as_str() == Some(b)))
    })
}

/// The organisation's file policy, on `conn`.
pub(crate) async fn org_policy_in(conn: &mut PgConnection) -> Result<OrgPolicy, sqlx::Error> {
    let value: Option<Value> = sqlx::query_scalar("select value from settings where key = $1")
        .bind(BLOCKED_FILE_TYPES)
        .fetch_optional(conn)
        .await?;
    let blocked_file_types = value
        .as_ref()
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    Ok(OrgPolicy { blocked_file_types })
}

impl Db {
    /// The organisation's file policy.
    pub async fn org_policy(&self) -> Result<OrgPolicy, DbError> {
        let mut conn = self.pool().acquire().await?;
        Ok(org_policy_in(&mut conn).await?)
    }

    /// Sets the organisation's block list, as [`Db::set_setting`] sets `blocked_file_types`
    /// (an enabled administrator only; `setting.changed` recorded), from a list cleaned by
    /// [`clean_blocked_types`]. Answers the list as stored.
    pub async fn set_blocked_file_types(
        &self,
        actor: UserId,
        value: &Value,
    ) -> Result<Vec<String>, AccessError> {
        let types = clean_blocked_types(value)?;
        self.set_setting(actor, BLOCKED_FILE_TYPES, Value::from(types.clone()))
            .await?;
        Ok(types)
    }

    /// The size of the current version of each of `docs` that has one recorded, in bytes.
    pub async fn current_sizes(&self, docs: &[DocId]) -> Result<Vec<(DocId, u64)>, DbError> {
        let ids: Vec<Uuid> = docs.iter().map(|d| d.0).collect();
        let rows: Vec<(Uuid, i64)> = sqlx::query_as(
            "select d.id, r.size from documents d \
             join revisions r on r.document_id = d.id and r.revision = d.revision \
             where d.id = any($1) and r.size is not null",
        )
        .bind(&ids)
        .fetch_all(self.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(|(id, size)| (DocId(id), u64::try_from(size).unwrap_or(0)))
            .collect())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::libraries::LibraryKind;
    use collab_core::ids::NodeId;
    use serde_json::json;

    fn library(edit: impl FnOnce(&mut LibrarySettings)) -> LibrarySettings {
        let mut settings = LibrarySettings::defaults(NodeId(Uuid::new_v4()));
        edit(&mut settings);
        settings
    }

    fn types(list: &[&str]) -> Vec<String> {
        list.iter().map(|t| (*t).to_owned()).collect()
    }

    fn blocked(name: &str, library: &LibrarySettings, org: &OrgPolicy) -> Option<String> {
        match check_name(name, library, org) {
            Ok(()) => None,
            Err(PolicyError::BlockedFileType(t)) => Some(t.as_str().to_owned()),
            Err(PolicyError::FileTooLarge(_)) => unreachable!(),
        }
    }

    #[test]
    fn longest_extension_matches_first() {
        let list = types(&[".gz", ".tar.gz", ".z"]);
        assert_eq!(matching_extension("a.TAR.GZ", &list), Some(".tar.gz"));
        assert_eq!(matching_extension("a.gz", &list), Some(".gz"));
        assert_eq!(matching_extension("a.tgz", &list), None);
        assert_eq!(matching_extension("tar.gz", &list), Some(".gz"));
        assert_eq!(matching_extension("é.gz", &list), Some(".gz"));
        let org = OrgPolicy::default();
        let lib = library(|l| l.blocked_types = list.clone());
        assert_eq!(
            blocked("Backup.Tar.Gz", &lib, &org).as_deref(),
            Some(".tar.gz")
        );
        let lib = library(|l| l.allowed_types = Some(types(&[".tar.gz"])));
        assert_eq!(blocked("Backup.tar.gz", &lib, &org), None);
        assert_eq!(blocked("Single.GZ", &lib, &org).as_deref(), Some(".gz"));
        assert_eq!(blocked("README", &lib, &org).as_deref(), Some(""));
        assert_eq!(blocked(".profile", &lib, &org).as_deref(), Some(""));
    }

    #[test]
    fn organisation_then_library_then_allowed() {
        let org = OrgPolicy {
            blocked_file_types: types(&[".exe"]),
        };
        let lib = library(|l| l.blocked_types = types(&[".bat"]));
        assert_eq!(blocked("x.EXE", &lib, &org).as_deref(), Some(".exe"));
        assert_eq!(blocked("x.bat", &lib, &org).as_deref(), Some(".bat"));
        assert_eq!(blocked("x.zip", &lib, &org), None);
        assert_eq!(blocked("x", &lib, &org), None);
        let images = library(|l| l.kind = LibraryKind::Image);
        assert_eq!(blocked("p.JPG", &images, &org), None);
        assert_eq!(blocked("p.pdf", &images, &org).as_deref(), Some(".pdf"));
        let long = format!("p.{}", "x".repeat(30));
        assert_eq!(blocked(&long, &images, &org).as_deref(), Some(""));
    }

    #[test]
    fn sizes() {
        let lib = library(|l| l.max_file_bytes = Some(10));
        assert_eq!(check_size(10, &lib), Ok(()));
        assert_eq!(check_size(11, &lib), Err(PolicyError::FileTooLarge(10)));
        assert_eq!(check_size(u64::MAX, &library(|_| {})), Ok(()));
    }

    #[test]
    fn block_lists_are_cleaned_and_checked() {
        assert_eq!(
            clean_blocked_types(&json!([" .EXE ", ".bat"])).unwrap(),
            types(&[".exe", ".bat"])
        );
        assert_eq!(
            clean_blocked_types(&json!([])).unwrap(),
            Vec::<String>::new()
        );
        for bad in [
            json!("exe"),
            json!(["exe"]),
            json!([".exe", ".EXE"]),
            json!([1]),
            json!([".a b"]),
        ] {
            assert_eq!(
                clean_blocked_types(&bad),
                Err(AccessError::InvalidSetting),
                "{bad}"
            );
        }
        let many: Vec<String> = (0..101).map(|n| format!(".x{n}")).collect();
        assert!(clean_blocked_types(&json!(many)).is_err());
        assert!(valid_blocked_types(&json!([".exe"])));
        assert!(!valid_blocked_types(&json!([".EXE"])));
    }

    #[test]
    fn file_types_are_inline() {
        assert_eq!(FileType::new(".TAR.GZ").as_str(), ".tar.gz");
        assert_eq!(FileType::new("exe"), FileType::NONE);
        assert_eq!(
            FileType::new(&format!(".{}", "a".repeat(19)))
                .as_str()
                .len(),
            20
        );
        assert_eq!(
            FileType::new(&format!(".{}", "a".repeat(20))),
            FileType::NONE
        );
        assert_eq!(
            serde_json::to_value(FileType::new(".png")).unwrap(),
            json!(".png")
        );
    }
}
