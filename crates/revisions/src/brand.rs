//! Branding records and the colour rules behind them: themes, brand images, a site's look, the
//! organisation's brand centre, WCAG contrast checks and the derivation of dark colours.
//!
//! The brand centre is recorded at `org/brand.json` ([`OrgBrand`]); a site's look is a field of
//! its site record. This module is pure: it holds the types, their limits and the colour maths,
//! and touches neither Git nor the database. The same checks exist in JavaScript
//! (`web/look.js`) for live feedback while a theme is edited; this copy is authoritative, and
//! both are tested against one table, `web/look-vectors.json`.
//!
//! Colours are `#rrggbb`, read in either case and kept in lower case. Contrast is the WCAG 2.x
//! ratio of relative luminances and must reach [`MIN_CONTRAST`] for text on every page colour a
//! theme meets. See [`check_theme`] for the six checks.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The least contrast ratio text needs (WCAG AA for normal text).
pub const MIN_CONTRAST: f64 = 4.5;
/// The most themes the brand centre holds.
pub const MAX_ORG_THEMES: usize = 50;
/// The most brand images the brand centre holds.
pub const MAX_ORG_IMAGES: usize = 100;
/// The most brand managers.
pub const MAX_MANAGERS: usize = 20;
/// The most themes of one site's own.
pub const MAX_SITE_THEMES: usize = 20;
/// The longest theme name, in characters.
pub const MAX_THEME_NAME_CHARS: usize = 60;
/// The longest logo description, in characters.
pub const MAX_LOGO_ALT_CHARS: usize = 120;
/// The longest footer name, in characters.
pub const MAX_FOOTER_NAME_CHARS: usize = 80;
/// The longest footer group heading, in characters.
pub const MAX_FOOTER_HEADING_CHARS: usize = 40;
/// The longest footer link label, in characters.
pub const MAX_FOOTER_LABEL_CHARS: usize = 60;
/// The most links in one footer group.
pub const MAX_FOOTER_LINKS: usize = 8;
/// The most groups in an extended footer.
pub const MAX_FOOTER_GROUPS: usize = 4;
/// The longest footer link address, in characters.
pub const MAX_LINK_CHARS: usize = 2000;
/// The longest image name, in characters.
pub const MAX_IMAGE_NAME_CHARS: usize = 120;
/// The largest logo, thumbnail or icon, in bytes.
pub const MAX_LOGO_BYTES: u64 = 1024 * 1024;
/// The largest background image, in bytes.
pub const MAX_BACKGROUND_BYTES: u64 = 5 * 1024 * 1024;
/// The most pixels along either side of a raster image.
pub const MAX_IMAGE_SIDE: u32 = 8000;
/// The media types a brand image may have, judged by the content and recorded with it.
pub const IMAGE_MEDIA_TYPES: [&str; 5] = [
    "image/png",
    "image/jpeg",
    "image/webp",
    "image/gif",
    "image/svg+xml",
];
/// The format version of `org/brand.json`; currently 1.
pub const BRAND_SCHEMA: u32 = 1;

/// The page colours of `web/style.css` that text must read on.
const LIGHT_PAPER: Colour = Colour::rgb(0xff, 0xff, 0xff);
const LIGHT_BACKGROUND: Colour = Colour::rgb(0xf5, 0xf7, 0xf8);
const DARK_PAPER: Colour = Colour::rgb(0x20, 0x2a, 0x2e);
const DARK_BACKGROUND: Colour = Colour::rgb(0x14, 0x1c, 0x20);
/// Button and header text is white or this, whichever reads better.
const WHITE: Colour = Colour::rgb(0xff, 0xff, 0xff);
const INK: Colour = Colour::rgb(0x14, 0x1c, 0x20);

/// Tolerance for float noise when a lightness is a whole number of percent.
const EDGE: f64 = 1e-9;

// ---------------------------------------------------------------------------------------------
// Colours

/// A colour written `#rrggbb`: six hexadecimal digits in either case, nothing else. Always
/// written back in lower case.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Colour {
    r: u8,
    g: u8,
    b: u8,
}

/// The text was not `#rrggbb`.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("a colour is written #rrggbb")]
pub struct ParseColourError;

impl Colour {
    /// The colour with these red, green and blue values.
    #[must_use]
    pub const fn rgb(r: u8, g: u8, b: u8) -> Colour {
        Colour { r, g, b }
    }

    /// Reads `#rrggbb`, in upper or lower case. Anything else (names, `#rgb`, spaces, extra
    /// text, CSS functions) is refused.
    pub fn parse(text: &str) -> Result<Colour, ParseColourError> {
        let digits = text.strip_prefix('#').ok_or(ParseColourError)?;
        let mut values = [0u8; 3];
        let mut chars = digits.chars();
        for value in &mut values {
            let high = chars
                .next()
                .and_then(|c| c.to_digit(16))
                .ok_or(ParseColourError)?;
            let low = chars
                .next()
                .and_then(|c| c.to_digit(16))
                .ok_or(ParseColourError)?;
            *value = u8::try_from(high * 16 + low).map_err(|_| ParseColourError)?;
        }
        if chars.next().is_some() {
            return Err(ParseColourError);
        }
        let [r, g, b] = values;
        Ok(Colour { r, g, b })
    }

    /// WCAG 2.x relative luminance, 0 (black) to 1 (white).
    #[must_use]
    pub fn luminance(self) -> f64 {
        fn channel(v: u8) -> f64 {
            let c = f64::from(v) / 255.0;
            if c <= 0.03928 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        }
        0.2126 * channel(self.r) + 0.7152 * channel(self.g) + 0.0722 * channel(self.b)
    }
}

impl fmt::Display for Colour {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
}

impl FromStr for Colour {
    type Err = ParseColourError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Colour::parse(s)
    }
}

impl TryFrom<String> for Colour {
    type Error = ParseColourError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        Colour::parse(&s)
    }
}

impl From<Colour> for String {
    fn from(c: Colour) -> String {
        c.to_string()
    }
}

impl Serialize for Colour {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Colour {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Colour::parse(&text).map_err(serde::de::Error::custom)
    }
}

