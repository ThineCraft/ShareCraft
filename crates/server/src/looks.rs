//! Sites' looks: each site's theme (a copy), logo, thumbnail and tab icon, header, navigation
//! and menu style, footer and fonts (a copy of a font package), recorded as `look` on its site
//! record in Git (see [`revisions::brand::Look`]), and its own themes and font packages
//! (`themes` and `font_packages` on the same record), with the history of its look, going back
//! to an earlier one, and copying the look of another site.
//!
//! Every change is one sites commit, mirrored with its event (`site.look_changed {before,
//! after}`, with `copied_from` for a copy; `site.theme_changed`; `site.font_package_changed`)
//! and judged again there, taken back if refused (see [`crate::sites::commit_sites`]). A look's
//! images and fonts, and the fonts of the site's own packages, pin their content in the content
//! store while they are used; content no longer used is erased unless something else names it.
//!
//! The brand policy (the limits on site owners kept in the brand centre) is held on every path
//! that sets a look or makes a site's own themes and font packages: changing the look, rolling
//! back, copying, and making or changing a theme or package. Administrators and brand managers
//! are never limited, and a look already stored is never changed by a change of the policy.
//!
//! The hub rule is worked out whenever a look is read, never written: a site that joined a hub
//! (registered, not deleted; an archived hub still counts) takes the theme and fonts of the
//! hub's own look, and everything else from its own. Its own are kept, unused, and come back
//! when it leaves. So joining, leaving, registering and unregistering a hub change no look.

use std::collections::HashSet;

use collab_core::ids::{DocId, NodeId};
use records::{Db, LookChange, Perms, SiteSettings, SiteState, User, site_pins};
use revisions::brand::{
    Actor, Align, BUILT_IN_FONT_PACKAGES, BUILT_IN_THEMES, Background, BuiltInTheme, FontPackage,
    FontRef, FontRole, FooterGroup, FooterLayout, Header, HeaderLayout, ImageKind, ImageRef,
    KnownSources, Look, LookFonts, MAX_SITE_FONT_PACKAGES, MAX_SITE_THEMES, MenuStyle, Navigation,
    OrgBrand, OrgTheme, ResolvedTheme, TemplateLook, Theme, check_theme, clean_text, family_name,
    fonts_match_source, icon_problem, image_policy_problems, policy_problems,
};
use revisions::{SiteRecord, SiteTheme};
use serde::Deserialize;
use serde_json::{Value, json};
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::AppState;
use crate::api::ApiError;
use crate::api::brand::{default_icon_url, font_url, image_url, package_out, theme_out};
use crate::brand::{
    BrandError, PackageEdit, ThemeInput, actor_kind, blank_package, checked_theme, edited_package,
    image_from_doc, org_theme, package_name_taken, resolve_edit,
};
use crate::sites::{SitesCommit, commit_sites, record_and_start, seen_site, site_event};
use crate::templates::{
    Unresolved, look_from_references, prune_references, usable_font, usable_image,
};
use crate::tree::{NewSiteLook, ensure_in_step, require};

/// How many changes of a look its history lists.
pub(crate) const HISTORY_LENGTH: i64 = 50;

// ---------------------------------------------------------------------------------------------
// What a person sends

/// An image chosen for a slot of a look: exactly one of `doc` (the current version of a
/// document), `brand_image` (an image of the brand centre, by id) or `keep` (the image with this
/// content hash that the look holds now, or that one of the last [`HISTORY_LENGTH`] entries of
/// its history names, before or after, as the exact snapshot recorded then).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ImageSlot {
    doc: Option<String>,
    brand_image: Option<String>,
    keep: Option<String>,
}

/// A theme chosen for a look: `from` a built-in (`builtin:<id>`), organisation (`org:<id>`) or
/// site (`site:<id>`) theme, which is copied; or a custom theme given in full.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ThemeChoice {
    from: Option<String>,
    name: Option<String>,
    primary: Option<String>,
    secondary: Option<String>,
    primary_dark: Option<String>,
    secondary_dark: Option<String>,
}

/// A header as sent: each field left out takes its default.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HeaderInput {
    layout: Option<HeaderLayout>,
    background: Option<Background>,
    image: Option<ImageSlot>,
    align: Option<Align>,
    hide_title: Option<bool>,
}

/// A footer as sent: each field left out takes its default.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FooterInput {
    enabled: Option<bool>,
    layout: Option<FooterLayout>,
    logo: Option<ImageSlot>,
    name: Option<String>,
    background: Option<Background>,
    groups: Option<Vec<FooterGroup>>,
}

/// A look as sent: each field left out takes its default (the ShareCraft theme, no images, an
/// empty logo description, the compact header, horizontal navigation, no footer).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LookInput {
    theme: Option<ThemeChoice>,
    logo: Option<ImageSlot>,
    thumbnail: Option<ImageSlot>,
    logo_alt: Option<String>,
    header: Option<HeaderInput>,
    navigation: Option<Navigation>,
    footer: Option<FooterInput>,
    fonts: Option<FontsChoice>,
    icon: Option<ImageSlot>,
    menu: Option<MenuStyle>,
}

/// The fonts chosen for a look: exactly one of `from`, the font package it names
/// (`builtin:<id>`, `org:<id>` or `site:<id>`), which is copied; or `keep: true`, the copy the
/// look holds now, unchanged (none if it holds none), which still works once its source is
/// gone.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FontsChoice {
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    keep: bool,
}

/// Which side of a history entry a rollback applies.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Which {
    /// The look before that change.
    Before,
    /// The look that change made (the default).
    #[default]
    After,
}

// ---------------------------------------------------------------------------------------------
// Themes

/// The theme `from` names as it is now (`builtin:<id>`, `org:<id>` in `org`, `site:<id>` among
/// `site_themes`); `None` when there is no such theme, or for `custom`.
fn source_theme(from: &str, org: &OrgBrand, site_themes: &[SiteTheme]) -> Option<Theme> {
    if from.starts_with("builtin:") {
        return BUILT_IN_THEMES
            .iter()
            .find(|t| t.id == from)
            .map(BuiltInTheme::theme);
    }
    let by_id = |themes: &[OrgTheme], id: &str| {
        let id = Uuid::parse_str(id).ok()?;
        themes.iter().find(|t| t.id == id).map(OrgTheme::theme)
    };
    if let Some(id) = from.strip_prefix("org:") {
        return by_id(&org.themes, id);
    }
    if let Some(id) = from.strip_prefix("site:") {
        return by_id(site_themes, id);
    }
    None
}

/// Whether the theme a look copied has changed at its source since, or is gone: never for a
/// custom theme.
fn source_changed(look: &Look, org: &OrgBrand, site_themes: &[SiteTheme]) -> bool {
    look.from != "custom"
        && source_theme(&look.from, org, site_themes).as_ref() != Some(&look.theme)
}

/// The theme `choice` asks for, checked, with where it came from.
///
/// # Errors
/// `InvalidTheme` for a source that names no theme, a choice that mixes a source with colours,
/// or a custom theme missing a name or colour; `LowContrast` for an unreadable one.
fn chosen_theme(
    choice: ThemeChoice,
    org: &OrgBrand,
    site_themes: &[SiteTheme],
) -> Result<(Theme, String), BrandError> {
    if let Some(from) = choice.from.as_deref().filter(|from| *from != "custom") {
        let given = [
            &choice.name,
            &choice.primary,
            &choice.secondary,
            &choice.primary_dark,
            &choice.secondary_dark,
        ];
        if given.iter().any(|field| field.is_some()) {
            return Err(BrandError::InvalidTheme(
                "a theme copied from another takes no name or colours".into(),
            ));
        }
        let theme = source_theme(from, org, site_themes)
            .ok_or_else(|| BrandError::InvalidTheme(format!("there is no theme {from}")))?;
        check_theme(&theme).map_err(BrandError::LowContrast)?;
        return Ok((theme, from.to_owned()));
    }
    let (Some(name), Some(primary), Some(secondary)) =
        (choice.name, choice.primary, choice.secondary)
    else {
        return Err(BrandError::InvalidTheme(
            "a custom theme has a name, a primary and a secondary colour".into(),
        ));
    };
    let theme = checked_theme(&ThemeInput {
        name,
        primary,
        secondary,
        primary_dark: choice.primary_dark,
        secondary_dark: choice.secondary_dark,
    })?;
    Ok((theme, "custom".to_owned()))
}

/// The colours a page uses for `look` (the defaults for none), or the ShareCraft theme's if
/// its theme no longer passes.
fn palettes(look: Option<&Look>) -> Option<ResolvedTheme> {
    let fallback = Look::default();
    check_theme(&look.unwrap_or(&fallback).theme)
        .or_else(|_| check_theme(&fallback.theme))
        .ok()
}

// ---------------------------------------------------------------------------------------------
// Building a look

/// `text` trimmed, or `InvalidLook` if it holds control or invisible characters.
fn cleaned(text: &str) -> Result<String, BrandError> {
    clean_text(text).map(str::to_owned).ok_or_else(|| {
        BrandError::InvalidLook("text holds no control or invisible characters".into())
    })
}

/// What an image slot is chosen from.
struct SlotContext<'a> {
    state: &'a AppState,
    actor: &'a User,
    site: NodeId,
    /// The images the look holds now, which `keep` may name.
    current: Vec<&'a ImageRef>,
    /// The images the last [`HISTORY_LENGTH`] entries of the look's history name (before or
    /// after), which `keep` may name too.
    history: Vec<ImageRef>,
    org: &'a OrgBrand,
}

