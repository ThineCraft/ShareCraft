//! Site templates: the built-in Team and Communication templates, templates saved from a site
//! (its libraries, folders, settings and navigation; never files, members or identifiers),
//! template packages for carrying a template to another installation, and turning a template
//! into a new site's shape.
//!
//! A saved template is recorded in Git as `templates/<xx>/<id>.json`, one commit per change,
//! then mirrored into PostgreSQL (`site_templates`), taken back if refused, and compared with
//! Git at every start (see [`crate::sites::reconcile`]). Only administrators work with
//! templates.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use collab_core::ids::NodeId;
use records::{
    AccessError, DbError, MAX_PACKAGE_BYTES, NewEvent, NodeKind, RegionalDefaults, SiteGroup,
    TemplateRow, User, check_template_size, validate_template_body, validate_template_description,
    validate_template_name,
};
use revisions::brand::{
    BUILT_IN_FONT_PACKAGES, Background, FontPackage, FontRef, FontRole, Footer, Header, ImageKind,
    ImageRef, Look, LookFonts, OrgBrand, TemplateFontPackage, TemplateFontRef, TemplateFonts,
    TemplateFooter, TemplateHeader, TemplateImage, TemplateLook, icon_problem,
};
use revisions::{
    CheckpointError, GitError, LibraryFields, NavItem, NavTarget, SITE_SCHEMA, SiteRecord, Stamp,
    TemplateBody, TemplateFolder, TemplateLibrary, TemplateNavItem, TemplateNavTarget,
    TemplateRecord, TreeRecord,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::AppState;
use crate::tree::{
    LIBRARY_NAME, SiteShape, Undo, author_of, clean_folder_name, ensure_in_step, fit, git_failure,
    now_text, read_tree, take_back_all, write_records,
};

/// The name of the built-in Team template.
pub const TEAM: &str = "team";
/// The name of the built-in Communication template.
pub const COMMUNICATION: &str = "communication";
/// The built-in templates, in the order they are listed.
pub const BUILT_INS: [&str; 2] = [TEAM, COMMUNICATION];
/// The format version of a template package; currently 1.
pub const PACKAGE_VERSION: u32 = 1;

/// A template as a file for carrying it to another installation. Field order here is the
/// file's field order. Nothing in it identifies anything of the installation it came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TemplatePackage {
    /// The package format version; currently 1.
    pub sharecraft_template: u32,
    /// The template's name; 1 to 80 characters.
    pub name: String,
    /// What it is for; 0 to 500 characters.
    #[serde(default)]
    pub description: String,
    /// The shape of the site it makes.
    pub body: TemplateBody,
}

/// A name an imported package had that had to change: refused characters replaced, or made
/// unique in its place.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Cleaned {
    /// The name in the package.
    pub from: String,
    /// The name kept.
    pub to: String,
}

/// A template as listed: a built-in or a saved one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TemplateSummary {
    /// A built-in's name (`team`, `communication`) or a saved template's id.
    pub id: String,
    pub name: String,
    pub description: String,
    pub built_in: bool,
    /// The kind of site it makes.
    pub kind: String,
}

/// A built-in template's name as shown and its description.
fn built_in_words(name: &str) -> Option<(&'static str, &'static str)> {
    match name {
        TEAM => Some((
            "Team site",
            "A site for a team to work on files together, with a Documents library.",
        )),
        COMMUNICATION => Some((
            "Communication site",
            "A site for sharing news and information with many readers, with a Documents \
             library.",
        )),
        _ => None,
    }
}

/// The body of the built-in template `name`: its kind, a Documents library, automatic
/// navigation, and the organisation's default language and time zone.
#[must_use]
pub fn built_in_body(name: &str, defaults: &RegionalDefaults) -> Option<TemplateBody> {
    built_in_words(name)?;
    Some(TemplateBody {
        kind: name.to_owned(),
        language: defaults.language.clone(),
        time_zone: defaults.time_zone.clone(),
        libraries: vec![TemplateLibrary {
            name: LIBRARY_NAME.to_owned(),
            settings: LibraryFields::default(),
            folders: Vec::new(),
        }],
        navigation: None,
        look: None,
    })
}

/// The shape the built-in template `name` makes now.
#[must_use]
pub fn built_in_shape(name: &str, defaults: &RegionalDefaults) -> Option<SiteShape> {
    SiteShape::from_body(name, built_in_body(name, defaults)?)
}

// ---------------------------------------------------------------------------------------------
// Looks in templates

/// A file of a site's look that a template saved from the site leaves out, because the brand
/// centre has no entry with its content: an image or font from the site's own libraries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Dropped {
    /// The slot it was in: `logo`, `thumbnail`, `icon`, `header.image`, `footer.logo`,
    /// `fonts.display` or `fonts.content`.
    pub slot: &'static str,
    /// The file's name as the look recorded it.
    pub name: String,
}

/// A reference of a template's look that is left out because it names nothing usable here: a
/// brand-centre image or font this organisation does not have (any more), an image too large
/// for its slot or not an icon, or content no longer available.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Unresolved {
    /// The slot, as in [`Dropped::slot`].
    pub slot: &'static str,
    /// The brand-centre identifier it named.
    pub id: Uuid,
}