/// The WCAG 2.x contrast ratio of two colours, from 1 (identical) to 21 (black on white). It is
/// the same whichever is the text.
#[must_use]
pub fn contrast(a: Colour, b: Colour) -> f64 {
    let (la, lb) = (a.luminance(), b.luminance());
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// Hue, lightness and saturation, each 0 to 1.
#[derive(Clone, Copy)]
struct Hls {
    h: f64,
    l: f64,
    s: f64,
}

impl Hls {
    fn of(c: Colour) -> Hls {
        let (r, g, b) = (
            f64::from(c.r) / 255.0,
            f64::from(c.g) / 255.0,
            f64::from(c.b) / 255.0,
        );
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let l = (min + max) / 2.0;
        if (max - min).abs() < f64::EPSILON {
            return Hls { h: 0.0, l, s: 0.0 };
        }
        let d = max - min;
        let s = if l <= 0.5 {
            d / (max + min)
        } else {
            d / (2.0 - max - min)
        };
        let (rc, gc, bc) = ((max - r) / d, (max - g) / d, (max - b) / d);
        let h = if (r - max).abs() < f64::EPSILON {
            bc - gc
        } else if (g - max).abs() < f64::EPSILON {
            2.0 + rc - bc
        } else {
            4.0 + gc - rc
        };
        Hls {
            h: (h / 6.0).rem_euclid(1.0),
            l,
            s,
        }
    }

    /// The colour with these values, each channel rounded to the nearest whole number.
    fn colour(self) -> Colour {
        fn ramp(m1: f64, m2: f64, hue: f64) -> f64 {
            let h = hue.rem_euclid(1.0);
            if h < 1.0 / 6.0 {
                m1 + (m2 - m1) * h * 6.0
            } else if h < 0.5 {
                m2
            } else if h < 2.0 / 3.0 {
                m1 + (m2 - m1) * (2.0 / 3.0 - h) * 6.0
            } else {
                m1
            }
        }
        fn byte(v: f64) -> u8 {
            (v * 255.0).round().clamp(0.0, 255.0) as u8
        }
        let Hls { h, l, s } = self;
        if s == 0.0 {
            let v = byte(l);
            return Colour::rgb(v, v, v);
        }
        let m2 = if l <= 0.5 {
            l * (1.0 + s)
        } else {
            l + s - l * s
        };
        let m1 = 2.0 * l - m2;
        Colour::rgb(
            byte(ramp(m1, m2, h + 1.0 / 3.0)),
            byte(ramp(m1, m2, h)),
            byte(ramp(m1, m2, h - 1.0 / 3.0)),
        )
    }

    /// This colour with its lightness moved by `steps` percent.
    fn shifted(self, steps: i32) -> Colour {
        let l = ((self.l + f64::from(steps) / 100.0) * 1e9).round() / 1e9;
        Hls { l, ..self }.colour()
    }

    /// Whether moving the lightness by `steps` percent stays within 0 to 100%.
    fn in_range(self, steps: i32) -> bool {
        let l = self.l + f64::from(steps) / 100.0;
        (-EDGE..=1.0 + EDGE).contains(&l)
    }
}

/// The better of white and the dark ink on `c`, with its ratio.
fn best_ink(c: Colour) -> (Colour, f64) {
    let (white, dark) = (contrast(WHITE, c), contrast(INK, c));
    if white >= dark {
        (WHITE, white)
    } else {
        (INK, dark)
    }
}

/// The lower ratio of `c` on the two light page colours.
fn on_light(c: Colour) -> f64 {
    contrast(c, LIGHT_PAPER).min(contrast(c, LIGHT_BACKGROUND))
}

/// The lower ratio of `c` on the two dark page colours.
fn on_dark(c: Colour) -> f64 {
    contrast(c, DARK_PAPER).min(contrast(c, DARK_BACKGROUND))
}

/// A light primary reads as text on both light page colours and carries white button text.
fn light_primary_ok(c: Colour) -> bool {
    on_light(c) >= MIN_CONTRAST && contrast(WHITE, c) >= MIN_CONTRAST
}

/// A dark primary reads as text on both dark page colours and carries button text.
fn dark_primary_ok(c: Colour) -> bool {
    on_dark(c) >= MIN_CONTRAST && best_ink(c).1 >= MIN_CONTRAST
}

/// A header colour carries white or dark text.
fn header_ok(c: Colour) -> bool {
    best_ink(c).1 >= MIN_CONTRAST
}

/// Which colour of a theme is being derived.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// The accent: links, buttons, the focus ring.
    Primary,
    /// The background of a strong header or footer.
    Secondary,
}

/// The dark-mode colour for the light colour `c`, keeping its hue and saturation in HSL.
///
/// For [`Role::Primary`] the lightness rises in steps of 1% (starting from its own) until the
/// colour reads on both dark page colours and carries button text. For [`Role::Secondary`] the
/// colour is kept if header text reads on it, and otherwise the lightness falls in steps of 1%
/// until it does. `None` if no step passes.
#[must_use]
pub fn derive_dark(c: Colour, role: Role) -> Option<Colour> {
    let hls = Hls::of(c);
    match role {
        Role::Primary => (0..=100)
            .take_while(|&k| hls.in_range(k))
            .map(|k| hls.shifted(k))
            .find(|&d| dark_primary_ok(d)),
        Role::Secondary => {
            if header_ok(c) {
                return Some(c);
            }
            (1..=100)
                .take_while(|&k| hls.in_range(-k))
                .map(|k| hls.shifted(-k))
                .find(|&d| header_ok(d))
        }
    }
}

/// The colour with the same hue and saturation as `c` whose lightness is nearest to its own and
/// for which `ok` holds, searched 1% at a time, darker first on a tie.
fn suggest(c: Colour, ok: fn(Colour) -> bool) -> Option<Colour> {
    let hls = Hls::of(c);
    (1..=100)
        .flat_map(|k| [-k, k])
        .filter(|&step| hls.in_range(step))
        .map(|step| hls.shifted(step))
        .find(|&d| ok(d))
}

// ---------------------------------------------------------------------------------------------
// Themes

/// A colour theme: what a site, the brand centre or a built-in theme offers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Theme {
    /// The theme's name: 1 to 60 characters.
    pub name: String,
    /// The accent: links, buttons, the focus ring and the bar stripe.
    pub primary: Colour,
    /// The background of a strong header or footer.
    pub secondary: Colour,
    /// The accent in dark mode; derived from `primary` when left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_dark: Option<Colour>,
    /// The strong background in dark mode; derived from `secondary` when left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secondary_dark: Option<Colour>,
}

impl Theme {
    /// Why the theme cannot be stored, if so: its name. (Colours are well-formed by type, and
    /// readable by [`check_theme`].)
    #[must_use]
    pub fn problem(&self) -> Option<String> {
        text_problem("theme name", &self.name, 1, MAX_THEME_NAME_CHARS)
    }
}

/// A built-in theme: a name and light colours; the dark ones are derived.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuiltInTheme {
    /// The identifier, `builtin:<name>`.
    pub id: &'static str,
    /// The display name.
    pub name: &'static str,
    /// The accent.
    pub primary: Colour,
    /// The strong header and footer background.
    pub secondary: Colour,
}

impl BuiltInTheme {
    /// The theme, with its dark colours left to be derived.
    #[must_use]
    pub fn theme(&self) -> Theme {
        Theme {
            name: self.name.to_owned(),
            primary: self.primary,
            secondary: self.secondary,
            primary_dark: None,
            secondary_dark: None,
        }
    }
}

const SHARECRAFT: BuiltInTheme = BuiltInTheme {
    id: "builtin:sharecraft",
    name: "ShareCraft",
    primary: Colour::rgb(0x31, 0x56, 0xc9),
    secondary: Colour::rgb(0x23, 0x32, 0x39),
};

/// The themes every installation has. Each passes every check of [`check_theme`].
pub const BUILT_IN_THEMES: [BuiltInTheme; 6] = [
    SHARECRAFT,
    BuiltInTheme {
        id: "builtin:harbour",
        name: "Harbour",
        primary: Colour::rgb(0x1f, 0x5f, 0x8b),
        secondary: Colour::rgb(0x16, 0x34, 0x47),
    },
    BuiltInTheme {
        id: "builtin:plum",
        name: "Plum",
        primary: Colour::rgb(0x7a, 0x3e, 0x8f),
        secondary: Colour::rgb(0x3b, 0x22, 0x44),
    },
    BuiltInTheme {
        id: "builtin:rust",
        name: "Rust",
        primary: Colour::rgb(0xa3, 0x43, 0x1c),
        secondary: Colour::rgb(0x40, 0x26, 0x1b),
    },
    BuiltInTheme {
        id: "builtin:forest",
        name: "Forest",
        primary: Colour::rgb(0x2f, 0x6b, 0x3a),
        secondary: Colour::rgb(0x1f, 0x33, 0x24),
    },
    BuiltInTheme {
        id: "builtin:graphite",
        name: "Graphite",
        primary: Colour::rgb(0x4a, 0x55, 0x60),
        secondary: Colour::rgb(0x1f, 0x26, 0x2c),
    },
];

