//! Site templates: the rules a template's body must keep (whether saved from a site, built in
//! or imported from a package made elsewhere), the mirror of saved templates
//! (`templates/<xx>/<id>.json`) in `site_templates`, and the site groups a template's
//! audiences name by purpose.

use std::collections::HashSet;

use collab_core::ids::{GroupId, NodeId, UserId};
use revisions::{
    MAX_DESCRIPTION_CHARS, MAX_TEMPLATE_NAME_CHARS, SITE_KINDS, TemplateBody, TemplateFolder,
    TemplateNavItem, TemplateNavTarget,
};
use sqlx::Row;
use sqlx::postgres::PgRow;
use uuid::Uuid;

use crate::events::{NewEvent, record};
use crate::groups::SiteGroup;
use crate::libraries::validate_library_fields;
use crate::sites::{TemplateRow, acceptable_url, validate_language, validate_time_zone};
use crate::tree::validate_node_name;
use crate::{AccessError, Db, DbError};

/// The largest template package taken, in bytes: 1 MiB.
pub const MAX_PACKAGE_BYTES: usize = 1024 * 1024;
/// The most libraries a template makes.
pub const MAX_TEMPLATE_LIBRARIES: usize = 100;
/// The most folders a template makes, in all its libraries together.
pub const MAX_TEMPLATE_FOLDERS: usize = 5_000;
/// How deep a template's folders go: a folder directly in a library is at depth 1.
pub const MAX_TEMPLATE_DEPTH: usize = 20;
/// The longest navigation label, in characters.
const MAX_LABEL_CHARS: usize = 60;
/// The most items one navigation holds, children included.
const MAX_NAV_ITEMS: usize = 100;
/// The most groups one navigation item is aimed at.
const MAX_AUDIENCE: usize = 10;

/// The columns of a `site_templates` row as [`template_from_row`] reads them.
const TEMPLATE_COLUMNS: &str = "id, name, description, body, created_by, created_at, deleted_at";

/// Checks a template's name: 1 to 80 characters after trimming, with no control characters.
/// Returns it trimmed; `InvalidTemplate("invalid_name")` otherwise.
pub fn validate_template_name(name: &str) -> Result<&str, AccessError> {
    let name = name.trim();
    let count = name.chars().count();
    if (1..=MAX_TEMPLATE_NAME_CHARS).contains(&count) && !name.chars().any(char::is_control) {
        Ok(name)
    } else {
        Err(AccessError::InvalidTemplate("invalid_name"))
    }
}

/// Checks a template's description: at most 500 characters, no control characters but line
/// breaks and tabs. `InvalidTemplate("invalid_description")` otherwise.
pub fn validate_template_description(text: &str) -> Result<&str, AccessError> {
    let ok = text.chars().count() <= MAX_DESCRIPTION_CHARS
        && !text
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'));
    if ok {
        Ok(text)
    } else {
        Err(AccessError::InvalidTemplate("invalid_description"))
    }
}

/// Checks only the size of a template's body, cheaply and before anything else is done with
/// it: `too_many_libraries` (over 100), `too_many_folders` (over 5,000 in all) or `too_deep` (a
/// folder more than 20 levels below its library). Stops as soon as a limit is passed, so a
/// package far over the limits costs no more than one within them.
pub fn check_template_size(body: &TemplateBody) -> Result<(), AccessError> {
    let refuse = |reason: &'static str| Err(AccessError::InvalidTemplate(reason));
    if body.libraries.len() > MAX_TEMPLATE_LIBRARIES {
        return refuse("too_many_libraries");
    }
    let mut count = 0usize;
    let mut deepest = 0usize;
    let mut stack: Vec<(&[TemplateFolder], usize)> = Vec::new();
    for library in &body.libraries {
        stack.push((&library.folders, 1));
    }
    while let Some((folders, depth)) = stack.pop() {
        count = count.saturating_add(folders.len());
        if !folders.is_empty() {
            deepest = deepest.max(depth);
        }
        if count > MAX_TEMPLATE_FOLDERS {
            return refuse("too_many_folders");
        }
        if deepest > MAX_TEMPLATE_DEPTH {
            return refuse("too_deep");
        }
        for folder in folders {
            stack.push((&folder.folders, depth.saturating_add(1)));
        }
    }
    Ok(())
}