/// The kind of image each image slot of a look takes, in slot order.
const IMAGE_SLOTS: [(&str, ImageKind); 5] = [
    ("logo", ImageKind::Logo),
    ("thumbnail", ImageKind::Logo),
    ("icon", ImageKind::Icon),
    ("header.image", ImageKind::Background),
    ("footer.logo", ImageKind::Logo),
];

/// The brand image `id` of `org` if a look can hold it in `slot`: no larger than the slot
/// takes, and a usable tab icon in `icon`.
#[must_use]
pub fn usable_image<'a>(org: &'a OrgBrand, slot: &str, id: Uuid) -> Option<&'a ImageRef> {
    let kind = IMAGE_SLOTS.iter().find(|(name, _)| *name == slot)?.1;
    let image = &org.images.iter().find(|b| b.id == id)?.image;
    let fits = image.bytes <= kind.max_bytes();
    let icon_ok = slot != "icon" || icon_problem(image).is_none();
    (fits && icon_ok).then_some(image)
}

/// The brand font `id` of `org`, if it has one.
#[must_use]
pub fn usable_font(org: &OrgBrand, id: Uuid) -> Option<&FontRef> {
    org.fonts.iter().find(|f| f.id == id).map(|f| &f.font)
}

/// `look`'s image `image` as a template refers to it: the brand image with the same content,
/// or `None`, noted in `dropped` as `slot`.
fn image_reference(
    org: &OrgBrand,
    slot: &'static str,
    image: Option<&ImageRef>,
    dropped: &mut Vec<Dropped>,
) -> Option<TemplateImage> {
    let image = image?;
    match org.images.iter().find(|b| b.image.sha256 == image.sha256) {
        Some(found) => Some(TemplateImage::BrandImage(found.id)),
        None => {
            dropped.push(Dropped {
                slot,
                name: image.name.clone(),
            });
            None
        }
    }
}

/// `font` as a template refers to it: the brand font with the same content, or `None`, noted
/// in `dropped` as `slot`.
fn font_reference(
    org: &OrgBrand,
    slot: &'static str,
    font: Option<&FontRef>,
    dropped: &mut Vec<Dropped>,
) -> Option<TemplateFontRef> {
    let font = font?;
    match org.fonts.iter().find(|f| f.font.sha256 == font.sha256) {
        Some(found) => Some(TemplateFontRef::BrandFont(found.id)),
        None => {
            dropped.push(Dropped {
                slot,
                name: font.name.clone(),
            });
            None
        }
    }
}

/// A site's `look` as a template keeps it: each image and font that the brand centre `org`
/// holds (by content) becomes a reference to it, and every other is left out and answered as
/// dropped, in slot order. A theme from the site's own themes keeps its colours as custom ones,
/// since no other site has that theme. What the left-out files needed is undone (see
/// [`tidy`]).
#[must_use]
pub fn template_look_of(look: &Look, org: &OrgBrand) -> (TemplateLook, Vec<Dropped>) {
    let mut dropped = Vec::new();
    let logo = image_reference(org, "logo", look.logo.as_ref(), &mut dropped);
    let thumbnail = image_reference(org, "thumbnail", look.thumbnail.as_ref(), &mut dropped);
    let icon = image_reference(org, "icon", look.icon.as_ref(), &mut dropped);
    let header_image = image_reference(
        org,
        "header.image",
        look.header.image.as_ref(),
        &mut dropped,
    );
    let footer_logo = image_reference(org, "footer.logo", look.footer.logo.as_ref(), &mut dropped);
    let fonts = look.fonts.as_ref().map(|fonts| {
        let display = font_reference(
            org,
            "fonts.display",
            fonts.package.display.as_ref(),
            &mut dropped,
        );
        let content = font_reference(
            org,
            "fonts.content",
            fonts.package.content.as_ref(),
            &mut dropped,
        );
        TemplateFonts {
            package: TemplateFontPackage {
                name: fonts.package.name.clone(),
                display,
                content,
                roles: fonts.package.roles,
            },
            from: fonts.from.clone(),
        }
    });
    let from = if look.from.starts_with("site:") {
        "custom".to_owned()
    } else {
        look.from.clone()
    };
    let mut out = TemplateLook {
        theme: look.theme.clone(),
        from,
        logo,
        thumbnail,
        logo_alt: look.logo_alt.clone(),
        header: TemplateHeader {
            layout: look.header.layout,
            background: look.header.background,
            image: header_image,
            align: look.header.align,
            hide_title: look.header.hide_title,
            bleed: look.header.bleed,
            frame: look.header.frame,
        },
        navigation: look.navigation,
        footer: TemplateFooter {
            enabled: look.footer.enabled,
            layout: look.footer.layout,
            logo: footer_logo,
            name: look.footer.name.clone(),
            background: look.footer.background,
            groups: look.footer.groups.clone(),
        },
        fonts,
        icon,
        menu: look.menu,
        page: look.page,
    };
    tidy(&mut out);
    (out, dropped)
}

/// Undoes what references left out of `look` were needed for, so it stays a look a site can
/// hold: an image background without its image goes back to none, a title hidden behind a
/// logo that is gone is shown, and a text role set in a font that is gone uses the system
/// fonts.
fn tidy(look: &mut TemplateLook) {
    if look.header.image.is_none() && look.header.background == Background::Image {
        look.header.background = Header::default().background;
    }
    if look.logo.is_none() {
        look.header.hide_title = false;
    }
    if let Some(fonts) = &mut look.fonts {
        let (display, content) = (
            fonts.package.display.is_some(),
            fonts.package.content.is_some(),
        );
        let roles = &mut fonts.package.roles;
        for role in [
            &mut roles.title,
            &mut roles.heading,
            &mut roles.body,
            &mut roles.interactive,
        ] {
            if (*role == FontRole::Display && !display) || (*role == FontRole::Content && !content)
            {
                *role = FontRole::System;
            }
        }
    }
}