/// One of the checks of [`check_theme`]. Serialised in snake case.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Check {
    /// The accent as text on both light page colours (the lower ratio).
    PrimaryOnLight,
    /// White button text on the accent.
    ButtonOnPrimary,
    /// The dark accent as text on both dark page colours (the lower ratio).
    PrimaryDarkOnDark,
    /// Button text (white or dark ink, the higher ratio) on the dark accent.
    ButtonOnPrimaryDark,
    /// Header text (white or dark ink, the higher ratio) on the strong background.
    HeaderOnSecondary,
    /// Header text on the dark strong background.
    HeaderOnSecondaryDark,
}

/// A pair of colours that is not readable.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FailedPair {
    /// Which check failed.
    pub check: Check,
    /// The ratio reached, rounded to 2 decimals.
    pub ratio: f64,
    /// The nearest passing shade (same hue and saturation, nearest lightness) of the colour that
    /// can be changed, or `None` if no shade passes or the colour is derived.
    pub suggestion: Option<Colour>,
}

/// The colours a page sets for one mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Palette {
    /// The accent: links, button fill, the focus ring and the bar stripe.
    pub accent: Colour,
    /// The text on an accent button: white or the dark ink.
    pub accent_ink: Colour,
    /// The background of a strong header or footer.
    pub header_bg: Colour,
    /// The text on it: white or the dark ink.
    pub header_ink: Colour,
}

/// A theme that passed every check, with the colours for both modes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedTheme {
    /// Light mode.
    pub light: Palette,
    /// Dark mode.
    pub dark: Palette,
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// Checks that a theme is readable and works out its colours.
///
/// All of these must reach 4.5:1: the primary on light paper (`#ffffff`) and on the light
/// background (`#f5f7f8`), reported as `primary_on_light` with the lower ratio; white on the
/// primary (`button_on_primary`); the dark primary on dark paper (`#202a2e`) and background
/// (`#141c20`), `primary_dark_on_dark`; the better of white and `#141c20` on the dark primary
/// (`button_on_primary_dark`); and that better text on the secondary (`header_on_secondary`)
/// and on the dark secondary (`header_on_secondary_dark`). A dark colour left out is derived by
/// [`derive_dark`].
///
/// On failure, every failing pair is returned in that order. A light primary or either
/// secondary, and an explicit dark colour, also get the nearest passing shade as a suggestion;
/// a derived colour gets none (change the light one).
///
/// `button_on_primary` and `button_on_primary_dark` cannot fail on their own: the light
/// background is darker than white, so `primary_on_light` always fails with the first; and a
/// colour that reads on the dark paper always carries the dark ink.
pub fn check_theme(theme: &Theme) -> Result<ResolvedTheme, Vec<FailedPair>> {
    let mut failed = Vec::new();
    let mut fail = |check: Check, ratio: f64, suggestion: Option<Colour>| {
        failed.push(FailedPair {
            check,
            ratio: round2(ratio),
            suggestion,
        });
    };
    let p = theme.primary;
    let s = theme.secondary;

    if on_light(p) < MIN_CONTRAST {
        fail(
            Check::PrimaryOnLight,
            on_light(p),
            suggest(p, light_primary_ok),
        );
    }
    let button = contrast(WHITE, p);
    if button < MIN_CONTRAST {
        fail(Check::ButtonOnPrimary, button, suggest(p, light_primary_ok));
    }

    let primary_dark = theme.primary_dark.or_else(|| derive_dark(p, Role::Primary));
    let mut dark_accent = None;
    match primary_dark {
        Some(d) => {
            let own = theme.primary_dark.map(|_| d);
            if on_dark(d) < MIN_CONTRAST {
                fail(
                    Check::PrimaryDarkOnDark,
                    on_dark(d),
                    own.and_then(|c| suggest(c, dark_primary_ok)),
                );
            }
            let (_, ratio) = best_ink(d);
            if ratio < MIN_CONTRAST {
                fail(
                    Check::ButtonOnPrimaryDark,
                    ratio,
                    own.and_then(|c| suggest(c, dark_primary_ok)),
                );
            }
            dark_accent = Some(d);
        }
        None => fail(Check::PrimaryDarkOnDark, on_dark(p), None),
    }

    let (light_ink, light_ratio) = best_ink(s);
    if light_ratio < MIN_CONTRAST {
        fail(Check::HeaderOnSecondary, light_ratio, suggest(s, header_ok));
    }
    let secondary_dark = theme
        .secondary_dark
        .or_else(|| derive_dark(s, Role::Secondary));
    let mut dark_header = None;
    match secondary_dark {
        Some(d) => {
            let (_, ratio) = best_ink(d);
            if ratio < MIN_CONTRAST {
                let own = theme.secondary_dark.map(|_| d);
                fail(
                    Check::HeaderOnSecondaryDark,
                    ratio,
                    own.and_then(|c| suggest(c, header_ok)),
                );
            }
            dark_header = Some(d);
        }
        None => fail(Check::HeaderOnSecondaryDark, light_ratio, None),
    }

    match (failed.is_empty(), dark_accent, dark_header) {
        (true, Some(accent), Some(header)) => Ok(ResolvedTheme {
            light: Palette {
                accent: p,
                accent_ink: WHITE,
                header_bg: s,
                header_ink: light_ink,
            },
            dark: Palette {
                accent,
                accent_ink: best_ink(accent).0,
                header_bg: header,
                header_ink: best_ink(header).0,
            },
        }),
        _ => Err(failed),
    }
}

// ---------------------------------------------------------------------------------------------
// Text and links

/// Whether `c` is an invisible format character (Unicode general category `Cf`): the direction
/// controls, zero-width characters, the soft hyphen, tag characters and the like. This is the
/// list `records::tree::is_format_char` uses for node names; `revisions` cannot depend on
/// `records`, so it is repeated here.
fn is_format_char(c: char) -> bool {
    matches!(
        u32::from(c),
        0x00AD
            | 0x0600..=0x0605
            | 0x061C
            | 0x06DD
            | 0x070F
            | 0x0890..=0x0891
            | 0x08E2
            | 0x180E
            | 0x200B..=0x200F
            | 0x202A..=0x202E
            | 0x2060..=0x2064
            | 0x2066..=0x206F
            | 0xFEFF
            | 0xFFF9..=0xFFFB
            | 0x110BD
            | 0x110CD
            | 0x13430..=0x1343F
            | 0x1BCA0..=0x1BCA3
            | 0x1D173..=0x1D17A
            | 0xE0001
            | 0xE0020..=0xE007F
    )
}

/// Cleans text typed for a brand field: trims it, and refuses (`None`) control characters and
/// invisible format characters.
#[must_use]
pub fn clean_text(text: &str) -> Option<&str> {
    let text = text.trim();
    let bad = text.chars().any(|c| c.is_control() || is_format_char(c));
    (!bad).then_some(text)
}

/// Why `text` is not acceptable for `what`: it must already be trimmed (see [`clean_text`]),
/// have `min` to `max` characters and hold no control or invisible format characters.
fn text_problem(what: &str, text: &str, min: usize, max: usize) -> Option<String> {
    if clean_text(text) != Some(text) {
        return Some(format!(
            "{what} must be trimmed and hold no control or invisible characters"
        ));
    }
    let count = text.chars().count();
    (!(min..=max).contains(&count)).then(|| format!("{what} must be {min} to {max} characters"))
}

