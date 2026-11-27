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

use crate::libraries::HeroFrame;
use crate::sites::SiteTheme;

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
/// The most fonts the brand centre holds.
pub const MAX_ORG_FONTS: usize = 30;
/// The most font packages the brand centre holds.
pub const MAX_ORG_FONT_PACKAGES: usize = 20;
/// The most font packages of one site's own.
pub const MAX_SITE_FONT_PACKAGES: usize = 10;
/// The largest font file, in bytes (2 MiB).
pub const MAX_FONT_BYTES: u64 = 2 * 1024 * 1024;
/// The largest tab icon, in bytes (256 KiB).
pub const MAX_ICON_BYTES: u64 = 256 * 1024;
/// The fewest pixels along either side of a png tab icon.
pub const MIN_ICON_SIDE: u32 = 32;
/// The media types a font may have, judged by the content and recorded with it.
pub const FONT_MEDIA_TYPES: [&str; 4] = ["font/woff2", "font/woff", "font/ttf", "font/otf"];
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
    /// The accent as link text and the focus ring (what `--accent-ink` is in `style.css`):
    /// the primary in light mode, the dark primary in dark mode.
    pub accent_ink: Colour,
    /// The text on an accent button: white in light mode; white or the dark ink, whichever
    /// reads better, on the dark primary.
    pub on_accent: Colour,
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
                accent_ink: p,
                on_accent: WHITE,
                header_bg: s,
                header_ink: light_ink,
            },
            dark: Palette {
                accent,
                accent_ink: accent,
                on_accent: best_ink(accent).0,
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
        if !is_sha256(&self.sha256) {
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
// Fonts

/// What a font file is, judged by its content (never by its name or the type it was sent as).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FontKind {
    /// A WOFF2 font (`wOF2`).
    Woff2,
    /// A WOFF font (`wOFF`).
    Woff,
    /// A TrueType font (`00 01 00 00` or `true`).
    Ttf,
    /// An OpenType font with CFF outlines (`OTTO`).
    Otf,
}

impl FontKind {
    /// The media type recorded with a font of this kind: one of [`FONT_MEDIA_TYPES`].
    #[must_use]
    pub fn media_type(self) -> &'static str {
        match self {
            FontKind::Woff2 => "font/woff2",
            FontKind::Woff => "font/woff",
            FontKind::Ttf => "font/ttf",
            FontKind::Otf => "font/otf",
        }
    }
}

/// What kind of font `bytes` is, from its first four bytes alone: `wOF2` is woff2, `wOFF` woff,
/// `00 01 00 00` or `true` ttf and `OTTO` otf. Anything else (a web page or picture renamed
/// `.woff2`, an empty file, a file shorter than four bytes) is `None`.
#[must_use]
pub fn sniff_font(bytes: &[u8]) -> Option<FontKind> {
    match bytes.get(..4)? {
        b"wOF2" => Some(FontKind::Woff2),
        b"wOFF" => Some(FontKind::Woff),
        [0, 1, 0, 0] | b"true" => Some(FontKind::Ttf),
        b"OTTO" => Some(FontKind::Otf),
        _ => None,
    }
}

/// The family name a page loads a font file under: `sc-` and the first 12 hexadecimal
/// characters of its SHA-256. It is made from the hash alone, never from a font's name or any
/// other text a person typed, so it can be written into a style safely. Whatever is passed, the
/// answer is `sc-` and 12 lowercase hexadecimal characters (a character that is not one is
/// replaced by `0`, and a short hash is padded with `0`); real hashes are never changed.
#[must_use]
pub fn family_name(sha256: &str) -> String {
    let mut name = String::from("sc-");
    let digits = sha256
        .chars()
        .map(|c| {
            if c.is_ascii_hexdigit() {
                c.to_ascii_lowercase()
            } else {
                '0'
            }
        })
        .chain(std::iter::repeat('0'));
    name.extend(digits.take(12));
    name
}

/// Whether `text` is a SHA-256 as the records write it: 64 lowercase hexadecimal characters.
fn is_sha256(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Who confirmed that the organisation holds the licence of a font, and when.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Licence {
    /// The account that confirmed it.
    pub confirmed_by: Uuid,
    /// When, in seconds since the Unix epoch; positive.
    pub confirmed_at: i64,
}

/// A font file: a snapshot of a file's content, like an [`ImageRef`], with a record of the
/// licence confirmation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FontRef {
    /// The content's SHA-256, 64 lowercase hexadecimal characters.
    pub sha256: String,
    /// The type, judged by the content: one of [`FONT_MEDIA_TYPES`].
    pub media_type: String,
    /// The size in bytes; at most [`MAX_FONT_BYTES`].
    pub bytes: u64,
    /// The file's name when it was chosen; shown as text only.
    pub name: String,
    /// The document it was chosen from.
    pub from_doc: Uuid,
    /// Who confirmed the organisation holds its licence, and when.
    pub licence: Licence,
}

impl FontRef {
    /// Why the reference cannot be stored, if so: the hash, the type, the size (1 byte to 2
    /// MiB), the name (text of 1 to 120 characters) and the licence confirmation.
    #[must_use]
    pub fn problem(&self) -> Option<String> {
        if !is_sha256(&self.sha256) {
            return Some("a font's hash is 64 lowercase hexadecimal characters".to_owned());
        }
        if !FONT_MEDIA_TYPES.contains(&self.media_type.as_str()) {
            return Some("a font is woff2, woff, ttf or otf".to_owned());
        }
        if self.bytes == 0 || self.bytes > MAX_FONT_BYTES {
            return Some(format!("a font is at most {MAX_FONT_BYTES} bytes"));
        }
        if self.licence.confirmed_at <= 0 {
            return Some("a font needs its licence confirmed".to_owned());
        }
        text_problem("font name", &self.name, 1, MAX_IMAGE_NAME_CHARS)
    }
}

/// A font of the brand centre.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrandFont {
    /// The font's lasting identifier.
    pub id: Uuid,
    /// The font file.
    pub font: FontRef,
}

/// What a text role of a font package is set in. Serialised in kebab case.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FontRole {
    /// The package's display font; the package must have one.
    Display,
    /// The package's content font; the package must have one.
    Content,
    /// The system sans-serif fonts.
    System,
    /// The system serif fonts.
    SystemSerif,
}

/// The font of each of the four text roles of a page.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FontRoles {
    /// The site title.
    pub title: FontRole,
    /// Headings.
    pub heading: FontRole,
    /// Running text.
    pub body: FontRole,
    /// Buttons, tabs and other controls.
    pub interactive: FontRole,
}

impl FontRoles {
    /// The four roles with their names, in the order of the fields.
    fn named(self) -> [(&'static str, FontRole); 4] {
        [
            ("title", self.title),
            ("heading", self.heading),
            ("body", self.body),
            ("interactive", self.interactive),
        ]
    }
}

/// A font package: a display font and a content font, with the four text roles mapped onto
/// them or onto the system fonts. Every role falls back to the system fonts when a file cannot
/// be loaded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FontPackage {
    /// The package's lasting identifier; the nil identifier in the copy of a built-in package
    /// that a look holds.
    pub id: Uuid,
    /// Its name: 1 to 60 characters, shown as text only.
    pub name: String,
    /// The display font, if the package has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<FontRef>,
    /// The content font, if the package has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<FontRef>,
    /// What each role is set in; a role names only a font the package has.
    pub roles: FontRoles,
}

impl FontPackage {
    /// Why the package cannot be stored, if so: its name, its font files, and each role, which
    /// must not name a display or content font the package lacks.
    #[must_use]
    pub fn problem(&self) -> Option<String> {
        if let Some(p) = text_problem("font package name", &self.name, 1, MAX_THEME_NAME_CHARS) {
            return Some(p);
        }
        for (what, font) in [
            ("display font", &self.display),
            ("content font", &self.content),
        ] {
            if let Some(p) = font.as_ref().and_then(FontRef::problem) {
                return Some(format!("{what}: {p}"));
            }
        }
        for (role, value) in self.roles.named() {
            let lacking = match value {
                FontRole::Display if self.display.is_none() => Some("display"),
                FontRole::Content if self.content.is_none() => Some("content"),
                _ => None,
            };
            if let Some(font) = lacking {
                return Some(format!(
                    "the {role} role uses the {font} font, which this package does not have"
                ));
            }
        }
        None
    }
}

/// A built-in font package: roles only, no files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuiltInFontPackage {
    /// The identifier, `builtin:<name>`.
    pub id: &'static str,
    /// The display name.
    pub name: &'static str,
    /// What each role is set in; only system fonts.
    pub roles: FontRoles,
}

