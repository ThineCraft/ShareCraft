//! Libraries' own settings: kind, description, folders, accepted and refused file types and the
//! largest file, mirrored from their records in Git (`libraries/<xx>/<id>.json`) as site
//! records are; the rules those settings keep; the asset libraries; and the files of a site's
//! template libraries.

use std::collections::HashSet;

use collab_core::ids::{DocId, NodeId, UserId};
use revisions::brand::ImageRef;
use revisions::{LIBRARY_SCHEMA, LibraryFields, LibraryRecord, MAX_DESCRIPTION_CHARS};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};
use uuid::Uuid;

use crate::documents::parse_editor;
use crate::events::{NewEvent, record};
use crate::tree::NodeChange;
use crate::{AccessError, Db, DbError, Editor, Perms};

/// The file types an image library accepts. Its own `allowed_types` may narrow this list but
/// never widen it.
pub const IMAGE_TYPES: [&str; 11] = [
    ".png", ".jpg", ".jpeg", ".gif", ".webp", ".svg", ".avif", ".bmp", ".tif", ".tiff", ".heic",
];
/// The most extensions one list of a library's settings holds.
pub const MAX_LIBRARY_TYPES: usize = 100;
/// The shortest extension, its dot included.
const MIN_EXTENSION_CHARS: usize = 2;
/// The longest extension, its dot included.
const MAX_EXTENSION_CHARS: usize = 20;
/// PostgreSQL's code for a row a check constraint refuses.
const CHECK_VIOLATION: &str = "23514";

/// The columns of a `libraries` row as [`settings_from_row`] reads them.
const LIBRARY_COLUMNS: &str =
    "id, kind, description, folders, allowed_types, blocked_types, max_file_bytes, hero_image";

/// A file in one of a site's template libraries: a "New from template" choice for whoever may
/// view it and its content is available (the caller judges both).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TemplateFile {
    /// The file's document.
    pub id: DocId,
    pub name: String,
    pub editor: Editor,
    /// The template library it is in, at any depth.
    pub library: NodeId,
    pub library_name: String,
    pub revised_at: Option<time::OffsetDateTime>,
}

/// What a library is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LibraryKind {
    /// Files of any kind, as every library was before kinds.
    Document,
    /// Images only, shown as a grid of thumbnails.
    Image,
    /// Its files are the site's "New from template" choices.
    Template,
    /// Logos and brand images, marked by an administrator.
    Asset,
}

impl LibraryKind {
    /// The word for the kind, as stored and as written in JSON.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            LibraryKind::Document => "document",
            LibraryKind::Image => "image",
            LibraryKind::Template => "template",
            LibraryKind::Asset => "asset",
        }
    }

    /// The kind named `word`.
    #[must_use]
    pub fn parse(word: &str) -> Option<LibraryKind> {
        match word {
            "document" => Some(LibraryKind::Document),
            "image" => Some(LibraryKind::Image),
            "template" => Some(LibraryKind::Template),
            "asset" => Some(LibraryKind::Asset),
            _ => None,
        }
    }
}

/// A library's own settings, as PostgreSQL mirrors them from its record in Git.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LibrarySettings {
    /// The library's node.
    pub id: NodeId,
    pub kind: LibraryKind,
    /// What the library is for; 0 to 500 characters.
    pub description: String,
    /// Whether new folders may be made in it.
    pub folders: bool,
    /// The file types it accepts, as extensions; `None` for every type (for an image
    /// library, every image type).
    pub allowed_types: Option<Vec<String>>,
    /// The file types it refuses, as extensions.
    pub blocked_types: Vec<String>,
    /// The largest file it takes, in bytes; `None` for no limit of its own.
    pub max_file_bytes: Option<u64>,
    /// The image shown in place of the site header's banner image on its pages, if any. It
    /// pins its content (`brand_refs` rows owned by the library's id).
    pub hero_image: Option<ImageRef>,
}

