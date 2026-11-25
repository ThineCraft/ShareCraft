//! Sites' own settings: kind, description, language, time zone, archive stamp, hub,
//! navigation, look and own themes, mirrored from their records in Git (`sites/<xx>/<id>.json`)
//! as the tree is; the checks of languages, time zones and navigation; and a site's owners.
//!
//! A site's look pins the brand images it uses (`brand_refs` rows owned by the site's id, see
//! [`crate::brand`]), written whenever its row is: after the row, the content's `blobs` rows
//! are locked `FOR SHARE`, then the pins written.

use std::collections::HashSet;

use collab_core::ids::{NodeId, UserId};
use revisions::brand::{FontPackage, ImageRef, Look};
use revisions::{BlobRef, MAX_DESCRIPTION_CHARS, NavItem, NavTarget, SiteTheme};
use serde_json::Value;
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::brand::{Pin, pin_contents, site_pins};
use crate::events::{NewEvent, record};
use crate::tree::{NodeChange, NodeRow, upsert_node};
use crate::{AccessError, Db, DbError, Perms};

/// The organisation's default language, a setting (a BCP 47 tag; `"en"` until changed).
pub const DEFAULT_LANGUAGE: &str = "default_language";
/// The organisation's default time zone, a setting (an IANA name; `"UTC"` until changed).
pub const DEFAULT_TIME_ZONE: &str = "default_time_zone";
/// The mirror of the global navigation (`org/navigation.json`); not set through the settings.
pub const GLOBAL_NAVIGATION: &str = "global_navigation";

/// The longest language tag, in characters.
const MAX_LANGUAGE_CHARS: usize = 35;
/// The longest time zone name, in characters.
const MAX_TIME_ZONE_CHARS: usize = 100;
/// The longest navigation label, in characters.
const MAX_LABEL_CHARS: usize = 60;
/// The longest navigation address, in characters.
const MAX_URL_CHARS: usize = 2_000;
/// The most items one navigation holds, children included.
const MAX_NAV_ITEMS: usize = 100;
/// The most groups one navigation item is aimed at.
const MAX_AUDIENCE: usize = 10;

/// The columns of a `sites` row as [`settings_from_row`] reads them.
const SITE_COLUMNS: &str = "id, kind, description, language, time_zone, template, archived_at, \
     archived_by, is_hub, hub_navigation, joined_hub, navigation, look, themes, font_packages, \
     purging";

/// What a site is for. Both start the same; a communication site is shown with its own label.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SiteKind {
    Team,
    Communication,
}

impl SiteKind {
    /// The word for the kind, as stored and as written in JSON.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            SiteKind::Team => "team",
            SiteKind::Communication => "communication",
        }
    }

    /// The kind named `word`.
    #[must_use]
    pub fn parse(word: &str) -> Option<SiteKind> {
        match word {
            "team" => Some(SiteKind::Team),
            "communication" => Some(SiteKind::Communication),
            _ => None,
        }
    }
}

/// A site's own settings, as PostgreSQL mirrors them from its record in Git.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SiteSettings {
    /// The site's node.
    pub id: NodeId,
    pub kind: SiteKind,
    /// What the site is for; 0 to 500 characters.
    pub description: String,
    /// A BCP 47 language tag (see [`validate_language`]).
    pub language: String,
    /// An IANA time zone name (see [`validate_time_zone`]).
    pub time_zone: String,
    /// The template the site was created from: a saved template's id or a built-in name.
    pub template: Option<String>,
    /// When it was archived, if it is.
    pub archived_at: Option<OffsetDateTime>,
    /// Who archived it; `None` for the server, or when it is not archived.
    pub archived_by: Option<UserId>,
    /// Whether it is a hub.
    pub is_hub: bool,
    /// A hub's navigation bar.
    pub hub_navigation: Option<Vec<NavItem>>,
    /// The hub it has joined, if any.
    pub joined_hub: Option<NodeId>,
    /// Its own navigation; `None` for automatic (every library, by name).
    pub navigation: Option<Vec<NavItem>>,
    /// Its look; `None` for the built-in ShareCraft theme with the defaults.
    pub look: Option<Look>,
    /// Its own themes.
    pub themes: Vec<SiteTheme>,
    /// Its own font packages.
    pub font_packages: Vec<FontPackage>,
    /// A purge of the deleted site has started and is not finished. Not part of its record.
    pub purging: bool,
}

impl SiteSettings {
    /// The settings of a site with no record: a team site with no description, `language` and
    /// `time_zone`, nothing else.
    #[must_use]
    pub fn defaults(id: NodeId, language: &str, time_zone: &str) -> SiteSettings {
        SiteSettings {
            id,
            kind: SiteKind::Team,
            description: String::new(),
            language: language.to_owned(),
            time_zone: time_zone.to_owned(),
            template: None,
            archived_at: None,
            archived_by: None,
            is_hub: false,
            hub_navigation: None,
            joined_hub: None,
            navigation: None,
            look: None,
            themes: Vec::new(),
            font_packages: Vec::new(),
            purging: false,
        }
    }