/// Leaves out of `look` each reference that `keep_image` (given the slot and identifier) or
/// `keep_font` refuses, then tidies it (see [`tidy`]). Answers what was left out, in slot
/// order.
pub fn prune_references(
    look: &mut TemplateLook,
    keep_image: &dyn Fn(&'static str, Uuid) -> bool,
    keep_font: &dyn Fn(Uuid) -> bool,
) -> Vec<Unresolved> {
    let mut unresolved = Vec::new();
    for (slot, image) in [
        ("logo", &mut look.logo),
        ("thumbnail", &mut look.thumbnail),
        ("icon", &mut look.icon),
        ("header.image", &mut look.header.image),
        ("footer.logo", &mut look.footer.logo),
    ] {
        if let Some(TemplateImage::BrandImage(id)) = *image
            && !keep_image(slot, id)
        {
            *image = None;
            unresolved.push(Unresolved { slot, id });
        }
    }
    if let Some(fonts) = &mut look.fonts {
        for (slot, font) in [
            ("fonts.display", &mut fonts.package.display),
            ("fonts.content", &mut fonts.package.content),
        ] {
            if let Some(TemplateFontRef::BrandFont(id)) = *font
                && !keep_font(id)
            {
                *font = None;
                unresolved.push(Unresolved { slot, id });
            }
        }
    }
    tidy(look);
    unresolved
}

/// Leaves out of `look` each reference the brand centre `org` cannot fill (see
/// [`usable_image`], [`usable_font`]). Answers what was left out.
pub fn prune_unknown(look: &mut TemplateLook, org: &OrgBrand) -> Vec<Unresolved> {
    prune_references(
        look,
        &|slot, id| usable_image(org, slot, id).is_some(),
        &|id| usable_font(org, id).is_some(),
    )
}

/// The site look `look` makes with the brand centre `org`: each reference becomes a snapshot
/// of the brand centre's file (a reference it cannot fill is left out; prune first to report
/// it). The font package copy takes the identifier its source names (the nil one for a
/// built-in package, as a look's copy of one has).
#[must_use]
pub fn look_from_references(look: &TemplateLook, org: &OrgBrand) -> Look {
    let image = |slot: &str, r: Option<TemplateImage>| {
        r.and_then(|TemplateImage::BrandImage(id)| usable_image(org, slot, id).cloned())
    };
    let font = |r: Option<TemplateFontRef>| {
        r.and_then(|TemplateFontRef::BrandFont(id)| usable_font(org, id).cloned())
    };
    let fonts = look.fonts.as_ref().map(|fonts| {
        let built_in = BUILT_IN_FONT_PACKAGES.iter().any(|b| b.id == fonts.from);
        let id = fonts
            .from
            .split_once(':')
            .filter(|_| !built_in)
            .and_then(|(_, id)| Uuid::parse_str(id).ok())
            .unwrap_or_else(Uuid::nil);
        LookFonts {
            package: FontPackage {
                id,
                name: fonts.package.name.clone(),
                display: font(fonts.package.display),
                content: font(fonts.package.content),
                roles: fonts.package.roles,
            },
            from: fonts.from.clone(),
        }
    });
    Look {
        theme: look.theme.clone(),
        from: look.from.clone(),
        logo: image("logo", look.logo),
        thumbnail: image("thumbnail", look.thumbnail),
        logo_alt: look.logo_alt.clone(),
        header: Header {
            layout: look.header.layout,
            background: look.header.background,
            image: image("header.image", look.header.image),
            align: look.header.align,
            hide_title: look.header.hide_title,
            bleed: look.header.bleed,
            frame: look.header.frame,
        },
        navigation: look.navigation,
        footer: Footer {
            enabled: look.footer.enabled,
            layout: look.footer.layout,
            logo: image("footer.logo", look.footer.logo),
            name: look.footer.name.clone(),
            background: look.footer.background,
            groups: look.footer.groups.clone(),
        },
        fonts,
        icon: image("icon", look.icon),
        menu: look.menu,
        page: look.page,
    }
}

/// The look the template `template` carries (a built-in's name, which carries none, or a
/// saved template's id that is not deleted), for `actor`, an administrator.
///
/// # Errors
/// `Forbidden` for anyone else; `NotFound` for an unknown or deleted template; otherwise the
/// database error.
pub async fn look_of(
    state: &AppState,
    actor: &User,
    template: &str,
) -> Result<Option<TemplateLook>, AccessError> {
    require_admin(state, actor).await?;
    if BUILT_INS.contains(&template) {
        return Ok(None);
    }
    Ok(body_of(&live_template(state, template).await?)?.look)
}

/// `Forbidden` unless `actor` is an enabled administrator as the database holds them now.
async fn require_admin(state: &AppState, actor: &User) -> Result<(), AccessError> {
    let current = state.db().user(actor.id).await?;
    if current.is_some_and(|u| u.is_admin && !u.disabled) {
        Ok(())
    } else {
        Err(AccessError::Forbidden)
    }
}

/// The body of a saved template as its mirror holds it.
fn body_of(row: &TemplateRow) -> Result<TemplateBody, AccessError> {
    serde_json::from_value(row.body.clone()).map_err(|e| AccessError::Db(DbError::new(e)))
}

/// The live saved template `id`; `NotFound` for a deleted or unknown one, or for text that is
/// not an id.
async fn live_template(state: &AppState, id: &str) -> Result<TemplateRow, AccessError> {
    let id = Uuid::parse_str(id).map_err(|_| AccessError::NotFound)?;
    state
        .db()
        .template(id)
        .await?
        .filter(|row| row.deleted_at.is_none())
        .ok_or(AccessError::NotFound)
}

/// Every template for `actor`, an administrator: the built-ins first, then the saved ones that
/// are not deleted, by name.
///
/// # Errors
/// `Forbidden` for anyone else; otherwise the database error.
pub async fn list(state: &AppState, actor: &User) -> Result<Vec<TemplateSummary>, AccessError> {
    require_admin(state, actor).await?;
    let mut out: Vec<TemplateSummary> = BUILT_INS
        .iter()
        .filter_map(|name| {
            built_in_words(name).map(|(shown, description)| TemplateSummary {
                id: (*name).to_owned(),
                name: shown.to_owned(),
                description: description.to_owned(),
                built_in: true,
                kind: (*name).to_owned(),
            })
        })
        .collect();
    for row in state.db().live_templates().await? {
        let kind = body_of(&row)?.kind;
        out.push(summary_of(&row.id, &row.name, &row.description, kind));
    }
    Ok(out)
}

/// A saved template as listed.
fn summary_of(id: &Uuid, name: &str, description: &str, kind: String) -> TemplateSummary {
    TemplateSummary {
        id: id.to_string(),
        name: name.to_owned(),
        description: description.to_owned(),
        built_in: false,
        kind,
    }
}

/// A saved template record as listed.
#[must_use]
pub fn summary_of_record(record: &TemplateRecord) -> TemplateSummary {
    summary_of(
        &record.id,
        &record.name,
        &record.description,
        record.body.kind.clone(),
    )
}

/// The shape a new site made from `template` gets, for `actor`, an administrator: a built-in
/// (`team` when `None`) with the organisation's defaults as they are now, or a saved template
/// that is not deleted.
///
/// # Errors
/// `Forbidden` for anyone else; `InvalidSetting` for a name that is neither a built-in nor an
/// id; `NotFound` for an unknown or deleted template; otherwise the database error.
pub async fn shape_for(
    state: &AppState,
    actor: &User,
    template: Option<&str>,
) -> Result<SiteShape, AccessError> {
    require_admin(state, actor).await?;
    let template = template.unwrap_or(TEAM);
    if BUILT_INS.contains(&template) {
        let defaults = state.db().regional_defaults().await?;
        return built_in_shape(template, &defaults).ok_or(AccessError::NotFound);
    }
    if Uuid::parse_str(template).is_err() {
        return Err(AccessError::InvalidSetting);
    }
    let row = live_template(state, template).await?;
    SiteShape::from_body(&row.id.to_string(), body_of(&row)?)
        .ok_or(AccessError::InvalidTemplate("invalid_kind"))
}

/// The package of `template` (a built-in's name or a saved template's id that is not deleted),
/// for `actor`, an administrator.
///
/// # Errors
/// `Forbidden` for anyone else; `NotFound` for an unknown or deleted template; otherwise the
/// database error.
pub async fn package(
    state: &AppState,
    actor: &User,
    template: &str,
) -> Result<TemplatePackage, AccessError> {
    require_admin(state, actor).await?;
    if let Some((name, description)) = built_in_words(template) {
        let defaults = state.db().regional_defaults().await?;
        let body = built_in_body(template, &defaults).ok_or(AccessError::NotFound)?;
        return Ok(TemplatePackage {
            sharecraft_template: PACKAGE_VERSION,
            name: name.to_owned(),
            description: description.to_owned(),
            body,
        });
    }
    let row = live_template(state, template).await?;
    Ok(TemplatePackage {
        sharecraft_template: PACKAGE_VERSION,
        body: body_of(&row)?,
        name: row.name,
        description: row.description,
    })
}

/// The `InvalidTemplate` refusal with `reason`.
fn refused<T>(reason: &'static str) -> Result<T, AccessError> {
    Err(AccessError::InvalidTemplate(reason))
}

/// Whether a navigation of a package (as plain JSON) names anything by identifier: an item
/// `id`, a `node` target, or an audience group given by id rather than purpose.
fn names_identifiers(navigation: &Value) -> bool {
    let Some(items) = navigation.as_array() else {
        return false;
    };
    items.iter().any(|item| {
        item.get("id").is_some()
            || item
                .get("target")
                .is_some_and(|target| target.get("node").is_some())
            || item
                .get("audience")
                .and_then(Value::as_array)
                .is_some_and(|audience| {
                    audience
                        .iter()
                        .any(|a| a.as_str().is_some_and(|text| Uuid::parse_str(text).is_ok()))
                })
            || item.get("children").is_some_and(names_identifiers)
    })
}

/// Reads a template package from another installation, refusing it with `InvalidTemplate` and
/// a reason (`too_large` over 1 MiB, `not_utf8`, `unreadable`, `unknown_version`,
/// `identifiers` for anything named by id, `unknown_field`, `invalid_name`,
/// `invalid_description`, the size limits of [`check_template_size`], then the other rules of
/// [`validate_template_body`]), or cleaning it (see [`clean_shape`]). The size limits are
/// checked before any name is cleaned, and cleaning takes time in proportion to the number of
/// names, so no package costs much more to refuse than a small one. Answers the package as it
/// will be kept and the names that changed, in the order they appear.
///
/// # Errors
/// `InvalidTemplate` as above.
pub fn read_package(bytes: &[u8]) -> Result<(TemplatePackage, Vec<Cleaned>), AccessError> {
    if bytes.len() > MAX_PACKAGE_BYTES {
        return refused("too_large");
    }
    let Ok(text) = std::str::from_utf8(bytes) else {
        return refused("not_utf8");
    };
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return refused("unreadable");
    };
    if !value.is_object() {
        return refused("unreadable");
    }
    if value.get("sharecraft_template").and_then(Value::as_u64) != Some(u64::from(PACKAGE_VERSION))
    {
        return refused("unknown_version");
    }
    if value
        .get("body")
        .and_then(|body| body.get("navigation"))
        .is_some_and(names_identifiers)
    {
        return refused("identifiers");
    }
    // serde_json names an unexpected field "unknown field" in its message; a change there
    // would answer `unreadable` instead, which the hostile-package test would catch.
    let mut package: TemplatePackage = match serde_json::from_value(value) {
        Ok(package) => package,
        Err(e) if e.to_string().contains("unknown field") => return refused("unknown_field"),
        Err(_) => return refused("unreadable"),
    };
    package.name = validate_template_name(&package.name)?.to_owned();
    validate_template_description(&package.description)?;
    check_template_size(&package.body)?;
    let cleaned = clean_shape(&mut package.body);
    validate_template_body(&package.body)?;
    Ok((package, cleaned))
}