impl LibrarySettings {
    /// The settings of a library with no record: a document library with no description,
    /// folders on, every type allowed, nothing blocked and no limit.
    #[must_use]
    pub fn defaults(id: NodeId) -> LibrarySettings {
        LibrarySettings {
            id,
            kind: LibraryKind::Document,
            description: String::new(),
            folders: true,
            allowed_types: None,
            blocked_types: Vec::new(),
            max_file_bytes: None,
            hero_image: None,
        }
    }

    /// The settings a library record holds; `None` for an unknown kind.
    #[must_use]
    pub fn from_record(record: &LibraryRecord) -> Option<LibrarySettings> {
        Some(LibrarySettings {
            id: NodeId(record.id),
            kind: LibraryKind::parse(&record.kind)?,
            description: record.description.clone(),
            folders: record.folders,
            allowed_types: record.allowed_types.clone(),
            blocked_types: record.blocked_types.clone(),
            max_file_bytes: record.max_file_bytes,
            hero_image: record.hero_image.clone(),
        })
    }

    /// The library record that holds these settings.
    #[must_use]
    pub fn to_record(&self) -> LibraryRecord {
        let mut record = LibraryRecord::new(
            self.id.0,
            LibraryFields {
                kind: self.kind.as_str().to_owned(),
                description: self.description.clone(),
                folders: self.folders,
                allowed_types: self.allowed_types.clone(),
                blocked_types: self.blocked_types.clone(),
                max_file_bytes: self.max_file_bytes,
            },
        );
        record.hero_image = self.hero_image.clone();
        record
    }

    /// The file types the library accepts in effect: its own list, and for an image library
    /// the image types when it has none. `None` for every type.
    #[must_use]
    pub fn effective_allowed(&self) -> Option<Vec<String>> {
        match (&self.allowed_types, self.kind) {
            (Some(types), _) => Some(types.clone()),
            (None, LibraryKind::Image) => {
                Some(IMAGE_TYPES.iter().map(|t| (*t).to_owned()).collect())
            }
            (None, _) => None,
        }
    }
}

/// Whether `text` is an extension as library settings write one: lower case, its leading dot,
/// 2 to 20 characters of letters, digits and dots, with no empty part (`".png"`, `".tar.gz"`).
#[must_use]
pub fn is_extension(text: &str) -> bool {
    let count = text.chars().count();
    (MIN_EXTENSION_CHARS..=MAX_EXTENSION_CHARS).contains(&count)
        && text.starts_with('.')
        && text
            .chars()
            .all(|c| c == '.' || c.is_ascii_lowercase() || c.is_ascii_digit())
        && text
            .get(1..)
            .is_some_and(|rest| rest.split('.').all(|part| !part.is_empty()))
}

/// Checks one list of extensions: at most 100, each an extension (see [`is_extension`]), none
/// twice.
fn check_types(types: &[String]) -> Result<(), &'static str> {
    if types.len() > MAX_LIBRARY_TYPES {
        return Err("too_many_types");
    }
    let mut seen = HashSet::new();
    for each in types {
        if !is_extension(each) {
            return Err("invalid_extension");
        }
        if !seen.insert(each.as_str()) {
            return Err("duplicate_type");
        }
    }
    Ok(())
}