/// Checks a template's body, answering `InvalidTemplate` with the first rule it breaks:
/// `invalid_kind` (not `team` or `communication`), `invalid_language`, `invalid_time_zone`,
/// `too_many_libraries` (over 100), `too_many_folders` (over 5,000 in all), `too_deep` (a
/// folder more than 20 levels below its library), `invalid_names` (a name that is not a node
/// name, or two alike in one place, ignoring case; cleaning names first avoids this),
/// `invalid_library_settings` (a library's kind or settings that break the rules of
/// [`crate::validate_library_settings`]), and `invalid_navigation` (the rules of a site's navigation, with libraries named rather than
/// identified, and audiences of `owners`, `members` or `visitors`), and `invalid_look` (a look
/// that breaks the rules of [`revisions::brand::TemplateLook::problem`]).
pub fn validate_template_body(body: &TemplateBody) -> Result<(), AccessError> {
    let refuse = |reason: &'static str| Err(AccessError::InvalidTemplate(reason));
    if !SITE_KINDS.contains(&body.kind.as_str()) {
        return refuse("invalid_kind");
    }
    if validate_language(&body.language).is_err() {
        return refuse("invalid_language");
    }
    if validate_time_zone(&body.time_zone).is_err() {
        return refuse("invalid_time_zone");
    }
    check_template_size(body)?;
    if !names_acceptable(body) {
        return refuse("invalid_names");
    }
    if body
        .libraries
        .iter()
        .any(|library| validate_library_fields(&library.settings).is_err())
    {
        return refuse("invalid_library_settings");
    }
    if let Some(items) = &body.navigation
        && !navigation_acceptable(items)
    {
        return refuse("invalid_navigation");
    }
    if let Some(look) = &body.look
        && look.problem().is_some()
    {
        return refuse("invalid_look");
    }
    Ok(())
}

/// Whether every library and folder name is a node name, unique in its place ignoring case.
fn names_acceptable(body: &TemplateBody) -> bool {
    fn unique<'n>(names: impl IntoIterator<Item = &'n str>) -> bool {
        let mut seen = HashSet::new();
        names.into_iter().all(|name| {
            validate_node_name(name).is_ok_and(|n| n == name) && seen.insert(name.to_lowercase())
        })
    }
    if !unique(body.libraries.iter().map(|l| l.name.as_str())) {
        return false;
    }
    let mut stack: Vec<&[TemplateFolder]> = body
        .libraries
        .iter()
        .map(|l| l.folders.as_slice())
        .collect();
    while let Some(folders) = stack.pop() {
        if !unique(folders.iter().map(|f| f.name.as_str())) {
            return false;
        }
        stack.extend(folders.iter().map(|f| f.folders.as_slice()));
    }
    true
}

/// Whether a template's navigation keeps the rules of a site's: at most 100 items, children
/// included; children one level deep; labels of 1 to 60 characters (empty only for a library
/// target); at most 10 site-group purposes in an audience; addresses a link may lead to.
fn navigation_acceptable(items: &[TemplateNavItem]) -> bool {
    let mut count = 0usize;
    let mut check = |item: &TemplateNavItem, top: bool| {
        count = count.saturating_add(1);
        let label = if item.label.is_empty() {
            matches!(item.target, Some(TemplateNavTarget::Library(_)))
        } else {
            item.label.chars().count() <= MAX_LABEL_CHARS
                && !item.label.trim().is_empty()
                && !item.label.chars().any(char::is_control)
        };
        let target = match &item.target {
            None => true,
            Some(TemplateNavTarget::Url(url)) => acceptable_url(url),
            Some(TemplateNavTarget::Library(name)) => {
                !name.is_empty() && name.chars().count() <= 200
            }
        };
        count <= MAX_NAV_ITEMS
            && label
            && target
            && item.audience.len() <= MAX_AUDIENCE
            && item.audience.iter().all(|a| SiteGroup::parse(a).is_some())
            && (top || item.children.is_empty())
    };
    items
        .iter()
        .all(|item| check(item, true) && item.children.iter().all(|child| check(child, false)))
}