/// Cleans the names of `body`'s libraries and folders by the rules of names made here
/// (refused characters become `_`, a name Windows keeps for a device gets `_`, and a name taken
/// in its place, ignoring case, gets ` (2)`, ` (3)` …), and leads each navigation link to the
/// library it named: a link naming a library as the body named it (exactly, or else ignoring
/// case) names that library's name as kept. Answers the names that changed, depth first.
fn clean_shape(body: &mut TemplateBody) -> Vec<Cleaned> {
    let mut cleaned = Vec::new();
    let mut libraries = Namer::default();
    let mut exact: HashMap<String, String> = HashMap::new();
    let mut folded: HashMap<String, String> = HashMap::new();
    for library in &mut body.libraries {
        let kept = libraries.keep(&library.name);
        exact
            .entry(library.name.clone())
            .or_insert_with(|| kept.clone());
        folded
            .entry(library.name.to_lowercase())
            .or_insert_with(|| kept.clone());
        note(&mut cleaned, &mut library.name, kept);
        clean_folders(&mut library.folders, &mut cleaned);
    }
    if let Some(items) = &mut body.navigation {
        lead_links(items, &exact, &folded);
    }
    cleaned
}

/// Each library link of `items` and their children led to the library's kept name (see
/// [`clean_shape`]); a link naming no library of the body is left as it is.
fn lead_links(
    items: &mut [TemplateNavItem],
    exact: &HashMap<String, String>,
    folded: &HashMap<String, String>,
) {
    for item in items {
        if let Some(TemplateNavTarget::Library(name)) = &mut item.target {
            let kept = exact
                .get(name.as_str())
                .or_else(|| folded.get(&name.to_lowercase()));
            if let Some(kept) = kept {
                kept.clone_into(name);
            }
        }
        lead_links(&mut item.children, exact, folded);
    }
}