/// Checks a library's settings, answering `InvalidLibrarySettings` with the first rule they
/// break: `invalid_schema`; `invalid_kind` (not `document`, `image`, `template` or `asset`);
/// `invalid_description` (over 500 characters, or control characters other than line breaks
/// and tabs); for `allowed_types` and `blocked_types`, `too_many_types` (over 100),
/// `invalid_extension` (see [`is_extension`]) or `duplicate_type`; `no_types` (an empty
/// `allowed_types`: `null` allows every type); `not_an_image_type` (an image library allowing
/// a type outside [`IMAGE_TYPES`]); and `invalid_max_file_bytes` (zero, or beyond what the
/// database holds). Kind `asset` is `Forbidden` unless `admin`: only administrators set it.
pub fn validate_library_settings(record: &LibraryRecord, admin: bool) -> Result<(), AccessError> {
    let refuse = |reason: &'static str| Err(AccessError::InvalidLibrarySettings(reason));
    if record.schema != LIBRARY_SCHEMA {
        return refuse("invalid_schema");
    }
    let Some(kind) = LibraryKind::parse(&record.kind) else {
        return refuse("invalid_kind");
    };
    let description_ok = record.description.chars().count() <= MAX_DESCRIPTION_CHARS
        && !record
            .description
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'));
    if !description_ok {
        return refuse("invalid_description");
    }
    if let Some(allowed) = &record.allowed_types {
        if let Err(reason) = check_types(allowed) {
            return refuse(reason);
        }
        if allowed.is_empty() {
            return refuse("no_types");
        }
        if kind == LibraryKind::Image && !allowed.iter().all(|t| IMAGE_TYPES.contains(&t.as_str()))
        {
            return refuse("not_an_image_type");
        }
    }
    if let Err(reason) = check_types(&record.blocked_types) {
        return refuse(reason);
    }
    if record
        .max_file_bytes
        .is_some_and(|bytes| bytes == 0 || i64::try_from(bytes).is_err())
    {
        return refuse("invalid_max_file_bytes");
    }
    if kind == LibraryKind::Asset && !admin {
        return Err(AccessError::Forbidden);
    }
    Ok(())
}

/// Checks the settings a site template carries for one library, as
/// [`validate_library_settings`] does for an administrator (only administrators work with
/// templates).
pub fn validate_library_fields(fields: &LibraryFields) -> Result<(), AccessError> {
    validate_library_settings(&LibraryRecord::new(Uuid::nil(), fields.clone()), true)
}

/// One change of a library's settings, mirrored once Git has recorded it.
#[derive(Clone, Debug)]
pub struct LibraryChange<'a> {
    /// The settings as Git now holds them.
    pub settings: &'a LibrarySettings,
    /// Who makes the change; `None` for the server itself.
    pub actor: Option<UserId>,
    /// Whether `actor` must be an enabled administrator.
    pub needs_admin: bool,
    /// The permissions `actor` must hold on the library; empty for none.
    pub needs: Perms,
    /// The events to record with the change.
    pub events: Vec<NewEvent>,
}

/// Reads one `libraries` row selected with [`LIBRARY_COLUMNS`].
fn settings_from_row(row: &PgRow) -> Result<LibrarySettings, sqlx::Error> {
    let kind: String = row.try_get("kind")?;
    let bytes: Option<i64> = row.try_get("max_file_bytes")?;
    let hero: Option<serde_json::Value> = row.try_get("hero_image")?;
    let hero_image = hero
        .map(serde_json::from_value::<ImageRef>)
        .transpose()
        .map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
    Ok(LibrarySettings {
        id: NodeId(row.try_get("id")?),
        kind: LibraryKind::parse(&kind)
            .ok_or_else(|| sqlx::Error::Decode(format!("unknown library kind {kind:?}").into()))?,
        description: row.try_get("description")?,
        folders: row.try_get("folders")?,
        allowed_types: row.try_get("allowed_types")?,
        blocked_types: row.try_get("blocked_types")?,
        max_file_bytes: bytes.map(|b| u64::try_from(b).unwrap_or(0)),
        hero_image,
    })
}

/// Gives the library `library` a `libraries` row with the defaults if it has none: a library
/// whose record Git does not hold yet.
pub(crate) async fn ensure_library_row(
    conn: &mut PgConnection,
    library: NodeId,
) -> Result<(), sqlx::Error> {
    sqlx::query("insert into libraries (id) values ($1) on conflict (id) do nothing")
        .bind(library.0)
        .execute(conn)
        .await?;
    Ok(())
}

