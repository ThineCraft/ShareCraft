//! Sites' looks: each site's theme (a copy), logo and thumbnail, header, navigation style and
//! footer, recorded as `look` on its site record in Git (see [`revisions::brand::Look`]), and its
//! own themes (`themes` on the same record), with the history of its look and going back to an
//! earlier one.
//!
//! Every change is one sites commit, mirrored with its event (`site.look_changed {before,
//! after}` or `site.theme_changed`) and judged again there, taken back if refused (see
//! [`crate::sites::commit_sites`]). A look's images pin their content in the content store
//! while the look uses them; content a look stops using is erased unless something else names
//! it.
//!
//! The hub rule is worked out whenever a look is read, never written: a site that joined a hub
//! (registered, not deleted; an archived hub still counts) takes the theme of the hub's own
//! look, and everything else from its own. Its own theme is kept, unused, and comes back when it
//! leaves. So joining, leaving, registering and unregistering a hub change no look.

use std::collections::HashSet;

use collab_core::ids::{DocId, NodeId};
use records::{Db, LookChange, Perms, SiteSettings, SiteState, User};
use revisions::brand::{
    Align, BUILT_IN_THEMES, Background, BuiltInTheme, FooterGroup, FooterLayout, Header,
    HeaderLayout, ImageKind, ImageRef, Look, MAX_SITE_THEMES, Navigation, OrgBrand, OrgTheme,
    ResolvedTheme, Theme, check_theme, clean_text,
};
use revisions::{SiteRecord, SiteTheme};
use serde::Deserialize;
use serde_json::{Value, json};
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::AppState;
use crate::api::ApiError;
use crate::api::brand::{image_url, theme_out};
use crate::brand::{BrandError, ThemeInput, checked_theme, image_from_doc, org_theme};
use crate::sites::{SitesCommit, commit_sites, record_and_start, seen_site, site_event};
use crate::tree::{ensure_in_step, require};

/// How many changes of a look its history lists.
pub(crate) const HISTORY_LENGTH: i64 = 50;

// ---------------------------------------------------------------------------------------------
// What a person sends

/// An image chosen for a slot of a look: exactly one of `doc` (the current version of a
/// document), `brand_image` (an image of the brand centre, by id) or `keep` (the image the look
/// holds now with this content hash).
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
    org: &'a OrgBrand,
}

/// The image `slot` asks for, for a slot of `kind`'s size.
///
/// # Errors
/// `InvalidLook` for a slot that gives no way or more than one, an unknown brand image, or a
/// `keep` of an image the look does not hold; `413 image_too_large`; `423 held`; and those of
/// [`image_from_doc`].
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
            .find(|image| image.sha256 == sha)
            .map(|image| Some((*image).clone()))
            .ok_or_else(|| {
                BrandError::InvalidLook("only an image the look holds now can be kept".into())
            }),
        _ => Err(BrandError::InvalidLook(
            "an image is given as one of {doc}, {brand_image} or {keep}".into(),
        )),
    }
}

/// Every image of `look`, each with the name of its slot.
fn slots(look: &Look) -> Vec<(&'static str, &ImageRef)> {
    [
        ("logo", look.logo.as_ref()),
        ("thumbnail", look.thumbnail.as_ref()),
        ("header.image", look.header.image.as_ref()),
        ("footer.logo", look.footer.logo.as_ref()),
    ]
    .into_iter()
    .filter_map(|(name, image)| image.map(|i| (name, i)))
    .collect()
}