/// Sets `name` to `kept`, noting the change if there is one.
fn note(cleaned: &mut Vec<Cleaned>, name: &mut String, kept: String) {
    if *name != kept {
        cleaned.push(Cleaned {
            from: std::mem::replace(name, kept.clone()),
            to: kept,
        });
    }
}

/// `folders`, and the folders inside them, with every name cleaned, depth first.
fn clean_folders(folders: &mut [TemplateFolder], cleaned: &mut Vec<Cleaned>) {
    let mut namer = Namer::default();
    for folder in folders {
        let kept = namer.keep(&folder.name);
        note(cleaned, &mut folder.name, kept);
        clean_folders(&mut folder.folders, cleaned);
    }
}

/// The names kept in one place, made acceptable and unique ignoring case, in time in
/// proportion to their number: for each name it remembers the next ` (n)` to try, so many
/// items with one name do not each count up from ` (2)`.
#[derive(Default)]
struct Namer {
    /// The names kept, lower-cased.
    taken: HashSet<String>,
    /// For each cleaned name (lower-cased) that was taken, the next number to try.
    next: HashMap<String, u64>,
}

impl Namer {
    /// `name` cleaned, and made unique among the names kept so far, which it then joins.
    fn keep(&mut self, name: &str) -> String {
        let cleaned = clean_folder_name(name);
        let lower = cleaned.to_lowercase();
        if self.taken.insert(lower.clone()) {
            return cleaned;
        }
        let mut n = self.next.get(&lower).copied().unwrap_or(2);
        loop {
            let candidate = fit(&cleaned, "", &format!(" ({n})"));
            n = n.saturating_add(1);
            if self.taken.insert(candidate.to_lowercase()) {
                self.next.insert(lower, n);
                return candidate;
            }
        }
    }
}