/// Writes `settings` into the library's row, which the caller has locked if it exists. A row
/// the database's own checks refuse is `InvalidLibrarySettings("refused")`.
pub(crate) async fn upsert_library(
    conn: &mut PgConnection,
    settings: &LibrarySettings,
) -> Result<(), AccessError> {
    let bytes = settings
        .max_file_bytes
        .map(|b| i64::try_from(b).map_err(|_| AccessError::InvalidLibrarySettings("refused")))
        .transpose()?;
    let result = sqlx::query(
        "insert into libraries (id, kind, description, folders, allowed_types, blocked_types, \
             max_file_bytes, hero_image) \
         values ($1, $2, $3, $4, $5, $6, $7, $8) \
         on conflict (id) do update set kind = excluded.kind, \
             description = excluded.description, folders = excluded.folders, \
             allowed_types = excluded.allowed_types, blocked_types = excluded.blocked_types, \
             max_file_bytes = excluded.max_file_bytes, hero_image = excluded.hero_image",
    )
    .bind(settings.id.0)
    .bind(settings.kind.as_str())
    .bind(&settings.description)
    .bind(settings.folders)
    .bind(&settings.allowed_types)
    .bind(&settings.blocked_types)
    .bind(bytes)
    .bind(
        settings
            .hero_image
            .as_ref()
            .map(serde_json::to_value)
            .transpose()
            .map_err(|_| AccessError::InvalidLibrarySettings("refused"))?,
    )
    .execute(conn)
    .await;
    match result {
        Ok(_) => Ok(()),
        Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some(CHECK_VIOLATION) => {
            Err(AccessError::InvalidLibrarySettings("refused"))
        }
        Err(e) => Err(e.into()),
    }
}

/// Locks the `libraries` rows of `libraries` `FOR UPDATE`, by id: after the node and site rows,
/// before the links, usage rows, groups and roles.
pub(crate) async fn lock_library_rows(
    conn: &mut PgConnection,
    libraries: &[NodeId],
) -> Result<(), sqlx::Error> {
    if libraries.is_empty() {
        return Ok(());
    }
    let mut ids: Vec<Uuid> = libraries.iter().map(|id| id.0).collect();
    ids.sort_unstable();
    sqlx::query("select id from libraries where id = any($1) order by id for update")
        .bind(&ids)
        .execute(conn)
        .await?;
    Ok(())
}