    /// Whether `other` says what these settings say of the site's record: everything but
    /// `purging`, and of the archive stamp whether there is one and who made it.
    #[must_use]
    pub fn same_record(&self, other: &SiteSettings) -> bool {
        self.id == other.id
            && self.kind == other.kind
            && self.description == other.description
            && self.language == other.language
            && self.time_zone == other.time_zone
            && self.template == other.template
            && self.archived_at.is_some() == other.archived_at.is_some()
            && self.archived_by == other.archived_by
            && self.is_hub == other.is_hub
            && self.hub_navigation == other.hub_navigation
            && self.joined_hub == other.joined_hub
            && self.navigation == other.navigation
            && self.look == other.look
            && self.themes == other.themes
            && self.font_packages == other.font_packages
    }

    /// The content the site pins: the images and fonts of its look and the fonts of its own
    /// font packages, each once (see [`site_pins`]).
    #[must_use]
    pub fn pins(&self) -> Vec<Pin<'_>> {
        site_pins(self.look.as_ref(), &self.font_packages)
    }

    /// The brand images the site's look uses, each once.
    #[must_use]
    pub fn look_images(&self) -> Vec<&ImageRef> {
        let mut images: Vec<&ImageRef> = Vec::new();
        if let Some(look) = &self.look {
            for image in [
                &look.logo,
                &look.thumbnail,
                &look.icon,
                &look.header.image,
                &look.footer.logo,
            ]
            .into_iter()
            .flatten()
            {
                if !images.iter().any(|i| i.sha256 == image.sha256) {
                    images.push(image);
                }
            }
        }
        images
    }
}

/// The organisation's regional defaults, for sites with no record and pages outside a site.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegionalDefaults {
    /// The default language tag.
    pub language: String,
    /// The default time zone name.
    pub time_zone: String,
}

/// Which navigation a list of items is: whose node targets may lie where.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NavScope {
    /// A site's own navigation: node targets must be in that site.
    Local(NodeId),
    /// A hub's navigation bar: node targets may be in any site.
    Hub,
    /// The organisation's global navigation: node targets may be in any site.
    Global,
}

/// One change of a site's settings, mirrored once Git has recorded it.
#[derive(Clone, Debug)]
pub struct SiteChange<'a> {
    /// The settings as Git now holds them; `purging` is not written.
    pub settings: &'a SiteSettings,
    /// The site's node as Git now holds it, when the same commit renamed it.
    pub node: Option<&'a NodeRow>,
    /// Who makes the change; `None` for the server itself.
    pub actor: Option<UserId>,
    /// Whether `actor` must be an enabled administrator.
    pub needs_admin: bool,
    /// The permissions `actor` must hold on the site; empty for none.
    pub needs: Perms,
    /// The events to record with the change.
    pub events: Vec<NewEvent>,
}

/// Checks a language tag: a well-formed BCP 47 tag of the form `ll`, `lll`, with an optional
/// four-letter script and an optional two-letter or three-digit region (`en`, `en-GB`,
/// `zh-Hant-TW`, `es-419`), at most 35 characters. Returns it unchanged; `InvalidLanguage`
/// otherwise.
pub fn validate_language(tag: &str) -> Result<&str, AccessError> {
    let letters = |part: &str, n: std::ops::RangeInclusive<usize>| {
        n.contains(&part.len()) && part.bytes().all(|b| b.is_ascii_alphabetic())
    };
    let digits = |part: &str| part.len() == 3 && part.bytes().all(|b| b.is_ascii_digit());
    let mut parts = tag.split('-').peekable();
    let mut ok = tag.chars().count() <= MAX_LANGUAGE_CHARS
        && parts.next().is_some_and(|first| letters(first, 2..=3));
    if ok && parts.peek().is_some_and(|part| letters(part, 4..=4)) {
        parts.next();
    }
    if ok && let Some(region) = parts.next() {
        ok = letters(region, 2..=2) || digits(region);
    }
    if ok && parts.next().is_none() {
        Ok(tag)
    } else {
        Err(AccessError::InvalidLanguage)
    }
}

/// Checks a time zone name: one the bundled time zone database knows, such as `UTC` or
/// `Europe/London`. Returns it unchanged; `InvalidTimeZone` otherwise.
pub fn validate_time_zone(name: &str) -> Result<&str, AccessError> {
    let ok = !name.is_empty()
        && name.chars().count() <= MAX_TIME_ZONE_CHARS
        && !name.chars().any(|c| c.is_control() || c.is_whitespace())
        && jiff::tz::TimeZone::get(name).is_ok();
    if ok {
        Ok(name)
    } else {
        Err(AccessError::InvalidTimeZone)
    }
}

/// Checks a site's description: at most 500 characters, no control characters but line
/// breaks and tabs. Returns it unchanged; `InvalidSetting` otherwise.
pub fn validate_description(text: &str) -> Result<&str, AccessError> {
    let ok = text.chars().count() <= MAX_DESCRIPTION_CHARS
        && !text
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'));
    if ok {
        Ok(text)
    } else {
        Err(AccessError::InvalidSetting)
    }
}