/// A template's navigation made a new site's: each library name becomes the new library of
/// that name (ignoring case) in `libraries`, and an item naming one it does not hold is left
/// out with its children; each purpose in an audience becomes the new site's group for it
/// (`groups`: Owners, Members and Visitors ids); each item gets a new id.
#[must_use]
pub fn resolve_navigation(
    items: &[TemplateNavItem],
    libraries: &HashMap<String, Uuid>,
    groups: [Uuid; 3],
) -> Vec<NavItem> {
    items
        .iter()
        .filter_map(|item| resolve_item(item, libraries, groups))
        .collect()
}

/// One item of [`resolve_navigation`]; `None` when it is left out.
fn resolve_item(
    item: &TemplateNavItem,
    libraries: &HashMap<String, Uuid>,
    groups: [Uuid; 3],
) -> Option<NavItem> {
    let target = match &item.target {
        None => None,
        Some(TemplateNavTarget::Url(url)) => Some(NavTarget::Url(url.clone())),
        Some(TemplateNavTarget::Library(name)) => {
            Some(NavTarget::Node(*libraries.get(&name.to_lowercase())?))
        }
    };
    let mut audience = Vec::new();
    for purpose in item.audience.iter().filter_map(|p| SiteGroup::parse(p)) {
        let group = SiteGroup::ALL
            .iter()
            .zip(groups)
            .find(|(each, _)| **each == purpose)
            .map(|(_, id)| id);
        if let Some(group) = group
            && !audience.contains(&group)
        {
            audience.push(group);
        }
    }
    Some(NavItem {
        id: Uuid::new_v4(),
        label: item.label.clone(),
        target,
        audience,
        children: resolve_navigation(&item.children, libraries, groups),
    })
}

/// The live libraries and folders of the site `site` in `tree`, by name ignoring case, each
/// library with its kind and settings as `settings` holds them (the defaults for a library with
/// no record).
fn site_libraries(
    tree: &[TreeRecord],
    site: Uuid,
    settings: &HashMap<Uuid, LibraryFields>,
) -> Vec<TemplateLibrary> {
    let mut children: HashMap<Uuid, Vec<&TreeRecord>> = HashMap::new();
    for record in tree.iter().filter(|r| {
        r.site == site && r.deleted.is_none() && matches!(r.kind.as_str(), "library" | "folder")
    }) {
        if let Some(parent) = record.parent {
            children.entry(parent).or_default().push(record);
        }
    }
    for list in children.values_mut() {
        list.sort_by_key(|r| (r.name.to_lowercase(), r.name.clone()));
    }
    fn folders(children: &HashMap<Uuid, Vec<&TreeRecord>>, id: Uuid) -> Vec<TemplateFolder> {
        children
            .get(&id)
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter(|r| r.kind == "folder")
            .map(|r| TemplateFolder {
                name: r.name.clone(),
                folders: folders(children, r.id),
            })
            .collect()
    }
    children
        .get(&site)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter(|r| r.kind == "library")
        .map(|r| TemplateLibrary {
            name: r.name.clone(),
            settings: settings.get(&r.id).cloned().unwrap_or_default(),
            folders: folders(&children, r.id),
        })
        .collect()
}

/// A site's navigation made a template's: a node target becomes the name of its library (a
/// target inside a library becomes that library), and an item whose target is not in a live
/// library of the site (another site, the site itself, or something deleted) is left out with
/// its children; audiences keep the site's own groups, by purpose, and drop every other group.
fn template_navigation(
    items: &[NavItem],
    tree: &[TreeRecord],
    site: Uuid,
    own_groups: &HashMap<Uuid, SiteGroup>,
) -> Vec<TemplateNavItem> {
    let live: HashMap<Uuid, &TreeRecord> = tree
        .iter()
        .filter(|r| r.site == site && r.deleted.is_none())
        .map(|r| (r.id, r))
        .collect();
    let library_of = |node: Uuid| -> Option<String> {
        let mut record = live.get(&node)?;
        loop {
            if record.kind == "library" {
                return Some(record.name.clone());
            }
            record = live.get(&record.parent?)?;
        }
    };
    fn convert(
        items: &[NavItem],
        library_of: &dyn Fn(Uuid) -> Option<String>,
        own_groups: &HashMap<Uuid, SiteGroup>,
    ) -> Vec<TemplateNavItem> {
        items
            .iter()
            .filter_map(|item| {
                let target = match &item.target {
                    None => None,
                    Some(NavTarget::Url(url)) => Some(TemplateNavTarget::Url(url.clone())),
                    Some(NavTarget::Node(node)) => {
                        Some(TemplateNavTarget::Library(library_of(*node)?))
                    }
                };
                let mut audience: Vec<String> = Vec::new();
                for group in &item.audience {
                    if let Some(purpose) = own_groups.get(group) {
                        let word = purpose.as_str().to_owned();
                        if !audience.contains(&word) {
                            audience.push(word);
                        }
                    }
                }
                Some(TemplateNavItem {
                    label: item.label.clone(),
                    target,
                    audience,
                    children: convert(&item.children, library_of, own_groups),
                })
            })
            .collect()
    }
    convert(items, &library_of, own_groups)
}

