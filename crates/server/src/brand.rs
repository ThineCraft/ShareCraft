//! The organisation's brand centre: its themes, brand images, fonts, font packages, default
//! tab icon, the limits on site owners (the brand policy) and the brand managers, recorded in
//! Git at `org/brand.json` (see [`revisions::brand`]) and mirrored into PostgreSQL, and
//! choosing a brand image or font file from a document.
//!
//! Each change is one commit, written under the tree lock, then mirrored with its event
//! (`brand.theme_changed`, `brand.image_changed`, `brand.font_changed`,
//! `brand.font_package_changed`, `brand.policy_changed` or `brand.managers_changed`), judged
//! again there; a refused change is taken back by a second commit, and a database failure
//! after judging is repaired from Git once or leaves the tree marked out of step. The start-up
//! reconcile rebuilds the mirror and the centre's pins from Git (see [`reconcile`]).
//!
//! A font file is a snapshot as an image is, chosen only once the person adding it confirms
//! the organisation holds its licence; its type is judged by its first bytes, and a page loads
//! it under a family name made from its hash, never from its name.
//!
//! A brand image is a snapshot: choosing one records its content's hash, type, size and
//! dimensions, and pins the content in the content store, so a later change to the file or
//! its deletion leaves the brand as it was. Its type is judged by its content, never its name;
//! an SVG must be well-formed XML with an `svg` root and nothing that runs or loads: no
//! scripts, event handlers, foreign objects, external links, style imports or `url()` outside
//! the image.

use std::collections::BTreeSet;
use std::io::{Cursor, Read};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use collab_core::ids::{DocId, NodeId};
use records::{
    AccessError, BRAND_CENTRE_OWNER, BrandChange, BrandNeeds, Db, DbError, LibraryKind, NewEvent,
    Perms, User, org_pins,
};
use revisions::brand::{
    Actor, BrandFont, BrandImage, BrandPolicy, Colour, FailedPair, FontPackage, FontRef, FontRole,
    FontRoles, ImageKind, ImageRef, Licence, MAX_FONT_BYTES, MAX_IMAGE_NAME_CHARS, MAX_IMAGE_SIDE,
    MAX_MANAGERS, MAX_ORG_FONT_PACKAGES, MAX_ORG_FONTS, MAX_ORG_IMAGES, MAX_ORG_THEMES,
    MAX_THEME_NAME_CHARS, OrgBrand, OrgTheme, Theme, check_theme, clean_text, icon_problem,
    sniff_font,
};
use revisions::{CheckpointError, Content, GitError};
use serde_json::{Value, json};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::AppState;
use crate::api::ApiError;
use crate::checkpoint::Checkpointer;
use crate::tree::{
    TreeError, Undo, author_of, ensure_in_step, git_failure, take_back_all, write_records,
};