/// Whether `url` is an address a navigation link may lead to: at most 2,000 characters, no
/// spaces or control characters, and `https:`, `http:`, `mailto:` or a path on this server
/// (one `/`, not `//` or `/\`).
pub(crate) fn acceptable_url(url: &str) -> bool {
    if url.is_empty()
        || url.chars().count() > MAX_URL_CHARS
        || url.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return false;
    }
    let lower = url.to_ascii_lowercase();
    if ["https:", "http:", "mailto:"]
        .iter()
        .any(|scheme| lower.starts_with(scheme))
    {
        return true;
    }
    let mut chars = url.chars();
    chars.next() == Some('/') && !matches!(chars.next(), Some('/' | '\\'))
}

/// Whether `item`'s label is acceptable: 1 to 60 characters with something besides spaces, no
/// control characters; empty only for a node target, which shows the node's name.
fn acceptable_label(item: &NavItem) -> bool {
    if item.label.is_empty() {
        return matches!(item.target, Some(NavTarget::Node(_)));
    }
    item.label.chars().count() <= MAX_LABEL_CHARS
        && !item.label.trim().is_empty()
        && !item.label.chars().any(char::is_control)
}

/// The structural rules of a navigation (see [`Db::validate_navigation`]); answers the node
/// targets, for checking where they are.
fn check_structure(items: &[NavItem]) -> Result<Vec<Uuid>, AccessError> {
    let mut count = 0usize;
    let mut ids = HashSet::new();
    let mut nodes = Vec::new();
    let mut check = |item: &NavItem, top: bool| -> Result<(), AccessError> {
        count = count.saturating_add(1);
        let ok = count <= MAX_NAV_ITEMS
            && ids.insert(item.id)
            && acceptable_label(item)
            && item.audience.len() <= MAX_AUDIENCE
            && (top || item.children.is_empty())
            && match &item.target {
                None => true,
                Some(NavTarget::Url(url)) => acceptable_url(url),
                Some(NavTarget::Node(node)) => {
                    nodes.push(*node);
                    true
                }
            };
        if ok {
            Ok(())
        } else {
            Err(AccessError::InvalidNavigation)
        }
    };
    for item in items {
        check(item, true)?;
        for child in &item.children {
            check(child, false)?;
        }
    }
    Ok(nodes)
}

/// Reads one `sites` row selected with [`SITE_COLUMNS`].
fn settings_from_row(row: &PgRow) -> Result<SiteSettings, sqlx::Error> {
    let kind: String = row.try_get("kind")?;
    let navigation = |column: &str| -> Result<Option<Vec<NavItem>>, sqlx::Error> {
        row.try_get::<Option<Value>, _>(column)?
            .map(serde_json::from_value)
            .transpose()
            .map_err(|e| sqlx::Error::Decode(Box::new(e)))
    };
    Ok(SiteSettings {
        id: NodeId(row.try_get("id")?),
        kind: SiteKind::parse(&kind)
            .ok_or_else(|| sqlx::Error::Decode(format!("unknown site kind {kind:?}").into()))?,
        description: row.try_get("description")?,
        language: row.try_get("language")?,
        time_zone: row.try_get("time_zone")?,
        template: row.try_get("template")?,
        archived_at: row.try_get("archived_at")?,
        archived_by: row.try_get::<Option<Uuid>, _>("archived_by")?.map(UserId),
        is_hub: row.try_get("is_hub")?,
        hub_navigation: navigation("hub_navigation")?,
        joined_hub: row.try_get::<Option<Uuid>, _>("joined_hub")?.map(NodeId),
        navigation: navigation("navigation")?,
        look: row
            .try_get::<Option<Value>, _>("look")?
            .map(serde_json::from_value)
            .transpose()
            .map_err(|e| sqlx::Error::Decode(Box::new(e)))?,
        themes: serde_json::from_value(row.try_get("themes")?)
            .map_err(|e| sqlx::Error::Decode(Box::new(e)))?,
        font_packages: serde_json::from_value(row.try_get("font_packages")?)
            .map_err(|e| sqlx::Error::Decode(Box::new(e)))?,
        purging: row.try_get("purging")?,
    })
}

/// A navigation as stored in a `jsonb` column.
fn navigation_value(items: Option<&Vec<NavItem>>) -> Result<Option<Value>, DbError> {
    items
        .map(serde_json::to_value)
        .transpose()
        .map_err(DbError::new)
}

/// Gives the site `site` a `sites` row with the organisation's defaults if it has none: a site
/// whose record Git does not hold yet.
pub(crate) async fn ensure_site_row(
    conn: &mut PgConnection,
    site: NodeId,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "insert into sites (id, language, time_zone) values ($1, \
             coalesce((select value #>> '{}' from settings where key = $2), 'en'), \
             coalesce((select value #>> '{}' from settings where key = $3), 'UTC')) \
         on conflict (id) do nothing",
    )
    .bind(site.0)
    .bind(DEFAULT_LANGUAGE)
    .bind(DEFAULT_TIME_ZONE)
    .execute(conn)
    .await?;
    Ok(())
}