/// A template saved from a site, with what changed on the way.
#[derive(Clone, Debug)]
pub struct SavedTemplate {
    /// The template.
    pub template: TemplateRecord,
    /// The names that had to change.
    pub cleaned: Vec<Cleaned>,
    /// The files of the site's look that were left out (see [`template_look_of`]).
    pub dropped: Vec<Dropped>,
}

/// Saves the shape of the site `site` as a new template named `name`, for `actor`, an
/// administrator: its live libraries, each with its kind and settings, and folders (no files,
/// nothing in its recycle bin), its kind, language, time zone and navigation (see
/// [`template_navigation`]), and its look with its images and fonts as references to the brand
/// centre (see [`template_look_of`]), as its records in Git hold them. Names made under older
/// rules are cleaned as an imported package's are (see [`clean_shape`]), and the links to them
/// follow. One commit, mirrored with `template.saved` (with the names cleaned and the files
/// dropped). Answers the template, the names that changed and the files dropped.
///
/// # Errors
/// `Forbidden` for anyone else; `InvalidTemplate` for a name or description that breaks the
/// rules (`invalid_name`, `invalid_description`) or a site too large for a template;
/// `NotFound` for anything but a live site; `OutOfStep` while the tree needs repair; otherwise
/// the Git or database error.
pub async fn save_from_site(
    state: &AppState,
    actor: &User,
    site: NodeId,
    name: &str,
    description: &str,
) -> Result<SavedTemplate, AccessError> {
    require_admin(state, actor).await?;
    let name = validate_template_name(name)?.to_owned();
    validate_template_description(description)?;
    let _turn = state.tree_lock().lock().await;
    ensure_in_step(state).await?;
    let db = state.db();
    let node = db.node(site).await?.ok_or(AccessError::NotFound)?;
    if node.kind != NodeKind::Site || node.deleted {
        return Err(AccessError::NotFound);
    }
    let record: SiteRecord = match crate::sites::read_site_record(state.checkpointer(), site)
        .await
        .map_err(git_failure)?
    {
        Some(record) => record,
        None => crate::sites::new_site_record(site, &db.regional_defaults().await?),
    };
    let tree = read_tree(state.checkpointer()).await.map_err(git_failure)?;
    let settings: HashMap<Uuid, LibraryFields> =
        match crate::libraries::read_libraries(state.checkpointer()).await {
            Ok(records) => records.iter().map(|r| (r.id, r.fields())).collect(),
            Err(CheckpointError::Unsupported) => HashMap::new(),
            Err(e) => return Err(git_failure(e)),
        };
    let own_groups: HashMap<Uuid, SiteGroup> = db
        .site_group_ids(site)
        .await?
        .into_iter()
        .map(|(id, purpose)| (id.0, purpose))
        .collect();
    let mut body = TemplateBody {
        kind: record.kind.clone(),
        language: record.language.clone(),
        time_zone: record.time_zone.clone(),
        libraries: site_libraries(&tree, site.0, &settings),
        navigation: record
            .navigation
            .as_ref()
            .map(|items| template_navigation(items, &tree, site.0, &own_groups)),
        look: None,
    };
    let mut dropped = Vec::new();
    if let Some(look) = &record.look {
        let (kept, left_out) = template_look_of(look, &db.org_brand().await?);
        body.look = Some(kept);
        dropped = left_out;
    }
    check_template_size(&body)?;
    let cleaned = clean_shape(&mut body);
    validate_template_body(&body)?;
    let template = new_template(actor, name, description.to_owned(), body);
    let event = NewEvent {
        kind: "template.saved",
        actor: Some(actor.id),
        subject: None,
        document: None,
        node: Some(site),
        detail: json!({
            "template": template.id,
            "name": template.name,
            "site": site.0,
            "cleaned": cleaned,
            "dropped": dropped,
        }),
    };
    let message = format!("Save template {}", template.name);
    let undo = Undo {
        removed_templates: vec![template.id],
        ..Undo::default()
    };
    commit_template(state, actor, &message, &template, undo, event).await?;
    Ok(SavedTemplate {
        template,
        cleaned,
        dropped,
    })
}

/// A new template record made by `actor` now.
fn new_template(
    actor: &User,
    name: String,
    description: String,
    body: TemplateBody,
) -> TemplateRecord {
    TemplateRecord {
        schema: SITE_SCHEMA,
        id: Uuid::new_v4(),
        name,
        description,
        created: Stamp {
            at: now_text(),
            by: author_of(actor),
        },
        deleted: None,
        body,
    }
}

/// Imports `package` (read by [`read_package`], with the names that were `cleaned`) as a new
/// saved template, for `actor`, an administrator. Its look keeps only the references this
/// organisation's brand centre can fill (see [`prune_unknown`]); the others are left out and
/// answered. One commit, mirrored with `template.imported` (its name, the names cleaned and the
/// references left out).
///
/// # Errors
/// `Forbidden` for anyone else; `OutOfStep` while the tree needs repair; otherwise the Git or
/// database error.
pub async fn import(
    state: &AppState,
    actor: &User,
    mut package: TemplatePackage,
    cleaned: &[Cleaned],
) -> Result<(TemplateRecord, Vec<Unresolved>), AccessError> {
    require_admin(state, actor).await?;
    let _turn = state.tree_lock().lock().await;
    ensure_in_step(state).await?;
    let mut unresolved = Vec::new();
    if let Some(look) = &mut package.body.look {
        unresolved = prune_unknown(look, &state.db().org_brand().await?);
        validate_template_body(&package.body)?;
    }
    let template = new_template(actor, package.name, package.description, package.body);
    let event = NewEvent {
        kind: "template.imported",
        actor: Some(actor.id),
        subject: None,
        document: None,
        node: None,
        detail: json!({
            "template": template.id,
            "name": template.name,
            "cleaned": cleaned,
            "unresolved": unresolved,
        }),
    };
    let message = format!("Import template {}", template.name);
    let undo = Undo {
        removed_templates: vec![template.id],
        ..Undo::default()
    };
    commit_template(state, actor, &message, &template, undo, event).await?;
    Ok((template, unresolved))
}