impl BuiltInFontPackage {
    /// The package as a look keeps its copy: the nil identifier, and no files.
    #[must_use]
    pub fn package(&self) -> FontPackage {
        FontPackage {
            id: Uuid::nil(),
            name: self.name.to_owned(),
            display: None,
            content: None,
            roles: self.roles,
        }
    }
}

/// The font packages every installation has: System (today's look) and Classic, which sets
/// the title and headings in the system serif fonts. Neither needs a file.
pub const BUILT_IN_FONT_PACKAGES: [BuiltInFontPackage; 2] = [
    BuiltInFontPackage {
        id: "builtin:system",
        name: "System",
        roles: FontRoles {
            title: FontRole::System,
            heading: FontRole::System,
            body: FontRole::System,
            interactive: FontRole::System,
        },
    },
    BuiltInFontPackage {
        id: "builtin:classic",
        name: "Classic",
        roles: FontRoles {
            title: FontRole::SystemSerif,
            heading: FontRole::SystemSerif,
            body: FontRole::System,
            interactive: FontRole::System,
        },
    },
];

/// Whether `text` names where a font package came from: `builtin:system`, `builtin:classic`,
/// `org:<uuid>` or `site:<uuid>`.
#[must_use]
pub fn font_source_ok(text: &str) -> bool {
    BUILT_IN_FONT_PACKAGES.iter().any(|p| p.id == text)
        || prefixed_uuid_ok(text, "org:")
        || prefixed_uuid_ok(text, "site:")
}

/// Whether `text` is `prefix` and a lowercase hyphenated UUID.
fn prefixed_uuid_ok(text: &str, prefix: &str) -> bool {
    text.strip_prefix(prefix)
        .is_some_and(|id| Uuid::parse_str(id).is_ok_and(|u| u.to_string() == id))
}

/// Whether the sides of an icon are equal within 10 %.
fn square_within_a_tenth(width: u32, height: u32) -> bool {
    let (w, h) = (u64::from(width), u64::from(height));
    w.max(h) * 10 <= w.min(h) * 11
}

/// Why `image` cannot be a site's tab icon, if so: it passes the usual image checks with the
/// icon's size limit ([`MAX_ICON_BYTES`]), is a png or an svg, and is square within 10 %. A png
/// must say its size and be at least 32 pixels a side; an svg need not say its size.
#[must_use]
pub fn icon_problem(image: &ImageRef) -> Option<String> {
    if let Some(p) = image.problem(MAX_ICON_BYTES) {
        return Some(p);
    }
    let square = || "an icon is square, within 10 %".to_owned();
    match (image.media_type.as_str(), image.width, image.height) {
        ("image/png", Some(w), Some(h)) => {
            if w.min(h) < MIN_ICON_SIDE {
                Some(format!(
                    "a png icon is at least {MIN_ICON_SIDE} pixels a side"
                ))
            } else {
                (!square_within_a_tenth(w, h)).then(square)
            }
        }
        ("image/png", _, _) => Some("a png icon needs its size".to_owned()),
        ("image/svg+xml", Some(w), Some(h)) => (!square_within_a_tenth(w, h)).then(square),
        ("image/svg+xml", _, _) => None,
        _ => Some("an icon is a png or an svg".to_owned()),
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
    /// How the banner picture bleeds past the banner; left out of the file at its defaults.
    #[serde(default, skip_serializing_if = "Bleed::is_default")]
    pub bleed: Bleed,
    /// Which part of the image the header shows, as a library's banner picture is framed;
    /// None (left out of the file) for the middle at its smallest. Only with an image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<HeroFrame>,
}

impl Default for Header {
    fn default() -> Header {
        Header {
            layout: HeaderLayout::Compact,
            background: Background::None,
            image: None,
            align: Align::Left,
            hide_title: false,
            bleed: Bleed::default(),
            frame: None,
        }
    }
}

/// The farthest a banner picture bleeds past the banner, in CSS pixels.
pub const MAX_BLEED_DISTANCE: u16 = 400;
/// The most a bleeding banner picture is blurred, in CSS pixels.
pub const MAX_BLEED_BLUR: u16 = 96;

/// How an image header's banner picture bleeds out past the banner: behind the page around and
/// below it the same picture shows, blurred, partly transparent and washed with a tint, fading
/// out over `distance`. Pages keep text readable by showing it fainter where the picture would
/// take text below 4.5:1 (`web/bleed.js`), so these are upper bounds. Only a header with an
/// image background bleeds. Each field left out takes its default.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Bleed {
    /// Whether the picture bleeds at all.
    pub enabled: bool,
    /// How far past the banner it reaches before it has faded out: 0 to
    /// [`MAX_BLEED_DISTANCE`] pixels.
    pub distance: u16,
    /// How much it is blurred: 0 to [`MAX_BLEED_BLUR`] pixels.
    pub blur: u16,
    /// How see-through it is: 0 to 100 percent (100 shows nothing).
    pub transparency: u8,
    /// The colour it is washed with in light mode.
    pub tint_light: Colour,
    /// The colour it is washed with in dark mode.
    pub tint_dark: Colour,
}

impl Default for Bleed {
    fn default() -> Bleed {
        Bleed {
            enabled: true,
            distance: 240,
            blur: 48,
            transparency: 30,
            tint_light: Colour::rgb(0xff, 0xff, 0xff),
            tint_dark: Colour::rgb(0x00, 0x00, 0x00),
        }
    }
}

impl Bleed {
    /// Whether this is the default, which a record leaves out of its file.
    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == Bleed::default()
    }

    /// Why it cannot be stored, if so: a distance, blur or transparency out of range.
    #[must_use]
    pub fn problem(&self) -> Option<String> {
        if self.distance > MAX_BLEED_DISTANCE {
            return Some(format!(
                "a banner bleeds at most {MAX_BLEED_DISTANCE} pixels"
            ));
        }
        if self.blur > MAX_BLEED_BLUR {
            return Some(format!(
                "a bleeding banner is blurred at most {MAX_BLEED_BLUR} pixels"
            ));
        }
        if self.transparency > 100 {
            return Some("transparency is 0 to 100 percent".to_owned());
        }
        None
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

/// How the children of a top-level link of horizontal navigation are shown.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MenuStyle {
    /// A dropdown list under the link (the default).
    #[default]
    Cascading,
    /// A full-width panel of columns; horizontal navigation only.
    Mega,
}

impl MenuStyle {
    /// Whether this is the default, which a record leaves out of its file.
    #[must_use]
    pub fn is_cascading(&self) -> bool {
        *self == MenuStyle::Cascading
    }
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

/// The fonts of a look: a copy of a font package, so editing or deleting the source never
/// breaks the site.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LookFonts {
    /// The copy of the package.
    pub package: FontPackage,
    /// Where it came from: `builtin:system`, `builtin:classic`, `org:<uuid>` or `site:<uuid>`.
    pub from: String,
}

/// A site's look: its theme (a copy), logo, header, navigation style and footer, and
/// optionally its fonts, tab icon and menu style.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Look {
    /// A copy of the theme, so editing or deleting the source never breaks the site.
    pub theme: Theme,
    /// Where the theme came from: `builtin:<id>`, `org:<uuid>`, `site:<uuid>` or `custom`.
    pub from: String,
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
    /// The fonts; `None` (left out of the file) for the system fonts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fonts: Option<LookFonts>,
    /// The browser-tab icon: a png or an svg, square within 10 %, at most 256 KiB; `None`
    /// (left out of the file) to use the thumbnail, then the organisation's icon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<ImageRef>,
    /// How the children of a link of horizontal navigation are shown; left out of the file
    /// when it is `cascading`.
    #[serde(default, skip_serializing_if = "MenuStyle::is_cascading")]
    pub menu: MenuStyle,
}