/// Whether `url` may be a footer link: `https://` or `http://` followed by an address, or a path
/// on this server starting with a single `/`. Spaces, control and invisible characters and
/// backslashes are refused everywhere, so `javascript:`, `data:`, `//evil.example` and
/// `/\evil` are all refused.
#[must_use]
pub fn safe_link(url: &str) -> bool {
    if url.chars().count() > MAX_LINK_CHARS
        || url
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || is_format_char(c) || c == '\\')
    {
        return false;
    }
    let after_scheme = |scheme: &str| {
        url.get(..scheme.len())
            .filter(|head| head.eq_ignore_ascii_case(scheme))
            .and_then(|_| url.get(scheme.len()..))
    };
    if let Some(rest) = after_scheme("https://").or_else(|| after_scheme("http://")) {
        return !rest.is_empty() && !rest.starts_with('/');
    }
    url.starts_with('/') && !url.starts_with("//")
}

// ---------------------------------------------------------------------------------------------
// Images

/// A brand image: a snapshot of a file's content, chosen once and then independent of the file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageRef {
    /// The content's SHA-256, 64 lowercase hexadecimal characters.
    pub sha256: String,
    /// The type, judged by the content: one of [`IMAGE_MEDIA_TYPES`].
    pub media_type: String,
    /// The size in bytes.
    pub bytes: u64,
    /// The width in pixels, for a raster image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    /// The height in pixels, for a raster image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// The file's name when it was chosen.
    pub name: String,
    /// The document it was chosen from.
    pub from_doc: Uuid,
}

impl ImageRef {
    /// Why the reference cannot be stored, if so: `max_bytes` is the slot's size limit
    /// ([`MAX_LOGO_BYTES`] or [`MAX_BACKGROUND_BYTES`]).
    #[must_use]
    pub fn problem(&self, max_bytes: u64) -> Option<String> {
        let hash_ok = self.sha256.len() == 64
            && self
                .sha256
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        if !hash_ok {
            return Some("an image's hash is 64 lowercase hexadecimal characters".to_owned());
        }
        if !IMAGE_MEDIA_TYPES.contains(&self.media_type.as_str()) {
            return Some("an image is png, jpeg, webp, gif or svg".to_owned());
        }
        if self.bytes == 0 || self.bytes > max_bytes {
            return Some(format!("an image here is at most {max_bytes} bytes"));
        }
        for side in [self.width, self.height].into_iter().flatten() {
            if side == 0 || side > MAX_IMAGE_SIDE {
                return Some(format!(
                    "an image is at most {MAX_IMAGE_SIDE} pixels a side"
                ));
            }
        }
        text_problem("image name", &self.name, 1, MAX_IMAGE_NAME_CHARS)
    }
}

/// What a brand-centre image is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageKind {
    /// A logo.
    Logo,
    /// An icon.
    Icon,
    /// A header background.
    Background,
}

impl ImageKind {
    /// The largest image of this kind, in bytes.
    #[must_use]
    pub fn max_bytes(self) -> u64 {
        match self {
            ImageKind::Logo | ImageKind::Icon => MAX_LOGO_BYTES,
            ImageKind::Background => MAX_BACKGROUND_BYTES,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// A site's look

/// How much of a header is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HeaderLayout {
    /// Only the site's name or logo.
    Minimal,
    /// The usual bar (the default).
    Compact,
    /// A taller header.
    Standard,
    /// The tallest; the only one with a background image and an alignment.
    Extended,
}

/// What a header or footer is filled with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Background {
    /// The page colour.
    None,
    /// A light tint of the accent.
    Soft,
    /// The theme's secondary colour.
    Strong,
    /// An image (headers only).
    Image,
}

/// Where an extended header's content sits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Align {
    /// At the left.
    Left,
    /// In the middle.
    Centre,
    /// At the right.
    Right,
}

/// A site's header.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Header {
    /// The layout.
    pub layout: HeaderLayout,
    /// The background; `image` needs the `extended` layout and an `image`.
    pub background: Background,
    /// The background image (at most 5 MiB); only with the `extended` layout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageRef>,
    /// Where the content sits; only `extended` may leave `left`.
    pub align: Align,
    /// Whether the site's name is left out because the logo says it; needs a logo.
    pub hide_title: bool,
}

impl Default for Header {
    fn default() -> Header {
        Header {
            layout: HeaderLayout::Compact,
            background: Background::None,
            image: None,
            align: Align::Left,
            hide_title: false,
        }
    }
}

/// Where the local navigation bar goes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Navigation {
    /// Across the top (the default).
    #[default]
    Horizontal,
    /// In a left column at 900 pixels and wider, else in the Menu.
    Vertical,
}

/// How much a footer holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FooterLayout {
    /// One group of links, no heading.
    Simple,
    /// Up to four groups with headings.
    Extended,
}

/// One link of a footer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FooterLink {
    /// The text: 1 to 60 characters.
    pub label: String,
    /// `https://`, `http://` or a path starting with a single `/` (see [`safe_link`]).
    pub url: String,
}

/// A group of footer links under a heading.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FooterGroup {
    /// The heading: 0 to 40 characters; empty in a simple footer.
    pub heading: String,
    /// The links: at most 8.
    pub links: Vec<FooterLink>,
}

/// A site's footer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Footer {
    /// Whether the footer is shown.
    pub enabled: bool,
    /// The layout.
    pub layout: FooterLayout,
    /// A logo (at most 1 MiB).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logo: Option<ImageRef>,
    /// The name shown: 0 to 80 characters.
    pub name: String,
    /// The background: `none`, `soft` or `strong`.
    pub background: Background,
    /// The link groups: one at most in a simple footer, four in an extended one.
    pub groups: Vec<FooterGroup>,
}

impl Default for Footer {
    fn default() -> Footer {
        Footer {
            enabled: false,
            layout: FooterLayout::Simple,
            logo: None,
            name: String::new(),
            background: Background::None,
            groups: Vec::new(),
        }
    }
}

/// A site's look: its theme (a copy), logo, header, navigation style and footer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Look {
    /// A copy of the theme, so editing or deleting the source never breaks the site.
    pub theme: Theme,
    /// Where the theme came from: `builtin:<id>`, `org:<uuid>`, `site:<uuid>` or `custom`.
    pub theme_from: String,
    /// The logo (at most 1 MiB).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logo: Option<ImageRef>,
    /// The logo thumbnail, for places a small mark is shown (at most 1 MiB).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<ImageRef>,
    /// What the logo shows, 0 to 120 characters; empty means the site's name.
    pub logo_alt: String,
    /// The header.
    pub header: Header,
    /// The navigation style.
    pub navigation: Navigation,
    /// The footer.
    pub footer: Footer,
}

impl Default for Look {
    /// The look of a site with none recorded: the built-in ShareCraft theme and the defaults.
    fn default() -> Look {
        Look {
            theme: SHARECRAFT.theme(),
            theme_from: SHARECRAFT.id.to_owned(),
            logo: None,
            thumbnail: None,
            logo_alt: String::new(),
            header: Header::default(),
            navigation: Navigation::Horizontal,
            footer: Footer::default(),
        }
    }
}