/// Deletes the saved template `id` for `actor`, an administrator: its record is kept, marked
/// deleted. One commit, mirrored with `template.deleted`.
///
/// # Errors
/// `Forbidden` for anyone else and for a built-in; `NotFound` for an unknown or deleted
/// template; `OutOfStep` while the tree needs repair; otherwise the Git or database error.
pub async fn delete(state: &AppState, actor: &User, id: &str) -> Result<(), AccessError> {
    require_admin(state, actor).await?;
    if BUILT_INS.contains(&id) {
        return Err(AccessError::Forbidden);
    }
    let id = Uuid::parse_str(id).map_err(|_| AccessError::NotFound)?;
    let _turn = state.tree_lock().lock().await;
    ensure_in_step(state).await?;
    let before = read_templates(state)
        .await
        .map_err(git_failure)?
        .into_iter()
        .find(|t| t.id == id && t.deleted.is_none())
        .ok_or(AccessError::NotFound)?;
    let mut after = before.clone();
    after.deleted = Some(Stamp {
        at: now_text(),
        by: author_of(actor),
    });
    let event = NewEvent {
        kind: "template.deleted",
        actor: Some(actor.id),
        subject: None,
        document: None,
        node: None,
        detail: json!({ "template": id, "name": before.name }),
    };
    let message = format!("Delete template {}", before.name);
    let undo = Undo {
        templates: vec![before],
        ..Undo::default()
    };
    commit_template(state, actor, &message, &after, undo, event).await
}

/// Every saved template in Git.
async fn read_templates(state: &AppState) -> Result<Vec<TemplateRecord>, CheckpointError> {
    let store = Arc::clone(state.checkpointer());
    tokio::task::spawn_blocking(move || store.read_templates())
        .await
        .map_err(|e| CheckpointError::Git(GitError::new(e)))?
}

/// Writes `template` to Git as one commit described by `message`, then mirrors it with `event`,
/// judged again there (`actor` must still be an enabled administrator). A refused change is
/// taken back by writing `undo` as a second commit. A database failure after it was judged is
/// repaired from Git once, and the tree marked out of step if that fails too.
async fn commit_template(
    state: &AppState,
    actor: &User,
    message: &str,
    template: &TemplateRecord,
    undo: Undo,
    event: NewEvent,
) -> Result<(), AccessError> {
    let author = author_of(actor);
    let row = crate::sites::template_row(template).map_err(DbError::new)?;
    let records = Undo {
        templates: vec![template.clone()],
        ..Undo::default()
    };
    write_records(
        state.checkpointer(),
        author.clone(),
        message.to_owned(),
        records,
    )
    .await
    .map_err(git_failure)?;
    match state
        .db()
        .apply_template_change(&row, actor.id, event)
        .await
    {
        Ok(()) => Ok(()),
        Err(AccessError::Db(error)) => {
            eprintln!(
                "sharecraft-server: template {} is in Git but not in the database: {error}",
                template.id
            );
            if !repair_template(state, template.id).await {
                state.tree_out_of_step().store(true, Ordering::SeqCst);
            }
            Err(AccessError::Db(error))
        }
        Err(refused) => {
            if let AccessError::Unjudged(error) = &refused {
                eprintln!(
                    "sharecraft-server: template {} could not be judged, so it is taken back: \
                     {error}",
                    template.id
                );
            }
            if !take_back_all(state, author, message, undo).await {
                state.tree_out_of_step().store(true, Ordering::SeqCst);
            }
            Err(refused)
        }
    }
}

/// Writes the saved template `id` from Git into PostgreSQL once, with `template.saved` marked
/// as a repair. Returns whether it is now in step.
async fn repair_template(state: &AppState, id: Uuid) -> bool {
    let records = match read_templates(state).await {
        Ok(records) => records,
        Err(e) => {
            eprintln!("sharecraft-server: could not read template {id} from Git: {e}");
            return false;
        }
    };
    let Some(record) = records.into_iter().find(|t| t.id == id) else {
        return true;
    };
    let row = match crate::sites::template_row(&record) {
        Ok(row) => row,
        Err(e) => {
            eprintln!("sharecraft-server: could not repair template {id}: {e}");
            return false;
        }
    };
    let event = NewEvent {
        kind: "template.saved",
        actor: None,
        subject: None,
        document: None,
        node: None,
        detail: json!({ "repaired": true, "template": id, "name": row.name }),
    };
    match state.db().apply_template(&row, Some(event)).await {
        Ok(()) => true,
        Err(e) => {
            eprintln!("sharecraft-server: could not repair template {id}: {e}");
            false
        }
    }
}