impl Default for Look {
    /// The look of a site with none recorded: the built-in ShareCraft theme and the defaults.
    fn default() -> Look {
        Look {
            theme: SHARECRAFT.theme(),
            from: SHARECRAFT.id.to_owned(),
            logo: None,
            thumbnail: None,
            logo_alt: String::new(),
            header: Header::default(),
            navigation: Navigation::Horizontal,
            footer: Footer::default(),
            fonts: None,
            icon: None,
            menu: MenuStyle::Cascading,
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
        if !theme_source_ok(&self.from) {
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
        if let Some(p) = self.icon.as_ref().and_then(icon_problem) {
            return Some(format!("icon: {p}"));
        }
        if self.menu == MenuStyle::Mega && self.navigation != Navigation::Horizontal {
            return Some("a mega menu needs horizontal navigation".to_owned());
        }
        if let Some(fonts) = &self.fonts {
            if !font_source_ok(&fonts.from) {
                return Some(
                    "fonts come from builtin:system, builtin:classic, org:<id> or site:<id>"
                        .to_owned(),
                );
            }
            if let Some(p) = fonts.package.problem() {
                return Some(format!("fonts: {p}"));
            }
            let built_in = BUILT_IN_FONT_PACKAGES.iter().find(|b| b.id == fonts.from);
            if built_in.is_some_and(|b| b.package() != fonts.package) {
                return Some("a built-in font package is kept exactly as it is".to_owned());
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
        if let Some(frame) = &h.frame {
            if h.image.is_none() {
                return Some("a header frame goes with a header image".to_owned());
            }
            if !frame.is_valid() {
                return Some("a header frame is x and y 0 to 1000 and zoom 100 to 400".to_owned());
            }
        }
        h.bleed.problem()
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
    prefixed_uuid_ok(text, "org:") || prefixed_uuid_ok(text, "site:")
}

// ---------------------------------------------------------------------------------------------
// Template looks

/// An image a template's look refers to: a brand-centre image by its identifier, never a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TemplateImage {
    /// `{"brand_image": id}`.
    BrandImage(Uuid),
}

/// A font a template's look refers to: a brand-centre font by its identifier, never a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TemplateFontRef {
    /// `{"brand_font": id}`.
    BrandFont(Uuid),
}

/// A [`Header`] whose image is a reference.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TemplateHeader {
    /// The layout.
    pub layout: HeaderLayout,
    /// The background.
    pub background: Background,
    /// The background image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<TemplateImage>,
    /// Where the content sits.
    pub align: Align,
    /// Whether the site's name is left out because the logo says it.
    pub hide_title: bool,
    /// How the banner picture bleeds past the banner.
    #[serde(default, skip_serializing_if = "Bleed::is_default")]
    pub bleed: Bleed,
    /// Which part of the image the header shows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<HeroFrame>,
}

/// A [`Footer`] whose logo is a reference.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TemplateFooter {
    /// Whether the footer is shown.
    pub enabled: bool,
    /// The layout.
    pub layout: FooterLayout,
    /// A logo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logo: Option<TemplateImage>,
    /// The name shown.
    pub name: String,
    /// The background.
    pub background: Background,
    /// The link groups.
    pub groups: Vec<FooterGroup>,
}

/// A [`FontPackage`] whose fonts are references, without an identifier.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TemplateFontPackage {
    /// The package's name.
    pub name: String,
    /// The display font.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<TemplateFontRef>,
    /// The content font.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<TemplateFontRef>,
    /// What each role is set in.
    pub roles: FontRoles,
}

/// The fonts of a template's look.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TemplateFonts {
    /// The package.
    pub package: TemplateFontPackage,
    /// Where the package came from, as in [`LookFonts::from`].
    pub from: String,
}

/// A look a template carries: a [`Look`] whose images and fonts are references to brand-centre
/// entries, so a template holds no files. A site made from the template gets them pinned.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TemplateLook {
    /// The theme.
    pub theme: Theme,
    /// Where the theme came from, as in [`Look::from`].
    pub from: String,
    /// The logo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logo: Option<TemplateImage>,
    /// The logo thumbnail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<TemplateImage>,
    /// What the logo shows.
    pub logo_alt: String,
    /// The header.
    pub header: TemplateHeader,
    /// The navigation style.
    pub navigation: Navigation,
    /// The footer.
    pub footer: TemplateFooter,
    /// The fonts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fonts: Option<TemplateFonts>,
    /// The tab icon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<TemplateImage>,
    /// The menu style.
    #[serde(default, skip_serializing_if = "MenuStyle::is_cascading")]
    pub menu: MenuStyle,
}

impl TemplateLook {
    /// Why the look cannot be stored, if so: every rule of [`Look::problem`] that does not
    /// depend on a file (text, layouts, links, theme source, font roles). Whether a reference
    /// names something the organisation has is for the server to say.
    #[must_use]
    pub fn problem(&self) -> Option<String> {
        self.stand_in().problem(ANY_TEMPLATE_SITE_NAME)
    }

    /// A [`Look`] with a valid placeholder in each place this look has a reference.
    fn stand_in(&self) -> Look {
        let image = |r: &Option<TemplateImage>| {
            r.map(|_| ImageRef {
                sha256: "0".repeat(64),
                media_type: "image/png".to_owned(),
                bytes: 1,
                width: Some(MIN_ICON_SIDE),
                height: Some(MIN_ICON_SIDE),
                name: "reference".to_owned(),
                from_doc: Uuid::nil(),
            })
        };
        let font = |r: &Option<TemplateFontRef>| {
            r.map(|_| FontRef {
                sha256: "0".repeat(64),
                media_type: "font/woff2".to_owned(),
                bytes: 1,
                name: "reference".to_owned(),
                from_doc: Uuid::nil(),
                licence: Licence {
                    confirmed_by: Uuid::nil(),
                    confirmed_at: 1,
                },
            })
        };
        Look {
            theme: self.theme.clone(),
            from: self.from.clone(),
            logo: image(&self.logo),
            thumbnail: image(&self.thumbnail),
            logo_alt: self.logo_alt.clone(),
            header: Header {
                layout: self.header.layout,
                background: self.header.background,
                image: image(&self.header.image),
                align: self.header.align,
                hide_title: self.header.hide_title,
                bleed: self.header.bleed,
                frame: self.header.frame,
            },
            navigation: self.navigation,
            footer: Footer {
                enabled: self.footer.enabled,
                layout: self.footer.layout,
                logo: image(&self.footer.logo),
                name: self.footer.name.clone(),
                background: self.footer.background,
                groups: self.footer.groups.clone(),
            },
            fonts: self.fonts.as_ref().map(|f| LookFonts {
                package: FontPackage {
                    id: Uuid::nil(),
                    name: f.package.name.clone(),
                    display: font(&f.package.display),
                    content: font(&f.package.content),
                    roles: f.package.roles,
                },
                from: f.from.clone(),
            }),
            icon: image(&self.icon),
            menu: self.menu,
        }
    }
}

/// The site name a template's look is checked with: a template has none.
const ANY_TEMPLATE_SITE_NAME: &str = "site";

// ---------------------------------------------------------------------------------------------
// The brand policy

/// The limits on what site owners may choose, kept in the brand centre. The first limit is a
/// restriction when `true`; the others are permissions and restrict when `false`. Every limit
/// starts off ([`BrandPolicy::default`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct BrandPolicy {
    /// Whether the built-in themes are hidden from site owners.
    pub hide_builtin_themes: bool,
    /// Whether site owners may choose their own colours.
    pub custom_colours: bool,
    /// Whether site owners may make themes only their site offers.
    pub site_themes: bool,
    /// Whether site owners may use images from their site's own libraries.
    pub site_images: bool,
    /// Whether site owners may make font packages only their site offers.
    pub site_fonts: bool,
}

impl Default for BrandPolicy {
    /// Every limit off: nothing is restricted.
    fn default() -> BrandPolicy {
        BrandPolicy {
            hide_builtin_themes: false,
            custom_colours: true,
            site_themes: true,
            site_images: true,
            site_fonts: true,
        }
    }
}

impl BrandPolicy {
    /// Whether every limit is off; a record in that state leaves the policy out of its file.
    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == BrandPolicy::default()
    }
}

/// Who is changing a site's look. Only site owners are limited by the policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Actor {
    /// A site owner (a person with `manage_site`, who is neither of the others).
    Owner,
    /// A brand manager.
    BrandManager,
    /// An administrator.
    Admin,
}

/// The records a look's named sources are checked against: the brand centre and the site's own
/// themes and font packages, exactly as the server holds them now.
#[derive(Clone, Copy, Debug)]
pub struct KnownSources<'a> {
    /// The organisation's brand centre.
    pub org: &'a OrgBrand,
    /// The site's own themes.
    pub site_themes: &'a [SiteTheme],
    /// The site's own font packages.
    pub site_packages: &'a [FontPackage],
}

/// The identifier after `prefix` in `from`, if it is a canonical UUID.
fn source_id(from: &str, prefix: &str) -> Option<Uuid> {
    let id = from.strip_prefix(prefix)?;
    Uuid::parse_str(id).ok().filter(|u| u.to_string() == id)
}

/// Whether the theme of `look` is the copy of the source it names: a built-in theme exactly, an
/// `org:` theme the brand centre's theme of that identifier, a `site:` theme the site's own.
/// `custom` names no source and answers `true`; an unknown identifier, or a copy whose name or
/// colours differ from its source, answers `false`.
#[must_use]
pub fn theme_matches_source(look: &Look, known: &KnownSources<'_>) -> bool {
    let from = look.from.as_str();
    if from == "custom" {
        return true;
    }
    if let Some(built_in) = BUILT_IN_THEMES.iter().find(|t| t.id == from) {
        return built_in.theme() == look.theme;
    }
    let in_org =
        source_id(from, "org:").and_then(|id| known.org.themes.iter().find(|t| t.id == id));
    let in_site =
        source_id(from, "site:").and_then(|id| known.site_themes.iter().find(|t| t.id == id));
    in_org.or(in_site).is_some_and(|t| t.theme() == look.theme)
}