/// Why a brand change or choice was refused.
#[derive(Debug, Clone)]
pub(crate) enum BrandError {
    /// An answer every route can give.
    Api(ApiError),
    /// `422 low_contrast {"pairs"}`: the theme is not readable.
    LowContrast(Vec<FailedPair>),
    /// `422 invalid_theme {"reason"}`: the theme breaks a rule other than contrast.
    InvalidTheme(String),
    /// `422 invalid_look {"reason"}`: a site's look breaks a rule other than its theme's.
    InvalidLook(String),
    /// `409 image_gone {"slots"}`: the content of the images in these slots of a look is no
    /// longer stored.
    ImagesGone(Vec<&'static str>),
    /// `422 invalid_font_package {"reason"}`: a font package breaks one of its rules.
    InvalidFontPackage(String),
    /// `409 not_allowed {"problems"}`: the brand policy does not let this person make this
    /// change; each problem in words.
    NotAllowed(Vec<String>),
}

impl From<ApiError> for BrandError {
    fn from(e: ApiError) -> Self {
        BrandError::Api(e)
    }
}

impl From<AccessError> for BrandError {
    fn from(e: AccessError) -> Self {
        BrandError::Api(e.into())
    }
}

impl From<DbError> for BrandError {
    fn from(e: DbError) -> Self {
        BrandError::Api(ApiError::unavailable(&e))
    }
}

/// A theme as typed: colours are text until checked.
#[derive(Clone, Debug, Default)]
pub(crate) struct ThemeInput {
    /// The name, trimmed when checked.
    pub(crate) name: String,
    /// The accent, `#rrggbb`.
    pub(crate) primary: String,
    /// The strong background, `#rrggbb`.
    pub(crate) secondary: String,
    /// The dark accent; derived when absent.
    pub(crate) primary_dark: Option<String>,
    /// The dark strong background; derived when absent.
    pub(crate) secondary_dark: Option<String>,
}

/// Checks a typed theme: the name trimmed, 1 to 60 characters with no control or invisible
/// characters; each colour `#rrggbb` (kept in lower case); then every contrast check.
///
/// # Errors
/// `InvalidTheme` with the reason, or `LowContrast` with every failing pair.
pub(crate) fn checked_theme(input: &ThemeInput) -> Result<Theme, BrandError> {
    let name = clean_text(&input.name).ok_or_else(|| {
        BrandError::InvalidTheme("a theme name holds no control or invisible characters".into())
    })?;
    let colour = |what: &str, text: &str| {
        Colour::parse(text)
            .map_err(|_| BrandError::InvalidTheme(format!("{what} is written #rrggbb")))
    };
    let theme = Theme {
        name: name.to_owned(),
        primary: colour("primary", &input.primary)?,
        secondary: colour("secondary", &input.secondary)?,
        primary_dark: input
            .primary_dark
            .as_deref()
            .map(|t| colour("primary_dark", t))
            .transpose()?,
        secondary_dark: input
            .secondary_dark
            .as_deref()
            .map(|t| colour("secondary_dark", t))
            .transpose()?,
    };
    if let Some(problem) = theme.problem() {
        return Err(BrandError::InvalidTheme(problem));
    }
    check_theme(&theme).map_err(BrandError::LowContrast)?;
    Ok(theme)
}

// ---------------------------------------------------------------------------------------------
// Judging images

/// Whether `text` loads something from outside the image, or could hide that it does: an
/// `@import`, `image-set(` or `src(`, a `url()` that does not point inside it (`#id`), a
/// backslash (a CSS escape such as `u\72l(`) or a character reference (`u&#114;l(`, which
/// XML decodes before CSS reads it).
fn loads_outside(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    if ["@import", "image-set(", "src(", "\\", "&#"]
        .iter()
        .any(|bad| lower.contains(bad))
    {
        return true;
    }
    lower.match_indices("url(").any(|(at, found)| {
        let after = lower.get(at + found.len()..).unwrap_or_default();
        !after
            .trim_start()
            .trim_start_matches(['"', '\''])
            .starts_with('#')
    })
}

/// The length of the XML name at the start of `text`.
fn name_len(text: &str) -> usize {
    text.find(|c: char| !(c.is_alphanumeric() || matches!(c, ':' | '-' | '_' | '.')))
        .unwrap_or(text.len())
}

/// The local part of an XML name (after any prefix), in lower case.
fn local(name: &str) -> String {
    name.rsplit(':').next().unwrap_or(name).to_ascii_lowercase()
}

/// Elements that run code, embed other documents, or animate: an animation can set an
/// attribute (an `href`, a handler) to a value no attribute check ever sees, so none is
/// allowed in a brand image.
const UNSAFE_ELEMENTS: [&str; 12] = [
    "script",
    "foreignobject",
    "iframe",
    "embed",
    "object",
    "handler",
    "listener",
    "animate",
    "set",
    "animatemotion",
    "animatetransform",
    "discard",
];

/// Whether `bytes` is a safe SVG: UTF-8, well-formed XML (no DOCTYPE, and no processing
/// instruction but one leading `<?xml …?>` declaration) whose one root element is `svg`, with
/// no element of [`UNSAFE_ELEMENTS`], no event handler (`on…`) attribute, no link (`href`)
/// outside the image, and nothing that loads from outside (see [`loads_outside`]).
pub(crate) fn safe_svg(bytes: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    let mut rest = text.strip_prefix('\u{feff}').unwrap_or(text);
    // The XML declaration, if any, comes first of all and at most once.
    if let Some(r) = rest.strip_prefix("<?xml") {
        let Some(end) = r.find("?>") else {
            return false;
        };
        let attributes = r.get(..end).unwrap_or_default();
        if !attributes.starts_with(|c: char| c.is_ascii_whitespace())
            || attributes.contains('<')
            || loads_outside(attributes)
        {
            return false;
        }
        rest = r.get(end + 2..).unwrap_or_default();
    }
    let mut open: Vec<&str> = Vec::new();
    let mut seen_root = false;
    // Every piece of text and CDATA in document order, comments left out: what any element's
    // text content (a style sheet's, say) is a contiguous part of, so a `url(` or `@import`
    // split by a comment, a CDATA section or a child element is still seen whole.
    let mut all_text = String::new();
    loop {
        let Some(at) = rest.find('<') else {
            return seen_root
                && open.is_empty()
                && rest.trim().is_empty()
                && !loads_outside(&all_text);
        };
        let (between, tag) = rest.split_at(at);
        if (open.is_empty() && !between.trim().is_empty()) || loads_outside(between) {
            return false;
        }
        all_text.push_str(between);
        rest = tag;
        if let Some(r) = rest.strip_prefix("<!--") {
            let Some(end) = r.find("-->") else {
                return false;
            };
            rest = r.get(end + 3..).unwrap_or_default();
            continue;
        }
        if let Some(r) = rest.strip_prefix("<![CDATA[") {
            let Some(end) = r.find("]]>") else {
                return false;
            };
            let cdata = r.get(..end).unwrap_or_default();
            if open.is_empty() || loads_outside(cdata) {
                return false;
            }
            all_text.push_str(cdata);
            rest = r.get(end + 3..).unwrap_or_default();
            continue;
        }
        if rest.starts_with("<!") || rest.starts_with("<?") {
            // A DOCTYPE, with its entities, or anything else XML allows there; a processing
            // instruction other than the leading declaration (`xml-stylesheet`, say).
            return false;
        }
        if let Some(r) = rest.strip_prefix("</") {
            let Some(end) = r.find('>') else {
                return false;
            };
            let name = r.get(..end).unwrap_or_default().trim_end();
            if open.pop() != Some(name) {
                return false;
            }
            rest = r.get(end + 1..).unwrap_or_default();
            continue;
        }
        let r = rest.get(1..).unwrap_or_default();
        let len = name_len(r);
        let (name, mut r) = r.split_at(len);
        if name.is_empty() || UNSAFE_ELEMENTS.contains(&local(name).as_str()) {
            return false;
        }
        if open.is_empty() {
            if seen_root || name != "svg" {
                return false;
            }
            seen_root = true;
        }
        loop {
            let trimmed = r.trim_start();
            if let Some(after) = trimmed.strip_prefix("/>") {
                r = after;
                break;
            }
            if let Some(after) = trimmed.strip_prefix('>') {
                open.push(name);
                r = after;
                break;
            }
            if trimmed.len() == r.len() {
                // Attributes are separated by white space.
                return false;
            }
            let len = name_len(trimmed);
            let (attribute, after) = trimmed.split_at(len);
            if attribute.is_empty() {
                return false;
            }
            let Some(after) = after.trim_start().strip_prefix('=') else {
                return false;
            };
            let after = after.trim_start();
            let Some(quote) = after.chars().next().filter(|c| matches!(c, '"' | '\'')) else {
                return false;
            };
            let after = after.get(1..).unwrap_or_default();
            let Some(end) = after.find(quote) else {
                return false;
            };
            let value = after.get(..end).unwrap_or_default();
            let attr = local(attribute);
            if value.contains('<')
                || attr.starts_with("on")
                || (attr == "href" && !value.trim_start().starts_with('#'))
                || loads_outside(value)
            {
                return false;
            }
            r = after.get(end + 1..).unwrap_or_default();
        }
        rest = r;
    }
}

/// An image's type, judged by its content, and a raster image's width and height.
pub(crate) type Judged = (&'static str, Option<(u32, u32)>);

/// The type of an image judged by its content, and a raster image's width and height read
/// from its header.
///
/// # Errors
/// `not_an_image` for anything but PNG, JPEG, WebP, GIF or a safe SVG (see [`safe_svg`]);
/// `image_too_large` past 8,000 pixels on a side.
pub(crate) fn judge_image(bytes: &[u8]) -> Result<Judged, ApiError> {
    if let Ok(format) = image::guess_format(bytes) {
        let media_type = match format {
            image::ImageFormat::Png => "image/png",
            image::ImageFormat::Jpeg => "image/jpeg",
            image::ImageFormat::WebP => "image/webp",
            image::ImageFormat::Gif => "image/gif",
            _ => return Err(ApiError::NotAnImage),
        };
        let (width, height) = image::ImageReader::with_format(Cursor::new(bytes), format)
            .into_dimensions()
            .map_err(|_| ApiError::NotAnImage)?;
        if width == 0 || height == 0 {
            return Err(ApiError::NotAnImage);
        }
        if width > MAX_IMAGE_SIDE || height > MAX_IMAGE_SIDE {
            return Err(ApiError::ImageTooLarge);
        }
        return Ok((media_type, Some((width, height))));
    }
    if safe_svg(bytes) {
        return Ok(("image/svg+xml", None));
    }
    Err(ApiError::NotAnImage)
}

/// A name for an image: `name` trimmed, cut to 120 characters; `None` if it is empty or holds
/// control or invisible characters.
fn image_name(name: &str) -> Option<String> {
    let clean = clean_text(name)?;
    let cut: String = clean.chars().take(MAX_IMAGE_NAME_CHARS).collect();
    let cut = cut.trim_end().to_owned();
    (!cut.is_empty()).then_some(cut)
}

/// Chooses the current version of the document `doc` as a brand image of `kind` for `actor`,
/// who needs `view` on it. The document must be in an asset library, or, when `site` is given
/// (choosing for that site's look), in one of that site's libraries. The content must be
/// available (not held by the malware scanner), within the size of `kind`, and an image by
/// its content (see [`judge_image`]). The image is named after the document.
///
/// # Errors
/// `404` for a document `actor` cannot view, or that is deleted; `400 wrong_place` for one in
/// any other library; `423 held {scan}`; `413 image_too_large`; `422 not_an_image` (also for
/// a document with no version, or one kept in Git before the content store); otherwise `503`.
pub(crate) async fn image_from_doc(
    state: &AppState,
    actor: &User,
    doc: DocId,
    kind: ImageKind,
    site: Option<NodeId>,
) -> Result<ImageRef, ApiError> {
    let chosen = doc_content(
        state,
        actor,
        doc,
        site,
        (
            kind.max_bytes(),
            ApiError::NotAnImage,
            ApiError::ImageTooLarge,
        ),
    )
    .await?;
    let (media_type, size) = judge_image(&chosen.bytes)?;
    Ok(ImageRef {
        sha256: chosen.sha256,
        media_type: media_type.to_owned(),
        bytes: chosen.size,
        width: size.map(|(w, _)| w),
        height: size.map(|(_, h)| h),
        name: image_name(&chosen.name).unwrap_or_else(|| "Image".to_owned()),
        from_doc: doc.0,
    })
}

/// Chooses the current version of the document `doc` as a font file for `actor`, who has
/// confirmed (`licence_confirmed`) that the organisation holds its licence; the record notes
/// who, and when. Where it may come from, and the checks of access and availability, are
/// those of [`image_from_doc`]. Its type is judged by its content (see
/// [`revisions::brand::sniff_font`]), and it is named after the document.
///
/// # Errors
/// `422 licence_not_confirmed` first of all; then as [`image_from_doc`], with `422 not_a_font`
/// and `413 font_too_large` (past 2 MiB) in place of the image errors.
pub(crate) async fn font_from_doc(
    state: &AppState,
    actor: &User,
    doc: DocId,
    site: Option<NodeId>,
    licence_confirmed: bool,
) -> Result<FontRef, ApiError> {
    if !licence_confirmed {
        return Err(ApiError::LicenceNotConfirmed);
    }
    let chosen = doc_content(
        state,
        actor,
        doc,
        site,
        (MAX_FONT_BYTES, ApiError::NotAFont, ApiError::FontTooLarge),
    )
    .await?;
    let kind = sniff_font(&chosen.bytes).ok_or(ApiError::NotAFont)?;
    Ok(FontRef {
        sha256: chosen.sha256,
        media_type: kind.media_type().to_owned(),
        bytes: chosen.size,
        name: image_name(&chosen.name).unwrap_or_else(|| "Font".to_owned()),
        from_doc: doc.0,
        licence: Licence {
            confirmed_by: actor.id.0,
            confirmed_at: now_seconds(),
        },
    })
}

/// The current content of a document chosen for the brand: its bytes, hash, size and the
/// document's name.
struct DocContent {
    bytes: Vec<u8>,
    sha256: String,
    size: u64,
    name: String,
}

/// The current content of the document `doc` for `actor`, who needs `view` on it, from an
/// asset library or, with `site`, one of that site's libraries; available, not empty and at
/// most `limits.0` bytes (`limits.1` for content that is missing or empty, `limits.2` for
/// content too large).
async fn doc_content(
    state: &AppState,
    actor: &User,
    doc: DocId,
    site: Option<NodeId>,
    limits: (u64, ApiError, ApiError),
) -> Result<DocContent, ApiError> {
    let (max_bytes, unusable, too_large) = limits;
    let db = state.db();
    let unavailable = |e: DbError| ApiError::unavailable(&e);
    let access = db
        .doc_access(actor.id, doc)
        .await
        .map_err(unavailable)?
        .ok_or(ApiError::NotFound)?;
    if !access.perms.contains(Perms::VIEW) {
        return Err(ApiError::NotFound);
    }
    let node = db
        .node(doc.into())
        .await
        .map_err(unavailable)?
        .filter(|node| !node.deleted)
        .ok_or(ApiError::NotFound)?;
    let document = db
        .document(doc)
        .await
        .map_err(unavailable)?
        .ok_or(ApiError::NotFound)?;
    let library = db
        .library_of(node.id)
        .await
        .map_err(unavailable)?
        .ok_or(ApiError::WrongPlace)?;
    let asset = db
        .library_settings(library)
        .await
        .map_err(unavailable)?
        .is_some_and(|settings| settings.kind == LibraryKind::Asset);
    if !asset && site != Some(node.site) {
        return Err(ApiError::WrongPlace);
    }
    let stored = crate::api::content::blocking(state, move |store| store.load(doc))
        .await?
        .ok_or(unusable)?;
    let Content::Blob(blob) = stored.content else {
        return Err(unusable);
    };
    if let Some(scan) = db.availability(&blob.sha256).await.map_err(unavailable)? {
        return Err(ApiError::Held(scan));
    }
    if blob.size == 0 {
        return Err(unusable);
    }
    if blob.size > max_bytes {
        return Err(too_large);
    }
    let blobs = Arc::clone(state.blobs());
    let limit = blob.size;
    let bytes = tokio::task::spawn_blocking(move || {
        let Some(file) = blobs.store().open_blob(&blob.sha256)? else {
            return Ok::<_, revisions::BlobError>(None);
        };
        let mut bytes = Vec::new();
        file.take(limit + 1).read_to_end(&mut bytes)?;
        Ok(Some(bytes))
    })
    .await
    .map_err(|e| ApiError::unavailable(&e))?
    .map_err(|e| ApiError::unavailable(&e))?
    .filter(|bytes| bytes.len() as u64 == limit)
    .ok_or_else(|| {
        eprintln!(
            "sharecraft-server: content {} of document {} is missing from the content store",
            blob.hex(),
            doc.0
        );
        ApiError::Unavailable
    })?;
    Ok(DocContent {
        bytes,
        sha256: blob.hex(),
        size: blob.size,
        name: document.name,
    })
}

// ---------------------------------------------------------------------------------------------
// Changing the brand centre

/// The brand centre as Git holds it, or an empty one if none was recorded.
async fn read_brand(store: &Arc<dyn Checkpointer>) -> Result<OrgBrand, CheckpointError> {
    let store = Arc::clone(store);
    tokio::task::spawn_blocking(move || store.read_org_brand())
        .await
        .map_err(|e| CheckpointError::Git(GitError::new(e)))?
        .map(Option::unwrap_or_default)
}

/// What one change of the brand centre does to it: the commit message, its events and the
/// answer.
pub(crate) struct Edited<T> {
    message: String,
    events: Vec<NewEvent>,
    answer: T,
}

/// An event of the brand centre by `actor`.
fn brand_event(kind: &'static str, actor: &User, detail: Value) -> NewEvent {
    NewEvent {
        kind,
        actor: Some(actor.id),
        subject: None,
        document: None,
        node: None,
        detail,
    }
}

/// Whether `actor` is what `needs` asks for, by PostgreSQL's mirror (the transaction judges
/// again).
async fn allowed(db: &Db, actor: &User, needs: BrandNeeds) -> Result<bool, DbError> {
    match needs {
        BrandNeeds::Admin => Ok(db
            .user(actor.id)
            .await?
            .is_some_and(|u| u.is_admin && !u.disabled)),
        BrandNeeds::Manager => db.can_manage_brand(actor.id).await,
    }
}

/// Changes the brand centre for `actor` with `edit`, under the tree lock: refused with `403`
/// unless `actor` is what `needs` asks for; then `edit` changes the centre as Git holds it (an
/// edit that changes nothing writes nothing), one commit records it, and it is mirrored into
/// PostgreSQL, judged again there. A refused change is taken back by a second commit; a
/// database failure after judging is repaired from Git once, or leaves the tree out of step.
/// Once mirrored, content that no image pins any more is erased unless something else names
/// it.
async fn change_brand<T>(
    state: &AppState,
    actor: &User,
    needs: BrandNeeds,
    edit: impl FnOnce(&mut OrgBrand) -> Result<Edited<T>, BrandError>,
) -> Result<T, BrandError> {
    let db = state.db();
    if !allowed(db, actor, needs).await? {
        return Err(ApiError::Forbidden.into());
    }
    let _turn = state.tree_lock().lock().await;
    ensure_in_step(state).await?;
    let before = read_brand(state.checkpointer())
        .await
        .map_err(git_failure)?;
    let mut after = before.clone();
    let Edited {
        message,
        events,
        answer,
    } = edit(&mut after)?;
    if after == before {
        return Ok(answer);
    }
    if let Some(problem) = after.problem() {
        // The edits check every limit first; a record that still breaks one is a fault here.
        return Err(ApiError::unavailable(&problem).into());
    }
    let author = author_of(actor);
    write_records(
        state.checkpointer(),
        author.clone(),
        message.clone(),
        Undo {
            org_brand: Some(after.clone()),
            ..Undo::default()
        },
    )
    .await
    .map_err(git_failure)?;
    let applied = db
        .apply_org_brand(BrandChange {
            record: &after,
            actor: Some(actor.id),
            needs,
            events,
        })
        .await;
    match applied {
        Ok(()) => {
            erase_unpinned(state, &before, &after).await;
            Ok(answer)
        }
        Err(AccessError::Db(error)) => {
            eprintln!("sharecraft-server: {message} is in Git but not in the database: {error}");
            if !repair(state).await {
                state.tree_out_of_step().store(true, Ordering::SeqCst);
            }
            Err(AccessError::Db(error).into())
        }
        Err(refused) => {
            if let AccessError::Unjudged(error) = &refused {
                eprintln!(
                    "sharecraft-server: {message} could not be judged, so it is taken back: \
                     {error}"
                );
            }
            let undo = Undo {
                org_brand: Some(before),
                ..Undo::default()
            };
            if !take_back_all(state, author, &message, undo).await {
                state.tree_out_of_step().store(true, Ordering::SeqCst);
            }
            Err(refused.into())
        }
    }
}

/// The contents `brand` pins: its images, fonts and the fonts of its packages.
fn pinned(brand: &OrgBrand) -> BTreeSet<String> {
    org_pins(brand)
        .into_iter()
        .map(|pin| pin.sha256.to_owned())
        .collect()
}

/// Erases the content `before` pinned that `after` does not, unless something else names it.
/// Failures are logged; the sweeper erases what is left.
async fn erase_unpinned(state: &AppState, before: &OrgBrand, after: &OrgBrand) {
    let kept = pinned(after);
    for sha in pinned(before).difference(&kept) {
        let Some(hash) = revisions::parse_hex(sha) else {
            continue;
        };
        if let Err(e) = state.blobs().erase_if_unreferenced(state.db(), &hash).await {
            eprintln!("sharecraft-server: cannot erase a brand image's content: {e}");
        }
    }
}

/// The theme `theme` as an event describes it.
fn theme_detail(theme: &OrgTheme) -> Value {
    serde_json::to_value(theme).unwrap_or(Value::Null)
}

/// The time now, in seconds since the Unix epoch.
pub(crate) fn now_seconds() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

/// Whether another theme than `except` of `brand` has the name `name`, ignoring case.
fn name_taken(brand: &OrgBrand, name: &str, except: Option<Uuid>) -> bool {
    let wanted = name.to_lowercase();
    brand
        .themes
        .iter()
        .any(|t| Some(t.id) != except && t.name.to_lowercase() == wanted)
}

/// The brand-centre (or site) theme of `id` and `theme`, changed now.
pub(crate) fn org_theme(id: Uuid, theme: Theme) -> OrgTheme {
    OrgTheme {
        id,
        name: theme.name,
        primary: theme.primary,
        secondary: theme.secondary,
        primary_dark: theme.primary_dark,
        secondary_dark: theme.secondary_dark,
        changed_at: now_seconds(),
    }
}

/// Adds the theme `input` to the brand centre for `actor`, a brand manager or administrator,
/// with `brand.theme_changed` (`created`). Answers the theme.
///
/// # Errors
/// `403`; `422 invalid_theme` / `low_contrast` (see [`checked_theme`]); `409 name_taken` for a
/// name another theme has, ignoring case; `409 too_many {limit: 50}`; otherwise `503`.
pub(crate) async fn create_theme(
    state: &AppState,
    actor: &User,
    input: &ThemeInput,
) -> Result<OrgTheme, BrandError> {
    let theme = checked_theme(input)?;
    change_brand(state, actor, BrandNeeds::Manager, |brand| {
        if name_taken(brand, &theme.name, None) {
            return Err(ApiError::NameTaken.into());
        }
        if brand.themes.len() >= MAX_ORG_THEMES {
            return Err(ApiError::TooMany(MAX_ORG_THEMES).into());
        }
        let created = org_theme(Uuid::new_v4(), theme);
        brand.themes.push(created.clone());
        Ok(Edited {
            message: format!("Add the theme {} to the brand centre", created.name),
            events: vec![brand_event(
                "brand.theme_changed",
                actor,
                json!({ "action": "created", "theme": theme_detail(&created) }),
            )],
            answer: created,
        })
    })
    .await
}

/// Changes the theme `id` of the brand centre to `input` for `actor`, with
/// `brand.theme_changed` (`updated`, the theme before and after). Sites that use a copy of it
/// keep their copy. The same theme again changes nothing. Answers the theme.
///
/// # Errors
/// As [`create_theme`], with `404` for no such theme.
pub(crate) async fn change_theme(
    state: &AppState,
    actor: &User,
    id: Uuid,
    input: &ThemeInput,
) -> Result<OrgTheme, BrandError> {
    let theme = checked_theme(input)?;
    change_brand(state, actor, BrandNeeds::Manager, |brand| {
        let at = brand
            .themes
            .iter()
            .position(|t| t.id == id)
            .ok_or(ApiError::NotFound)?;
        let old = brand.themes.get(at).cloned().ok_or(ApiError::NotFound)?;
        if old.theme() == theme {
            return Ok(Edited {
                message: String::new(),
                events: Vec::new(),
                answer: old,
            });
        }
        if name_taken(brand, &theme.name, Some(id)) {
            return Err(ApiError::NameTaken.into());
        }
        let changed = org_theme(id, theme);
        if let Some(slot) = brand.themes.get_mut(at) {
            *slot = changed.clone();
        }
        Ok(Edited {
            message: format!("Change the theme {} of the brand centre", old.name),
            events: vec![brand_event(
                "brand.theme_changed",
                actor,
                json!({
                    "action": "updated",
                    "before": theme_detail(&old),
                    "theme": theme_detail(&changed),
                }),
            )],
            answer: changed,
        })
    })
    .await
}

/// Deletes the theme `id` from the brand centre for `actor`, with `brand.theme_changed`
/// (`deleted`). Sites that use a copy of it keep their copy.
///
/// # Errors
/// `403`; `404` for no such theme; otherwise `503`.
pub(crate) async fn delete_theme(
    state: &AppState,
    actor: &User,
    id: Uuid,
) -> Result<(), BrandError> {
    change_brand(state, actor, BrandNeeds::Manager, |brand| {
        let at = brand
            .themes
            .iter()
            .position(|t| t.id == id)
            .ok_or(ApiError::NotFound)?;
        let old = brand.themes.remove(at);
        Ok(Edited {
            message: format!("Delete the theme {} from the brand centre", old.name),
            events: vec![brand_event(
                "brand.theme_changed",
                actor,
                json!({ "action": "deleted", "theme": theme_detail(&old) }),
            )],
            answer: (),
        })
    })
    .await
}

/// The image `image` as an event describes it.
fn image_detail(image: &BrandImage) -> Value {
    json!({
        "id": image.id,
        "kind": image.kind,
        "name": image.image.name,
        "sha256": image.image.sha256,
        "media_type": image.image.media_type,
    })
}

/// Adds the current version of the document `doc` to the brand centre as an image of `kind`
/// for `actor`, a brand manager or administrator who can view it (see [`image_from_doc`]),
/// named `name` or else after the document, with `brand.image_changed` (`added`). Its content
/// is pinned from then on. Answers the image.
///
/// # Errors
/// Those of [`image_from_doc`]; `403`; `422 invalid_request` for a name with control or
/// invisible characters, or empty; `409 too_many {limit: 100}`; `409 image_gone` when the
/// content was erased meanwhile; otherwise `503`.
pub(crate) async fn add_image(
    state: &AppState,
    actor: &User,
    doc: DocId,
    kind: ImageKind,
    name: Option<&str>,
) -> Result<BrandImage, BrandError> {
    if !allowed(state.db(), actor, BrandNeeds::Manager).await? {
        return Err(ApiError::Forbidden.into());
    }
    let mut image = image_from_doc(state, actor, doc, kind, None).await?;
    if let Some(name) = name {
        image.name = clean_text(name)
            .filter(|n| (1..=MAX_IMAGE_NAME_CHARS).contains(&n.chars().count()))
            .ok_or(ApiError::InvalidRequest)?
            .to_owned();
    }
    change_brand(state, actor, BrandNeeds::Manager, |brand| {
        if brand.images.len() >= MAX_ORG_IMAGES {
            return Err(ApiError::TooMany(MAX_ORG_IMAGES).into());
        }
        let added = BrandImage {
            id: Uuid::new_v4(),
            kind,
            image,
        };
        brand.images.push(added.clone());
        Ok(Edited {
            message: format!("Add the image {} to the brand centre", added.image.name),
            events: vec![brand_event(
                "brand.image_changed",
                actor,
                json!({ "action": "added", "image": image_detail(&added) }),
            )],
            answer: added,
        })
    })
    .await
}

/// Removes the image `id` from the brand centre for `actor`, with `brand.image_changed`
/// (`removed`). Its content is erased unless something else names or pins it.
///
/// # Errors
/// `403`; `404` for no such image; otherwise `503`.
pub(crate) async fn remove_image(
    state: &AppState,
    actor: &User,
    id: Uuid,
) -> Result<(), BrandError> {
    change_brand(state, actor, BrandNeeds::Manager, |brand| {
        let at = brand
            .images
            .iter()
            .position(|i| i.id == id)
            .ok_or(ApiError::NotFound)?;
        let old = brand.images.remove(at);
        // The organisation's default icon goes with its image.
        if brand.default_icon == Some(id) {
            brand.default_icon = None;
        }
        Ok(Edited {
            message: format!("Remove the image {} from the brand centre", old.image.name),
            events: vec![brand_event(
                "brand.image_changed",
                actor,
                json!({ "action": "removed", "image": image_detail(&old) }),
            )],
            answer: (),
        })
    })
    .await
}

/// Renames the image `id` of the brand centre to `name` (trimmed) for `actor`, with
/// `brand.image_changed` (`renamed`, and the name `before`). Its content stays the same. The
/// same name again changes nothing. Answers the image.
///
/// # Errors
/// `403`; `404` for no such image; `422 invalid_request` for a name that is empty, longer than
/// 120 characters or holds control or invisible characters; otherwise `503`.
pub(crate) async fn rename_image(
    state: &AppState,
    actor: &User,
    id: Uuid,
    name: &str,
) -> Result<BrandImage, BrandError> {
    if !allowed(state.db(), actor, BrandNeeds::Manager).await? {
        return Err(ApiError::Forbidden.into());
    }
    let name = clean_text(name)
        .filter(|n| (1..=MAX_IMAGE_NAME_CHARS).contains(&n.chars().count()))
        .ok_or(ApiError::InvalidRequest)?
        .to_owned();
    change_brand(state, actor, BrandNeeds::Manager, |brand| {
        let image = brand
            .images
            .iter_mut()
            .find(|i| i.id == id)
            .ok_or(ApiError::NotFound)?;
        let before = std::mem::replace(&mut image.image.name, name.clone());
        let renamed = image.clone();
        Ok(Edited {
            message: format!("Rename the brand image {before} to {name}"),
            events: vec![brand_event(
                "brand.image_changed",
                actor,
                json!({ "action": "renamed", "before": before, "image": image_detail(&renamed) }),
            )],
            answer: renamed,
        })
    })
    .await
}

/// Names the brand managers for `actor`, an enabled administrator: `users`, each an enabled
/// account, each once, in the order given, with `brand.managers_changed {from, to}`. Naming
/// the same managers again changes nothing.
///
/// # Errors
/// `403` for anyone else; `404 no_such_user` for an account that does not exist or is
/// disabled; `409 too_many {limit: 20}`; otherwise `503`.
pub(crate) async fn set_managers(
    state: &AppState,
    actor: &User,
    users: &[Uuid],
) -> Result<(), BrandError> {
    let db = state.db();
    if !allowed(db, actor, BrandNeeds::Admin).await? {
        return Err(ApiError::Forbidden.into());
    }
    let mut wanted: Vec<Uuid> = Vec::new();
    for id in users {
        if !wanted.contains(id) {
            wanted.push(*id);
        }
    }
    if wanted.len() > MAX_MANAGERS {
        return Err(ApiError::TooMany(MAX_MANAGERS).into());
    }
    for id in &wanted {
        let enabled = db
            .user(collab_core::ids::UserId(*id))
            .await?
            .is_some_and(|u| !u.disabled);
        if !enabled {
            return Err(ApiError::NoSuchUser.into());
        }
    }
    change_brand(state, actor, BrandNeeds::Admin, |brand| {
        let from = std::mem::replace(&mut brand.managers, wanted.clone());
        Ok(Edited {
            message: "Name the brand managers".to_owned(),
            events: vec![brand_event(
                "brand.managers_changed",
                actor,
                json!({ "from": from, "to": wanted }),
            )],
            answer: (),
        })
    })
    .await
}

// ---------------------------------------------------------------------------------------------
// Fonts

/// The font `font` as an event describes it.
fn font_detail(font: &BrandFont) -> Value {
    json!({
        "id": font.id,
        "name": font.font.name,
        "sha256": font.font.sha256,
        "media_type": font.font.media_type,
    })
}

/// A name typed for a font: trimmed, 1 to 120 characters with no control or invisible
/// characters.
///
/// # Errors
/// `422 invalid_request` otherwise.
fn typed_font_name(name: &str) -> Result<String, ApiError> {
    Ok(clean_text(name)
        .filter(|n| (1..=MAX_IMAGE_NAME_CHARS).contains(&n.chars().count()))
        .ok_or(ApiError::InvalidRequest)?
        .to_owned())
}

/// Adds the current version of the document `doc` to the brand centre as a font for `actor`, a
/// brand manager or administrator who can view it and confirms the organisation holds its
/// licence (see [`font_from_doc`]), named `name` or else after the document, with
/// `brand.font_changed` (`added`). Its content is pinned from then on. Answers the font.
///
/// # Errors
/// `403`; those of [`font_from_doc`]; `422 invalid_request` for a bad name; `409 too_many
/// {limit: 30}`; `409 image_gone` when the content was erased meanwhile; otherwise `503`.
pub(crate) async fn add_font(
    state: &AppState,
    actor: &User,
    doc: DocId,
    licence_confirmed: bool,
    name: Option<&str>,
) -> Result<BrandFont, BrandError> {
    if !allowed(state.db(), actor, BrandNeeds::Manager).await? {
        return Err(ApiError::Forbidden.into());
    }
    let mut font = font_from_doc(state, actor, doc, None, licence_confirmed).await?;
    if let Some(name) = name {
        font.name = typed_font_name(name)?;
    }
    change_brand(state, actor, BrandNeeds::Manager, |brand| {
        if brand.fonts.len() >= MAX_ORG_FONTS {
            return Err(ApiError::TooMany(MAX_ORG_FONTS).into());
        }
        let added = BrandFont {
            id: Uuid::new_v4(),
            font,
        };
        brand.fonts.push(added.clone());
        Ok(Edited {
            message: format!("Add the font {} to the brand centre", added.font.name),
            events: vec![brand_event(
                "brand.font_changed",
                actor,
                json!({ "action": "added", "font": font_detail(&added) }),
            )],
            answer: added,
        })
    })
    .await
}

/// Removes the font `id` from the brand centre for `actor`, with `brand.font_changed`
/// (`removed`). Packages and looks that hold a copy of it keep their copy; its content is
/// erased once nothing names or pins it.
///
/// # Errors
/// `403`; `404` for no such font; otherwise `503`.
pub(crate) async fn remove_font(
    state: &AppState,
    actor: &User,
    id: Uuid,
) -> Result<(), BrandError> {
    change_brand(state, actor, BrandNeeds::Manager, |brand| {
        let at = brand
            .fonts
            .iter()
            .position(|f| f.id == id)
            .ok_or(ApiError::NotFound)?;
        let old = brand.fonts.remove(at);
        Ok(Edited {
            message: format!("Remove the font {} from the brand centre", old.font.name),
            events: vec![brand_event(
                "brand.font_changed",
                actor,
                json!({ "action": "removed", "font": font_detail(&old) }),
            )],
            answer: (),
        })
    })
    .await
}

// ---------------------------------------------------------------------------------------------
// Font packages

/// Where a font of a package comes from, as sent.
#[derive(Clone, Debug)]
pub(crate) enum FontChoice {
    /// A font of the brand centre, by id.
    BrandFont(Uuid),
    /// The current version of a document, with the licence confirmed or not.
    Doc { doc: DocId, licence_confirmed: bool },
}

/// A change of a font package as sent: each field given is set (`Some(None)` removes a font).
/// A new package gives its name and roles.
#[derive(Clone, Debug, Default)]
pub(crate) struct PackageEdit {
    pub(crate) name: Option<String>,
    pub(crate) display: Option<Option<FontChoice>>,
    pub(crate) content: Option<Option<FontChoice>>,
    pub(crate) roles: Option<FontRoles>,
}

/// A [`PackageEdit`] with its fonts found.
pub(crate) struct ResolvedEdit {
    name: Option<String>,
    display: Option<Option<FontRef>>,
    content: Option<Option<FontRef>>,
    roles: Option<FontRoles>,
}

/// The fonts of `edit` found for `actor`: a brand font among the brand centre's (`org`); a
/// document only for a site's package (`site`), from its libraries or an asset library (see
/// [`font_from_doc`]).
///
/// # Errors
/// `InvalidFontPackage` for an unknown brand font, or a document for the brand centre; those
/// of [`font_from_doc`].
pub(crate) async fn resolve_edit(
    state: &AppState,
    actor: &User,
    org: &OrgBrand,
    site: Option<NodeId>,
    edit: PackageEdit,
) -> Result<ResolvedEdit, BrandError> {
    let mut fonts = Vec::new();
    for choice in [edit.display, edit.content] {
        let resolved = match choice {
            None => None,
            Some(None) => Some(None),
            Some(Some(FontChoice::BrandFont(id))) => {
                let found = org.fonts.iter().find(|f| f.id == id).ok_or_else(|| {
                    BrandError::InvalidFontPackage("there is no such brand font".into())
                })?;
                Some(Some(found.font.clone()))
            }
            Some(Some(FontChoice::Doc {
                doc,
                licence_confirmed,
            })) => {
                if site.is_none() {
                    return Err(BrandError::InvalidFontPackage(
                        "the brand centre's packages use the brand centre's fonts".into(),
                    ));
                }
                Some(Some(
                    font_from_doc(state, actor, doc, site, licence_confirmed).await?,
                ))
            }
        };
        fonts.push(resolved);
    }
    let mut fonts = fonts.into_iter();
    Ok(ResolvedEdit {
        name: edit.name,
        display: fonts.next().flatten(),
        content: fonts.next().flatten(),
        roles: edit.roles,
    })
}

/// `package` with `edit` applied, checked: the name trimmed, 1 to 60 characters with no
/// control or invisible characters, and every rule of [`FontPackage::problem`].
///
/// # Errors
/// `InvalidFontPackage` with the reason.
pub(crate) fn edited_package(
    mut package: FontPackage,
    edit: ResolvedEdit,
) -> Result<FontPackage, BrandError> {
    if let Some(name) = edit.name {
        let name = clean_text(&name).ok_or_else(|| {
            BrandError::InvalidFontPackage(
                "a package name holds no control or invisible characters".into(),
            )
        })?;
        if !(1..=MAX_THEME_NAME_CHARS).contains(&name.chars().count()) {
            return Err(BrandError::InvalidFontPackage(
                "a package name is 1 to 60 characters".into(),
            ));
        }
        name.clone_into(&mut package.name);
    }
    if let Some(display) = edit.display {
        package.display = display;
    }
    if let Some(content) = edit.content {
        package.content = content;
    }
    if let Some(roles) = edit.roles {
        package.roles = roles;
    }
    if let Some(problem) = package.problem() {
        return Err(BrandError::InvalidFontPackage(problem));
    }
    Ok(package)
}

/// A new package of `id` with nothing set yet: [`edited_package`] fills it.
pub(crate) fn blank_package(id: Uuid) -> FontPackage {
    FontPackage {
        id,
        name: String::new(),
        display: None,
        content: None,
        roles: FontRoles {
            title: FontRole::System,
            heading: FontRole::System,
            body: FontRole::System,
            interactive: FontRole::System,
        },
    }
}

/// Whether another package than `except` of `packages` is named `name`, ignoring case.
pub(crate) fn package_name_taken(
    packages: &[FontPackage],
    name: &str,
    except: Option<Uuid>,
) -> bool {
    let wanted = name.to_lowercase();
    packages
        .iter()
        .any(|p| Some(p.id) != except && p.name.to_lowercase() == wanted)
}

/// Adds a font package to the brand centre for `actor`, a brand manager or administrator, with
/// `brand.font_package_changed` (`created`). Its fonts are the brand centre's. Answers the
/// package.
///
/// # Errors
/// `403`; `422 invalid_font_package {reason}`; `409 name_taken`; `409 too_many {limit: 20}`;
/// otherwise `503`.
pub(crate) async fn create_package(
    state: &AppState,
    actor: &User,
    edit: PackageEdit,
) -> Result<FontPackage, BrandError> {
    let db = state.db();
    if !allowed(db, actor, BrandNeeds::Manager).await? {
        return Err(ApiError::Forbidden.into());
    }
    let org = db.org_brand().await?;
    let resolved = resolve_edit(state, actor, &org, None, edit).await?;
    let package = edited_package(blank_package(Uuid::new_v4()), resolved)?;
    change_brand(state, actor, BrandNeeds::Manager, |brand| {
        if package_name_taken(&brand.font_packages, &package.name, None) {
            return Err(ApiError::NameTaken.into());
        }
        if brand.font_packages.len() >= MAX_ORG_FONT_PACKAGES {
            return Err(ApiError::TooMany(MAX_ORG_FONT_PACKAGES).into());
        }
        brand.font_packages.push(package.clone());
        Ok(Edited {
            message: format!("Add the font package {} to the brand centre", package.name),
            events: vec![brand_event(
                "brand.font_package_changed",
                actor,
                json!({ "action": "created", "package": package }),
            )],
            answer: package,
        })
    })
    .await
}

/// Changes the font package `id` of the brand centre for `actor` with `edit`, with
/// `brand.font_package_changed` (`updated`, before and after). Looks that copied it keep their
/// copy. The same package again changes nothing. Answers the package.
///
/// # Errors
/// As [`create_package`], with `404` for no such package.
pub(crate) async fn change_package(
    state: &AppState,
    actor: &User,
    id: Uuid,
    edit: PackageEdit,
) -> Result<FontPackage, BrandError> {
    let db = state.db();
    if !allowed(db, actor, BrandNeeds::Manager).await? {
        return Err(ApiError::Forbidden.into());
    }
    let org = db.org_brand().await?;
    let resolved = resolve_edit(state, actor, &org, None, edit).await?;
    change_brand(state, actor, BrandNeeds::Manager, |brand| {
        let at = brand
            .font_packages
            .iter()
            .position(|p| p.id == id)
            .ok_or(ApiError::NotFound)?;
        let old = brand
            .font_packages
            .get(at)
            .cloned()
            .ok_or(ApiError::NotFound)?;
        let changed = edited_package(old.clone(), resolved)?;
        if package_name_taken(&brand.font_packages, &changed.name, Some(id)) {
            return Err(ApiError::NameTaken.into());
        }
        if let Some(slot) = brand.font_packages.get_mut(at) {
            *slot = changed.clone();
        }
        Ok(Edited {
            message: format!("Change the font package {} of the brand centre", old.name),
            events: vec![brand_event(
                "brand.font_package_changed",
                actor,
                json!({ "action": "updated", "before": old, "package": changed }),
            )],
            answer: changed,
        })
    })
    .await
}

/// Deletes the font package `id` from the brand centre for `actor`, with
/// `brand.font_package_changed` (`deleted`). Looks that copied it keep their copy.
///
/// # Errors
/// `403`; `404` for no such package; otherwise `503`.
pub(crate) async fn delete_package(
    state: &AppState,
    actor: &User,
    id: Uuid,
) -> Result<(), BrandError> {
    change_brand(state, actor, BrandNeeds::Manager, |brand| {
        let at = brand
            .font_packages
            .iter()
            .position(|p| p.id == id)
            .ok_or(ApiError::NotFound)?;
        let old = brand.font_packages.remove(at);
        Ok(Edited {
            message: format!("Delete the font package {} from the brand centre", old.name),
            events: vec![brand_event(
                "brand.font_package_changed",
                actor,
                json!({ "action": "deleted", "package": old }),
            )],
            answer: (),
        })
    })
    .await
}

// ---------------------------------------------------------------------------------------------
// The policy and the default icon

/// A change of the brand policy as sent: each limit given is set.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PolicyEdit {
    pub(crate) hide_builtin_themes: Option<bool>,
    pub(crate) custom_colours: Option<bool>,
    pub(crate) site_themes: Option<bool>,
    pub(crate) site_images: Option<bool>,
    pub(crate) site_fonts: Option<bool>,
}

/// Sets the limits on site owners for `actor`, an enabled administrator, with
/// `brand.policy_changed {before, after}`. Looks already stored are never changed; the limits
/// apply to the next change. The same policy again changes nothing. Answers the policy.
///
/// # Errors
/// `403` for anyone else; otherwise `503`.
pub(crate) async fn set_policy(
    state: &AppState,
    actor: &User,
    edit: PolicyEdit,
) -> Result<BrandPolicy, BrandError> {
    change_brand(state, actor, BrandNeeds::Admin, |brand| {
        let before = brand.policy;
        let set = |given: Option<bool>, now: bool| given.unwrap_or(now);
        let after = BrandPolicy {
            hide_builtin_themes: set(edit.hide_builtin_themes, before.hide_builtin_themes),
            custom_colours: set(edit.custom_colours, before.custom_colours),
            site_themes: set(edit.site_themes, before.site_themes),
            site_images: set(edit.site_images, before.site_images),
            site_fonts: set(edit.site_fonts, before.site_fonts),
        };
        brand.policy = after;
        Ok(Edited {
            message: "Change the brand policy".to_owned(),
            events: vec![brand_event(
                "brand.policy_changed",
                actor,
                json!({ "before": before, "after": after }),
            )],
            answer: after,
        })
    })
    .await
}

/// Makes the brand image `image` the organisation's default tab icon (`None`: no default) for
/// `actor`, a brand manager or administrator, with `brand.image_changed` (`default_icon`).
///
/// # Errors
/// `403`; `404` for no such image; `422 not_an_icon` for an image that is not of kind `icon`
/// or cannot be an icon (see [`icon_problem`]); otherwise `503`.
pub(crate) async fn set_default_icon(
    state: &AppState,
    actor: &User,
    image: Option<Uuid>,
) -> Result<(), BrandError> {
    change_brand(state, actor, BrandNeeds::Manager, |brand| {
        if let Some(id) = image {
            let found = brand
                .images
                .iter()
                .find(|i| i.id == id)
                .ok_or(ApiError::NotFound)?;
            if found.kind != ImageKind::Icon || icon_problem(&found.image).is_some() {
                return Err(ApiError::NotAnIcon.into());
            }
        }
        let before = std::mem::replace(&mut brand.default_icon, image);
        Ok(Edited {
            message: "Set the organisation's default icon".to_owned(),
            events: vec![brand_event(
                "brand.image_changed",
                actor,
                json!({ "action": "default_icon", "before": before, "image": image }),
            )],
            answer: (),
        })
    })
    .await
}

/// Who `actor` is to the brand policy: an enabled administrator, a brand manager, or else a
/// site owner (the only one the policy limits).
pub(crate) async fn actor_kind(db: &Db, actor: &User) -> Result<Actor, DbError> {
    if allowed(db, actor, BrandNeeds::Admin).await? {
        Ok(Actor::Admin)
    } else if db.can_manage_brand(actor.id).await? {
        Ok(Actor::BrandManager)
    } else {
        Ok(Actor::Owner)
    }
}

// ---------------------------------------------------------------------------------------------
// Reconcile

/// The pins `brand` asks for: each content its images, fonts and packages name, with its
/// type, by hash.
fn wanted_pins(brand: &OrgBrand) -> Vec<(String, String)> {
    let mut pins: Vec<(String, String)> = org_pins(brand)
        .into_iter()
        .map(|pin| (pin.sha256.to_owned(), pin.media_type.to_owned()))
        .collect();
    pins.sort();
    pins.dedup_by(|a, b| a.0 == b.0);
    pins
}

/// An event of a repair from Git.
fn repaired(kind: &'static str) -> NewEvent {
    NewEvent {
        kind,
        actor: None,
        subject: None,
        document: None,
        node: None,
        detail: json!({ "repaired": true }),
    }
}

/// Brings PostgreSQL's mirror of the brand centre, and the content it pins, up to date with
/// Git: when the row or the pins differ, they are rewritten from `org/brand.json` (an empty
/// centre if there is none), creating the content rows of pinned content that has none, with
/// `"repaired": true` events of each part that differed (`brand.managers_changed`,
/// `brand.theme_changed`, `brand.image_changed`). Answers how many rows were rewritten (0 or
/// 1). Run after the site records, as a site's look will pin images too.
///
/// # Errors
/// The Git error if the record cannot be read (`Unsupported` for a store without it), or the
/// database error.
pub(crate) async fn reconcile(store: Arc<dyn Checkpointer>, db: &Db) -> Result<usize, TreeError> {
    let record = read_brand(&store).await?;
    let row = db.org_brand().await?;
    let pins = db.brand_pins(BRAND_CENTRE_OWNER).await?;
    if row == record && pins == wanted_pins(&record) {
        return Ok(0);
    }
    let mut events = Vec::new();
    if row.managers != record.managers {
        events.push(repaired("brand.managers_changed"));
    }
    if row.themes != record.themes {
        events.push(repaired("brand.theme_changed"));
    }
    if row.images != record.images
        || row.default_icon != record.default_icon
        || pins != wanted_pins(&record)
    {
        events.push(repaired("brand.image_changed"));
    }
    if row.fonts != record.fonts {
        events.push(repaired("brand.font_changed"));
    }
    if row.font_packages != record.font_packages {
        events.push(repaired("brand.font_package_changed"));
    }
    if row.policy != record.policy {
        events.push(repaired("brand.policy_changed"));
    }
    db.apply_org_brand(BrandChange {
        record: &record,
        actor: None,
        needs: BrandNeeds::Admin,
        events,
    })
    .await
    .map_err(|e| match e {
        AccessError::Db(e) | AccessError::Unjudged(e) => TreeError::Db(e),
        other => TreeError::Db(DbError::new(other)),
    })?;
    Ok(1)
}

/// Reconciles the brand centre from Git once; whether it is now in step.
async fn repair(state: &AppState) -> bool {
    match reconcile(Arc::clone(state.checkpointer()), state.db()).await {
        Ok(_) => true,
        Err(e) => {
            eprintln!("sharecraft-server: could not repair the brand centre: {e}");
            false
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn safe_svgs_pass() {
        for svg in [
            "<svg/>",
            "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 10 10\"><rect width=\"10\" \
             height=\"10\" fill=\"#3156c9\"/></svg>",
            "\u{feff}<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!-- a logo -->\n<svg \
             xmlns:xlink=\"http://www.w3.org/1999/xlink\"><defs><linearGradient id=\"g\"/>\
             </defs><use xlink:href=\"#g\"/><rect fill=\"url(#g)\"/><text>A &amp; B</text>\
             </svg>\n",
            "<svg><style><![CDATA[rect { fill: url('#g') }]]></style></svg>",
        ] {
            assert!(safe_svg(svg.as_bytes()), "{svg}");
        }
    }

    #[test]
    fn unsafe_or_malformed_svgs_refused() {
        for svg in [
            "",
            "<svg>",
            "<svg></g>",
            "<html><svg/></html>",
            "<svg/><svg/>",
            "text<svg/>",
            "<!DOCTYPE svg [<!ENTITY x \"y\">]><svg/>",
            "<svg><script>alert(1)</script></svg>",
            "<svg><SCRIPT>alert(1)</SCRIPT></svg>",
            "<svg><svg:script>alert(1)</svg:script></svg>",
            "<svg onload=\"alert(1)\"/>",
            "<svg><rect ONCLICK='x'/></svg>",
            "<svg><image href=\"https://evil.example/x.png\"/></svg>",
            "<svg><use xlink:href=\"data:image/svg+xml,x\"/></svg>",
            "<svg><a href=\"javascript:alert(1)\"><text>x</text></a></svg>",
            "<svg><foreignObject><div/></foreignObject></svg>",
            "<svg><rect style=\"fill:url(https://evil.example/)\"/></svg>",
            "<svg><style>@import url(x.css);</style></svg>",
            "<svg><style>rect{fill:url( \"http://x\" )}</style></svg>",
            "<svg width=10/>",
            "<svg a=\"1\"b=\"2\"/>",
            "<svg><rect x=\"<\"/></svg>",
            "<svg/><?pi?>",
        ] {
            assert!(!safe_svg(svg.as_bytes()), "{svg}");
        }
        assert!(!safe_svg(&[0xff, 0xfe, b'<']));
    }

    #[test]
    fn processing_instructions_refused_but_the_declaration() {
        assert!(safe_svg(b"<?xml version=\"1.0\"?><svg/>"));
        for svg in [
            "<?xml-stylesheet href=\"https://evil.example/a.css\"?><svg/>",
            "<?xml-stylesheet type=\"text/css\" href=\"#x\"?><svg/>",
            "<?xml version=\"1.0\"?><?xml-stylesheet href=\"https://evil.example/\"?><svg/>",
            "<?xml version=\"1.0\"?><?xml version=\"1.0\"?><svg/>",
            "<!-- first --><?xml version=\"1.0\"?><svg/>",
            "<?php echo 1 ?><svg/>",
            "<svg><?pi data?></svg>",
            "<?xml version=\"1.0\"",
        ] {
            assert!(!safe_svg(svg.as_bytes()), "{svg}");
        }
    }

    #[test]
    fn animations_refused() {
        for svg in [
            "<svg><a><set attributeName=\"href\" to=\"javascript:alert(1)\"/><text>x</text>\
             </a></svg>",
            "<svg><a><animate attributeName=\"xlink:href\" values=\"javascript:alert(1)\"/>\
             </a></svg>",
            "<svg><rect><animate attributeName=\"onclick\" to=\"alert(1)\"/></rect></svg>",
            "<svg><rect><animate attributeName=\"x\" from=\"0\" to=\"10\" dur=\"1s\"/></rect>\
             </svg>",
            "<svg><rect><animateTransform attributeName=\"transform\" type=\"rotate\"/></rect>\
             </svg>",
            "<svg><rect><animateMotion path=\"M0 0\"/></rect></svg>",
            "<svg><rect><discard begin=\"1s\"/></rect></svg>",
            "<svg><svg:set xmlns:svg=\"http://www.w3.org/2000/svg\" attributeName=\"href\"/>\
             </svg>",
        ] {
            assert!(!safe_svg(svg.as_bytes()), "{svg}");
        }
    }

    #[test]
    fn css_escapes_and_other_loaders_refused() {
        for svg in [
            "<svg><style>rect{fill:u\\72l(https://evil.example/)}</style></svg>",
            "<svg><rect style=\"fill:u\\rl(https://evil.example/)\"/></svg>",
            "<svg><rect style=\"background:image-set('https://evil.example/a.png' 1x)\"/></svg>",
            "<svg><style>@font-face{src(https://evil.example/f.woff)}</style></svg>",
            "<svg><rect style=\"fill:u&#114;l(https://evil.example/)\"/></svg>",
            "<svg><style>rect{fill:IMAGE-SET(\"x.png\" 1x)}</style></svg>",
        ] {
            assert!(!safe_svg(svg.as_bytes()), "{svg}");
        }
        // A local url() is still fine.
        assert!(safe_svg(b"<svg><rect style=\"fill:url(#g)\"/></svg>"));
    }

    #[test]
    fn style_text_checked_whole() {
        for svg in [
            "<svg><style>rect{fill:u<!---->rl(https://evil.example/)}</style></svg>",
            "<svg><style>rect{fill:u<![CDATA[rl(https://evil.example/)]]>}</style></svg>",
            "<svg><style><![CDATA[rect{fill:ur]]><![CDATA[l(https://evil.example/)}]]>\
             </style></svg>",
            "<svg><style>@im<!---->port 'https://evil.example/a.css';</style></svg>",
            "<svg><style>@im<![CDATA[port 'https://evil.example/a.css';]]></style></svg>",
            "<svg><style>rect{fill:url<!-- x -->(https://evil.example/)}</style></svg>",
            "<svg><style>rect{fill:url(<!---->https://evil.example/)}</style></svg>",
        ] {
            assert!(!safe_svg(svg.as_bytes()), "{svg}");
        }
        // A split local url() is still fine.
        assert!(safe_svg(
            b"<svg><style>rect{fill:u<!---->rl(#g)}</style></svg>"
        ));
    }

    #[test]
    fn images_judged_by_content() {
        assert_eq!(judge_image(b"<svg/>").unwrap(), ("image/svg+xml", None));
        assert_eq!(
            judge_image(b"<html><script>alert(1)</script></html>"),
            Err(ApiError::NotAnImage)
        );
        assert_eq!(
            judge_image(b"\x89PNG\r\n\x1a\nbroken"),
            Err(ApiError::NotAnImage)
        );
    }

    #[test]
    fn theme_input_checked() {
        let input = ThemeInput {
            name: "  Ours ".into(),
            primary: "#3156C9".into(),
            secondary: "#233239".into(),
            ..ThemeInput::default()
        };
        let theme = checked_theme(&input).unwrap();
        assert_eq!(theme.name, "Ours");
        assert_eq!(theme.primary.to_string(), "#3156c9");
        for bad in [
            ThemeInput {
                primary: "red;background:url(x)".into(),
                ..input.clone()
            },
            ThemeInput {
                name: String::new(),
                ..input.clone()
            },
            ThemeInput {
                name: "a\u{202e}b".into(),
                ..input.clone()
            },
        ] {
            assert!(matches!(
                checked_theme(&bad),
                Err(BrandError::InvalidTheme(_))
            ));
        }
        assert!(matches!(
            checked_theme(&ThemeInput {
                primary: "#cccccc".into(),
                ..input
            }),
            Err(BrandError::LowContrast(_))
        ));
    }
}