impl Db {
    /// The settings of the library `library`, whoever may see it; `None` if there is no such
    /// library.
    pub async fn library_settings(
        &self,
        library: NodeId,
    ) -> Result<Option<LibrarySettings>, DbError> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "select {LIBRARY_COLUMNS} from libraries where id = $1"
        )))
        .bind(library.0)
        .fetch_optional(self.pool())
        .await?;
        Ok(row.as_ref().map(settings_from_row).transpose()?)
    }

    /// The settings of every library, deleted or not, for comparing with Git.
    pub async fn all_library_settings(&self) -> Result<Vec<LibrarySettings>, DbError> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "select {LIBRARY_COLUMNS} from libraries"
        )))
        .fetch_all(self.pool())
        .await?;
        rows.iter()
            .map(settings_from_row)
            .collect::<Result<_, sqlx::Error>>()
            .map_err(DbError::from)
    }

    /// The library that `node` is in, or `node` itself when it is one; `None` for a site, or
    /// for no such node. The walk ends even if the data held a cycle.
    pub async fn library_of(&self, node: NodeId) -> Result<Option<NodeId>, DbError> {
        let found: Option<Uuid> = sqlx::query_scalar(
            "with recursive up(id, parent_id, kind, depth) as ( \
                 select id, parent_id, kind, 0 from nodes where id = $1 \
                 union all \
                 select n.id, n.parent_id, n.kind, up.depth + 1 \
                 from nodes n join up on n.id = up.parent_id \
                 where up.kind <> 'library' and up.depth < 10000 \
             ) select id from up where kind = 'library' limit 1",
        )
        .bind(node.0)
        .fetch_optional(self.pool())
        .await?;
        Ok(found.map(NodeId))
    }

    /// The asset libraries that are not deleted, in sites that are not deleted, by name
    /// ignoring case. Whoever asks: the caller keeps those the person can view.
    pub async fn asset_libraries(&self) -> Result<Vec<NodeId>, DbError> {
        let ids: Vec<Uuid> = sqlx::query_scalar(
            "select n.id from libraries l join nodes n on n.id = l.id \
             join nodes s on s.id = n.site_id \
             where l.kind = 'asset' and n.deleted_at is null and s.deleted_at is null \
             order by lower(n.name), n.id",
        )
        .fetch_all(self.pool())
        .await?;
        Ok(ids.into_iter().map(NodeId).collect())
    }

    /// The live files with at least one version in the live template libraries of the site
    /// `site`, at any depth below them, by name ignoring case. Whoever asks: the caller keeps
    /// those the person can view whose content is available. Empty for a deleted site. The
    /// walk ends even if the data held a cycle.
    pub async fn template_files(&self, site: NodeId) -> Result<Vec<TemplateFile>, DbError> {
        type Row = (
            Uuid,
            String,
            String,
            Uuid,
            String,
            Option<time::OffsetDateTime>,
        );
        let rows: Vec<Row> = sqlx::query_as(
            "with recursive below(id, library_id, depth) as ( \
                 select n.id, n.id, 0 from libraries l join nodes n on n.id = l.id \
                 join nodes s on s.id = n.site_id \
                 where l.kind = 'template' and n.site_id = $1 and n.kind = 'library' \
                   and n.deleted_at is null and s.deleted_at is null \
                 union all \
                 select c.id, b.library_id, b.depth + 1 from nodes c join below b \
                   on c.parent_id = b.id \
                 where c.deleted_at is null and b.depth < 10000 \
             ) \
             select n.id, n.name, d.editor, b.library_id, lib.name, \
                    coalesce(d.revised_at, si.modified_at) as revised_at \
             from below b join nodes n on n.id = b.id \
             join documents d on d.id = n.id \
             left join source_items si on si.node_id = n.id \
             join nodes lib on lib.id = b.library_id \
             where n.kind = 'document' and coalesce(d.revision, si.revision) is not null \
             order by lower(n.name), n.id",
        )
        .bind(site.0)
        .fetch_all(self.pool())
        .await?;
        rows.into_iter()
            .map(|(id, name, editor, library, library_name, revised_at)| {
                Ok(TemplateFile {
                    id: DocId(id),
                    name,
                    editor: parse_editor(&editor)?,
                    library: NodeId(library),
                    library_name,
                    revised_at,
                })
            })
            .collect::<Result<_, sqlx::Error>>()
            .map_err(DbError::from)
    }

    /// Mirrors one committed change of a library's settings, in one transaction: judges what
    /// it needs as a tree change does (`NotFound` if the actor can no longer see the library,
    /// `Forbidden` without the permissions `needs` on it, `Archived` on an archived site or,
    /// when `needs_admin`, `Forbidden` unless they are an enabled administrator), writes the
    /// library's row (locked `FOR UPDATE` after the node and site rows and before any group or
    /// role; `InvalidLibrarySettings("refused")` when the database's checks refuse it) and
    /// records the events. A database failure before the change was judged allowed is
    /// `Unjudged`.
    pub async fn apply_library_change(&self, change: LibraryChange<'_>) -> Result<(), AccessError> {
        let library = change.settings.id;
        let needs: Vec<(NodeId, Perms)> = if change.needs == Perms::NONE {
            Vec::new()
        } else {
            vec![(library, change.needs)]
        };
        let (mut tx, _) = self
            .judged(&NodeChange {
                actor: change.actor,
                needs_admin: change.needs_admin,
                needs: &needs,
                libraries: std::slice::from_ref(&library),
                ..NodeChange::default()
            })
            .await
            .map_err(|e| match e {
                AccessError::Db(e) => AccessError::Unjudged(e),
                other => other,
            })?;
        upsert_library(&mut tx, change.settings).await?;
        // The hero image pins its content, as a site's look pins its images; a repair from Git
        // (no actor) brings back a content row that is missing.
        let pins: Vec<crate::brand::Pin<'_>> = change
            .settings
            .hero_image
            .iter()
            .map(crate::brand::Pin::from)
            .collect();
        let pinned = crate::brand::pin_contents(
            &mut tx,
            &library.0.to_string(),
            &pins,
            change.actor.is_none(),
        )
        .await?;
        if !pinned {
            return Err(AccessError::InvalidLibrarySettings("image_gone"));
        }
        for event in change.events {
            record(&mut tx, event).await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;

    fn record(edit: impl FnOnce(&mut LibraryRecord)) -> LibraryRecord {
        let mut record = LibraryRecord::defaults(Uuid::new_v4());
        edit(&mut record);
        record
    }

    fn reason(record: &LibraryRecord) -> Option<&'static str> {
        match validate_library_settings(record, true) {
            Ok(()) => None,
            Err(AccessError::InvalidLibrarySettings(reason)) => Some(reason),
            Err(other) => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn extensions() {
        for good in [".png", ".tar.gz", ".7z", ".a1", ".abcdefghijklmnopqrs"] {
            assert!(is_extension(good), "{good}");
        }
        for bad in [
            "",
            ".",
            "png",
            ".PNG",
            ".p ng",
            "..",
            ".tar..gz",
            ".png.",
            ".pñg",
            "./x",
            ".abcdefghijklmnopqrst",
        ] {
            assert!(!is_extension(bad), "{bad}");
        }
    }

    #[test]
    fn settings_rules() {
        assert_eq!(reason(&record(|_| {})), None);
        assert_eq!(
            reason(&record(|r| r.kind = "wiki".into())),
            Some("invalid_kind")
        );
        assert_eq!(reason(&record(|r| r.schema = 2)), Some("invalid_schema"));
        assert_eq!(
            reason(&record(|r| r.description = "x".repeat(501))),
            Some("invalid_description")
        );
        assert_eq!(reason(&record(|r| r.description = "x".repeat(500))), None);
        assert_eq!(
            reason(&record(|r| r.description = "bell\u{7}".into())),
            Some("invalid_description")
        );
        let many: Vec<String> = (0..101).map(|n| format!(".x{n}")).collect();
        assert_eq!(
            reason(&record(|r| r.blocked_types = many.clone())),
            Some("too_many_types")
        );
        assert_eq!(
            reason(&record(|r| r.allowed_types = Some(many[..100].to_vec()))),
            None
        );
        assert_eq!(
            reason(&record(|r| r.blocked_types = vec!["exe".into()])),
            Some("invalid_extension")
        );
        assert_eq!(
            reason(&record(
                |r| r.blocked_types = vec![".exe".into(), ".exe".into()]
            )),
            Some("duplicate_type")
        );
        assert_eq!(
            reason(&record(|r| r.allowed_types = Some(Vec::new()))),
            Some("no_types")
        );
        assert_eq!(
            reason(&record(|r| {
                r.kind = "image".into();
                r.allowed_types = Some(vec![".png".into(), ".pdf".into()]);
            })),
            Some("not_an_image_type")
        );
        assert_eq!(
            reason(&record(|r| {
                r.kind = "image".into();
                r.allowed_types = Some(vec![".png".into()]);
            })),
            None
        );
        assert_eq!(
            reason(&record(|r| r.max_file_bytes = Some(0))),
            Some("invalid_max_file_bytes")
        );
        assert_eq!(
            reason(&record(|r| r.max_file_bytes = Some(u64::MAX))),
            Some("invalid_max_file_bytes")
        );
        let asset = record(|r| r.kind = "asset".into());
        assert_eq!(
            validate_library_settings(&asset, false),
            Err(AccessError::Forbidden)
        );
        assert_eq!(validate_library_settings(&asset, true), Ok(()));
    }

    #[test]
    fn image_libraries_allow_images_in_effect() {
        let mut settings = LibrarySettings::defaults(NodeId(Uuid::new_v4()));
        assert_eq!(settings.effective_allowed(), None);
        settings.kind = LibraryKind::Image;
        assert_eq!(
            settings.effective_allowed().unwrap().len(),
            IMAGE_TYPES.len()
        );
        settings.allowed_types = Some(vec![".png".into()]);
        assert_eq!(settings.effective_allowed(), Some(vec![".png".to_owned()]));
        let back = LibrarySettings::from_record(&settings.to_record()).unwrap();
        assert_eq!(back, settings);
    }
}