/// Whether the fonts of `look` are the copy of the package they name, the same way as
/// [`theme_matches_source`]: a built-in package exactly, an `org:` package the brand centre's
/// of that identifier, a `site:` package the site's own. A look without fonts answers `true`.
#[must_use]
pub fn fonts_match_source(look: &Look, known: &KnownSources<'_>) -> bool {
    let Some(fonts) = &look.fonts else {
        return true;
    };
    let from = fonts.from.as_str();
    if let Some(built_in) = BUILT_IN_FONT_PACKAGES.iter().find(|b| b.id == from) {
        return built_in.package() == fonts.package;
    }
    let in_org =
        source_id(from, "org:").and_then(|id| known.org.font_packages.iter().find(|p| p.id == id));
    let in_site =
        source_id(from, "site:").and_then(|id| known.site_packages.iter().find(|p| p.id == id));
    in_org.or(in_site).is_some_and(|p| *p == fonts.package)
}

/// What `policy` refuses to `actor` in a change of a site's look, in plain words, each problem
/// once. `site_themes` and `site_packages` are the site-only themes and font packages this
/// request makes or changes (empty when it makes none). Nothing is refused to a brand manager
/// or an administrator, and nothing while every limit is off. A look already stored is never
/// checked again; the policy applies to the next change.
///
/// A look names where its theme and fonts came from, and a label is not trusted: `known` holds
/// the brand centre and the site's own records as the server has them, and a theme or font
/// package whose copy does not equal what its label names (a built-in one changed, an `org:` or
/// `site:` identifier nobody has, or colours or fonts that differ from that source) counts as a
/// custom colour scheme, or as site-only fonts, whatever it is labelled. A built-in theme is
/// hidden by `hide_builtin_themes` only when it really is the built-in one.
///
/// Images need the brand centre's images to be told apart from a site's own: see
/// [`image_policy_problems`].
#[must_use]
pub fn policy_problems(
    look: &Look,
    policy: &BrandPolicy,
    known: &KnownSources<'_>,
    site_themes: &[SiteTheme],
    site_packages: &[FontPackage],
    actor: Actor,
) -> Vec<String> {
    let mut problems = Vec::new();
    if actor != Actor::Owner {
        return problems;
    }
    let theme_ok = theme_matches_source(look, known);
    if policy.hide_builtin_themes && look.from.starts_with("builtin:") && theme_ok {
        problems.push("built-in themes are turned off for site owners".to_owned());
    }
    if !policy.custom_colours && (look.from == "custom" || !theme_ok) {
        problems.push("custom colours are turned off for site owners".to_owned());
    }
    if !policy.site_themes
        && ((look.from.starts_with("site:") && theme_ok) || !site_themes.is_empty())
    {
        problems.push("site-only themes are turned off for site owners".to_owned());
    }
    let site_font_used = look
        .fonts
        .as_ref()
        .is_some_and(|f| !fonts_match_source(look, known) || f.from.starts_with("site:"));
    if !policy.site_fonts && (site_font_used || !site_packages.is_empty()) {
        problems.push("site-only font packages are turned off for site owners".to_owned());
    }
    problems
}

/// What `policy` refuses to `actor` among the images of `look`: with `site_images` off, a site
/// owner may use only images the brand centre holds (`brand_images`, matched by content), so
/// each logo, thumbnail, icon, header image or footer logo that is not one is a problem. Nothing
/// is refused to a brand manager or an administrator, or while the limit is off.
#[must_use]
pub fn image_policy_problems(
    look: &Look,
    policy: &BrandPolicy,
    brand_images: &[BrandImage],
    actor: Actor,
) -> Vec<String> {
    if actor != Actor::Owner || policy.site_images {
        return Vec::new();
    }
    [
        ("logo", &look.logo),
        ("thumbnail", &look.thumbnail),
        ("icon", &look.icon),
        ("header image", &look.header.image),
        ("footer logo", &look.footer.logo),
    ]
    .into_iter()
    .filter_map(|(slot, image)| {
        let image = image.as_ref()?;
        let known = brand_images.iter().any(|b| b.image.sha256 == image.sha256);
        (!known).then(|| {
            format!("{slot}: images from the site's own libraries are turned off for site owners")
        })
    })
    .collect()
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
    /// The approved fonts: at most 30. Left out of the file when there are none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fonts: Vec<BrandFont>,
    /// The approved font packages: at most 20, names unique ignoring case. Left out of the
    /// file when there are none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub font_packages: Vec<FontPackage>,
    /// The organisation's default tab icon: a brand image of kind `icon`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_icon: Option<Uuid>,
    /// The limits on site owners; left out of the file while every limit is off.
    #[serde(default, skip_serializing_if = "BrandPolicy::is_default")]
    pub policy: BrandPolicy,
}