impl Look {
    /// Why the look cannot be stored, if so. Checks every limit: text lengths and characters,
    /// image slots and sizes, header and footer rules, link safety. (Contrast is
    /// [`check_theme`]'s business.) `site_name` is the site's name, which a logo without a
    /// description falls back to, so it must not be empty then.
    #[must_use]
    pub fn problem(&self, site_name: &str) -> Option<String> {
        if let Some(p) = self.theme.problem() {
            return Some(p);
        }
        if !theme_source_ok(&self.theme_from) {
            return Some(
                "a theme comes from builtin:<id>, org:<id>, site:<id> or custom".to_owned(),
            );
        }
        if let Some(p) = text_problem("logo description", &self.logo_alt, 0, MAX_LOGO_ALT_CHARS) {
            return Some(p);
        }
        if self.logo.is_some() && self.logo_alt.is_empty() && site_name.trim().is_empty() {
            return Some("a logo needs a description or a site name".to_owned());
        }
        for (what, image) in [("logo", &self.logo), ("thumbnail", &self.thumbnail)] {
            if let Some(p) = image.as_ref().and_then(|i| i.problem(MAX_LOGO_BYTES)) {
                return Some(format!("{what}: {p}"));
            }
        }
        self.header_problem().or_else(|| self.footer_problem())
    }

    fn header_problem(&self) -> Option<String> {
        let h = &self.header;
        let extended = h.layout == HeaderLayout::Extended;
        if !extended && (h.image.is_some() || h.align != Align::Left) {
            return Some("only an extended header has an image or an alignment".to_owned());
        }
        if (h.background == Background::Image) != h.image.is_some() {
            return Some("a header image goes with the image background, and no other".to_owned());
        }
        if h.background == Background::Image && !extended {
            return Some("an image background needs the extended header".to_owned());
        }
        if let Some(p) = h
            .image
            .as_ref()
            .and_then(|i| i.problem(MAX_BACKGROUND_BYTES))
        {
            return Some(format!("header image: {p}"));
        }
        if h.hide_title && self.logo.is_none() {
            return Some("hiding the title needs a logo".to_owned());
        }
        None
    }

    fn footer_problem(&self) -> Option<String> {
        let f = &self.footer;
        if f.background == Background::Image {
            return Some("a footer has no image background".to_owned());
        }
        if let Some(p) = f.logo.as_ref().and_then(|i| i.problem(MAX_LOGO_BYTES)) {
            return Some(format!("footer logo: {p}"));
        }
        if let Some(p) = text_problem("footer name", &f.name, 0, MAX_FOOTER_NAME_CHARS) {
            return Some(p);
        }
        let (max_groups, simple) = match f.layout {
            FooterLayout::Simple => (1, true),
            FooterLayout::Extended => (MAX_FOOTER_GROUPS, false),
        };
        if f.groups.len() > max_groups {
            return Some(format!(
                "this footer has at most {max_groups} group(s) of links"
            ));
        }
        for group in &f.groups {
            if simple && !group.heading.is_empty() {
                return Some("a simple footer has no heading".to_owned());
            }
            if let Some(p) = text_problem(
                "footer heading",
                &group.heading,
                0,
                MAX_FOOTER_HEADING_CHARS,
            ) {
                return Some(p);
            }
            if group.links.len() > MAX_FOOTER_LINKS {
                return Some(format!(
                    "a footer group has at most {MAX_FOOTER_LINKS} links"
                ));
            }
            for link in &group.links {
                if let Some(p) = text_problem("footer link", &link.label, 1, MAX_FOOTER_LABEL_CHARS)
                {
                    return Some(p);
                }
                if !safe_link(&link.url) {
                    return Some(
                        "a footer link is https://, http:// or a path starting with /".to_owned(),
                    );
                }
            }
        }
        None
    }
}

/// Whether `text` names where a theme came from.
fn theme_source_ok(text: &str) -> bool {
    if text == "custom" {
        return true;
    }
    if let Some(id) = text.strip_prefix("builtin:") {
        return BUILT_IN_THEMES
            .iter()
            .any(|t| t.id.strip_prefix("builtin:") == Some(id));
    }
    text.strip_prefix("org:")
        .or_else(|| text.strip_prefix("site:"))
        .is_some_and(|id| Uuid::parse_str(id).is_ok_and(|u| u.to_string() == id))
}

// ---------------------------------------------------------------------------------------------
// The brand centre

/// A theme in the brand centre: a [`Theme`] with an identifier and the time it last changed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrgTheme {
    /// The theme's lasting identifier.
    pub id: Uuid,
    /// The theme's name: 1 to 60 characters, unique in the centre ignoring case.
    pub name: String,
    /// The accent.
    pub primary: Colour,
    /// The strong header and footer background.
    pub secondary: Colour,
    /// The accent in dark mode; derived when left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_dark: Option<Colour>,
    /// The strong background in dark mode; derived when left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secondary_dark: Option<Colour>,
    /// When the theme was last created or changed, in seconds since the Unix epoch.
    pub changed_at: i64,
}

impl OrgTheme {
    /// The colours and name, without the identifier.
    #[must_use]
    pub fn theme(&self) -> Theme {
        Theme {
            name: self.name.clone(),
            primary: self.primary,
            secondary: self.secondary,
            primary_dark: self.primary_dark,
            secondary_dark: self.secondary_dark,
        }
    }
}

/// An image in the brand centre.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrandImage {
    /// The image's lasting identifier.
    pub id: Uuid,
    /// What it is for, which sets its size limit.
    pub kind: ImageKind,
    /// The image.
    pub image: ImageRef,
}

/// Contents of `org/brand.json`: the organisation's brand centre.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrgBrand {
    /// The format version; [`BRAND_SCHEMA`].
    pub schema: u32,
    /// The accounts that look after the brand centre besides the administrators.
    pub managers: Vec<Uuid>,
    /// The approved themes.
    pub themes: Vec<OrgTheme>,
    /// The approved brand images.
    pub images: Vec<BrandImage>,
}

impl Default for OrgBrand {
    /// An empty brand centre.
    fn default() -> OrgBrand {
        OrgBrand {
            schema: BRAND_SCHEMA,
            managers: Vec::new(),
            themes: Vec::new(),
            images: Vec::new(),
        }
    }
}

impl OrgBrand {
    /// Why the record cannot be stored or was stored wrongly, if so: schema, counts (50 themes,
    /// 100 images, 20 managers), duplicate managers, identifiers and theme names (ignoring
    /// case), theme names and image sizes. (Contrast is [`check_theme`]'s business.)
    #[must_use]
    pub fn problem(&self) -> Option<String> {
        if self.schema != BRAND_SCHEMA {
            return Some(format!("unknown schema {}", self.schema));
        }
        if self.managers.len() > MAX_MANAGERS {
            return Some(format!("at most {MAX_MANAGERS} brand managers"));
        }
        if self.themes.len() > MAX_ORG_THEMES {
            return Some(format!("at most {MAX_ORG_THEMES} themes"));
        }
        if self.images.len() > MAX_ORG_IMAGES {
            return Some(format!("at most {MAX_ORG_IMAGES} images"));
        }
        if has_duplicates(self.managers.iter().copied()) {
            return Some("a brand manager is listed twice".to_owned());
        }
        if has_duplicates(self.themes.iter().map(|t| t.id)) {
            return Some("a theme identifier is used twice".to_owned());
        }
        if has_duplicates(self.images.iter().map(|i| i.id)) {
            return Some("an image identifier is used twice".to_owned());
        }
        if has_duplicates(self.themes.iter().map(|t| t.name.to_lowercase())) {
            return Some("theme names are unique, ignoring case".to_owned());
        }
        if let Some(p) = self.themes.iter().find_map(|t| t.theme().problem()) {
            return Some(p);
        }
        self.images
            .iter()
            .find_map(|i| i.image.problem(i.kind.max_bytes()))
    }
}