/// Every image the last [`HISTORY_LENGTH`] changes of the look of `site` name, before or after
/// each change. An entry that cannot be read as a look names none.
async fn history_images(db: &Db, site: NodeId) -> Result<Vec<ImageRef>, BrandError> {
    let mut images: Vec<ImageRef> = Vec::new();
    for change in db.look_changes(site, HISTORY_LENGTH).await? {
        for side in [change.before, change.after] {
            let Ok(look) = serde_json::from_value::<Look>(side) else {
                continue;
            };
            for (_, image) in slots(&look) {
                if !images.iter().any(|i| i.sha256 == image.sha256) {
                    images.push(image.clone());
                }
            }
        }
    }
    Ok(images)
}

/// Refuses with `409 image_gone {slots}` when an image `input` keeps from the look's history
/// (not one the look holds now) has content that is no longer stored, or not available (held
/// or quarantined after a rescan). Keeps of content the history does not name are left to
/// [`slot_image`] to refuse.
async fn history_keeps_available(
    ctx: &SlotContext<'_>,
    input: &LookInput,
) -> Result<(), BrandError> {
    let header = input.header.as_ref().and_then(|h| h.image.as_ref());
    let footer = input.footer.as_ref().and_then(|f| f.logo.as_ref());
    let kept: Vec<(&'static str, &str)> = [
        ("logo", input.logo.as_ref()),
        ("thumbnail", input.thumbnail.as_ref()),
        ("icon", input.icon.as_ref()),
        ("header.image", header),
        ("footer.logo", footer),
    ]
    .into_iter()
    .filter_map(|(name, slot)| {
        let slot = slot?;
        if slot.doc.is_some() || slot.brand_image.is_some() {
            return None;
        }
        let sha = slot.keep.as_deref()?;
        let from_history = !ctx.current.iter().any(|i| i.sha256 == sha)
            && ctx.history.iter().any(|i| i.sha256 == sha);
        from_history.then_some((name, sha))
    })
    .collect();
    if kept.is_empty() {
        return Ok(());
    }
    let db = ctx.state.db();
    let hashes: Vec<String> = kept.iter().map(|(_, sha)| (*sha).to_owned()).collect();
    let missing = db.missing_blobs(&hashes).await?;
    let mut gone = Vec::new();
    for (name, sha) in kept {
        let held = match revisions::parse_hex(sha) {
            Some(hash) => db.availability(&hash).await?.is_some(),
            None => true,
        };
        if held || missing.iter().any(|m| m == sha) {
            gone.push(name);
        }
    }
    if gone.is_empty() {
        Ok(())
    } else {
        Err(BrandError::ImagesGone(gone))
    }
}

/// The image `slot` asks for, for a slot of `kind`'s size.
///
/// # Errors
/// `InvalidLook` for a slot that gives no way or more than one, an unknown brand image, or a
/// `keep` of an image neither the look nor its history holds; `413 image_too_large`; `423
/// held`; and those of [`image_from_doc`].
async fn slot_image(
    ctx: &SlotContext<'_>,
    slot: Option<ImageSlot>,
    kind: ImageKind,
) -> Result<Option<ImageRef>, BrandError> {
    let Some(slot) = slot else {
        return Ok(None);
    };
    match (slot.doc, slot.brand_image, slot.keep) {
        (Some(doc), None, None) => {
            let doc = Uuid::parse_str(&doc).map_err(|_| ApiError::InvalidRequest)?;
            let image =
                image_from_doc(ctx.state, ctx.actor, DocId(doc), kind, Some(ctx.site)).await?;
            Ok(Some(image))
        }
        (None, Some(id), None) => {
            let found = Uuid::parse_str(&id)
                .ok()
                .and_then(|id| ctx.org.images.iter().find(|i| i.id == id))
                .ok_or_else(|| BrandError::InvalidLook("there is no such brand image".into()))?;
            if found.image.bytes > kind.max_bytes() {
                return Err(ApiError::ImageTooLarge.into());
            }
            if let Some(hash) = revisions::parse_hex(&found.image.sha256)
                && let Some(scan) = ctx.state.db().availability(&hash).await?
            {
                return Err(ApiError::Held(scan).into());
            }
            Ok(Some(found.image.clone()))
        }
        (None, None, Some(sha)) => ctx
            .current
            .iter()
            .copied()
            .chain(ctx.history.iter())
            .find(|image| image.sha256 == sha)
            .map(|image| Some(image.clone()))
            .ok_or_else(|| {
                BrandError::InvalidLook(
                    "only an image the look holds now or had in its history can be kept".into(),
                )
            }),
        _ => Err(BrandError::InvalidLook(
            "an image is given as one of {doc}, {brand_image} or {keep}".into(),
        )),
    }
}

/// The hero image of a library of `site`, chosen by `actor` as a header image is: `slot` is
/// `{doc}` (the current version of a document in an asset library or in one of `site`'s
/// libraries, which `actor` can view), `{brand_image}` (an image of the brand centre), or
/// `{keep}` (the hash of `current`, the library's hero image now, kept as it is).
///
/// # Errors
/// As a header image of a look (see `slot_image`): `422 invalid_look` for a slot that gives no
/// way or more than one, an unknown brand image or a `keep` of anything but `current`; `413
/// image_too_large`; `423 held`; `404`, `400 wrong_place` or `422 not_an_image` for the
/// document. `409 not_allowed {problems}` when the brand policy turns off images from the
/// site's own libraries and `actor` is a site owner (neither an administrator nor a brand
/// manager): only an image the brand centre holds (matched by content) may be chosen or kept.
pub(crate) async fn hero_image(
    state: &AppState,
    actor: &User,
    site: NodeId,
    slot: ImageSlot,
    current: Option<&ImageRef>,
) -> Result<ImageRef, BrandError> {
    let org = state.db().org_brand().await?;
    let ctx = SlotContext {
        state,
        actor,
        site,
        current: current.into_iter().collect(),
        history: Vec::new(),
        org: &org,
    };
    let image = slot_image(&ctx, Some(slot), ImageKind::Background)
        .await?
        .ok_or_else(|| BrandError::InvalidLook("no image was chosen".into()))?;
    if !org.policy.site_images
        && !org.images.iter().any(|b| b.image.sha256 == image.sha256)
        && actor_kind(state.db(), actor).await? == Actor::Owner
    {
        return Err(BrandError::NotAllowed(vec![
            "banner picture: images from the site's own libraries are turned off for site owners"
                .to_owned(),
        ]));
    }
    Ok(image)
}

/// Every image of `look`, each with the name of its slot.
fn slots(look: &Look) -> Vec<(&'static str, &ImageRef)> {
    [
        ("logo", look.logo.as_ref()),
        ("thumbnail", look.thumbnail.as_ref()),
        ("icon", look.icon.as_ref()),
        ("header.image", look.header.image.as_ref()),
        ("footer.logo", look.footer.logo.as_ref()),
    ]
    .into_iter()
    .filter_map(|(name, image)| image.map(|i| (name, i)))
    .collect()
}

/// Every font file of `look`, each with the name of its slot.
fn font_slots(look: &Look) -> Vec<(&'static str, &FontRef)> {
    let Some(fonts) = &look.fonts else {
        return Vec::new();
    };
    [
        ("fonts.display", fonts.package.display.as_ref()),
        ("fonts.content", fonts.package.content.as_ref()),
    ]
    .into_iter()
    .filter_map(|(name, font)| font.map(|f| (name, f)))
    .collect()
}