impl Default for OrgBrand {
    /// An empty brand centre.
    fn default() -> OrgBrand {
        OrgBrand {
            schema: BRAND_SCHEMA,
            managers: Vec::new(),
            themes: Vec::new(),
            images: Vec::new(),
            fonts: Vec::new(),
            font_packages: Vec::new(),
            default_icon: None,
            policy: BrandPolicy::default(),
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
        if let Some(p) = self
            .images
            .iter()
            .find_map(|i| i.image.problem(i.kind.max_bytes()))
        {
            return Some(p);
        }
        self.font_problem()
    }

    /// The part of [`OrgBrand::problem`] about fonts, font packages and the default icon.
    fn font_problem(&self) -> Option<String> {
        if self.fonts.len() > MAX_ORG_FONTS {
            return Some(format!("at most {MAX_ORG_FONTS} fonts"));
        }
        if self.font_packages.len() > MAX_ORG_FONT_PACKAGES {
            return Some(format!("at most {MAX_ORG_FONT_PACKAGES} font packages"));
        }
        if has_duplicates(self.fonts.iter().map(|f| f.id)) {
            return Some("a font identifier is used twice".to_owned());
        }
        if has_duplicates(self.font_packages.iter().map(|p| p.id)) {
            return Some("a font package identifier is used twice".to_owned());
        }
        if has_duplicates(self.font_packages.iter().map(|p| p.name.to_lowercase())) {
            return Some("font package names are unique, ignoring case".to_owned());
        }
        if let Some(p) = self.fonts.iter().find_map(|f| f.font.problem()) {
            return Some(p);
        }
        if let Some(p) = self.font_packages.iter().find_map(FontPackage::problem) {
            return Some(p);
        }
        let is_icon = |id: Uuid| {
            self.images
                .iter()
                .any(|i| i.id == id && i.kind == ImageKind::Icon)
        };
        match self.default_icon {
            Some(id) if !is_icon(id) => {
                Some("the default icon is a brand image of kind icon".to_owned())
            }
            _ => None,
        }
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
    clippy::indexing_slicing,
    clippy::field_reassign_with_default
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
            change(&|l| l.from = from.into());
        }
        for from in [
            "custom",
            "builtin:rust",
            "org:5e5e5e5e-5e5e-45e5-85e5-5e5e5e5e5e5e",
            "site:5e5e5e5e-5e5e-45e5-85e5-5e5e5e5e5e5e",
        ] {
            let mut look = ok.clone();
            look.from = from.into();
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
            bleed: Bleed::default(),
            frame: None,
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
            ..OrgBrand::default()
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
        look.from = "org:11111111-1111-4111-8111-111111111111".into();
        look.logo = Some(image(2048));
        look.logo_alt = "The Dusk logo".into();
        look.header = Header {
            layout: HeaderLayout::Extended,
            background: Background::Strong,
            image: None,
            align: Align::Centre,
            hide_title: true,
            bleed: Bleed::default(),
            frame: None,
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
  "from": "org:11111111-1111-4111-8111-111111111111",
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

    // ---- Plan 4B: fonts, the policy, icons ------------------------------------------------

    fn font(name: &str) -> FontRef {
        FontRef {
            sha256: "cd".repeat(32),
            media_type: "font/woff2".into(),
            bytes: 50_000,
            name: name.into(),
            from_doc: doc(),
            licence: Licence {
                confirmed_by: doc(),
                confirmed_at: 1_763_200_000,
            },
        }
    }

    fn roles(
        title: FontRole,
        heading: FontRole,
        body: FontRole,
        interactive: FontRole,
    ) -> FontRoles {
        FontRoles {
            title,
            heading,
            body,
            interactive,
        }
    }

    fn package(name: &str) -> FontPackage {
        FontPackage {
            id: Uuid::parse_str("33333333-3333-4333-8333-333333333333").unwrap(),
            name: name.into(),
            display: Some(font("Display.woff2")),
            content: Some(font("Content.woff2")),
            roles: roles(
                FontRole::Display,
                FontRole::Display,
                FontRole::Content,
                FontRole::System,
            ),
        }
    }

    fn uuid(n: u8) -> Uuid {
        Uuid::from_bytes([n; 16])
    }

    #[test]
    fn font_sniffing() {
        let table = vectors();
        let rows = table["font_sniffing"].as_array().unwrap();
        assert!(rows.len() >= 8);
        for row in rows {
            let bytes: Vec<u8> = row["hex"]
                .as_str()
                .unwrap()
                .as_bytes()
                .chunks(2)
                .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
                .collect();
            let want = row["kind"].as_str().map(|k| match k {
                "woff2" => FontKind::Woff2,
                "woff" => FontKind::Woff,
                "ttf" => FontKind::Ttf,
                _ => FontKind::Otf,
            });
            assert_eq!(sniff_font(&bytes), want, "{}", row["note"]);
        }
        assert_eq!(sniff_font(b"wOF2\0\0\0\0"), Some(FontKind::Woff2));
        assert_eq!(sniff_font(b"wOFF\0\0\0\0"), Some(FontKind::Woff));
        assert_eq!(sniff_font(&[0, 1, 0, 0, 9]), Some(FontKind::Ttf));
        assert_eq!(sniff_font(b"true\0"), Some(FontKind::Ttf));
        assert_eq!(sniff_font(b"OTTO\0"), Some(FontKind::Otf));
        // A renamed web page or picture is not a font, whatever its name says.
        assert_eq!(sniff_font(b"<!doctype html><title>x</title>"), None);
        assert_eq!(
            sniff_font(b"<svg xmlns='http://www.w3.org/2000/svg'/>"),
            None
        );
        assert_eq!(sniff_font(b""), None);
        assert_eq!(sniff_font(b"wOF"), None);
        assert_eq!(sniff_font(&[0, 1, 0]), None);
        // Only the start counts, and case matters.
        assert_eq!(sniff_font(b"woff2"), None);
        assert_eq!(sniff_font(b" wOF2"), None);
        assert_eq!(FontKind::Woff2.media_type(), "font/woff2");
        assert_eq!(FontKind::Woff.media_type(), "font/woff");
        assert_eq!(FontKind::Ttf.media_type(), "font/ttf");
        assert_eq!(FontKind::Otf.media_type(), "font/otf");
        for kind in [
            FontKind::Woff2,
            FontKind::Woff,
            FontKind::Ttf,
            FontKind::Otf,
        ] {
            assert!(FONT_MEDIA_TYPES.contains(&kind.media_type()));
        }
    }

    #[test]
    fn font_package_problems() {
        assert_eq!(package("Brand pair").problem(), None);
        // Names are text: markup and CSS are kept as text, never run.
        assert_eq!(package("<b>x</b>").problem(), None);
        assert_eq!(package("\"; } body { display:none").problem(), None);
        for bad in ["", " x", "x ", "a\nb", "a\u{202e}b"] {
            assert!(package(bad).problem().is_some(), "{bad:?}");
        }
        assert_eq!(package(&"n".repeat(60)).problem(), None);
        assert!(package(&"n".repeat(61)).problem().is_some());

        // A role names only a font the package has.
        let mut p = package("P");
        p.display = None;
        assert!(p.problem().unwrap().contains("display"));
        let mut p = package("P");
        p.content = None;
        assert!(p.problem().unwrap().contains("content"));
        let mut p = package("P");
        p.display = None;
        p.content = None;
        p.roles = roles(
            FontRole::System,
            FontRole::SystemSerif,
            FontRole::System,
            FontRole::System,
        );
        assert_eq!(p.problem(), None);
        // Fonts the roles do not use are allowed.
        let mut p = package("P");
        p.roles = roles(
            FontRole::System,
            FontRole::System,
            FontRole::System,
            FontRole::System,
        );
        assert_eq!(p.problem(), None);

        // The font files: hash, type, size, name and licence.
        let broken = |change: fn(&mut FontRef)| {
            let mut p = package("P");
            let mut f = font("Display.woff2");
            change(&mut f);
            p.display = Some(f);
            p.problem()
        };
        assert!(broken(|f| f.sha256 = "AB".repeat(32)).is_some());
        assert!(broken(|f| f.sha256 = "ab".repeat(31)).is_some());
        assert!(broken(|f| f.media_type = "font/eot".into()).is_some());
        assert!(broken(|f| f.media_type = "text/html".into()).is_some());
        assert!(broken(|f| f.bytes = 0).is_some());
        assert_eq!(broken(|f| f.bytes = MAX_FONT_BYTES), None);
        assert!(broken(|f| f.bytes = MAX_FONT_BYTES + 1).is_some());
        assert!(broken(|f| f.name = String::new()).is_some());
        assert!(broken(|f| f.name = "a\u{0}b".into()).is_some());
        assert_eq!(
            broken(|f| f.name = "\"; } body { display:none".into()),
            None
        );
        assert!(broken(|f| f.licence.confirmed_at = 0).is_some());

        // The two built-in packages need no files.
        let [system, classic] = BUILT_IN_FONT_PACKAGES;
        assert_eq!(system.id, "builtin:system");
        assert_eq!(classic.id, "builtin:classic");
        assert_eq!(
            system.roles,
            roles(
                FontRole::System,
                FontRole::System,
                FontRole::System,
                FontRole::System
            )
        );
        assert_eq!(
            classic.roles,
            roles(
                FontRole::SystemSerif,
                FontRole::SystemSerif,
                FontRole::System,
                FontRole::System
            )
        );
        for built_in in BUILT_IN_FONT_PACKAGES {
            assert_eq!(built_in.package().problem(), None, "{}", built_in.id);
            assert!(built_in.package().display.is_none());
            assert!(built_in.package().content.is_none());
        }
        assert!(font_source_ok("builtin:system"));
        assert!(font_source_ok("builtin:classic"));
        assert!(font_source_ok("org:11111111-1111-4111-8111-111111111111"));
        assert!(font_source_ok("site:11111111-1111-4111-8111-111111111111"));
        for bad in ["custom", "builtin:harbour", "org:x", "", "org:", "site:ABC"] {
            assert!(!font_source_ok(bad), "{bad:?}");
        }
    }

    #[test]
    fn family_names_are_generated() {
        let table = vectors();
        for row in table["family_names"].as_array().unwrap() {
            assert_eq!(
                family_name(row["sha256"].as_str().unwrap()),
                row["family"].as_str().unwrap()
            );
        }
        assert_eq!(family_name(&"ab".repeat(32)), "sc-abababababab");
        // Only the first 12 characters count, so equal files share one name.
        assert_eq!(
            family_name(&format!("0123456789ab{}", "f".repeat(52))),
            "sc-0123456789ab"
        );
        // Whatever is passed, the answer is `sc-` and 12 lowercase hexadecimal characters.
        for hostile in [
            "",
            "x",
            "\"; } body { display:none",
            "<script>alert(1)</script>",
        ] {
            let name = family_name(hostile);
            assert_eq!(name.len(), 15, "{hostile:?} -> {name:?}");
            assert!(name.starts_with("sc-"));
            assert!(
                name[3..]
                    .bytes()
                    .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
                "{name:?}"
            );
        }
    }

    #[test]
    fn icon_problems() {
        let png = |w: u32, h: u32, bytes: u64| ImageRef {
            media_type: "image/png".into(),
            width: Some(w),
            height: Some(h),
            bytes,
            ..image(0)
        };
        assert_eq!(icon_problem(&png(64, 64, 1000)), None);
        assert_eq!(icon_problem(&png(32, 32, 1000)), None);
        assert_eq!(icon_problem(&png(64, 70, 1000)), None);
        assert_eq!(icon_problem(&png(70, 64, 1000)), None);
        assert_eq!(icon_problem(&png(512, 512, MAX_ICON_BYTES)), None);
        // Too small, not square within 10 %, too large, or no size known.
        assert!(icon_problem(&png(31, 31, 1000)).unwrap().contains("32"));
        assert!(icon_problem(&png(64, 71, 1000)).unwrap().contains("square"));
        assert!(icon_problem(&png(71, 64, 1000)).unwrap().contains("square"));
        assert!(icon_problem(&png(64, 128, 1000)).is_some());
        assert!(icon_problem(&png(64, 64, MAX_ICON_BYTES + 1)).is_some());
        assert!(icon_problem(&png(64, 64, 0)).is_some());
        let no_size = ImageRef {
            width: None,
            height: None,
            ..png(64, 64, 1000)
        };
        assert!(icon_problem(&no_size).is_some());
        // Other formats are not icons.
        for media in ["image/jpeg", "image/webp", "image/gif"] {
            let other = ImageRef {
                media_type: media.into(),
                ..png(64, 64, 1000)
            };
            assert!(icon_problem(&other).is_some(), "{media}");
        }
        // An svg may leave its size out; if it has one it is square too.
        let svg = ImageRef {
            media_type: "image/svg+xml".into(),
            width: None,
            height: None,
            ..png(64, 64, 1000)
        };
        assert_eq!(icon_problem(&svg), None);
        let small_svg = ImageRef {
            width: Some(16),
            height: Some(16),
            ..svg.clone()
        };
        assert_eq!(icon_problem(&small_svg), None);
        let wide_svg = ImageRef {
            width: Some(100),
            height: Some(40),
            ..svg.clone()
        };
        assert!(icon_problem(&wide_svg).is_some());
        let big_svg = ImageRef {
            bytes: MAX_ICON_BYTES + 1,
            ..svg
        };
        assert!(icon_problem(&big_svg).is_some());
        // The usual reference checks still apply (hash, name).
        let bad_hash = ImageRef {
            sha256: "nope".into(),
            ..png(64, 64, 1000)
        };
        assert!(icon_problem(&bad_hash).is_some());
    }

    #[test]
    fn look_additions_refused() {
        let mut look = Look::default();
        look.icon = Some(ImageRef {
            media_type: "image/jpeg".into(),
            ..image(1000)
        });
        assert!(look.problem("Site").unwrap().starts_with("icon: "));
        look.icon = None;
        look.menu = MenuStyle::Mega;
        assert_eq!(look.problem("Site"), None);
        look.navigation = Navigation::Vertical;
        assert!(look.problem("Site").unwrap().contains("horizontal"));
        look.navigation = Navigation::Horizontal;
        look.fonts = Some(LookFonts {
            package: package("Pair"),
            from: "builtin:harbour".into(),
        });
        assert!(look.problem("Site").is_some());
        look.fonts = Some(LookFonts {
            package: FontPackage {
                name: String::new(),
                ..package("x")
            },
            from: "org:11111111-1111-4111-8111-111111111111".into(),
        });
        assert!(look.problem("Site").is_some());
        look.fonts = Some(LookFonts {
            package: package("Pair"),
            from: "org:11111111-1111-4111-8111-111111111111".into(),
        });
        assert_eq!(look.problem("Site"), None);
        // A package copied from a built-in one carries the nil identifier and no files.
        look.fonts = Some(LookFonts {
            package: BUILT_IN_FONT_PACKAGES[1].package(),
            from: "builtin:classic".into(),
        });
        assert_eq!(look.problem("Site"), None);
        assert_eq!(BUILT_IN_FONT_PACKAGES[1].package().id, Uuid::nil());
    }

    #[test]
    fn org_brand_font_limits_refused() {
        let entry = |n: u8| BrandFont {
            id: uuid(n),
            font: font("F.woff2"),
        };
        let mut brand = OrgBrand::default();
        brand.fonts = (1..=30).map(entry).collect();
        assert_eq!(brand.problem(), None);
        brand.fonts.push(entry(31));
        assert!(brand.problem().unwrap().contains("30"));
        brand.fonts.truncate(2);
        brand.fonts[1].id = uuid(1);
        assert!(brand.problem().is_some());
        brand.fonts[1].id = uuid(2);
        brand.fonts[1].font.bytes = 0;
        assert!(brand.problem().is_some());

        let mut brand = OrgBrand::default();
        brand.font_packages = (1..=20)
            .map(|n| FontPackage {
                id: uuid(n),
                name: format!("Package {n}"),
                ..package("x")
            })
            .collect();
        assert_eq!(brand.problem(), None);
        brand.font_packages.push(FontPackage {
            id: uuid(21),
            name: "Package 21".into(),
            ..package("x")
        });
        assert!(brand.problem().unwrap().contains("20"));
        brand.font_packages.truncate(2);
        brand.font_packages[1].name = "PACKAGE 1".into();
        assert!(brand.problem().unwrap().contains("unique"));
        brand.font_packages[1].name = "Package 2".into();
        brand.font_packages[1].id = uuid(1);
        assert!(brand.problem().is_some());

        // The default icon is a brand image of kind icon.
        let mut brand = OrgBrand::default();
        brand.images.push(BrandImage {
            id: uuid(7),
            kind: ImageKind::Logo,
            image: image(100),
        });
        brand.default_icon = Some(uuid(7));
        assert!(brand.problem().unwrap().contains("icon"));
        brand.images[0].kind = ImageKind::Icon;
        assert_eq!(brand.problem(), None);
        brand.default_icon = Some(uuid(8));
        assert!(brand.problem().is_some());
    }

    /// The sources a look's labels name, made to hold exactly the copies the look carries.
    fn pp(
        look: &Look,
        policy: &BrandPolicy,
        site_themes: &[SiteTheme],
        site_packages: &[FontPackage],
        actor: Actor,
    ) -> Vec<String> {
        let theme_id = |prefix: &str| {
            look.from
                .strip_prefix(prefix)
                .map(|id| Uuid::parse_str(id).unwrap())
        };
        let as_theme = |id: Uuid| OrgTheme {
            id,
            name: look.theme.name.clone(),
            primary: look.theme.primary,
            secondary: look.theme.secondary,
            primary_dark: look.theme.primary_dark,
            secondary_dark: look.theme.secondary_dark,
            changed_at: 1,
        };
        let mut org = OrgBrand::default();
        org.themes.extend(theme_id("org:").map(as_theme));
        let own_themes: Vec<SiteTheme> = theme_id("site:").map(as_theme).into_iter().collect();
        let mut own_packages = Vec::new();
        if let Some(fonts) = &look.fonts {
            if fonts.from.starts_with("org:") {
                org.font_packages.push(fonts.package.clone());
            } else if fonts.from.starts_with("site:") {
                own_packages.push(fonts.package.clone());
            }
        }
        let known = KnownSources {
            org: &org,
            site_themes: &own_themes,
            site_packages: &own_packages,
        };
        policy_problems(look, policy, &known, site_themes, site_packages, actor)
    }

    #[test]
    fn labels_must_match_their_sources() {
        let mut org = OrgBrand::default();
        let org_id = uuid(1);
        let org_theme = OrgTheme {
            id: org_id,
            name: "Dusk".into(),
            primary: colour("#3156c9"),
            secondary: colour("#233239"),
            primary_dark: None,
            secondary_dark: None,
            changed_at: 1,
        };
        org.themes.push(org_theme.clone());
        let org_package = package("Pair");
        org.font_packages.push(org_package.clone());
        let known = KnownSources {
            org: &org,
            site_themes: &[],
            site_packages: &[],
        };
        let strict = BrandPolicy {
            hide_builtin_themes: false,
            custom_colours: false,
            site_themes: true,
            site_images: true,
            site_fonts: false,
        };
        let check = |look: &Look| policy_problems(look, &strict, &known, &[], &[], Actor::Owner);
        let pink = Theme {
            name: "Pink".into(),
            primary: colour("#aa1166"),
            secondary: colour("#233239"),
            primary_dark: None,
            secondary_dark: None,
        };
        let custom = "custom colours are turned off for site owners";
        let fonts = "site-only font packages are turned off for site owners";

        // Honest labels pass: the built-in theme, the brand centre's theme and package.
        assert!(check(&Look::default()).is_empty());
        let honest = Look {
            theme: org_theme.theme(),
            from: format!("org:{org_id}"),
            fonts: Some(LookFonts {
                package: org_package.clone(),
                from: format!("org:{}", org_package.id),
            }),
            ..Look::default()
        };
        assert!(check(&honest).is_empty());
        for built_in in BUILT_IN_FONT_PACKAGES {
            let look = Look {
                fonts: Some(LookFonts {
                    package: built_in.package(),
                    from: built_in.id.into(),
                }),
                ..Look::default()
            };
            assert!(check(&look).is_empty(), "{}", built_in.id);
        }

        // A built-in label on other colours is a custom theme.
        let spoofed = Look {
            theme: pink.clone(),
            ..Look::default()
        };
        assert_eq!(check(&spoofed), [custom]);
        // An org label with an id the brand centre does not have.
        let unknown = Look {
            theme: pink.clone(),
            from: format!("org:{}", uuid(77)),
            ..Look::default()
        };
        assert_eq!(check(&unknown), [custom]);
        // An org label whose colours differ from that org theme.
        let edited = Look {
            theme: pink,
            from: format!("org:{org_id}"),
            ..Look::default()
        };
        assert_eq!(check(&edited), [custom]);
        // A site label for a theme the site does not have.
        let not_there = Look {
            from: format!("site:{}", uuid(78)),
            ..Look::default()
        };
        assert_eq!(check(&not_there), [custom]);

        // The same three for fonts: a built-in label on uploaded fonts, an org label with an
        // unknown id, and an org label on a package that differs from the brand centre's.
        let with_fonts = |package: FontPackage, from: String| Look {
            fonts: Some(LookFonts { package, from }),
            ..Look::default()
        };
        assert_eq!(
            check(&with_fonts(package("Mine"), "builtin:system".into())),
            [fonts]
        );
        assert_eq!(
            check(&with_fonts(
                FontPackage {
                    roles: BUILT_IN_FONT_PACKAGES[1].roles,
                    ..BUILT_IN_FONT_PACKAGES[1].package()
                },
                "builtin:system".into()
            )),
            [fonts]
        );
        assert_eq!(
            check(&with_fonts(package("Mine"), format!("org:{}", uuid(77)))),
            [fonts]
        );
        let changed = FontPackage {
            name: "Changed".into(),
            ..org_package.clone()
        };
        assert_eq!(
            check(&with_fonts(changed, format!("org:{}", org_package.id))),
            [fonts]
        );
        // A hidden built-in theme stays hidden only for the real one.
        let hide = BrandPolicy {
            hide_builtin_themes: true,
            ..BrandPolicy::default()
        };
        let real = policy_problems(&Look::default(), &hide, &known, &[], &[], Actor::Owner);
        assert_eq!(real, ["built-in themes are turned off for site owners"]);
        // Administrators are never limited by a label either.
        assert!(policy_problems(&spoofed, &strict, &known, &[], &[], Actor::Admin).is_empty());
        // A built-in font package that is not exactly the built-in one is not stored at all.
        let bad = with_fonts(package("Mine"), "builtin:system".into());
        assert!(bad.problem("Site").is_some());
    }

    #[test]
    fn policy_problems_by_actor() {
        let none: [SiteTheme; 0] = [];
        let no_packages: [FontPackage; 0] = [];
        let site_theme = SiteTheme {
            id: uuid(9),
            name: "Mine".into(),
            primary: colour("#3156c9"),
            secondary: colour("#233239"),
            primary_dark: None,
            secondary_dark: None,
            changed_at: 1,
        };
        let mut custom = Look::default();
        custom.from = "custom".into();
        let site_look = Look {
            from: format!("site:{}", uuid(9)),
            ..Look::default()
        };
        let site_fonts = Look {
            fonts: Some(LookFonts {
                package: package("Pair"),
                from: format!("site:{}", package("x").id),
            }),
            ..Look::default()
        };

        // Every limit starts off: nothing is refused to anyone.
        let open = BrandPolicy::default();
        assert!(open.is_default());
        assert!(!open.hide_builtin_themes);
        assert!(open.custom_colours && open.site_themes && open.site_images && open.site_fonts);
        for actor in [Actor::Owner, Actor::BrandManager, Actor::Admin] {
            for look in [&Look::default(), &custom, &site_look, &site_fonts] {
                assert_eq!(
                    pp(
                        look,
                        &open,
                        std::slice::from_ref(&site_theme),
                        &[package("P")],
                        actor
                    ),
                    Vec::<String>::new()
                );
            }
        }

        // Each limit refuses its own thing to a site owner, in words, and only that.
        let hide = BrandPolicy {
            hide_builtin_themes: true,
            ..BrandPolicy::default()
        };
        assert!(!hide.is_default());
        assert_eq!(
            pp(&Look::default(), &hide, &none, &no_packages, Actor::Owner),
            ["built-in themes are turned off for site owners"]
        );
        let org_look = Look {
            from: format!("org:{}", uuid(1)),
            ..Look::default()
        };
        assert!(pp(&org_look, &hide, &none, &no_packages, Actor::Owner).is_empty());

        let no_custom = BrandPolicy {
            custom_colours: false,
            ..BrandPolicy::default()
        };
        assert_eq!(
            pp(&custom, &no_custom, &none, &no_packages, Actor::Owner),
            ["custom colours are turned off for site owners"]
        );
        assert!(
            pp(
                &Look::default(),
                &no_custom,
                &none,
                &no_packages,
                Actor::Owner
            )
            .is_empty()
        );

        let no_site_themes = BrandPolicy {
            site_themes: false,
            ..BrandPolicy::default()
        };
        assert_eq!(
            pp(
                &site_look,
                &no_site_themes,
                &none,
                &no_packages,
                Actor::Owner
            ),
            ["site-only themes are turned off for site owners"]
        );
        // Making a site theme is refused the same way.
        assert_eq!(
            pp(
                &Look::default(),
                &no_site_themes,
                std::slice::from_ref(&site_theme),
                &no_packages,
                Actor::Owner
            ),
            ["site-only themes are turned off for site owners"]
        );

        let no_site_fonts = BrandPolicy {
            site_fonts: false,
            ..BrandPolicy::default()
        };
        assert_eq!(
            pp(
                &site_fonts,
                &no_site_fonts,
                &none,
                &no_packages,
                Actor::Owner
            ),
            ["site-only font packages are turned off for site owners"]
        );
        assert_eq!(
            pp(
                &Look::default(),
                &no_site_fonts,
                &none,
                &[package("P")],
                Actor::Owner
            ),
            ["site-only font packages are turned off for site owners"]
        );
        // Built-in and organisation packages are always fine.
        let org_fonts = Look {
            fonts: Some(LookFonts {
                package: package("Pair"),
                from: format!("org:{}", package("x").id),
            }),
            ..Look::default()
        };
        assert!(
            pp(
                &org_fonts,
                &no_site_fonts,
                &none,
                &no_packages,
                Actor::Owner
            )
            .is_empty()
        );

        // Everything off at once: one problem for each, none twice.
        let strict = BrandPolicy {
            hide_builtin_themes: true,
            custom_colours: false,
            site_themes: false,
            site_images: false,
            site_fonts: false,
        };
        let all = pp(
            &Look {
                from: "custom".into(),
                ..site_fonts.clone()
            },
            &strict,
            std::slice::from_ref(&site_theme),
            &[package("P")],
            Actor::Owner,
        );
        // Custom colours, a site-only theme made, a site-only package used and one made.
        assert_eq!(all.len(), 3, "{all:?}");
        let mut sorted = all.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), all.len());
        for p in &all {
            assert!(p.ends_with("turned off for site owners"), "{p}");
        }

        // Brand managers and administrators are never limited.
        for actor in [Actor::BrandManager, Actor::Admin] {
            for look in [&Look::default(), &custom, &site_look, &site_fonts] {
                assert!(
                    pp(
                        look,
                        &strict,
                        std::slice::from_ref(&site_theme),
                        &[package("P")],
                        actor
                    )
                    .is_empty()
                );
            }
        }

        // Images from the site's own libraries are told apart by the brand centre's images.
        let brand_image = BrandImage {
            id: uuid(5),
            kind: ImageKind::Logo,
            image: image(100),
        };
        let own = ImageRef {
            sha256: "ee".repeat(32),
            ..image(100)
        };
        let mut look = Look::default();
        look.logo = Some(image(100));
        look.thumbnail = Some(own.clone());
        look.icon = Some(own.clone());
        look.header.layout = HeaderLayout::Extended;
        look.header.background = Background::Image;
        look.header.image = Some(own.clone());
        look.footer.logo = Some(own);
        let images = std::slice::from_ref(&brand_image);
        assert!(image_policy_problems(&look, &open, images, Actor::Owner).is_empty());
        let no_images = BrandPolicy {
            site_images: false,
            ..BrandPolicy::default()
        };
        let problems = image_policy_problems(&look, &no_images, images, Actor::Owner);
        assert_eq!(problems.len(), 4, "{problems:?}");
        assert!(problems.iter().all(|p| p.contains("site's own libraries")));
        for slot in ["thumbnail", "icon", "header image", "footer logo"] {
            assert!(problems.iter().any(|p| p.starts_with(slot)), "{slot}");
        }
        for actor in [Actor::BrandManager, Actor::Admin] {
            assert!(image_policy_problems(&look, &no_images, images, actor).is_empty());
        }
    }

    /// Records written by plan 4A, as their files are: the new fields must neither appear nor
    /// change a byte when such a record is read and written back.
    #[test]
    fn new_fields_keep_existing_bytes() {
        let brand = format!(
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
      "kind": "icon",
      "image": {{
        "sha256": "{hash}",
        "media_type": "image/png",
        "bytes": 1234,
        "width": 64,
        "height": 64,
        "name": "mark.png",
        "from_doc": "5e5e5e5e-5e5e-45e5-85e5-5e5e5e5e5e5e"
      }}
    }}
  ]
}}
"##,
            hash = "ab".repeat(32)
        );
        let record: OrgBrand = serde_json::from_str(&brand).unwrap();
        assert!(record.fonts.is_empty() && record.font_packages.is_empty());
        assert_eq!(record.default_icon, None);
        assert!(record.policy.is_default());
        assert_eq!(file_bytes(&record).unwrap(), brand.as_bytes());

        let site = r##"{
  "schema": 1,
  "id": "44444444-4444-4444-8444-444444444444",
  "kind": "team",
  "description": "",
  "language": "en",
  "time_zone": "UTC",
  "template": null,
  "archived": null,
  "hub": null,
  "joined_hub": null,
  "navigation": null,
  "look": {
    "theme": {
      "name": "ShareCraft",
      "primary": "#3156c9",
      "secondary": "#233239"
    },
    "from": "builtin:sharecraft",
    "logo_alt": "",
    "header": {
      "layout": "compact",
      "background": "none",
      "align": "left",
      "hide_title": false
    },
    "navigation": "horizontal",
    "footer": {
      "enabled": false,
      "layout": "simple",
      "name": "",
      "background": "none",
      "groups": []
    }
  },
  "themes": [
    {
      "id": "11111111-1111-4111-8111-111111111111",
      "name": "Dusk",
      "primary": "#3156c9",
      "secondary": "#233239",
      "changed_at": 1763200000
    }
  ]
}
"##;
        let record: crate::sites::SiteRecord = serde_json::from_str(site).unwrap();
        assert!(record.font_packages.is_empty());
        let look = record.look.as_ref().unwrap();
        assert!(look.fonts.is_none() && look.icon.is_none());
        assert_eq!(look.menu, MenuStyle::Cascading);
        assert_eq!(file_bytes(&record).unwrap(), site.as_bytes());

        // A bare site and a bare template body too.
        let bare = r##"{
  "schema": 1,
  "id": "44444444-4444-4444-8444-444444444444",
  "kind": "team",
  "description": "",
  "language": "en",
  "time_zone": "UTC",
  "template": null,
  "archived": null,
  "hub": null,
  "joined_hub": null,
  "navigation": null
}
"##;
        let record: crate::sites::SiteRecord = serde_json::from_str(bare).unwrap();
        assert_eq!(file_bytes(&record).unwrap(), bare.as_bytes());
        let body = r##"{
  "kind": "team",
  "language": "en",
  "time_zone": "UTC",
  "libraries": [],
  "navigation": null
}
"##;
        let record: crate::sites::TemplateBody = serde_json::from_str(body).unwrap();
        assert!(record.look.is_none());
        assert_eq!(file_bytes(&record).unwrap(), body.as_bytes());

        // The new fields, once set, are written and read back.
        let mut brand = OrgBrand::default();
        brand.fonts = vec![BrandFont {
            id: uuid(1),
            font: font("Inter.woff2"),
        }];
        brand.font_packages = vec![package("Pair")];
        brand.default_icon = Some(uuid(2));
        brand.policy.site_images = false;
        let text = String::from_utf8(file_bytes(&brand).unwrap()).unwrap();
        for key in [
            "\"fonts\"",
            "\"font_packages\"",
            "\"default_icon\"",
            "\"policy\"",
            "\"licence\"",
            "\"confirmed_by\"",
            "\"site_images\": false",
            // A policy with one limit set keeps every field.
            "\"hide_builtin_themes\": false",
        ] {
            assert!(text.contains(key), "{key}");
        }
        let back: OrgBrand = serde_json::from_str(&text).unwrap();
        assert_eq!(back, brand);
        assert_eq!(file_bytes(&back).unwrap(), text.as_bytes());

        let mut look = Look::default();
        look.fonts = Some(LookFonts {
            package: package("Pair"),
            from: "builtin:classic".into(),
        });
        look.icon = Some(image(500));
        look.menu = MenuStyle::Mega;
        let text = String::from_utf8(file_bytes(&look).unwrap()).unwrap();
        assert!(text.contains("\"menu\": \"mega\""));
        let back: Look = serde_json::from_str(&text).unwrap();
        assert_eq!(back, look);
        assert_eq!(file_bytes(&back).unwrap(), text.as_bytes());
        let wide = text.replace("\"menu\": \"mega\"", "\"menu\": \"wide\"");
        assert!(serde_json::from_str::<Look>(&wide).is_err());
        assert!(serde_json::from_str::<BrandPolicy>(r#"{"extra": true}"#).is_err());
        assert!(serde_json::from_str::<FontRef>(r#"{"extra": true}"#).is_err());
    }

    #[test]
    fn template_look_holds_references_only() {
        let text = r##"{
  "theme": {"name": "Dusk", "primary": "#3156c9", "secondary": "#233239"},
  "from": "org:11111111-1111-4111-8111-111111111111",
  "logo": {"brand_image": "22222222-2222-4222-8222-222222222222"},
  "logo_alt": "",
  "header": {"layout": "compact", "background": "none", "align": "left", "hide_title": false},
  "navigation": "horizontal",
  "footer": {"enabled": false, "layout": "simple", "name": "", "background": "none", "groups": []},
  "fonts": {
    "package": {
      "name": "Pair",
      "display": {"brand_font": "33333333-3333-4333-8333-333333333333"},
      "roles": {"title": "display", "heading": "display", "body": "system", "interactive": "system"}
    },
    "from": "org:44444444-4444-4444-8444-444444444444"
  }
}"##;
        let look: TemplateLook = serde_json::from_str(text).unwrap();
        assert_eq!(look.problem(), None);
        assert_eq!(
            look.logo,
            Some(TemplateImage::BrandImage(
                Uuid::parse_str("22222222-2222-4222-8222-222222222222").unwrap()
            ))
        );
        let back = serde_json::to_value(&look).unwrap();
        assert_eq!(back, serde_json::from_str::<Value>(text).unwrap());
        // A file or a pinned snapshot is not a reference.
        let with_file = text.replace(
            r#"{"brand_image": "22222222-2222-4222-8222-222222222222"}"#,
            r#"{"sha256": "ab"}"#,
        );
        assert!(serde_json::from_str::<TemplateLook>(&with_file).is_err());
        // A role cannot name a font the package lacks.
        let mut bad = look.clone();
        bad.fonts.as_mut().unwrap().package.roles.body = FontRole::Content;
        assert!(bad.problem().unwrap().contains("content"));
        // A bad theme source is refused as in a look.
        let mut bad = look;
        bad.from = "javascript:1".into();
        assert!(bad.problem().is_some());
    }

    #[test]
    fn bleed_defaults_stay_out_of_the_file_and_limits_hold() {
        // A header without a bleed reads the defaults, and writes none back.
        let header: Header = serde_json::from_value(
            json!({"layout": "extended", "background": "none", "align": "left", "hide_title": false}),
        )
        .unwrap();
        assert_eq!(header.bleed, Bleed::default());
        assert!(
            serde_json::to_value(&header)
                .unwrap()
                .get("bleed")
                .is_none()
        );
        // A bleed given in part takes the defaults for the rest; one changed is written.
        let mut header: Header = serde_json::from_value(json!({
            "layout": "extended", "background": "none", "align": "left", "hide_title": false,
            "bleed": {"enabled": false, "tint_dark": "#ABCDEF"}
        }))
        .unwrap();
        assert!(!header.bleed.enabled);
        assert_eq!(header.bleed.distance, Bleed::default().distance);
        assert_eq!(
            serde_json::to_value(&header).unwrap()["bleed"]["tint_dark"],
            "#abcdef"
        );
        assert!(serde_json::from_value::<Bleed>(json!({"glow": 1})).is_err());
        // Limits.
        let mut look = Look::default();
        assert_eq!(look.problem("Site"), None);
        header.bleed.distance = MAX_BLEED_DISTANCE;
        header.bleed.blur = MAX_BLEED_BLUR;
        header.bleed.transparency = 100;
        look.header = header.clone();
        assert_eq!(look.problem("Site"), None);
        for change in [
            |b: &mut Bleed| b.distance = MAX_BLEED_DISTANCE + 1,
            |b: &mut Bleed| b.blur = MAX_BLEED_BLUR + 1,
            |b: &mut Bleed| b.transparency = 101,
        ] {
            let mut bad = look.clone();
            change(&mut bad.header.bleed);
            assert!(bad.problem("Site").is_some());
        }
    }
}