/// Whether any value occurs twice.
fn has_duplicates<T: Ord>(values: impl Iterator<Item = T>) -> bool {
    let mut all: Vec<T> = values.collect();
    all.sort();
    all.windows(2).any(|w| matches!(w, [a, b] if a == b))
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::sites::file_bytes;

    fn vectors() -> Value {
        serde_json::from_str(include_str!("../../../web/look-vectors.json")).unwrap()
    }

    fn colour(text: &str) -> Colour {
        text.parse().unwrap()
    }

    fn doc() -> Uuid {
        Uuid::parse_str("5e5e5e5e-5e5e-45e5-85e5-5e5e5e5e5e5e").unwrap()
    }

    fn image(bytes: u64) -> ImageRef {
        ImageRef {
            sha256: "ab".repeat(32),
            media_type: "image/png".into(),
            bytes,
            width: Some(200),
            height: Some(100),
            name: "logo.png".into(),
            from_doc: doc(),
        }
    }

    #[test]
    fn contrast_matches_shared_vectors() {
        let table = vectors();
        let rows = table["contrast"].as_array().unwrap();
        assert!(rows.len() > 10);
        for row in rows {
            let (a, b) = (
                colour(row["a"].as_str().unwrap()),
                colour(row["b"].as_str().unwrap()),
            );
            let want = row["ratio"].as_f64().unwrap();
            assert_eq!(round2(contrast(a, b)), want, "{a} on {b}");
            assert_eq!(round2(contrast(b, a)), want, "{b} on {a}");
        }
        assert!((contrast(colour("#000000"), colour("#ffffff")) - 21.0).abs() < 1e-12);
    }

    #[test]
    fn derive_dark_matches_shared_vectors() {
        let table = vectors();
        for row in table["derive_dark"].as_array().unwrap() {
            let from = colour(row["from"].as_str().unwrap());
            let role = match row["role"].as_str().unwrap() {
                "primary" => Role::Primary,
                _ => Role::Secondary,
            };
            let want = colour(row["dark"].as_str().unwrap());
            assert_eq!(derive_dark(from, role), Some(want), "{role:?} {from}");
        }
    }

    #[test]
    fn built_in_themes_pass_every_check() {
        let table = vectors();
        let rows = table["built_in_themes"].as_array().unwrap();
        assert_eq!(rows.len(), BUILT_IN_THEMES.len());
        for (built_in, row) in BUILT_IN_THEMES.iter().zip(rows) {
            assert_eq!(row["id"], built_in.id);
            assert_eq!(row["name"], built_in.name);
            assert_eq!(row["primary"], built_in.primary.to_string());
            assert_eq!(row["secondary"], built_in.secondary.to_string());
            let resolved = check_theme(&built_in.theme()).unwrap();
            assert_eq!(
                serde_json::to_value(resolved).unwrap(),
                row["resolved"],
                "{}",
                built_in.id
            );
            let ratio = on_dark(resolved.dark.accent);
            assert!(
                (4.5..=4.7).contains(&ratio),
                "{} dark accent {ratio}",
                built_in.id
            );
            assert!(built_in.theme().problem().is_none());
        }
        // The worked example in the plan.
        let sharecraft = check_theme(&SHARECRAFT.theme()).unwrap();
        assert_eq!(sharecraft.dark.accent, colour("#748ddd"));
    }

    #[test]
    fn every_check_on_every_path() {
        let table = vectors();
        let mut seen = Vec::new();
        for case in table["failing_themes"].as_array().unwrap() {
            let theme: Theme = serde_json::from_value(case["theme"].clone()).unwrap();
            let pairs = check_theme(&theme).unwrap_err();
            assert_eq!(
                serde_json::to_value(&pairs).unwrap(),
                case["pairs"],
                "{}",
                case["note"]
            );
            seen.extend(pairs.iter().map(|p| p.check));
        }
        for check in [
            Check::PrimaryOnLight,
            Check::ButtonOnPrimary,
            Check::PrimaryDarkOnDark,
            Check::HeaderOnSecondary,
            Check::HeaderOnSecondaryDark,
        ] {
            assert!(seen.contains(&check), "{check:?} never fails in the table");
        }

        let base = SHARECRAFT.theme();
        let alone = |theme: Theme, check: Check| {
            let pairs = check_theme(&theme).unwrap_err();
            let checks: Vec<Check> = pairs.iter().map(|p| p.check).collect();
            assert_eq!(checks, vec![check], "{theme:?}");
            for pair in &pairs {
                assert!(pair.ratio < MIN_CONTRAST);
            }
        };
        // Each check that can fail on its own, on each path that sets that colour.
        alone(
            Theme {
                primary: colour("#767676"),
                ..base.clone()
            },
            Check::PrimaryOnLight,
        );
        alone(
            Theme {
                primary_dark: Some(colour("#3a4a52")),
                ..base.clone()
            },
            Check::PrimaryDarkOnDark,
        );
        alone(
            Theme {
                secondary: colour("#7b7b7b"),
                ..base.clone()
            },
            Check::HeaderOnSecondary,
        );
        alone(
            Theme {
                secondary_dark: Some(colour("#7b7b7b")),
                ..base.clone()
            },
            Check::HeaderOnSecondaryDark,
        );
        // A pale primary fails the light checks together: white on it is the weaker half of the
        // pair, so button_on_primary never fails without primary_on_light.
        let pale = Theme {
            primary: colour("#cccccc"),
            ..base.clone()
        };
        let checks: Vec<Check> = check_theme(&pale)
            .unwrap_err()
            .iter()
            .map(|p| p.check)
            .collect();
        assert_eq!(checks, [Check::PrimaryOnLight, Check::ButtonOnPrimary]);
        // Explicit dark values that pass are kept as given.
        let explicit = Theme {
            primary_dark: Some(colour("#9db4ff")),
            secondary_dark: Some(colour("#233239")),
            ..base
        };
        let resolved = check_theme(&explicit).unwrap();
        assert_eq!(resolved.dark.accent, colour("#9db4ff"));
        assert_eq!(resolved.dark.header_bg, colour("#233239"));
    }

    #[test]
    fn colour_parsing_refuses_anything_but_hex() {
        assert_eq!(colour("#3156C9"), colour("#3156c9"));
        assert_eq!(colour("#3156C9").to_string(), "#3156c9");
        for bad in [
            "red",
            "#fff",
            "3156c9",
            "#3156c9 ",
            " #3156c9",
            "#3156c9ff",
            "#3156cg",
            "red;background:url(x)",
            "#3156c9;x",
            "rgb(1,2,3)",
            "#+156c9",
            "#３156c9",
            "",
            "#",
        ] {
            assert!(Colour::parse(bad).is_err(), "{bad:?}");
        }
        let bad: Result<Theme, _> =
            serde_json::from_value(json!({"name": "x", "primary": "red", "secondary": "#233239"}));
        assert!(bad.is_err());
        let upper: Theme = serde_json::from_value(
            json!({"name": "x", "primary": "#3156C9", "secondary": "#233239"}),
        )
        .unwrap();
        assert_eq!(serde_json::to_value(&upper).unwrap()["primary"], "#3156c9");
        let unknown: Result<Theme, _> = serde_json::from_value(
            json!({"name": "x", "primary": "#3156c9", "secondary": "#233239", "css": "x"}),
        );
        assert!(unknown.is_err());
    }

    fn problem_of(look: &Look) -> String {
        look.problem("Site").expect("expected a problem")
    }

    #[test]
    fn look_limits_refused() {
        let ok = Look::default();
        assert_eq!(ok.problem("Site"), None);
        assert_eq!(ok.problem(""), None);
        let mut with_logo = ok.clone();
        with_logo.logo = Some(image(MAX_LOGO_BYTES));
        assert_eq!(with_logo.problem("Site"), None);
        // An empty description falls back to the site's name, which must then exist.
        assert!(with_logo.problem("  ").is_some());

        let change = |f: &dyn Fn(&mut Look)| {
            let mut look = ok.clone();
            f(&mut look);
            problem_of(&look)
        };
        // Theme.
        change(&|l| l.theme.name = String::new());
        change(&|l| l.theme.name = "x".repeat(61));
        change(&|l| l.theme.name = " padded ".into());
        change(&|l| l.theme.name = "bell\u{7}".into());
        change(&|l| l.theme.name = "a\u{202e}b".into());
        let mut longest = ok.clone();
        longest.theme.name = "é".repeat(60);
        assert_eq!(longest.problem("Site"), None);
        for from in ["", "builtin:nope", "org:x", "site:", "elsewhere", "custom "] {
            change(&|l| l.theme_from = from.into());
        }
        for from in [
            "custom",
            "builtin:rust",
            "org:5e5e5e5e-5e5e-45e5-85e5-5e5e5e5e5e5e",
            "site:5e5e5e5e-5e5e-45e5-85e5-5e5e5e5e5e5e",
        ] {
            let mut look = ok.clone();
            look.theme_from = from.into();
            assert_eq!(look.problem("Site"), None, "{from}");
        }
        // Logo text and images.
        change(&|l| l.logo_alt = "x".repeat(121));
        change(&|l| l.logo_alt = "line\nbreak".into());
        let mut alt = ok.clone();
        alt.logo_alt = "x".repeat(120);
        assert_eq!(alt.problem("Site"), None);
        change(&|l| l.logo = Some(image(MAX_LOGO_BYTES + 1)));
        change(&|l| l.thumbnail = Some(image(MAX_LOGO_BYTES + 1)));
        change(&|l| l.logo = Some(image(0)));
        change(&|l| {
            l.logo = Some(ImageRef {
                media_type: "text/html".into(),
                ..image(10)
            })
        });
        change(&|l| {
            l.logo = Some(ImageRef {
                sha256: "AB".repeat(32),
                ..image(10)
            })
        });
        change(&|l| {
            l.logo = Some(ImageRef {
                width: Some(8001),
                ..image(10)
            })
        });
        change(&|l| {
            l.logo = Some(ImageRef {
                height: Some(0),
                ..image(10)
            })
        });
        change(&|l| {
            l.logo = Some(ImageRef {
                name: String::new(),
                ..image(10)
            })
        });
        let mut big_side = with_logo.clone();
        big_side.logo = Some(ImageRef {
            width: Some(8000),
            height: Some(8000),
            ..image(10)
        });
        assert_eq!(big_side.problem("Site"), None);
        // Header.
        change(&|l| l.header.hide_title = true);
        change(&|l| l.header.align = Align::Right);
        change(&|l| l.header.image = Some(image(10)));
        change(&|l| l.header.background = Background::Image);
        change(&|l| {
            l.header.layout = HeaderLayout::Standard;
            l.header.background = Background::Image;
            l.header.image = Some(image(10));
        });
        change(&|l| {
            l.header.layout = HeaderLayout::Extended;
            l.header.background = Background::Image;
        });
        change(&|l| {
            l.header.layout = HeaderLayout::Extended;
            l.header.background = Background::Strong;
            l.header.image = Some(image(10));
        });
        change(&|l| {
            l.header.layout = HeaderLayout::Extended;
            l.header.background = Background::Image;
            l.header.image = Some(image(MAX_BACKGROUND_BYTES + 1));
        });
        let mut extended = with_logo.clone();
        extended.header = Header {
            layout: HeaderLayout::Extended,
            background: Background::Image,
            image: Some(image(MAX_BACKGROUND_BYTES)),
            align: Align::Centre,
            hide_title: true,
        };
        assert_eq!(extended.problem("Site"), None);
        // Footer.
        let link = |n: usize| FooterLink {
            label: format!("Link {n}"),
            url: "/about".into(),
        };
        let group = |heading: &str, links: usize| FooterGroup {
            heading: heading.into(),
            links: (0..links).map(link).collect(),
        };
        change(&|l| l.footer.name = "x".repeat(81));
        change(&|l| l.footer.name = "<b>\u{0}".into());
        change(&|l| l.footer.background = Background::Image);
        change(&|l| l.footer.logo = Some(image(MAX_LOGO_BYTES + 1)));
        change(&|l| l.footer.groups = vec![group("", 1), group("", 1)]);
        change(&|l| l.footer.groups = vec![group("Heading", 1)]);
        change(&|l| l.footer.groups = vec![group("", 9)]);
        change(&|l| {
            l.footer.layout = FooterLayout::Extended;
            l.footer.groups = (0..5).map(|_| group("H", 1)).collect();
        });
        change(&|l| {
            l.footer.layout = FooterLayout::Extended;
            l.footer.groups = vec![group(&"h".repeat(41), 1)];
        });
        change(&|l| {
            l.footer.layout = FooterLayout::Extended;
            l.footer.groups = vec![group("H", 9)];
        });
        change(&|l| {
            let mut g = group("", 1);
            g.links[0].label = String::new();
            l.footer.groups = vec![g];
        });
        change(&|l| {
            let mut g = group("", 1);
            g.links[0].label = "x".repeat(61);
            l.footer.groups = vec![g];
        });
        change(&|l| {
            let mut g = group("", 1);
            g.links[0].label = "<script>\u{7}".into();
            l.footer.groups = vec![g];
        });
        let mut full = ok.clone();
        full.footer = Footer {
            enabled: true,
            layout: FooterLayout::Extended,
            logo: Some(image(MAX_LOGO_BYTES)),
            name: "x".repeat(80),
            background: Background::Strong,
            groups: (0..4).map(|_| group(&"h".repeat(40), 8)).collect(),
        };
        assert_eq!(full.problem("Site"), None);
        let mut simple = ok.clone();
        simple.footer.groups = vec![group("", 8)];
        assert_eq!(simple.problem("Site"), None);
    }

    #[test]
    fn footer_links_must_be_safe() {
        for good in [
            "https://example.org",
            "http://example.org/a?b=c#d",
            "HTTPS://Example.org",
            "/",
            "/about",
            "/sites/1/pages?x=1",
        ] {
            assert!(safe_link(good), "{good}");
        }
        for bad in [
            "",
            "javascript:alert(1)",
            "JaVaScRiPt:alert(1)",
            "data:text/html,<script>",
            "//evil.example",
            "///evil.example",
            "/\\evil.example",
            "\\\\evil.example",
            "https://",
            "https:///evil",
            "https:evil.example",
            "http:/evil.example",
            "ftp://example.org",
            "mailto:a@example.org",
            "example.org",
            "about",
            " https://example.org",
            "https://example.org ",
            "https://exa mple.org",
            "https://example.org\n",
            "https://example.org/\u{0}",
            "https://example.org/\u{202e}",
            "/a\\b",
            "vbscript:x",
            "file:///etc/passwd",
        ] {
            assert!(!safe_link(bad), "{bad:?}");
        }
        assert!(!safe_link(&format!("/{}", "a".repeat(MAX_LINK_CHARS))));
        assert!(safe_link(&format!("/{}", "a".repeat(MAX_LINK_CHARS - 1))));
    }

    #[test]
    fn org_brand_limits_refused() {
        let theme = |n: usize| OrgTheme {
            id: Uuid::from_u128(n as u128 + 1),
            name: format!("Theme {n}"),
            primary: colour("#3156c9"),
            secondary: colour("#233239"),
            primary_dark: None,
            secondary_dark: None,
            changed_at: 0,
        };
        let brand_image = |n: usize, kind: ImageKind, bytes: u64| BrandImage {
            id: Uuid::from_u128(n as u128 + 1000),
            kind,
            image: image(bytes),
        };
        let ok = OrgBrand::default();
        assert_eq!(ok.problem(), None);
        let mut full = ok.clone();
        full.themes = (0..MAX_ORG_THEMES).map(theme).collect();
        full.images = (0..MAX_ORG_IMAGES)
            .map(|n| brand_image(n, ImageKind::Logo, 10))
            .collect();
        full.managers = (0..MAX_MANAGERS as u128).map(Uuid::from_u128).collect();
        assert_eq!(full.problem(), None);

        let change = |f: &dyn Fn(&mut OrgBrand)| {
            let mut brand = full.clone();
            f(&mut brand);
            brand.problem().expect("expected a problem")
        };
        change(&|b| b.schema = 2);
        change(&|b| b.themes.push(theme(99)));
        change(&|b| b.images.push(brand_image(999, ImageKind::Logo, 10)));
        change(&|b| b.managers.push(Uuid::from_u128(99)));
        change(&|b| b.managers[1] = b.managers[0]);
        change(&|b| b.themes[1].id = b.themes[0].id);
        change(&|b| b.images[1].id = b.images[0].id);
        change(&|b| b.themes[1].name = b.themes[0].name.to_uppercase());
        change(&|b| b.themes[0].name = String::new());
        change(&|b| b.themes[0].name = "x".repeat(61));
        change(&|b| b.themes[0].name = "a\u{200b}b".into());
        change(&|b| b.images[0] = brand_image(0, ImageKind::Logo, MAX_LOGO_BYTES + 1));
        change(&|b| b.images[0] = brand_image(0, ImageKind::Icon, MAX_LOGO_BYTES + 1));
        let mut background = ok.clone();
        background.images = vec![brand_image(0, ImageKind::Background, MAX_BACKGROUND_BYTES)];
        assert_eq!(background.problem(), None);
        background.images = vec![brand_image(
            0,
            ImageKind::Background,
            MAX_BACKGROUND_BYTES + 1,
        )];
        assert!(background.problem().is_some());
        let unknown: Result<OrgBrand, _> = serde_json::from_value(
            json!({"schema": 1, "managers": [], "themes": [], "images": [], "extra": 1}),
        );
        assert!(unknown.is_err());
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn records_round_trip_exact_bytes() {
        let brand = OrgBrand {
            schema: 1,
            managers: vec![doc()],
            themes: vec![OrgTheme {
                id: Uuid::parse_str("11111111-1111-4111-8111-111111111111").unwrap(),
                name: "Dusk".into(),
                primary: colour("#3156C9"),
                secondary: colour("#233239"),
                primary_dark: Some(colour("#9db4ff")),
                secondary_dark: None,
                changed_at: 1_763_200_000,
            }],
            images: vec![BrandImage {
                id: Uuid::parse_str("22222222-2222-4222-8222-222222222222").unwrap(),
                kind: ImageKind::Logo,
                image: ImageRef {
                    width: None,
                    height: None,
                    media_type: "image/svg+xml".into(),
                    ..image(1234)
                },
            }],
        };
        let text = String::from_utf8(file_bytes(&brand).unwrap()).unwrap();
        let expected = format!(
            r##"{{
  "schema": 1,
  "managers": [
    "5e5e5e5e-5e5e-45e5-85e5-5e5e5e5e5e5e"
  ],
  "themes": [
    {{
      "id": "11111111-1111-4111-8111-111111111111",
      "name": "Dusk",
      "primary": "#3156c9",
      "secondary": "#233239",
      "primary_dark": "#9db4ff",
      "changed_at": 1763200000
    }}
  ],
  "images": [
    {{
      "id": "22222222-2222-4222-8222-222222222222",
      "kind": "logo",
      "image": {{
        "sha256": "{hash}",
        "media_type": "image/svg+xml",
        "bytes": 1234,
        "name": "logo.png",
        "from_doc": "5e5e5e5e-5e5e-45e5-85e5-5e5e5e5e5e5e"
      }}
    }}
  ]
}}
"##,
            hash = "ab".repeat(32)
        );
        assert_eq!(text, expected);
        let back: OrgBrand = serde_json::from_str(&text).unwrap();
        assert_eq!(file_bytes(&back).unwrap(), text.as_bytes());
        assert_eq!(back.themes[0].primary, colour("#3156c9"));

        let mut look = Look::default();
        look.theme_from = "org:11111111-1111-4111-8111-111111111111".into();
        look.logo = Some(image(2048));
        look.logo_alt = "The Dusk logo".into();
        look.header = Header {
            layout: HeaderLayout::Extended,
            background: Background::Strong,
            image: None,
            align: Align::Centre,
            hide_title: true,
        };
        look.navigation = Navigation::Vertical;
        look.footer = Footer {
            enabled: true,
            layout: FooterLayout::Simple,
            logo: None,
            name: "Dusk Ltd".into(),
            background: Background::Soft,
            groups: vec![FooterGroup {
                heading: String::new(),
                links: vec![FooterLink {
                    label: "Privacy".into(),
                    url: "/privacy".into(),
                }],
            }],
        };
        assert_eq!(look.problem("Dusk"), None);
        let text = String::from_utf8(file_bytes(&look).unwrap()).unwrap();
        let expected = format!(
            r##"{{
  "theme": {{
    "name": "ShareCraft",
    "primary": "#3156c9",
    "secondary": "#233239"
  }},
  "theme_from": "org:11111111-1111-4111-8111-111111111111",
  "logo": {{
    "sha256": "{hash}",
    "media_type": "image/png",
    "bytes": 2048,
    "width": 200,
    "height": 100,
    "name": "logo.png",
    "from_doc": "5e5e5e5e-5e5e-45e5-85e5-5e5e5e5e5e5e"
  }},
  "logo_alt": "The Dusk logo",
  "header": {{
    "layout": "extended",
    "background": "strong",
    "align": "centre",
    "hide_title": true
  }},
  "navigation": "vertical",
  "footer": {{
    "enabled": true,
    "layout": "simple",
    "name": "Dusk Ltd",
    "background": "soft",
    "groups": [
      {{
        "heading": "",
        "links": [
          {{
            "label": "Privacy",
            "url": "/privacy"
          }}
        ]
      }}
    ]
  }}
}}
"##,
            hash = "ab".repeat(32)
        );
        assert_eq!(text, expected);
        let back: Look = serde_json::from_str(&text).unwrap();
        assert_eq!(back, look);
        assert_eq!(file_bytes(&back).unwrap(), text.as_bytes());
    }
}