/// The slots of `contents` (each a slot name and a content hash) whose content is no longer
/// stored, or not available (held or quarantined after a rescan): content that is never
/// pinned again.
async fn gone_contents(
    db: &Db,
    contents: &[(&'static str, &str)],
) -> Result<Vec<&'static str>, BrandError> {
    let hashes: Vec<String> = contents.iter().map(|(_, sha)| (*sha).to_owned()).collect();
    let missing = db.missing_blobs(&hashes).await?;
    let mut gone: Vec<&'static str> = Vec::new();
    for (slot, sha) in contents {
        let held = match revisions::parse_hex(sha) {
            Some(hash) => db.availability(&hash).await?.is_some(),
            None => true,
        };
        if (held || missing.iter().any(|m| m == sha)) && !gone.contains(slot) {
            gone.push(slot);
        }
    }
    Ok(gone)
}

/// The images and fonts of `look`, each a slot name and a content hash.
fn look_contents(look: &Look) -> Vec<(&'static str, &str)> {
    slots(look)
        .into_iter()
        .map(|(slot, image)| (slot, image.sha256.as_str()))
        .chain(
            font_slots(look)
                .into_iter()
                .map(|(slot, font)| (slot, font.sha256.as_str())),
        )
        .collect()
}

/// The font package `from` names as it is now (`builtin:<id>`, `org:<id>` in `org`,
/// `site:<id>` among `site_packages`), as a look copies it.
///
/// # Errors
/// `InvalidLook` when there is no such package.
fn source_package(
    from: &str,
    org: &OrgBrand,
    site_packages: &[FontPackage],
) -> Result<FontPackage, BrandError> {
    let by_id = |packages: &[FontPackage], id: &str| {
        let id = Uuid::parse_str(id).ok()?;
        packages.iter().find(|p| p.id == id).cloned()
    };
    let found = if let Some(built_in) = BUILT_IN_FONT_PACKAGES.iter().find(|p| p.id == from) {
        Some(built_in.package())
    } else if let Some(id) = from.strip_prefix("org:") {
        by_id(&org.font_packages, id)
    } else if let Some(id) = from.strip_prefix("site:") {
        by_id(site_packages, id)
    } else {
        None
    };
    found.ok_or_else(|| BrandError::InvalidLook(format!("there is no font package {from}")))
}

// ---------------------------------------------------------------------------------------------
// The brand policy

/// What the brand policy refuses `kind` in `look` for a site whose own themes and font
/// packages (including any this request makes) are `own`, and which makes or changes the
/// themes and packages `new`: [`policy_problems`] and [`image_policy_problems`] together, each
/// problem once.
fn problems(
    look: &Look,
    org: &OrgBrand,
    own: (&[SiteTheme], &[FontPackage]),
    new: (&[SiteTheme], &[FontPackage]),
    kind: Actor,
) -> Vec<String> {
    let known = KnownSources {
        org,
        site_themes: own.0,
        site_packages: own.1,
    };
    let mut found = policy_problems(look, &org.policy, &known, new.0, new.1, kind);
    for problem in image_policy_problems(look, &org.policy, &org.images, kind) {
        if !found.contains(&problem) {
            found.push(problem);
        }
    }
    found
}

/// Refuses with `409 not_allowed {problems}` a look `actor` may not set under the brand policy
/// of `org` on a site whose own themes and font packages are `record`'s (see [`problems`]);
/// administrators and brand managers are never refused.
async fn allow_look(
    state: &AppState,
    actor: &User,
    org: &OrgBrand,
    look: &Look,
    record: &SiteRecord,
    new: (&[SiteTheme], &[FontPackage]),
) -> Result<(), BrandError> {
    let kind = actor_kind(state.db(), actor).await?;
    let own = (record.themes.as_slice(), record.font_packages.as_slice());
    let found = problems(look, org, own, new, kind);
    if found.is_empty() {
        Ok(())
    } else {
        Err(BrandError::NotAllowed(found))
    }
}

/// Refuses with `409 not_allowed {problems}` site-only themes or font packages that `actor`
/// makes or changes (`new`) on a site whose records become `after`, when the brand policy does
/// not let them: only the problems these records add, judged against the default look, so a
/// look already stored never stands in the way.
async fn allow_records(
    state: &AppState,
    actor: &User,
    org: &OrgBrand,
    after: &SiteRecord,
    new: (&[SiteTheme], &[FontPackage]),
) -> Result<(), BrandError> {
    if new.0.is_empty() && new.1.is_empty() {
        return Ok(());
    }
    let kind = actor_kind(state.db(), actor).await?;
    let neutral = Look::default();
    let own = (after.themes.as_slice(), after.font_packages.as_slice());
    let without = problems(&neutral, org, own, (&[], &[]), kind);
    let added: Vec<String> = problems(&neutral, org, own, new, kind)
        .into_iter()
        .filter(|p| !without.contains(p))
        .collect();
    if added.is_empty() {
        Ok(())
    } else {
        Err(BrandError::NotAllowed(added))
    }
}

/// The look `input` asks for, for `actor` on `site` (named `site_name`), whose look is now
/// `current` and whose own themes and font packages are `record`'s: its theme and fonts
/// copied and checked, its images resolved (an icon checked as one, `not_an_icon`), its text
/// trimmed, and every rule of [`Look::problem`] checked.
async fn built_look(
    ctx: &SlotContext<'_>,
    site_name: &str,
    record: &SiteRecord,
    input: LookInput,
) -> Result<Look, BrandError> {
    let defaults = Look::default();
    let (theme, from) = match input.theme {
        Some(choice) => chosen_theme(choice, ctx.org, &record.themes)?,
        None => (defaults.theme.clone(), defaults.from.clone()),
    };
    let fonts = match input.fonts {
        Some(FontsChoice {
            from: Some(from),
            keep: false,
        }) => Some(LookFonts {
            package: source_package(&from, ctx.org, &record.font_packages)?,
            from,
        }),
        Some(FontsChoice {
            from: None,
            keep: true,
        }) => record.look.as_ref().and_then(|l| l.fonts.clone()),
        Some(_) => {
            return Err(BrandError::InvalidLook(
                "fonts are given as {from} or {keep: true}".into(),
            ));
        }
        None => None,
    };
    let icon = slot_image(ctx, input.icon, ImageKind::Icon).await?;
    if icon.as_ref().and_then(icon_problem).is_some() {
        return Err(ApiError::NotAnIcon.into());
    }
    let header_in = input.header.unwrap_or_default();
    let footer_in = input.footer.unwrap_or_default();
    let header_default = Header::default();
    let footer_default = revisions::brand::Footer::default();
    let mut groups = Vec::new();
    for group in footer_in.groups.unwrap_or_default() {
        let mut links = Vec::new();
        for link in group.links {
            links.push(revisions::brand::FooterLink {
                label: cleaned(&link.label)?,
                url: cleaned(&link.url)?,
            });
        }
        groups.push(FooterGroup {
            heading: cleaned(&group.heading)?,
            links,
        });
    }
    let look = Look {
        theme,
        from,
        logo: slot_image(ctx, input.logo, ImageKind::Logo).await?,
        thumbnail: slot_image(ctx, input.thumbnail, ImageKind::Logo).await?,
        logo_alt: cleaned(input.logo_alt.as_deref().unwrap_or_default())?,
        header: Header {
            layout: header_in.layout.unwrap_or(header_default.layout),
            background: header_in.background.unwrap_or(header_default.background),
            image: slot_image(ctx, header_in.image, ImageKind::Background).await?,
            align: header_in.align.unwrap_or(header_default.align),
            hide_title: header_in.hide_title.unwrap_or(header_default.hide_title),
        },
        navigation: input.navigation.unwrap_or_default(),
        footer: revisions::brand::Footer {
            enabled: footer_in.enabled.unwrap_or(footer_default.enabled),
            layout: footer_in.layout.unwrap_or(footer_default.layout),
            logo: slot_image(ctx, footer_in.logo, ImageKind::Logo).await?,
            name: cleaned(footer_in.name.as_deref().unwrap_or_default())?,
            background: footer_in.background.unwrap_or(footer_default.background),
            groups,
        },
        fonts,
        icon,
        menu: input.menu.unwrap_or_default(),
    };
    if let Some(problem) = look.problem(site_name) {
        return Err(BrandError::InvalidLook(problem));
    }
    Ok(look)
}

// ---------------------------------------------------------------------------------------------
// Changing a look

/// The site `site` for `actor` to change: live, seen, and `actor` holding `manage_site` on it
/// (`403 archived` on an archived site). Answers its name.
async fn managed(state: &AppState, actor: &User, site: NodeId) -> Result<String, BrandError> {
    let (node, access) = seen_site(state, actor, site).await?;
    require(&access, Perms::MANAGE_SITE)?;
    Ok(node.name)
}

/// The site `site` for `actor` to read the settings of: live, seen, and `actor` managing it,
/// archived or not. Answers its name.
async fn readable(state: &AppState, actor: &User, site: NodeId) -> Result<String, BrandError> {
    let (node, access) = seen_site(state, actor, site).await?;
    if !access.ignoring_archive().contains(Perms::MANAGE_SITE) {
        return Err(BrandError::from(access.refusal()));
    }
    Ok(node.name)
}

/// Erases the content `before` pinned (its look's images and fonts, its own packages' fonts)
/// that `after` does not, unless something else names it. Failures are logged; the sweeper
/// erases what is left.
async fn erase_unused(state: &AppState, before: &SiteRecord, after: &SiteRecord) {
    let kept: HashSet<&str> = site_pins(after.look.as_ref(), &after.font_packages)
        .into_iter()
        .map(|pin| pin.sha256)
        .collect();
    for pin in site_pins(before.look.as_ref(), &before.font_packages) {
        if kept.contains(pin.sha256) {
            continue;
        }
        let Some(hash) = revisions::parse_hex(pin.sha256) else {
            continue;
        };
        if let Err(e) = state.blobs().erase_if_unreferenced(state.db(), &hash).await {
            eprintln!("sharecraft-server: cannot erase a look's image or font: {e}");
        }
    }
}

/// Writes `look` as the look of `site` (named `name`) for `actor`, whose record Git holds as
/// `start.0` (`None` if it has none; `start.1` is where the change starts), with
/// `site.look_changed {name, before, after}` and the fields `extra` (`rollback`, the entry it
/// goes back to; `applied_by_admin`). The same look again writes nothing. The caller holds the
/// tree lock.
async fn commit_look(
    state: &AppState,
    actor: &User,
    site: NodeId,
    name: &str,
    start: (Option<SiteRecord>, SiteRecord),
    look: Option<Look>,
    extra: Extra,
) -> Result<(), BrandError> {
    let (stored, before) = start;
    if before.look == look {
        return Ok(());
    }
    let mut after = before.clone();
    after.look = look;
    let detail = with_extra(
        json!({ "name": name, "before": before.look, "after": after.look }),
        extra,
    );
    commit_sites(
        state,
        actor,
        SitesCommit {
            message: format!("Change the look of {name}"),
            changed: vec![(stored, after.clone())],
            needs_admin: false,
            needs: vec![(site, Perms::MANAGE_SITE)],
            also_lock: Vec::new(),
            release_hub: None,
            events: vec![site_event("site.look_changed", actor, site, detail)],
        },
    )
    .await?;
    erase_unused(state, &before, &after).await;
    Ok(())
}

/// Fields added to a `site.look_changed` event's detail.
pub(crate) type Extra = Vec<(&'static str, Value)>;

/// `detail` (an object) with the fields `extra`.
fn with_extra(mut detail: Value, extra: Extra) -> Value {
    if let Some(map) = detail.as_object_mut() {
        for (key, value) in extra {
            map.insert(key.to_owned(), value);
        }
    }
    detail
}

/// Sets the look of `site` to `input` for `actor`, who needs `manage_site` on it (see
/// [`built_look`] for what is checked). Answers the look settings (see [`settings`]).
///
/// # Errors
/// `404`; `403` (`archived` on an archived site); `422 invalid_theme`, `low_contrast` or
/// `invalid_look`; the image errors of [`slot_image`]; `409 image_gone` when an image's content
/// was erased meanwhile; otherwise `503`.
pub(crate) async fn set_look(
    state: &AppState,
    actor: &User,
    site: NodeId,
    input: LookInput,
) -> Result<Value, BrandError> {
    change_look(state, actor, site, input, Vec::new()).await?;
    settings_answer(state, actor, site).await
}

/// Sets the look of `site` to `input` for `actor` as [`set_look`] does, with the fields `extra`
/// in its event's detail.
///
/// # Errors
/// As [`set_look`].
pub(crate) async fn change_look(
    state: &AppState,
    actor: &User,
    site: NodeId,
    input: LookInput,
    extra: Extra,
) -> Result<(), BrandError> {
    let _turn = state.tree_lock().lock().await;
    ensure_in_step(state).await?;
    let name = managed(state, actor, site).await?;
    let (stored, before) = record_and_start(state, site).await?;
    let org = state.db().org_brand().await?;
    let ctx = SlotContext {
        state,
        actor,
        site,
        current: before.look.iter().flat_map(slots).map(|(_, i)| i).collect(),
        history: history_images(state.db(), site).await?,
        org: &org,
    };
    history_keeps_available(&ctx, &input).await?;
    let kept_fonts = matches!(
        input.fonts,
        Some(FontsChoice {
            from: None,
            keep: true
        })
    );
    let look = built_look(&ctx, &name, &before, input).await?;
    allow_look(
        state,
        actor,
        &org,
        &judged(&look, &before, kept_fonts),
        &before,
        (&[], &[]),
    )
    .await?;
    commit_look(
        state,
        actor,
        site,
        &name,
        (stored.clone(), before.clone()),
        Some(look),
        extra,
    )
    .await
}

/// `look` as the brand policy judges it, for a site whose record is `before`: fonts kept as
/// they are (`kept_fonts`, a `{keep: true}`) that equal the copy the site holds now are the
/// look already stored, so they are left out and not judged against their source again (a
/// change of the source, or of the policy, applies to the next choice); anything else is
/// judged as it is.
fn judged(look: &Look, before: &SiteRecord, kept_fonts: bool) -> Look {
    let stored = before.look.as_ref().and_then(|l| l.fonts.as_ref());
    let mut out = look.clone();
    if kept_fonts && look.fonts.is_some() && look.fonts.as_ref() == stored {
        out.fonts = None;
    }
    out
}

/// Applies a brand-centre theme `theme` and font package `fonts` (both as `from`; no package
/// for the system fonts) to `site` for `actor`, who needs `manage_site` on it, with the fields
/// `extra` in its event's detail. Only the theme and fonts change: the site's logo, thumbnail,
/// tab icon, header, navigation, menu and footer stay as they are, so its history records only
/// those two parts as changed. Checked as any look is.
///
/// # Errors
/// `422 invalid_theme` or `low_contrast` for the theme, `invalid_look` for an unknown package
/// or a look that no longer passes; otherwise as [`set_look`].
pub(crate) async fn apply_brand(
    state: &AppState,
    actor: &User,
    site: NodeId,
    (theme, fonts): (&str, Option<&str>),
    extra: Extra,
) -> Result<(), BrandError> {
    let _turn = state.tree_lock().lock().await;
    ensure_in_step(state).await?;
    let name = managed(state, actor, site).await?;
    let (stored, before) = record_and_start(state, site).await?;
    let org = state.db().org_brand().await?;
    let choice = ThemeChoice {
        from: Some(theme.to_owned()),
        ..ThemeChoice::default()
    };
    let (theme, from) = chosen_theme(choice, &org, &before.themes)?;
    let fonts = match fonts {
        Some(fonts) => Some(LookFonts {
            package: source_package(fonts, &org, &before.font_packages)?,
            from: fonts.to_owned(),
        }),
        None => None,
    };
    let mut look = before.look.clone().unwrap_or_default();
    look.theme = theme;
    look.from = from;
    look.fonts = fonts;
    if let Some(problem) = look.problem(&name) {
        return Err(BrandError::InvalidLook(problem));
    }
    allow_look(state, actor, &org, &look, &before, (&[], &[])).await?;
    commit_look(
        state,
        actor,
        site,
        &name,
        (stored, before),
        Some(look),
        extra,
    )
    .await
}

/// Applies the look before (`Which::Before`) or after a change in the history of `site` as a
/// new change for `actor`, who needs `manage_site` on it, checked again as any look is.
/// Answers the look settings.
///
/// # Errors
/// `404` for no such entry of this site; `422 low_contrast` or `invalid_look` for a look that
/// no longer passes; `409 image_gone {slots}` when the content of its images is gone or held;
/// otherwise as [`set_look`].
pub(crate) async fn rollback(
    state: &AppState,
    actor: &User,
    site: NodeId,
    event_id: i64,
    which: Which,
) -> Result<Value, BrandError> {
    {
        let _turn = state.tree_lock().lock().await;
        ensure_in_step(state).await?;
        let name = managed(state, actor, site).await?;
        let db = state.db();
        let entry = db
            .look_change(site, event_id)
            .await?
            .ok_or(ApiError::NotFound)?;
        let value = match which {
            Which::Before => entry.before,
            Which::After => entry.after,
        };
        let look: Option<Look> =
            if value.is_null() {
                None
            } else {
                Some(serde_json::from_value(value).map_err(|_| {
                    BrandError::InvalidLook("this entry's look cannot be read".into())
                })?)
            };
        if let Some(look) = &look {
            check_theme(&look.theme).map_err(BrandError::LowContrast)?;
            if let Some(problem) = look.problem(&name) {
                return Err(BrandError::InvalidLook(problem));
            }
            // Content no longer stored, or held (infected or awaiting the scanner) after a
            // rescan, is never pinned again, as a `keep` of it is refused.
            let gone = gone_contents(db, &look_contents(look)).await?;
            if !gone.is_empty() {
                return Err(BrandError::ImagesGone(gone));
            }
        }
        let (stored, before) = record_and_start(state, site).await?;
        if let Some(look) = &look {
            let org = db.org_brand().await?;
            allow_look(state, actor, &org, look, &before, (&[], &[])).await?;
        }
        commit_look(
            state,
            actor,
            site,
            &name,
            (stored, before),
            look,
            vec![("rollback", json!(event_id))],
        )
        .await?;
    }
    settings_answer(state, actor, site).await
}

/// The site's settings as PostgreSQL mirrors them, or a record-less site's defaults.
async fn site_settings(db: &Db, site: NodeId) -> Result<SiteSettings, BrandError> {
    match db.site_settings(site).await? {
        Some(settings) => Ok(settings),
        None => {
            let defaults = db.regional_defaults().await?;
            Ok(SiteSettings::defaults(
                site,
                &defaults.language,
                &defaults.time_zone,
            ))
        }
    }
}

/// The hub whose theme `settings`' site takes: the hub it joined, if that is a hub that is not
/// deleted. With its settings.
async fn lending_hub(
    db: &Db,
    settings: &SiteSettings,
) -> Result<Option<(NodeId, SiteSettings)>, BrandError> {
    let Some(hub) = settings.joined_hub else {
        return Ok(None);
    };
    if db.site_state(hub).await? != Some(SiteState::Live) {
        return Ok(None);
    }
    Ok(db
        .site_settings(hub)
        .await?
        .filter(|s| s.is_hub)
        .map(|s| (hub, s)))
}

/// A site's own theme as listed.
fn site_theme_out(theme: &SiteTheme) -> Value {
    let id = theme.id.to_string();
    theme_out(
        &id,
        &format!("site:{id}"),
        &theme.theme(),
        Some(theme.changed_at),
    )
}

/// A site's own font package as listed.
fn site_package_out(package: &FontPackage) -> Value {
    let id = package.id.to_string();
    package_out(package, &format!("site:{id}"), &id, false)
}

/// The look settings of `site` for `actor`: `{look, themes, font_packages,
/// theme_source_changed, fonts_source_changed, in_hub?}`. `fonts_source_changed` is whether the
/// look keeps a copy of a font package that has changed at its source since, or is gone.
/// `in_hub` is `{id, name}` for a hub `actor` can see, as the effective look and `/hubs` name
/// it, and `{hidden: true}` for one they cannot.
async fn settings_answer(
    state: &AppState,
    actor: &User,
    site: NodeId,
) -> Result<Value, BrandError> {
    let db = state.db();
    let settings = site_settings(db, site).await?;
    let org = db.org_brand().await?;
    let look = settings.look.clone().unwrap_or_default();
    let mut out = json!({
        "look": look,
        "themes": settings.themes.iter().map(site_theme_out).collect::<Vec<_>>(),
        "font_packages": settings
            .font_packages
            .iter()
            .map(site_package_out)
            .collect::<Vec<_>>(),
        "theme_source_changed": source_changed(&look, &org, &settings.themes),
        "fonts_source_changed": !fonts_match_source(
            &look,
            &KnownSources {
                org: &org,
                site_themes: &settings.themes,
                site_packages: &settings.font_packages,
            },
        ),
    });
    if let Some((hub, _)) = lending_hub(db, &settings).await? {
        let named = if db.path(actor.id, hub).await?.is_some() {
            db.node(hub).await?
        } else {
            None
        };
        let in_hub = match named {
            Some(node) => json!({ "id": hub.0.to_string(), "name": node.name }),
            None => json!({ "hidden": true }),
        };
        if let Some(map) = out.as_object_mut() {
            map.insert("in_hub".into(), in_hub);
        }
    }
    Ok(out)
}

/// `GET /api/sites/{id}/look/settings` for `actor`, who manages the site (archived or not).
///
/// # Errors
/// `404` unless a live site they can see; `403` unless they manage it; otherwise `503`.
pub(crate) async fn settings(
    state: &AppState,
    actor: &User,
    site: NodeId,
) -> Result<Value, BrandError> {
    readable(state, actor, site).await?;
    settings_answer(state, actor, site).await
}

/// The last [`HISTORY_LENGTH`] changes of the look of `site`, newest first, for `actor`, who
/// manages it (archived or not): each `{event_id, at, by: {id, name} | null, look, before}`, `look`
/// being what the change made it and `before` what it was (`null` for none).
///
/// # Errors
/// As [`settings`].
pub(crate) async fn history(
    state: &AppState,
    actor: &User,
    site: NodeId,
) -> Result<Value, BrandError> {
    readable(state, actor, site).await?;
    let changes = state.db().look_changes(site, HISTORY_LENGTH).await?;
    let entry = |change: LookChange| {
        json!({
            "event_id": change.event_id,
            "at": change.at.format(&Rfc3339).ok(),
            "by": change.by.map(|(id, name)| json!({ "id": id.0, "name": name })),
            "look": change.after,
            "before": change.before,
        })
    };
    Ok(Value::Array(changes.into_iter().map(entry).collect()))
}

// ---------------------------------------------------------------------------------------------
// The effective look

/// The address of `image`, if any.
fn url_of(image: Option<&ImageRef>) -> Option<String> {
    image.map(|i| image_url(&i.sha256))
}

/// The fonts a page uses for `look` (the system fonts for none): `{title, heading, body,
/// interactive}`, each `{family, url?}`. The family of a font file is generated from its hash
/// (see [`family_name`]) and comes with the address it is served at; otherwise it is `system`
/// or `system-serif`, which a page turns into its fixed stacks.
fn fonts_out(look: Option<&Look>) -> Value {
    let package = look.and_then(|l| l.fonts.as_ref()).map(|f| &f.package);
    let role = |role: FontRole| {
        let file = match role {
            FontRole::Display => package.and_then(|p| p.display.as_ref()),
            FontRole::Content => package.and_then(|p| p.content.as_ref()),
            FontRole::System => None,
            FontRole::SystemSerif => return json!({ "family": "system-serif" }),
        };
        match file {
            Some(font) => json!({
                "family": family_name(&font.sha256),
                "url": font_url(&font.sha256),
            }),
            None => json!({ "family": "system" }),
        }
    };
    match package {
        Some(p) => json!({
            "title": role(p.roles.title),
            "heading": role(p.roles.heading),
            "body": role(p.roles.body),
            "interactive": role(p.roles.interactive),
        }),
        None => json!({
            "title": role(FontRole::System),
            "heading": role(FontRole::System),
            "body": role(FontRole::System),
            "interactive": role(FontRole::System),
        }),
    }
}

/// A site's mark in a list: its thumbnail's address (from its own look) and its light accent
/// (from the hub whose theme it takes, `hub`, if any).
pub(crate) fn list_marks(
    own: &SiteSettings,
    hub: Option<&SiteSettings>,
) -> (Option<String>, String) {
    let themed = hub.unwrap_or(own);
    let accent = palettes(themed.look.as_ref())
        .map(|p| p.light.accent.to_string())
        .unwrap_or_default();
    (
        url_of(own.look.as_ref().and_then(|l| l.thumbnail.as_ref())),
        accent,
    )
}

/// The hub of `own` that lends it its theme: the one it joined, among `all`, if that is a hub
/// not in `deleted`.
pub(crate) fn lending_hub_in<'a>(
    own: &SiteSettings,
    all: &'a std::collections::HashMap<NodeId, SiteSettings>,
    deleted: &HashSet<NodeId>,
) -> Option<&'a SiteSettings> {
    let hub = own.joined_hub?;
    if deleted.contains(&hub) {
        return None;
    }
    all.get(&hub).filter(|s| s.is_hub)
}