/// The look `input` asks for, for `actor` on `site` (named `site_name`), whose look is now
/// `current` and whose own themes are `site_themes`: its theme copied and checked, its images
/// resolved, its text trimmed, and every rule of [`Look::problem`] checked.
async fn built_look(
    ctx: &SlotContext<'_>,
    site_name: &str,
    site_themes: &[SiteTheme],
    input: LookInput,
) -> Result<Look, BrandError> {
    let defaults = Look::default();
    let (theme, from) = match input.theme {
        Some(choice) => chosen_theme(choice, ctx.org, site_themes)?,
        None => (defaults.theme.clone(), defaults.from.clone()),
    };
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

/// Erases the content `before`'s look used that `after`'s does not, unless something else
/// names it. Failures are logged; the sweeper erases what is left.
async fn erase_unused(state: &AppState, before: &SiteRecord, after: &SiteRecord) {
    let kept: HashSet<&str> = after
        .look
        .iter()
        .flat_map(slots)
        .map(|(_, image)| image.sha256.as_str())
        .collect();
    let mut done = HashSet::new();
    for (_, image) in before.look.iter().flat_map(slots) {
        if kept.contains(image.sha256.as_str()) || !done.insert(image.sha256.clone()) {
            continue;
        }
        let Some(hash) = revisions::parse_hex(&image.sha256) else {
            continue;
        };
        if let Err(e) = state.blobs().erase_if_unreferenced(state.db(), &hash).await {
            eprintln!("sharecraft-server: cannot erase a look's image: {e}");
        }
    }
}

/// Writes `look` as the look of `site` (named `name`) for `actor`, whose record Git holds as
/// `start.0` (`None` if it has none; `start.1` is where the change starts), with
/// `site.look_changed {name, before, after}` (and `rollback`, the entry it goes back to). The
/// same look again writes nothing. The caller holds the tree lock.
async fn commit_look(
    state: &AppState,
    actor: &User,
    site: NodeId,
    name: &str,
    start: (Option<SiteRecord>, SiteRecord),
    look: Option<Look>,
    rollback: Option<i64>,
) -> Result<(), BrandError> {
    let (stored, before) = start;
    if before.look == look {
        return Ok(());
    }
    let mut after = before.clone();
    after.look = look;
    let mut detail = json!({ "name": name, "before": before.look, "after": after.look });
    if let (Some(id), Some(map)) = (rollback, detail.as_object_mut()) {
        map.insert("rollback".into(), json!(id));
    }
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
    {
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
            org: &org,
        };
        let look = built_look(&ctx, &name, &before.themes, input).await?;
        commit_look(
            state,
            actor,
            site,
            &name,
            (stored.clone(), before.clone()),
            Some(look),
            None,
        )
        .await?;
    }
    settings_answer(state, site).await
}

/// Applies the look before (`Which::Before`) or after a change in the history of `site` as a
/// new change for `actor`, who needs `manage_site` on it, checked again as any look is.
/// Answers the look settings.
///
/// # Errors
/// `404` for no such entry of this site; `422 low_contrast` or `invalid_look` for a look that
/// no longer passes; `409 image_gone {slots}` when the content of its images is gone;
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
            let used = slots(look);
            let hashes: Vec<String> = used.iter().map(|(_, i)| i.sha256.clone()).collect();
            let missing = db.missing_blobs(&hashes).await?;
            let gone: Vec<&'static str> = used
                .iter()
                .filter(|(_, image)| missing.contains(&image.sha256))
                .map(|(slot, _)| *slot)
                .collect();
            if !gone.is_empty() {
                return Err(BrandError::ImagesGone(gone));
            }
        }
        let (stored, before) = record_and_start(state, site).await?;
        commit_look(
            state,
            actor,
            site,
            &name,
            (stored, before),
            look,
            Some(event_id),
        )
        .await?;
    }
    settings_answer(state, site).await
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

/// The look settings of `site`: `{look, themes, theme_source_changed, in_hub?: {id, name}}`.
async fn settings_answer(state: &AppState, site: NodeId) -> Result<Value, BrandError> {
    let db = state.db();
    let settings = site_settings(db, site).await?;
    let org = db.org_brand().await?;
    let look = settings.look.clone().unwrap_or_default();
    let mut out = json!({
        "look": look,
        "themes": settings.themes.iter().map(site_theme_out).collect::<Vec<_>>(),
        "theme_source_changed": source_changed(&look, &org, &settings.themes),
    });
    if let Some((hub, _)) = lending_hub(db, &settings).await?
        && let Some(node) = db.node(hub).await?
        && let Some(map) = out.as_object_mut()
    {
        map.insert(
            "in_hub".into(),
            json!({ "id": hub.0.to_string(), "name": node.name }),
        );
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
    settings_answer(state, site).await
}

/// The last [`HISTORY_LENGTH`] changes of the look of `site`, newest first, for `actor`, who
/// manages it (archived or not): each `{event_id, at, by: {id, name} | null, look}`, `look`
/// being what the change made it (`null` for none).
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
/// it): `{theme: {light, dark}, logo_url?, thumbnail_url?, logo_alt, header, navigation,
/// footer, theme_from: "site" | "hub", hub?: {id, name, thumbnail_url?}}`. The theme comes from
/// the hub the site joined, if any (see the module documentation), everything else from the
/// site; `hub` is named only to people who can see it. An empty logo description is the site's
/// name.
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
    let mut out = json!({
        "theme": palettes(themed.look.as_ref()),
        "logo_alt": logo_alt,
        "navigation": look.navigation,
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