/// Writes `settings` into the site's row, which the caller has locked if it exists, then makes
/// the site's pins what it uses (see [`SiteSettings::pins`]); `purging` is left as it is.
/// `repair` (a write from Git by nobody) creates the `blobs` rows of pinned content that has
/// none; otherwise content erased meanwhile answers `ImageGone`, a refusal.
pub(crate) async fn upsert_site(
    conn: &mut PgConnection,
    settings: &SiteSettings,
    repair: bool,
) -> Result<(), AccessError> {
    let look = settings
        .look
        .as_ref()
        .map(serde_json::to_value)
        .transpose()
        .map_err(DbError::new)?;
    let themes = serde_json::to_value(&settings.themes).map_err(DbError::new)?;
    let font_packages = serde_json::to_value(&settings.font_packages).map_err(DbError::new)?;
    sqlx::query(
        "insert into sites (id, kind, description, language, time_zone, template, archived_at, \
             archived_by, is_hub, hub_navigation, joined_hub, navigation, look, themes, \
             font_packages) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15) \
         on conflict (id) do update set kind = excluded.kind, \
             description = excluded.description, language = excluded.language, \
             time_zone = excluded.time_zone, template = excluded.template, \
             archived_at = excluded.archived_at, archived_by = excluded.archived_by, \
             is_hub = excluded.is_hub, hub_navigation = excluded.hub_navigation, \
             joined_hub = excluded.joined_hub, navigation = excluded.navigation, \
             look = excluded.look, themes = excluded.themes, \
             font_packages = excluded.font_packages",
    )
    .bind(settings.id.0)
    .bind(settings.kind.as_str())
    .bind(&settings.description)
    .bind(&settings.language)
    .bind(&settings.time_zone)
    .bind(&settings.template)
    .bind(settings.archived_at)
    .bind(settings.archived_by.map(|u| u.0))
    .bind(settings.is_hub)
    .bind(navigation_value(settings.hub_navigation.as_ref())?)
    .bind(settings.joined_hub.map(|h| h.0))
    .bind(navigation_value(settings.navigation.as_ref())?)
    .bind(look)
    .bind(themes)
    .bind(font_packages)
    .execute(&mut *conn)
    .await?;
    let owner = settings.id.0.to_string();
    if !pin_contents(conn, &owner, &settings.pins(), repair).await? {
        return Err(AccessError::ImageGone);
    }
    Ok(())
}

impl Db {
    /// The organisation's default language and time zone (`en` and `UTC` if unset).
    pub async fn regional_defaults(&self) -> Result<RegionalDefaults, DbError> {
        let rows: Vec<(String, Value)> =
            sqlx::query_as("select key, value from settings where key = any($1)")
                .bind(vec![DEFAULT_LANGUAGE, DEFAULT_TIME_ZONE])
                .fetch_all(self.pool())
                .await?;
        let mut defaults = RegionalDefaults {
            language: "en".to_owned(),
            time_zone: "UTC".to_owned(),
        };
        for (key, value) in rows {
            if let Some(text) = value.as_str() {
                match key.as_str() {
                    DEFAULT_LANGUAGE => text.clone_into(&mut defaults.language),
                    DEFAULT_TIME_ZONE => text.clone_into(&mut defaults.time_zone),
                    _ => {}
                }
            }
        }
        Ok(defaults)
    }