/// The effective look of `site` for `actor`, who can see it (access to it, or somewhere inside
/// it): `{theme: {light, dark}, fonts: {title, heading, body, interactive}, logo_url?,
/// thumbnail_url?, icon_url, logo_alt, header, navigation, navigation_menu, footer, theme_from:
/// "site" | "hub", hub?: {id, name, thumbnail_url?}, own?: {theme, fonts}}`. `own`, given only
/// when the theme and fonts come from a hub, is the site's own theme and fonts, resolved as
/// `theme` and `fonts` are: what copying the site's look writes. The theme and fonts come from the hub
/// the site joined, if any (see the module documentation), everything else from the site; `hub`
/// is named only to people who can see it. An empty logo description is the site's name. The
/// tab icon is the look's icon, else its thumbnail, else the organisation's default icon; `null`
/// means ShareCraft's own.
///
/// # Errors
/// `404` unless `site` is a site that is not deleted and `actor` can see; otherwise `503`.
pub(crate) async fn effective(
    state: &AppState,
    actor: &User,
    site: NodeId,
) -> Result<Value, BrandError> {
    let db = state.db();
    let node = db
        .node(site)
        .await?
        .filter(|n| n.kind == records::NodeKind::Site && !n.deleted)
        .ok_or(ApiError::NotFound)?;
    if db.path(actor.id, site).await?.is_none() {
        return Err(ApiError::NotFound.into());
    }
    let own = site_settings(db, site).await?;
    let hub = lending_hub(db, &own).await?;
    let look = own.look.clone().unwrap_or_default();
    let themed = hub.as_ref().map_or(&own, |(_, s)| s);
    let header = &look.header;
    let mut header_out = json!({
        "layout": header.layout,
        "background": header.background,
        "align": header.align,
        "hide_title": header.hide_title,
    });
    let footer = &look.footer;
    let mut footer_out = json!({
        "enabled": footer.enabled,
        "layout": footer.layout,
        "name": footer.name,
        "background": footer.background,
        "groups": footer.groups,
    });
    let logo_alt = if look.logo_alt.is_empty() {
        node.name.clone()
    } else {
        look.logo_alt.clone()
    };
    let org = db.org_brand().await?;
    let icon_url =
        url_of(look.icon.as_ref().or(look.thumbnail.as_ref())).or_else(|| default_icon_url(&org));
    let mut out = json!({
        "theme": palettes(themed.look.as_ref()),
        "fonts": fonts_out(themed.look.as_ref()),
        "logo_alt": logo_alt,
        "navigation": look.navigation,
        "navigation_menu": look.menu,
        "icon_url": icon_url,
        "theme_from": if hub.is_some() { "hub" } else { "site" },
    });
    let put = |value: &mut Value, key: &str, url: Option<String>| {
        if let (Some(url), Some(map)) = (url, value.as_object_mut()) {
            map.insert(key.to_owned(), json!(url));
        }
    };
    put(&mut header_out, "image_url", url_of(header.image.as_ref()));
    put(&mut footer_out, "logo_url", url_of(footer.logo.as_ref()));
    put(&mut out, "logo_url", url_of(look.logo.as_ref()));
    put(&mut out, "thumbnail_url", url_of(look.thumbnail.as_ref()));
    if let Some(map) = out.as_object_mut() {
        map.insert("header".into(), header_out);
        map.insert("footer".into(), footer_out);
        // A site in a hub shows the hub's theme and fonts; its own (what copying it writes)
        // are given too, resolved the same way, so a preview of a copy shows what it gets.
        if hub.is_some() {
            map.insert(
                "own".into(),
                json!({
                    "theme": palettes(own.look.as_ref()),
                    "fonts": fonts_out(own.look.as_ref()),
                }),
            );
        }
    }
    if let Some((hub, settings)) = &hub
        && db.path(actor.id, *hub).await?.is_some()
        && let Some(hub_node) = db.node(*hub).await?
    {
        let mut named = json!({ "id": hub.0.to_string(), "name": hub_node.name });
        put(
            &mut named,
            "thumbnail_url",
            url_of(settings.look.as_ref().and_then(|l| l.thumbnail.as_ref())),
        );
        if let Some(map) = out.as_object_mut() {
            map.insert("hub".into(), named);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// A site's own themes

/// What one change of a site's themes does: the commit message, the event detail and the
/// answer.
struct ThemesEdited<T> {
    message: String,
    detail: Value,
    answer: T,
}

/// Whether another theme than `except` among `themes` is named `name`, ignoring case.
fn name_taken(themes: &[SiteTheme], name: &str, except: Option<Uuid>) -> bool {
    let wanted = name.to_lowercase();
    themes
        .iter()
        .any(|t| Some(t.id) != except && t.name.to_lowercase() == wanted)
}

/// Changes the own themes of `site` for `actor`, who needs `manage_site` on it, with `edit`,
/// under the tree lock: one sites commit with `site.theme_changed`, taken back if refused. An
/// edit that changes nothing writes nothing. A look that copied a theme keeps its copy.
async fn change_themes<T>(
    state: &AppState,
    actor: &User,
    site: NodeId,
    edit: impl FnOnce(&mut Vec<SiteTheme>) -> Result<ThemesEdited<T>, BrandError>,
) -> Result<T, BrandError> {
    let _turn = state.tree_lock().lock().await;
    ensure_in_step(state).await?;
    let name = managed(state, actor, site).await?;
    let (stored, before) = record_and_start(state, site).await?;
    let mut after = before.clone();
    let ThemesEdited {
        message,
        mut detail,
        answer,
    } = edit(&mut after.themes)?;
    if after.themes == before.themes {
        return Ok(answer);
    }
    let made: Vec<SiteTheme> = after
        .themes
        .iter()
        .filter(|t| !before.themes.contains(t))
        .cloned()
        .collect();
    let org = state.db().org_brand().await?;
    allow_records(state, actor, &org, &after, (&made, &[])).await?;
    if let Some(map) = detail.as_object_mut() {
        map.insert("name".into(), json!(name));
    }
    commit_sites(
        state,
        actor,
        SitesCommit {
            message: format!("{message} of {name}"),
            changed: vec![(stored, after)],
            needs_admin: false,
            needs: vec![(site, Perms::MANAGE_SITE)],
            also_lock: Vec::new(),
            release_hub: None,
            events: vec![site_event("site.theme_changed", actor, site, detail)],
        },
    )
    .await?;
    Ok(answer)
}

/// Adds the theme `input` to the own themes of `site` for `actor`, with `site.theme_changed`
/// (`created`). Answers the theme as listed.
///
/// # Errors
/// `404`; `403` (`archived`); `422 invalid_theme` / `low_contrast`; `409 name_taken` for a name
/// another of the site's themes has, ignoring case; `409 too_many {limit: 20}`; otherwise `503`.
pub(crate) async fn create_theme(
    state: &AppState,
    actor: &User,
    site: NodeId,
    input: ThemeInput,
) -> Result<Value, BrandError> {
    change_themes(state, actor, site, |themes| {
        let theme = checked_theme(&input)?;
        if name_taken(themes, &theme.name, None) {
            return Err(ApiError::NameTaken.into());
        }
        if themes.len() >= MAX_SITE_THEMES {
            return Err(ApiError::TooMany(MAX_SITE_THEMES).into());
        }
        let created = org_theme(Uuid::new_v4(), theme);
        themes.push(created.clone());
        Ok(ThemesEdited {
            message: format!("Add the theme {}", created.name),
            detail: json!({ "action": "created", "theme": created }),
            answer: site_theme_out(&created),
        })
    })
    .await
}

/// Changes the own theme `id` of `site` for `actor` to what `edit` makes of it, with
/// `site.theme_changed` (`updated`, before and after). The same theme again changes nothing.
/// Answers the theme as listed.
///
/// # Errors
/// As [`create_theme`], with `404` for no such theme of this site.
pub(crate) async fn change_theme(
    state: &AppState,
    actor: &User,
    site: NodeId,
    id: Uuid,
    edit: impl FnOnce(&Theme) -> ThemeInput,
) -> Result<Value, BrandError> {
    change_themes(state, actor, site, |themes| {
        let at = themes
            .iter()
            .position(|t| t.id == id)
            .ok_or(ApiError::NotFound)?;
        let old = themes.get(at).cloned().ok_or(ApiError::NotFound)?;
        let theme = checked_theme(&edit(&old.theme()))?;
        if old.theme() == theme {
            return Ok(ThemesEdited {
                message: String::new(),
                detail: Value::Null,
                answer: site_theme_out(&old),
            });
        }
        if name_taken(themes, &theme.name, Some(id)) {
            return Err(ApiError::NameTaken.into());
        }
        let changed = org_theme(id, theme);
        if let Some(slot) = themes.get_mut(at) {
            *slot = changed.clone();
        }
        Ok(ThemesEdited {
            message: format!("Change the theme {}", old.name),
            detail: json!({ "action": "updated", "before": old, "theme": changed }),
            answer: site_theme_out(&changed),
        })
    })
    .await
}

/// Deletes the own theme `id` of `site` for `actor`, with `site.theme_changed` (`deleted`).
///
/// # Errors
/// `404` for no such theme of this site; otherwise as [`create_theme`].
pub(crate) async fn delete_theme(
    state: &AppState,
    actor: &User,
    site: NodeId,
    id: Uuid,
) -> Result<(), BrandError> {
    change_themes(state, actor, site, |themes| {
        let at = themes
            .iter()
            .position(|t| t.id == id)
            .ok_or(ApiError::NotFound)?;
        let old = themes.remove(at);
        Ok(ThemesEdited {
            message: format!("Delete the theme {}", old.name),
            detail: json!({ "action": "deleted", "theme": old }),
            answer: (),
        })
    })
    .await
}

// ---------------------------------------------------------------------------------------------
// A site's own font packages

/// Changes the own font packages of `site` for `actor`, who needs `manage_site` on it, under
/// the tree lock: `edit`'s fonts are found first (a document from the site's own libraries or
/// an asset library, its licence confirmed; a brand-centre font), then `apply` changes the
/// packages, a package made or changed is held to the brand policy, and one sites commit
/// records it with `site.font_package_changed`, taken back if refused. An edit that changes
/// nothing writes nothing. A look that copied a package keeps its copy; font content nothing
/// uses any more is erased.
async fn change_packages<T>(
    state: &AppState,
    actor: &User,
    site: NodeId,
    edit: Option<PackageEdit>,
    apply: impl FnOnce(
        &mut Vec<FontPackage>,
        Option<crate::brand::ResolvedEdit>,
    ) -> Result<ThemesEdited<T>, BrandError>,
) -> Result<T, BrandError> {
    let _turn = state.tree_lock().lock().await;
    ensure_in_step(state).await?;
    let name = managed(state, actor, site).await?;
    let org = state.db().org_brand().await?;
    let resolved = match edit {
        Some(edit) => Some(resolve_edit(state, actor, &org, Some(site), edit).await?),
        None => None,
    };
    let (stored, before) = record_and_start(state, site).await?;
    let mut after = before.clone();
    let ThemesEdited {
        message,
        mut detail,
        answer,
    } = apply(&mut after.font_packages, resolved)?;
    if after.font_packages == before.font_packages {
        return Ok(answer);
    }
    let made: Vec<FontPackage> = after
        .font_packages
        .iter()
        .filter(|p| !before.font_packages.contains(p))
        .cloned()
        .collect();
    allow_records(state, actor, &org, &after, (&[], &made)).await?;
    if let Some(map) = detail.as_object_mut() {
        map.insert("name".into(), json!(name));
    }
    commit_sites(
        state,
        actor,
        SitesCommit {
            message: format!("{message} of {name}"),
            changed: vec![(stored, after.clone())],
            needs_admin: false,
            needs: vec![(site, Perms::MANAGE_SITE)],
            also_lock: Vec::new(),
            release_hub: None,
            events: vec![site_event("site.font_package_changed", actor, site, detail)],
        },
    )
    .await?;
    erase_unused(state, &before, &after).await;
    Ok(answer)
}

/// Adds a font package only `site` offers for `actor`, with `site.font_package_changed`
/// (`created`). Answers the package as listed.
///
/// # Errors
/// `404`; `403` (`archived`); `422 licence_not_confirmed` and the other errors of choosing a
/// font; `422 invalid_font_package {reason}`; `409 name_taken`; `409 too_many {limit: 10}`;
/// `409 not_allowed {problems}`; otherwise `503`.
pub(crate) async fn create_package(
    state: &AppState,
    actor: &User,
    site: NodeId,
    edit: PackageEdit,
) -> Result<Value, BrandError> {
    change_packages(state, actor, site, Some(edit), |packages, resolved| {
        let resolved = resolved.ok_or(ApiError::InvalidRequest)?;
        let package = edited_package(blank_package(Uuid::new_v4()), resolved)?;
        if package_name_taken(packages, &package.name, None) {
            return Err(ApiError::NameTaken.into());
        }
        if packages.len() >= MAX_SITE_FONT_PACKAGES {
            return Err(ApiError::TooMany(MAX_SITE_FONT_PACKAGES).into());
        }
        packages.push(package.clone());
        Ok(ThemesEdited {
            message: format!("Add the font package {}", package.name),
            detail: json!({ "action": "created", "package": package }),
            answer: site_package_out(&package),
        })
    })
    .await
}

/// Changes the own font package `id` of `site` for `actor` with `edit`, with
/// `site.font_package_changed` (`updated`, before and after). Answers the package as listed.
///
/// # Errors
/// As [`create_package`], with `404` for no such package of this site.
pub(crate) async fn change_package(
    state: &AppState,
    actor: &User,
    site: NodeId,
    id: Uuid,
    edit: PackageEdit,
) -> Result<Value, BrandError> {
    change_packages(state, actor, site, Some(edit), |packages, resolved| {
        let resolved = resolved.ok_or(ApiError::InvalidRequest)?;
        let at = packages
            .iter()
            .position(|p| p.id == id)
            .ok_or(ApiError::NotFound)?;
        let old = packages.get(at).cloned().ok_or(ApiError::NotFound)?;
        let changed = edited_package(old.clone(), resolved)?;
        if package_name_taken(packages, &changed.name, Some(id)) {
            return Err(ApiError::NameTaken.into());
        }
        if let Some(slot) = packages.get_mut(at) {
            *slot = changed.clone();
        }
        Ok(ThemesEdited {
            message: format!("Change the font package {}", old.name),
            detail: json!({ "action": "updated", "before": old, "package": changed }),
            answer: site_package_out(&changed),
        })
    })
    .await
}

/// Deletes the own font package `id` of `site` for `actor`, with `site.font_package_changed`
/// (`deleted`). A look that copied it keeps its copy.
///
/// # Errors
/// `404` for no such package of this site; otherwise as [`create_package`].
pub(crate) async fn delete_package(
    state: &AppState,
    actor: &User,
    site: NodeId,
    id: Uuid,
) -> Result<(), BrandError> {
    change_packages(state, actor, site, None, |packages, _| {
        let at = packages
            .iter()
            .position(|p| p.id == id)
            .ok_or(ApiError::NotFound)?;
        let old = packages.remove(at);
        Ok(ThemesEdited {
            message: format!("Delete the font package {}", old.name),
            detail: json!({ "action": "deleted", "package": old }),
            answer: (),
        })
    })
    .await
}

// ---------------------------------------------------------------------------------------------
// Copying a look

/// Adds `copies` to `own` (a site's themes or font packages, by `id_of` and `name_of`): one
/// with the id of one `own` has replaces it; any other is added. Answers what was added or
/// changed.
///
/// # Errors
/// `409 name_taken` for a name another of `own` has, ignoring case; `409 too_many {limit}` past
/// `limit`.
fn merged<T: Clone + PartialEq>(
    own: &mut Vec<T>,
    copies: &[T],
    limit: usize,
    id_of: impl Fn(&T) -> Uuid,
    name_of: impl Fn(&T) -> &str,
) -> Result<Vec<T>, BrandError> {
    let mut made = Vec::new();
    for copy in copies {
        if let Some(slot) = own.iter_mut().find(|t| id_of(t) == id_of(copy)) {
            if slot != copy {
                slot.clone_from(copy);
                made.push(copy.clone());
            }
            continue;
        }
        let wanted = name_of(copy).to_lowercase();
        if own.iter().any(|t| name_of(t).to_lowercase() == wanted) {
            return Err(ApiError::NameTaken.into());
        }
        own.push(copy.clone());
        made.push(copy.clone());
    }
    if own.len() > limit {
        return Err(ApiError::TooMany(limit).into());
    }
    Ok(made)
}

/// Copies the look of the site `from` onto `site` for `actor`, who needs view on `from` and
/// `manage_site` on `site`: the look itself (its theme, images, fonts and the rest, as
/// snapshots pinned to `site`), and with `themes` and `fonts` the source's own themes and font
/// packages too. A copied theme from a site theme that is not copied becomes `custom`; fonts
/// from a package of the source site's own always bring that one package along (as a site package
/// of the target, held to the policy as site-only fonts), so the copy keeps its source. The
/// look is checked as any look is, and held to the brand policy; one sites commit records it
/// with `site.look_changed {name, before, after, copied_from}`. Answers the look settings.
///
/// # Errors
/// `404` for a target or source the caller cannot see; `403` (`archived` on an archived
/// target, or without view on the source); `422 low_contrast` / `invalid_look`; `409
/// image_gone {slots}` when content it names is gone or held; `409 name_taken` / `too_many`
/// for copied themes or packages; `409 not_allowed {problems}`; otherwise `503`.
pub(crate) async fn copy_look(
    state: &AppState,
    actor: &User,
    site: NodeId,
    from: NodeId,
    copy: (bool, bool),
) -> Result<Value, BrandError> {
    copy_into(state, actor, site, from, copy, Vec::new()).await?;
    settings_answer(state, actor, site).await
}

/// Copies the look of `from` onto `site` for `actor` as [`copy_look`] does, with the fields
/// `extra` in its event's detail.
///
/// # Errors
/// As [`copy_look`].
pub(crate) async fn copy_into(
    state: &AppState,
    actor: &User,
    site: NodeId,
    from: NodeId,
    copy: (bool, bool),
    extra: Extra,
) -> Result<(), BrandError> {
    let (themes, fonts) = copy;
    {
        let _turn = state.tree_lock().lock().await;
        ensure_in_step(state).await?;
        let name = managed(state, actor, site).await?;
        let (source_node, access) = seen_site(state, actor, from).await?;
        if !access.ignoring_archive().contains(Perms::VIEW) {
            return Err(ApiError::Forbidden.into());
        }
        let db = state.db();
        let (_, source) = record_and_start(state, from).await?;
        let (stored, before) = record_and_start(state, site).await?;
        let mut after = before.clone();
        let made_themes = if themes {
            merged(
                &mut after.themes,
                &source.themes,
                MAX_SITE_THEMES,
                |t| t.id,
                |t| &t.name,
            )?
        } else {
            Vec::new()
        };
        // The source's own packages when asked; otherwise still the one its look's fonts came
        // from, so the copy keeps a source in the target (and is held to the policy as
        // site-only fonts).
        let looks_package: Vec<FontPackage> = source
            .look
            .as_ref()
            .and_then(|l| l.fonts.as_ref())
            .and_then(|f| f.from.strip_prefix("site:"))
            .and_then(|id| Uuid::parse_str(id).ok())
            .and_then(|id| source.font_packages.iter().find(|p| p.id == id))
            .cloned()
            .into_iter()
            .collect();
        let brought: &[FontPackage] = if fonts {
            &source.font_packages
        } else {
            &looks_package
        };
        let made_packages = merged(
            &mut after.font_packages,
            brought,
            MAX_SITE_FONT_PACKAGES,
            |p| p.id,
            |p| &p.name,
        )?;
        let mut look = source.look.clone();
        if let Some(look) = &mut look {
            if look.from.starts_with("site:") && !themes {
                "custom".clone_into(&mut look.from);
            }
            check_theme(&look.theme).map_err(BrandError::LowContrast)?;
            if let Some(problem) = look.problem(&name) {
                return Err(BrandError::InvalidLook(problem));
            }
        }
        after.look = look;
        let mut contents = after.look.as_ref().map(look_contents).unwrap_or_default();
        for package in &made_packages {
            for font in package.display.iter().chain(package.content.iter()) {
                contents.push(("font_packages", font.sha256.as_str()));
            }
        }
        let gone = gone_contents(db, &contents).await?;
        if !gone.is_empty() {
            return Err(BrandError::ImagesGone(gone));
        }
        let org = db.org_brand().await?;
        let fallback = Look::default();
        allow_look(
            state,
            actor,
            &org,
            after.look.as_ref().unwrap_or(&fallback),
            &after,
            (&made_themes, &made_packages),
        )
        .await?;
        if after != before {
            let detail = with_extra(
                json!({
                    "name": name,
                    "before": before.look,
                    "after": after.look,
                    "copied_from": from.0,
                }),
                extra,
            );
            commit_sites(
                state,
                actor,
                SitesCommit {
                    message: format!("Copy the look of {} to {name}", source_node.name),
                    changed: vec![(stored, after.clone())],
                    needs_admin: false,
                    needs: vec![(site, Perms::MANAGE_SITE)],
                    also_lock: Vec::new(),
                    release_hub: None,
                    events: vec![site_event("site.look_changed", actor, site, detail)],
                },
            )
            .await?;
            erase_unused(state, &before, &after).await;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Looks from templates

/// The hashes among `hashes` whose content is no longer stored, or not available (held or
/// quarantined after a rescan): content that is never pinned again.
async fn unavailable(db: &Db, hashes: Vec<String>) -> Result<HashSet<String>, BrandError> {
    let missing = db.missing_blobs(&hashes).await?;
    let mut gone: HashSet<String> = missing.into_iter().collect();
    for sha in hashes {
        let held = match revisions::parse_hex(&sha) {
            Some(hash) => db.availability(&hash).await?.is_some(),
            None => true,
        };
        if held {
            gone.insert(sha);
        }
    }
    Ok(gone)
}

/// The content hashes of the brand-centre files `template` refers to that `org` has.
fn referenced_hashes(template: &TemplateLook, org: &OrgBrand) -> Vec<String> {
    let look = look_from_references(template, org);
    let mut out: Vec<String> = slots(&look)
        .into_iter()
        .map(|(_, image)| image.sha256.clone())
        .chain(
            font_slots(&look)
                .into_iter()
                .map(|(_, font)| font.sha256.clone()),
        )
        .collect();
    out.sort();
    out.dedup();
    out
}

/// The site look `template` makes for a site named `site_name` whose own font packages are
/// `own_packages`: each reference resolved from the brand centre `org` to a snapshot, and each
/// that names nothing usable (see [`usable_image`], [`usable_font`]) or content no longer
/// available left out and answered. Fonts from a site's own package come with that package (as
/// a copy of a look does): the site's own package equal to it, or else a new one, answered so
/// the caller adds it. The look is checked as any look is.
///
/// # Errors
/// `422 low_contrast` or `invalid_look`; otherwise `503`.
async fn resolve_template(
    db: &Db,
    org: &OrgBrand,
    template: &TemplateLook,
    own_packages: &[FontPackage],
    site_name: &str,
) -> Result<(Look, Option<FontPackage>, Vec<Unresolved>), BrandError> {
    let gone = unavailable(db, referenced_hashes(template, org)).await?;
    let mut pruned = template.clone();
    let unresolved = prune_references(
        &mut pruned,
        &|slot, id| usable_image(org, slot, id).is_some_and(|i| !gone.contains(&i.sha256)),
        &|id| usable_font(org, id).is_some_and(|f| !gone.contains(&f.sha256)),
    );
    let mut look = look_from_references(&pruned, org);
    let mut brought = None;
    if let Some(fonts) = &mut look.fonts
        && fonts.from.starts_with("site:")
    {
        let same = own_packages.iter().find(|p| {
            p.name == fonts.package.name
                && p.display == fonts.package.display
                && p.content == fonts.package.content
                && p.roles == fonts.package.roles
        });
        let id = match same {
            Some(package) => package.id,
            None => Uuid::new_v4(),
        };
        fonts.package.id = id;
        fonts.from = format!("site:{id}");
        if same.is_none() {
            brought = Some(fonts.package.clone());
        }
    }
    check_theme(&look.theme).map_err(BrandError::LowContrast)?;
    if let Some(problem) = look.problem(site_name) {
        return Err(BrandError::InvalidLook(problem));
    }
    Ok((look, brought, unresolved))
}

/// What the brand policy of `org` refuses `kind` in `look` as the look of a new site whose
/// only own records are the font packages `packages` it brings (see [`problems`]).
fn new_site_problems(
    look: &Look,
    org: &OrgBrand,
    packages: &[FontPackage],
    kind: Actor,
) -> Vec<String> {
    problems(look, org, (&[], packages), (&[], packages), kind)
}

/// The look a new site named `site_name` made by `actor` from a template with the look
/// `template` gets (see [`resolve_template`]), with the font package it brings. The brand
/// policy holds when the creator is not an administrator (today only administrators create
/// sites, so it is the rule for when others may).
///
/// # Errors
/// `422 low_contrast` or `invalid_look`; `409 not_allowed {problems}`; otherwise `503`.
pub(crate) async fn template_site_look(
    state: &AppState,
    actor: &User,
    template: &TemplateLook,
    site_name: &str,
) -> Result<NewSiteLook, BrandError> {
    let db = state.db();
    let org = db.org_brand().await?;
    let (look, brought, unresolved) = resolve_template(db, &org, template, &[], site_name).await?;
    let font_packages: Vec<FontPackage> = brought.into_iter().collect();
    let kind = actor_kind(db, actor).await?;
    let found = new_site_problems(&look, &org, &font_packages, kind);
    if !found.is_empty() {
        return Err(BrandError::NotAllowed(found));
    }
    Ok(NewSiteLook {
        look: Some(look),
        font_packages,
        unresolved,
    })
}

/// Applies the look `look` of the template `template` to `site` for `actor`, an administrator
/// applying looks to many sites: resolved as for a new site (see [`resolve_template`]), the
/// font package it brings added to the site's own, one sites commit with
/// `site.look_changed {name, before, after, template, applied_by_admin}`; the same look again
/// writes nothing. Answers the references left out.
///
/// # Errors
/// As [`set_look`]; `409 name_taken` / `too_many` for the package it brings.
pub(crate) async fn apply_template_look(
    state: &AppState,
    actor: &User,
    site: NodeId,
    template: &str,
    look: &TemplateLook,
) -> Result<Vec<Unresolved>, BrandError> {
    let _turn = state.tree_lock().lock().await;
    ensure_in_step(state).await?;
    let name = managed(state, actor, site).await?;
    let (stored, before) = record_and_start(state, site).await?;
    let db = state.db();
    let org = db.org_brand().await?;
    let (resolved, brought, unresolved) =
        resolve_template(db, &org, look, &before.font_packages, &name).await?;
    let mut after = before.clone();
    let brought: Vec<FontPackage> = brought.into_iter().collect();
    let made = merged(
        &mut after.font_packages,
        &brought,
        MAX_SITE_FONT_PACKAGES,
        |p| p.id,
        |p| &p.name,
    )?;
    allow_look(state, actor, &org, &resolved, &after, (&[], &made)).await?;
    after.look = Some(resolved);
    if after != before {
        let detail = json!({
            "name": name,
            "before": before.look,
            "after": after.look,
            "template": template,
            "applied_by_admin": true,
        });
        commit_sites(
            state,
            actor,
            SitesCommit {
                message: format!("Apply a template's look to {name}"),
                changed: vec![(stored, after.clone())],
                needs_admin: true,
                needs: vec![(site, Perms::MANAGE_SITE)],
                also_lock: Vec::new(),
                release_hub: None,
                events: vec![site_event("site.look_changed", actor, site, detail)],
            },
        )
        .await?;
        erase_unused(state, &before, &after).await;
    }
    Ok(unresolved)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn settings_with(look: Option<Look>, hub: Option<NodeId>, is_hub: bool) -> SiteSettings {
        let mut s = SiteSettings::defaults(NodeId(Uuid::new_v4()), "en", "UTC");
        s.look = look;
        s.joined_hub = hub;
        s.is_hub = is_hub;
        s
    }

    fn built_in(id: &str) -> Look {
        let found = BUILT_IN_THEMES.iter().find(|t| t.id == id).unwrap();
        Look {
            theme: found.theme(),
            from: found.id.to_owned(),
            ..Look::default()
        }
    }

    #[test]
    fn hub_lends_its_theme_only_while_a_live_hub() {
        let hub = settings_with(Some(built_in("builtin:plum")), None, true);
        let own = settings_with(Some(built_in("builtin:forest")), Some(hub.id), false);
        let mut all = std::collections::HashMap::new();
        all.insert(hub.id, hub.clone());
        let none = HashSet::new();
        let lent = lending_hub_in(&own, &all, &none);
        assert_eq!(lent.map(|s| s.id), Some(hub.id));
        let plum = check_theme(&built_in("builtin:plum").theme).unwrap();
        assert_eq!(list_marks(&own, lent).1, plum.light.accent.to_string());
        // Deleted, or no longer a hub: the site's own theme.
        let deleted: HashSet<NodeId> = [hub.id].into_iter().collect();
        assert!(lending_hub_in(&own, &all, &deleted).is_none());
        all.insert(hub.id, settings_with(hub.look.clone(), None, false));
        assert!(lending_hub_in(&own, &all, &none).is_none());
        assert_eq!(list_marks(&own, None).1, "#2f6b3a");
    }

    #[test]
    fn template_looks_hold_owners_to_the_policy() {
        let mut org = OrgBrand {
            policy: revisions::brand::BrandPolicy {
                hide_builtin_themes: true,
                custom_colours: false,
                site_themes: false,
                site_images: false,
                site_fonts: false,
            },
            ..OrgBrand::default()
        };
        let template: TemplateLook = serde_json::from_value(serde_json::json!({
            "theme": { "name": "Ours", "primary": "#1f5f8b", "secondary": "#163447" },
            "from": "custom",
            "logo_alt": "",
            "header": { "layout": "compact", "background": "none", "align": "left",
                        "hide_title": false },
            "navigation": "horizontal",
            "footer": { "enabled": false, "layout": "simple", "name": "", "background": "none",
                        "groups": [] },
            "fonts": { "package": { "name": "Ours", "roles": { "title": "system-serif",
                       "heading": "system", "body": "system", "interactive": "system" } },
                       "from": format!("site:{}", Uuid::new_v4()) },
        }))
        .unwrap();
        let mut look = look_from_references(&template, &org);
        let package = look.fonts.as_ref().unwrap().package.clone();
        look.fonts.as_mut().unwrap().from = format!("site:{}", package.id);
        let packages = vec![package];
        let owner = new_site_problems(&look, &org, &packages, Actor::Owner);
        assert!(
            owner.iter().any(|p| p.contains("custom colours")),
            "{owner:?}"
        );
        assert!(
            owner.iter().any(|p| p.contains("site-only font packages")),
            "{owner:?}"
        );
        assert!(new_site_problems(&look, &org, &packages, Actor::BrandManager).is_empty());
        assert!(new_site_problems(&look, &org, &packages, Actor::Admin).is_empty());
        // The same look from an open brand centre is fine for an owner too.
        org.policy = revisions::brand::BrandPolicy::default();
        assert!(new_site_problems(&look, &org, &packages, Actor::Owner).is_empty());
    }

    #[test]
    fn source_change_marked() {
        let org = OrgBrand::default();
        let mut look = built_in("builtin:harbour");
        assert!(!source_changed(&look, &org, &[]));
        look.theme.name = "Edited".into();
        assert!(source_changed(&look, &org, &[]));
        look.from = "custom".into();
        assert!(!source_changed(&look, &org, &[]));
        look.from = format!("site:{}", Uuid::new_v4());
        assert!(source_changed(&look, &org, &[]));
    }
}