/// Reads one `site_templates` row selected with [`TEMPLATE_COLUMNS`].
fn template_from_row(row: &PgRow) -> Result<TemplateRow, sqlx::Error> {
    Ok(TemplateRow {
        id: row.try_get("id")?,
        name: row.try_get("name")?,
        description: row.try_get("description")?,
        body: row.try_get("body")?,
        created_by: row.try_get::<Option<Uuid>, _>("created_by")?.map(UserId),
        created_at: row.try_get("created_at")?,
        deleted_at: row.try_get("deleted_at")?,
    })
}

impl Db {
    /// The saved templates that are not deleted, by name ignoring case.
    pub async fn live_templates(&self) -> Result<Vec<TemplateRow>, DbError> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "select {TEMPLATE_COLUMNS} from site_templates where deleted_at is null \
             order by lower(name), name, id"
        )))
        .fetch_all(self.pool())
        .await?;
        rows.iter()
            .map(template_from_row)
            .collect::<Result<_, sqlx::Error>>()
            .map_err(DbError::from)
    }

    /// The saved template `id`, deleted or not; `None` if there is none.
    pub async fn template(&self, id: Uuid) -> Result<Option<TemplateRow>, DbError> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "select {TEMPLATE_COLUMNS} from site_templates where id = $1"
        )))
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        Ok(row.as_ref().map(template_from_row).transpose()?)
    }

    /// Mirrors one committed change of a saved template made by `actor`, in one transaction:
    /// `Forbidden` unless they are still an enabled administrator (judged with their account
    /// row locked), then writes the template as Git holds it and records `event`. A database
    /// failure before the change was judged allowed is `Unjudged`.
    pub async fn apply_template_change(
        &self,
        row: &TemplateRow,
        actor: UserId,
        event: NewEvent,
    ) -> Result<(), AccessError> {
        let unjudged = |e: sqlx::Error| AccessError::Unjudged(e.into());
        let mut tx = self.pool().begin().await.map_err(unjudged)?;
        let admin: Option<bool> = sqlx::query_scalar(
            "select is_admin and disabled_at is null from users where id = $1 for share",
        )
        .bind(actor.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(unjudged)?;
        if admin != Some(true) {
            return Err(AccessError::Forbidden);
        }
        sqlx::query(
            "insert into site_templates \
                 (id, name, description, body, created_by, created_at, deleted_at) \
             values ($1, $2, $3, $4, (select id from users where id = $5), $6, $7) \
             on conflict (id) do update set name = excluded.name, \
                 description = excluded.description, body = excluded.body, \
                 created_by = excluded.created_by, created_at = excluded.created_at, \
                 deleted_at = excluded.deleted_at",
        )
        .bind(row.id)
        .bind(&row.name)
        .bind(&row.description)
        .bind(&row.body)
        .bind(row.created_by.map(|u| u.0))
        .bind(row.created_at)
        .bind(row.deleted_at)
        .execute(&mut *tx)
        .await?;
        record(&mut tx, event).await?;
        tx.commit().await?;
        Ok(())
    }

    /// The site `site`'s own groups, each with its purpose.
    pub async fn site_group_ids(&self, site: NodeId) -> Result<Vec<(GroupId, SiteGroup)>, DbError> {
        let rows: Vec<(Uuid, String)> = sqlx::query_as(
            "select id, purpose from groups \
             where site_id = $1 and purpose in ('owners', 'members', 'visitors') order by id",
        )
        .bind(site.0)
        .fetch_all(self.pool())
        .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(id, purpose)| SiteGroup::parse(&purpose).map(|p| (GroupId(id), p)))
            .collect())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use revisions::{LibraryFields, TemplateLibrary};

    use super::*;

    fn body() -> TemplateBody {
        TemplateBody {
            kind: "team".into(),
            language: "en".into(),
            time_zone: "UTC".into(),
            libraries: vec![TemplateLibrary {
                name: "Documents".into(),
                settings: LibraryFields::default(),
                folders: Vec::new(),
            }],
            navigation: Some(vec![TemplateNavItem {
                label: String::new(),
                target: Some(TemplateNavTarget::Library("Documents".into())),
                audience: vec!["members".into()],
                children: Vec::new(),
            }]),
            look: None,
        }
    }

    fn folder(name: &str, folders: Vec<TemplateFolder>) -> TemplateFolder {
        TemplateFolder {
            name: name.into(),
            folders,
        }
    }

    fn refused(body: &TemplateBody) -> &'static str {
        match validate_template_body(body) {
            Err(AccessError::InvalidTemplate(reason)) => reason,
            other => panic!("not refused: {other:?}"),
        }
    }

    #[test]
    fn template_looks_are_validated() {
        let look = |from: &str| -> revisions::brand::TemplateLook {
            serde_json::from_value(serde_json::json!({
                "theme": {"name": "Dusk", "primary": "#3156c9", "secondary": "#233239"},
                "from": from,
                "logo_alt": "",
                "header": {"layout": "compact", "background": "none", "align": "left", "hide_title": false},
                "navigation": "horizontal",
                "footer": {"enabled": false, "layout": "simple", "name": "", "background": "none", "groups": []}
            }))
            .unwrap()
        };
        let mut ok = body();
        ok.look = Some(look("builtin:harbour"));
        assert_eq!(validate_template_body(&ok), Ok(()));
        ok.look = Some(look("javascript:1"));
        assert_eq!(refused(&ok), "invalid_look");
    }

    #[test]
    fn template_bodies() {
        assert_eq!(validate_template_body(&body()), Ok(()));

        let mut deep = folder("20", Vec::new());
        for n in (1..20).rev() {
            deep = folder(&n.to_string(), vec![deep]);
        }
        let mut ok = body();
        ok.libraries[0].folders = vec![deep.clone()];
        assert_eq!(validate_template_body(&ok), Ok(()));
        let mut too_deep = body();
        too_deep.libraries[0].folders = vec![folder("0", vec![deep])];
        assert_eq!(refused(&too_deep), "too_deep");

        let mut many = body();
        many.libraries[0].folders = (0..5_001)
            .map(|n| folder(&n.to_string(), Vec::new()))
            .collect();
        assert_eq!(refused(&many), "too_many_folders");
        many.libraries[0].folders.pop();
        assert_eq!(validate_template_body(&many), Ok(()));

        let mut libraries = body();
        libraries.libraries = (0..101)
            .map(|n| TemplateLibrary {
                name: format!("L{n}"),
                settings: LibraryFields::default(),
                folders: Vec::new(),
            })
            .collect();
        assert_eq!(refused(&libraries), "too_many_libraries");

        let mut image = body();
        image.libraries[0].settings.kind = "image".into();
        image.libraries[0].settings.allowed_types = Some(vec![".png".into()]);
        assert_eq!(validate_template_body(&image), Ok(()));
        image.libraries[0].settings.allowed_types = Some(vec![".exe".into()]);
        assert_eq!(refused(&image), "invalid_library_settings");
        let mut kind = body();
        kind.libraries[0].settings.kind = "wiki".into();
        assert_eq!(refused(&kind), "invalid_library_settings");

        let mut twice = body();
        twice.libraries[0].folders = vec![folder("A", Vec::new()), folder("a", Vec::new())];
        assert_eq!(refused(&twice), "invalid_names");
        let mut bad = body();
        bad.libraries[0].name = "a:b".into();
        assert_eq!(refused(&bad), "invalid_names");

        let mut audience = body();
        audience.navigation.as_mut().unwrap()[0].audience = vec!["everyone".into()];
        assert_eq!(refused(&audience), "invalid_navigation");
        let mut url = body();
        url.navigation.as_mut().unwrap()[0].target =
            Some(TemplateNavTarget::Url("javascript:alert(1)".into()));
        assert_eq!(refused(&url), "invalid_navigation");
        let mut kind = body();
        kind.kind = "wiki".into();
        assert_eq!(refused(&kind), "invalid_kind");

        assert!(validate_template_name(" Project ").is_ok_and(|n| n == "Project"));
        assert!(validate_template_name(&"x".repeat(81)).is_err());
        assert!(validate_template_name("  ").is_err());
    }
}