    /// The settings of the site `site`, whoever may see it; `None` if there is no such site.
    pub async fn site_settings(&self, site: NodeId) -> Result<Option<SiteSettings>, DbError> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "select {SITE_COLUMNS} from sites where id = $1"
        )))
        .bind(site.0)
        .fetch_optional(self.pool())
        .await?;
        Ok(row.as_ref().map(settings_from_row).transpose()?)
    }

    /// The settings of every site, deleted or not, for the directory and for comparing with
    /// Git.
    pub async fn all_site_settings(&self) -> Result<Vec<SiteSettings>, DbError> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "select {SITE_COLUMNS} from sites"
        )))
        .fetch_all(self.pool())
        .await?;
        rows.iter()
            .map(settings_from_row)
            .collect::<Result<_, sqlx::Error>>()
            .map_err(DbError::from)
    }

    /// Mirrors one committed change of a site's settings, in one transaction: judges what it
    /// needs as a tree change does (`NotFound` if the actor can no longer see the site,
    /// `Forbidden` without the permissions `needs` on it or, when `needs_admin`, unless they are
    /// an enabled administrator), writes the renamed site node if given (`NameTaken` for a
    /// name taken), then the site's row (locked `FOR UPDATE` after the node rows and before any
    /// group or role), and records the events. A database failure before the change was judged allowed is `Unjudged`.
    pub async fn apply_site_change(&self, change: SiteChange<'_>) -> Result<(), AccessError> {
        let site = change.settings.id;
        let needs: Vec<(NodeId, Perms)> = if change.needs == Perms::NONE {
            Vec::new()
        } else {
            vec![(site, change.needs)]
        };
        let rows: &[NodeRow] = change.node.map_or(&[], std::slice::from_ref);
        let (mut tx, _) = self
            .judged(&NodeChange {
                rows,
                actor: change.actor,
                needs_admin: change.needs_admin,
                needs: &needs,
                sites: std::slice::from_ref(&site),
                ..NodeChange::default()
            })
            .await
            .map_err(|e| match e {
                AccessError::Db(e) => AccessError::Unjudged(e),
                other => other,
            })?;
        for row in rows {
            upsert_node(&mut tx, row).await?;
        }
        upsert_site(&mut tx, change.settings, change.actor.is_none()).await?;
        for event in change.events {
            record(&mut tx, event).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Checks a navigation: at most 100 items, children included; children one level deep;
    /// each item's id used once; labels of 1 to 60 characters (empty only for a node target,
    /// which shows the node's name); at most 10 groups in an audience; addresses of at most
    /// 2,000 characters that are `https:`, `http:`, `mailto:` or a path on this server; and
    /// node targets that exist and are not deleted, in the site itself for a site's own
    /// navigation. `InvalidNavigation` otherwise.
    pub async fn validate_navigation(
        &self,
        items: &[NavItem],
        scope: NavScope,
    ) -> Result<(), AccessError> {
        let mut nodes = check_structure(items)?;
        if nodes.is_empty() {
            return Ok(());
        }
        nodes.sort_unstable();
        nodes.dedup();
        let found: Vec<(Uuid, Uuid)> = sqlx::query_as(
            "select id, site_id from nodes where id = any($1) and deleted_at is null",
        )
        .bind(&nodes)
        .fetch_all(self.pool())
        .await?;
        let in_place = |site: Uuid| match scope {
            NavScope::Local(own) => site == own.0,
            NavScope::Hub | NavScope::Global => true,
        };
        if found.len() == nodes.len() && found.iter().all(|(_, site)| in_place(*site)) {
            Ok(())
        } else {
            Err(AccessError::InvalidNavigation)
        }
    }

    /// The names of the site's owners, by name: the enabled members of its Owners group and
    /// the enabled people assigned Owner on the site itself. Members of other groups assigned
    /// there are not named.
    pub async fn site_owners(&self, site: NodeId) -> Result<Vec<(UserId, String)>, DbError> {
        let rows: Vec<(Uuid, String)> = sqlx::query_as(
            "select u.id, u.name from users u \
             where u.disabled_at is null and ( \
                 u.id in (select m.user_id from group_members m \
                          join groups g on g.id = m.group_id \
                          where g.site_id = $1 and g.purpose = 'owners') \
                 or u.id in (select a.principal_id from assignments a \
                             where a.node_id = $1 and a.principal_kind = 'user' \
                               and a.role_id = 'owner')) \
             order by lower(u.name), u.id",
        )
        .bind(site.0)
        .fetch_all(self.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(|(id, name)| (UserId(id), name))
            .collect())
    }
}

/// A saved template as PostgreSQL mirrors it from `templates/<xx>/<id>.json`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TemplateRow {
    /// The template's id.
    pub id: Uuid,
    /// Its name; 1 to 80 characters.
    pub name: String,
    /// What it is for; 0 to 500 characters.
    pub description: String,
    /// The shape of the site it makes, as its record holds it.
    pub body: Value,
    /// Who saved it, if it was a person with an account here.
    pub created_by: Option<UserId>,
    /// When it was saved.
    pub created_at: OffsetDateTime,
    /// When it was deleted, if it was.
    pub deleted_at: Option<OffsetDateTime>,
}

impl TemplateRow {
    /// Whether `other` says what this row says of the template's record: everything, and of
    /// the times whether it is deleted.
    #[must_use]
    pub fn same_record(&self, other: &TemplateRow) -> bool {
        self.id == other.id
            && self.name == other.name
            && self.description == other.description
            && self.body == other.body
            && self.created_by == other.created_by
            && self.deleted_at.is_some() == other.deleted_at.is_some()
    }
}

impl Db {
    /// Every saved template, deleted ones included, for comparing with Git.
    pub async fn template_rows(&self) -> Result<Vec<TemplateRow>, DbError> {
        let rows = sqlx::query(
            "select id, name, description, body, created_by, created_at, deleted_at \
             from site_templates",
        )
        .fetch_all(self.pool())
        .await?;
        rows.iter()
            .map(|row| {
                Ok(TemplateRow {
                    id: row.try_get("id")?,
                    name: row.try_get("name")?,
                    description: row.try_get("description")?,
                    body: row.try_get("body")?,
                    created_by: row.try_get::<Option<Uuid>, _>("created_by")?.map(UserId),
                    created_at: row.try_get("created_at")?,
                    deleted_at: row.try_get("deleted_at")?,
                })
            })
            .collect::<Result<_, sqlx::Error>>()
            .map_err(DbError::from)
    }

    /// Writes one saved template as Git holds it, with `event` if given, in one transaction.
    /// A creator with no account here is recorded as nobody.
    pub async fn apply_template(
        &self,
        row: &TemplateRow,
        event: Option<NewEvent>,
    ) -> Result<(), DbError> {
        let mut tx = self.pool().begin().await?;
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
        if let Some(event) = event {
            record(&mut tx, event).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// The global navigation as PostgreSQL mirrors it from `org/navigation.json`; empty if
    /// none was ever recorded.
    pub async fn global_navigation(&self) -> Result<Vec<NavItem>, DbError> {
        let value: Option<Value> = sqlx::query_scalar("select value from settings where key = $1")
            .bind(GLOBAL_NAVIGATION)
            .fetch_optional(self.pool())
            .await?;
        match value {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(value) => serde_json::from_value(value).map_err(DbError::new),
        }
    }

    /// Writes the global navigation as Git holds it, with `event` if given, in one
    /// transaction.
    pub async fn apply_global_navigation(
        &self,
        items: &[NavItem],
        event: Option<NewEvent>,
    ) -> Result<(), DbError> {
        let value = serde_json::to_value(items).map_err(DbError::new)?;
        let mut tx = self.pool().begin().await?;
        sqlx::query(
            "insert into settings (key, value, changed_at) values ($1, $2, now()) \
             on conflict (key) do update set value = excluded.value, \
                 changed_at = excluded.changed_at",
        )
        .bind(GLOBAL_NAVIGATION)
        .bind(&value)
        .execute(&mut *tx)
        .await?;
        if let Some(event) = event {
            record(&mut tx, event).await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

/// One site as the administrators' Sites page lists it: live, archived or deleted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdminSite {
    pub id: NodeId,
    pub name: String,
    pub kind: SiteKind,
    /// Whether it is archived (a deleted site may be archived too).
    pub archived: bool,
    pub is_hub: bool,
    /// The hub it joined, with the hub's name.
    pub hub: Option<(NodeId, String)>,
    /// The bytes it holds (see [`crate::SiteUsage`]).
    pub used: u64,
    /// Its quota in bytes; `None` for no limit.
    pub quota: Option<u64>,
    /// How many people own it (see [`Db::site_owners`]).
    pub owners: i64,
    /// When it was deleted, if it is.
    pub deleted_at: Option<OffsetDateTime>,
    /// The name of whoever deleted it, if known.
    pub deleted_by: Option<String>,
}

/// Clears the hub `hub` from every site that joined it (`joined_hub`), recording `hub.left` by
/// `actor` for each with `{"hub", "released": true}`. The caller has locked the rows of those
/// sites and writes their records, cleared the same way, in the same commit (see
/// `server::sites::released_members`). Purging a hub does this, and so does unregistering one.
/// Answers the sites it cleared, by id.
pub(crate) async fn release_hub_rows(
    conn: &mut PgConnection,
    actor: Option<UserId>,
    hub: NodeId,
) -> Result<Vec<NodeId>, sqlx::Error> {
    let members: Vec<Uuid> =
        sqlx::query_scalar("update sites set joined_hub = null where joined_hub = $1 returning id")
            .bind(hub.0)
            .fetch_all(&mut *conn)
            .await?;
    let mut members: Vec<NodeId> = members.into_iter().map(NodeId).collect();
    members.sort_unstable();
    for member in &members {
        record(
            &mut *conn,
            NewEvent {
                kind: "hub.left",
                actor,
                subject: None,
                document: None,
                node: Some(*member),
                detail: serde_json::json!({ "hub": hub.0, "released": true }),
            },
        )
        .await?;
    }
    Ok(members)
}

/// Deletes everything the purged site `site` owned besides its nodes (which stay as
/// tombstones), in the transaction that purged its rows: the links, access requests and
/// assignments of its nodes, its groups with their memberships and every assignment naming
/// them, its upload sessions, its usage row and the pins of its look's brand images; the sites
/// that joined it as a hub leave it (see [`release_hub_rows`]), its connected libraries lose
/// their connection, item records and sealed secrets, and its own row keeps no hub,
/// no hub it joined, no hub navigation, no look and no themes. Its groups are locked
/// `FOR UPDATE`, by id, first. Answers the contents its pins named, for the caller to erase
/// what nothing names any more.
pub(crate) async fn purge_site_rows(
    conn: &mut PgConnection,
    actor: Option<UserId>,
    site: NodeId,
) -> Result<Vec<BlobRef>, sqlx::Error> {
    let groups: Vec<Uuid> =
        sqlx::query_scalar("select id from groups where site_id = $1 order by id for update")
            .bind(site.0)
            .fetch_all(&mut *conn)
            .await?;
    sqlx::query(
        "delete from links \
         where node_id in (select id from nodes where site_id = $1) or group_id = any($2)",
    )
    .bind(site.0)
    .bind(&groups)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "delete from access_requests where node_id in (select id from nodes where site_id = $1)",
    )
    .bind(site.0)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "delete from assignments \
         where node_id in (select id from nodes where site_id = $1) \
            or (principal_kind = 'group' and principal_id = any($2))",
    )
    .bind(site.0)
    .bind(&groups)
    .execute(&mut *conn)
    .await?;
    sqlx::query("delete from group_members where group_id = any($1)")
        .bind(&groups)
        .execute(&mut *conn)
        .await?;
    sqlx::query("delete from groups where id = any($1)")
        .bind(&groups)
        .execute(&mut *conn)
        .await?;
    // Its connected libraries: their item records, their connections, then the sealed secrets
    // those connections named (nothing else names a secret).
    sqlx::query(
        "delete from source_writes where library_id in \
             (select id from nodes where site_id = $1 and kind = 'library')",
    )
    .bind(site.0)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "delete from source_held where entry_id in (select id from nodes where site_id = $1) \
            or library_id in (select id from nodes where site_id = $1 and kind = 'library')",
    )
    .bind(site.0)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "delete from source_multipart where node_id in (select id from nodes where site_id = $1) \
            or library_id in (select id from nodes where site_id = $1 and kind = 'library')",
    )
    .bind(site.0)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "delete from source_items where library_id in \
             (select id from nodes where site_id = $1 and kind = 'library')",
    )
    .bind(site.0)
    .execute(&mut *conn)
    .await?;
    let sealed: Vec<(Option<Uuid>, Option<Uuid>)> = sqlx::query_as(
        "delete from library_sources where library_id in \
             (select id from nodes where site_id = $1 and kind = 'library') \
         returning token_secret, webhook_secret",
    )
    .bind(site.0)
    .fetch_all(&mut *conn)
    .await?;
    let sealed: Vec<Uuid> = sealed
        .into_iter()
        .flat_map(|(token, webhook)| [token, webhook])
        .flatten()
        .collect();
    sqlx::query("delete from secrets where id = any($1)")
        .bind(&sealed)
        .execute(&mut *conn)
        .await?;
    sqlx::query("delete from upload_sessions where site_id = $1")
        .bind(site.0)
        .execute(&mut *conn)
        .await?;
    sqlx::query("delete from site_usage where site_id = $1")
        .bind(site.0)
        .execute(&mut *conn)
        .await?;
    let unpinned: Vec<(String, i64)> = sqlx::query_as(
        "delete from brand_refs r using blobs b \
         where r.owner = $1 and b.sha256 = r.sha256 returning r.sha256, b.size",
    )
    .bind(site.0.to_string())
    .fetch_all(&mut *conn)
    .await?;
    release_hub_rows(&mut *conn, actor, site).await?;
    sqlx::query(
        "update sites set joined_hub = null, is_hub = false, hub_navigation = null, \
             look = null, themes = '[]', font_packages = '[]' where id = $1",
    )
    .bind(site.0)
    .execute(&mut *conn)
    .await?;
    Ok(unpinned
        .into_iter()
        .filter_map(|(sha, size)| {
            Some(BlobRef {
                sha256: revisions::parse_hex(&sha)?,
                size: u64::try_from(size).unwrap_or(0),
            })
        })
        .collect())
}

/// Where a site is in its lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SiteState {
    /// Not deleted (it may be archived).
    Live,
    /// Deleted, not yet purged (its purge may have started: see [`SiteSettings::purging`]).
    Deleted,
    /// Purged: only its tombstones are left.
    Purged,
}

impl Db {
    /// Where the site `site` is in its lifecycle; `None` if there is no such site.
    pub async fn site_state(&self, site: NodeId) -> Result<Option<SiteState>, DbError> {
        let row: Option<(bool, bool)> = sqlx::query_as(
            "select deleted_at is not null, purged_at is not null from nodes \
             where id = $1 and kind = 'site'",
        )
        .bind(site.0)
        .fetch_optional(self.pool())
        .await?;
        Ok(row.map(|(deleted, purged)| match (deleted, purged) {
            (_, true) => SiteState::Purged,
            (true, false) => SiteState::Deleted,
            (false, false) => SiteState::Live,
        }))
    }

    /// Marks the deleted site `site` as being purged (`purging`), or the purge as finished.
    pub async fn set_site_purging(&self, site: NodeId, purging: bool) -> Result<(), DbError> {
        sqlx::query("update sites set purging = $2 where id = $1")
            .bind(site.0)
            .bind(purging)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// The deleted sites to purge at `now`: every site whose purge was started and not finished
    /// (`purging`) first, then every site deleted [`crate::BIN_DAYS`] days or more before
    /// `now` and not purged, oldest deletion first.
    pub async fn sites_to_purge(&self, now: OffsetDateTime) -> Result<Vec<NodeId>, DbError> {
        let ids: Vec<Uuid> = sqlx::query_scalar(
            "select n.id from nodes n join sites s on s.id = n.id \
             where n.kind = 'site' and n.deleted_at is not null \
               and (s.purging or (n.purged_at is null and n.deleted_at <= $1)) \
             order by s.purging desc, n.deleted_at, n.id",
        )
        .bind(now - time::Duration::days(crate::BIN_DAYS))
        .fetch_all(self.pool())
        .await?;
        Ok(ids.into_iter().map(NodeId).collect())
    }

    /// The names of the sites that are not deleted, lower-cased, for finding a free name.
    pub async fn live_site_names(&self) -> Result<Vec<String>, DbError> {
        let names = sqlx::query_scalar(
            "select lower(name) from nodes \
             where kind = 'site' and parent_id is null and deleted_at is null",
        )
        .fetch_all(self.pool())
        .await?;
        Ok(names)
    }

    /// Every site that is not purged, live, archived or deleted, by name, for the
    /// administrators' Sites page. Whoever asks: the caller checks they are an administrator.
    pub async fn admin_sites(&self) -> Result<Vec<AdminSite>, DbError> {
        let rows = sqlx::query(
            "select n.id, n.name, n.deleted_at, u.name as deleted_by, \
                    coalesce(s.kind, 'team') as kind, s.archived_at is not null as archived, \
                    coalesce(s.is_hub, false) as is_hub, s.joined_hub, h.name as hub_name, \
                    coalesce(su.used, 0) as used, su.quota, \
                    (select count(*) from users o where o.disabled_at is null and ( \
                         o.id in (select m.user_id from group_members m \
                                  join groups g on g.id = m.group_id \
                                  where g.site_id = n.id and g.purpose = 'owners') \
                         or o.id in (select a.principal_id from assignments a \
                                     where a.node_id = n.id and a.principal_kind = 'user' \
                                       and a.role_id = 'owner'))) as owners \
             from nodes n \
             left join sites s on s.id = n.id \
             left join nodes h on h.id = s.joined_hub \
             left join site_usage su on su.site_id = n.id \
             left join users u on u.id = n.deleted_by \
             where n.kind = 'site' and n.parent_id is null and n.purged_at is null \
             order by lower(n.name), n.id",
        )
        .fetch_all(self.pool())
        .await?;
        let bytes = |value: i64| u64::try_from(value).unwrap_or(0);
        rows.iter()
            .map(|row| {
                let kind: String = row.try_get("kind")?;
                let hub = row
                    .try_get::<Option<Uuid>, _>("joined_hub")?
                    .zip(row.try_get::<Option<String>, _>("hub_name")?)
                    .map(|(id, name)| (NodeId(id), name));
                Ok(AdminSite {
                    id: NodeId(row.try_get("id")?),
                    name: row.try_get("name")?,
                    kind: SiteKind::parse(&kind).ok_or_else(|| {
                        sqlx::Error::Decode(format!("unknown site kind {kind:?}").into())
                    })?,
                    archived: row.try_get::<Option<bool>, _>("archived")?.unwrap_or(false),
                    is_hub: row.try_get("is_hub")?,
                    hub,
                    used: bytes(row.try_get("used")?),
                    quota: row.try_get::<Option<i64>, _>("quota")?.map(bytes),
                    owners: row.try_get("owners")?,
                    deleted_at: row.try_get("deleted_at")?,
                    deleted_by: row.try_get("deleted_by")?,
                })
            })
            .collect::<Result<_, sqlx::Error>>()
            .map_err(DbError::from)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn item(label: &str, target: Option<NavTarget>) -> NavItem {
        NavItem {
            id: Uuid::new_v4(),
            label: label.to_owned(),
            target,
            audience: Vec::new(),
            children: Vec::new(),
        }
    }

    fn url(text: &str) -> Option<NavTarget> {
        Some(NavTarget::Url(text.to_owned()))
    }

    #[test]
    fn languages() {
        for good in [
            "en",
            "fr",
            "haw",
            "en-GB",
            "zh-Hant",
            "zh-Hant-TW",
            "es-419",
        ] {
            assert_eq!(validate_language(good), Ok(good), "{good}");
        }
        for bad in [
            "",
            "e",
            "english",
            "en_GB",
            "en-",
            "en-G",
            "en-GBR",
            "en-1234",
            "zh-Hant-TW-x",
            "en GB",
            "-en",
            "en-GB-",
        ] {
            assert_eq!(
                validate_language(bad),
                Err(AccessError::InvalidLanguage),
                "{bad}"
            );
        }
    }

    #[test]
    fn time_zones() {
        for good in ["UTC", "Europe/London", "America/New_York", "Asia/Kolkata"] {
            assert_eq!(validate_time_zone(good), Ok(good), "{good}");
        }
        for bad in [
            "",
            "Mars/Olympus",
            "Europe/London ",
            "../etc/passwd",
            "Nowhere",
        ] {
            assert_eq!(
                validate_time_zone(bad),
                Err(AccessError::InvalidTimeZone),
                "{bad}"
            );
        }
    }

    #[test]
    fn navigation_structure() {
        let node = Some(NavTarget::Node(Uuid::new_v4()));
        let mut heading = item("Teams", None);
        heading.children = vec![
            item("Wiki", url("https://example.invalid/wiki")),
            item("", node),
        ];
        let good = vec![
            item("Home", url("/index.html")),
            item("Mail", url("mailto:help@example.invalid")),
            item("Old", url("HTTP://example.invalid")),
            heading.clone(),
        ];
        assert_eq!(check_structure(&good).unwrap().len(), 1);

        let refused = |items: Vec<NavItem>| {
            assert_eq!(check_structure(&items), Err(AccessError::InvalidNavigation));
        };
        for bad in [
            "javascript:alert(1)",
            "JavaScript:alert(1)",
            "data:text/html,x",
            "//evil.invalid",
            "/\\evil.invalid",
            "ftp://example.invalid",
            "relative/path",
            "https://example.invalid/a b",
            "",
        ] {
            refused(vec![item("Bad", url(bad))]);
        }
        refused(vec![item("Long", url(&format!("/{}", "a".repeat(2_000))))]);
        refused(vec![item(&"x".repeat(61), None)]);
        refused(vec![item("", None)]);
        refused(vec![item("", url("/x"))]);
        refused(vec![item("   ", None)]);
        refused(vec![item("Bad\u{7}", None)]);
        let mut wide = item("Wide", None);
        wide.audience = (0..11).map(|_| Uuid::new_v4()).collect();
        refused(vec![wide]);
        let mut deep = heading.clone();
        deep.children[0].children = vec![item("Too deep", None)];
        refused(vec![deep]);
        let twice = item("Twice", None);
        refused(vec![twice.clone(), twice]);
        refused((0..101).map(|n| item(&format!("Item {n}"), None)).collect());
        let mut many = item("Many", None);
        many.children = (0..100)
            .map(|n| item(&format!("Child {n}"), None))
            .collect();
        refused(vec![many]);
        assert!(
            check_structure(
                &(0..100)
                    .map(|n| item(&format!("{n}"), None))
                    .collect::<Vec<_>>()
            )
            .is_ok()
        );
    }
}
