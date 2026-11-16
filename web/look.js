// Brand colours: the contrast check and the dark-colour derivation, all pure. The Rust code in
// crates/revisions/src/brand.rs is authoritative; this copy gives live feedback while a theme is
// edited. Both are tested against the one table in look-vectors.json.
//
// A colour is `{r, g, b}` with whole numbers 0-255; `parseColour` makes one from `#rrggbb`.

/** The page reference colours, from style.css. */
export const LIGHT_PAPER = '#ffffff';
export const LIGHT_BACKGROUND = '#f5f7f8';
export const DARK_PAPER = '#202a2e';
export const DARK_BACKGROUND = '#141c20';
/** The darker of the two header and button text colours; the other is white. */
export const INK = '#141c20';
export const WHITE = '#ffffff';
/** The least WCAG contrast ratio text needs. */
export const MIN_RATIO = 4.5;

const HEX = /^#[0-9a-fA-F]{6}$/;

/** The colour `#rrggbb` (either case) as `{r, g, b}`, or `null` for anything else. */
export function parseColour(text) {
  if (typeof text !== 'string' || !HEX.test(text)) return null;
  return {
    r: parseInt(text.slice(1, 3), 16),
    g: parseInt(text.slice(3, 5), 16),
    b: parseInt(text.slice(5, 7), 16),
  };
}

/** The colour as lower-case `#rrggbb`. */
export function hexOf({ r, g, b }) {
  return '#' + [r, g, b].map((v) => v.toString(16).padStart(2, '0')).join('');
}

function channel(v) {
  const c = v / 255;
  return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
}

/** WCAG 2.x relative luminance. */
export function luminance({ r, g, b }) {
  return 0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b);
}

/** WCAG contrast ratio of two colours (given as `{r,g,b}` or `#rrggbb`), 1 to 21. */
export function contrast(a, b) {
  const la = luminance(typeof a === 'string' ? parseColour(a) : a);
  const lb = luminance(typeof b === 'string' ? parseColour(b) : b);
  return (Math.max(la, lb) + 0.05) / (Math.min(la, lb) + 0.05);
}

/** The colour as `[hue, lightness, saturation]`, each 0 to 1. */
function toHls({ r, g, b }) {
  const [rr, gg, bb] = [r / 255, g / 255, b / 255];
  const max = Math.max(rr, gg, bb);
  const min = Math.min(rr, gg, bb);
  const l = (min + max) / 2;
  if (min === max) return [0, l, 0];
  const d = max - min;
  const s = l <= 0.5 ? d / (max + min) : d / (2 - max - min);
  const rc = (max - rr) / d;
  const gc = (max - gg) / d;
  const bc = (max - bb) / d;
  let h;
  if (rr === max) h = bc - gc;
  else if (gg === max) h = 2 + rc - bc;
  else h = 4 + gc - rc;
  return [(((h / 6) % 1) + 1) % 1, l, s];
}

function hueToChannel(m1, m2, hue) {
  const h = ((hue % 1) + 1) % 1;
  if (h < 1 / 6) return m1 + (m2 - m1) * h * 6;
  if (h < 0.5) return m2;
  if (h < 2 / 3) return m1 + (m2 - m1) * (2 / 3 - h) * 6;
  return m1;
}

/** `[h, l, s]` back to `{r, g, b}`, each channel rounded to the nearest whole number. */
function fromHls(h, l, s) {
  let rgb;
  if (s === 0) rgb = [l, l, l];
  else {
    const m2 = l <= 0.5 ? l * (1 + s) : l + s - l * s;
    const m1 = 2 * l - m2;
    rgb = [
      hueToChannel(m1, m2, h + 1 / 3),
      hueToChannel(m1, m2, h),
      hueToChannel(m1, m2, h - 1 / 3),
    ];
  }
  const [r, g, b] = rgb.map((v) => Math.max(0, Math.min(255, Math.round(v * 255))));
  return { r, g, b };
}

/** The colour with its lightness moved by `steps` percent, hue and saturation kept. */
function shifted(hls, steps) {
  const l = Math.round((hls[1] + steps / 100) * 1e9) / 1e9;
  return fromHls(hls[0], l, hls[2]);
}

/** The better of white and the dark ink on `c`: `{ink, ratio}`. */
export function bestInk(c) {
  const white = contrast(WHITE, c);
  const dark = contrast(INK, c);
  return white >= dark ? { ink: WHITE, ratio: white } : { ink: INK, ratio: dark };
}

/** Lowest ratio of `c` on the two dark page colours. */
const onDark = (c) => Math.min(contrast(c, DARK_PAPER), contrast(c, DARK_BACKGROUND));
/** Lowest ratio of `c` on the two light page colours. */
const onLight = (c) => Math.min(contrast(c, LIGHT_PAPER), contrast(c, LIGHT_BACKGROUND));

const passes = {
  primary_on_light: (c) => onLight(c) >= MIN_RATIO,
  button_on_primary: (c) => contrast(WHITE, c) >= MIN_RATIO,
  primary_dark_on_dark: (c) => onDark(c) >= MIN_RATIO,
  button_on_primary_dark: (c) => bestInk(c).ratio >= MIN_RATIO,
  header_on_secondary: (c) => bestInk(c).ratio >= MIN_RATIO,
  header_on_secondary_dark: (c) => bestInk(c).ratio >= MIN_RATIO,
};

const primaryLightOk = (c) => passes.primary_on_light(c) && passes.button_on_primary(c);
const primaryDarkOk = (c) => passes.primary_dark_on_dark(c) && passes.button_on_primary_dark(c);
const secondaryOk = (c) => passes.header_on_secondary(c);

/**
 * The dark counterpart of a light colour, keeping hue and saturation. For `role` `'primary'`
 * the lightness rises 1% at a time (from its own) until it reads on both dark page colours and
 * carries button text; for `'secondary'` the colour is kept if header text reads on it, else
 * the lightness falls 1% at a time. `null` when nothing passes.
 */
export function deriveDark(colour, role) {
  const c = typeof colour === 'string' ? parseColour(colour) : colour;
  if (!c) return null;
  const hls = toHls(c);
  if (role === 'secondary') {
    if (secondaryOk(c)) return c;
    for (let k = 1; hls[1] - k / 100 >= -1e-9; k++) {
      const d = shifted(hls, -k);
      if (secondaryOk(d)) return d;
    }
    return null;
  }
  for (let k = 0; hls[1] + k / 100 <= 1 + 1e-9; k++) {
    const d = shifted(hls, k);
    if (primaryDarkOk(d)) return d;
  }
  return null;
}

/** The passing colour with the lightness nearest to `c`'s (darker first on a tie), or `null`. */
export function suggest(c, ok) {
  const hls = toHls(c);
  for (let k = 1; k <= 100; k++) {
    for (const step of [-k, k]) {
      const l = hls[1] + step / 100;
      if (l < -1e-9 || l > 1 + 1e-9) continue;
      const d = shifted(hls, step);
      if (ok(d)) return d;
    }
  }
  return null;
}

const round2 = (x) => Math.round(x * 100) / 100;

/**
 * Checks a theme `{primary, secondary, primary_dark?, secondary_dark?}` (colours as `#rrggbb`).
 * Returns `{ok: true, resolved}` with the light and dark `{accent, accent_ink, on_accent, header_bg,
 * header_ink}` as `#rrggbb`, or `{ok: false, pairs}` with every failing `{check, ratio,
 * suggestion}` (ratio to 2 decimals, suggestion `#rrggbb` or `null`). Check names:
 * primary_on_light, button_on_primary, primary_dark_on_dark, button_on_primary_dark,
 * header_on_secondary, header_on_secondary_dark.
 */
export function checkTheme(theme) {
  const parse = (v) => (v === undefined || v === null ? undefined : parseColour(v));
  const p = parse(theme.primary);
  const s = parse(theme.secondary);
  const pd = parse(theme.primary_dark);
  const sd = parse(theme.secondary_dark);
  if (!p || !s || pd === null || sd === null) {
    return { ok: false, pairs: [], invalid: true };
  }
  const pairs = [];
  const fail = (check, ratio, source, ok) => {
    const s2 = source ? suggest(source, ok) : null;
    pairs.push({ check, ratio: round2(ratio), suggestion: s2 ? hexOf(s2) : null });
  };

  if (!passes.primary_on_light(p)) fail('primary_on_light', onLight(p), p, primaryLightOk);
  const buttonLight = contrast(WHITE, p);
  if (buttonLight < MIN_RATIO) fail('button_on_primary', buttonLight, p, primaryLightOk);

  const primaryDark = pd ?? deriveDark(p, 'primary');
  if (primaryDark) {
    const explicit = Boolean(pd);
    if (!passes.primary_dark_on_dark(primaryDark)) {
      fail('primary_dark_on_dark', onDark(primaryDark), explicit ? primaryDark : null, primaryDarkOk);
    }
    const best = bestInk(primaryDark);
    if (best.ratio < MIN_RATIO) {
      fail('button_on_primary_dark', best.ratio, explicit ? primaryDark : null, primaryDarkOk);
    }
  } else {
    fail('primary_dark_on_dark', onDark(p), null, primaryDarkOk);
  }

  const lightHeader = bestInk(s);
  if (lightHeader.ratio < MIN_RATIO) fail('header_on_secondary', lightHeader.ratio, s, secondaryOk);

  const secondaryDark = sd ?? deriveDark(s, 'secondary');
  if (secondaryDark) {
    const best = bestInk(secondaryDark);
    if (best.ratio < MIN_RATIO) {
      fail('header_on_secondary_dark', best.ratio, sd ? secondaryDark : null, secondaryOk);
    }
  } else {
    fail('header_on_secondary_dark', lightHeader.ratio, null, secondaryOk);
  }

  if (pairs.length) return { ok: false, pairs };
  return {
    ok: true,
    resolved: {
      light: {
        accent: hexOf(p),
        accent_ink: hexOf(p),
        on_accent: WHITE,
        header_bg: hexOf(s),
        header_ink: lightHeader.ink,
      },
      dark: {
        accent: hexOf(primaryDark),
        accent_ink: hexOf(primaryDark),
        on_accent: bestInk(primaryDark).ink,
        header_bg: hexOf(secondaryDark),
        header_ink: bestInk(secondaryDark).ink,
      },
    },
  };
}

/**
 * The CSS variables an effective look sets, as `{name: 'light-dark(a,b)'}`. `effective` is the
 * answer of `GET /api/sites/{id}/look`: its `theme` holds `light` and `dark`, each with
 * `accent` (button fill), `accent_ink` (link and focus colour), `on_accent` (button text),
 * `header_bg` and `header_ink`. Pages set them with
 * `style.setProperty`, never through markup.
 */
export function lookTokens(effective) {
  const { light, dark } = effective.theme;
  const pair = (key) => `light-dark(${light[key]},${dark[key]})`;
  return {
    '--accent': pair('accent'),
    '--accent-ink': pair('accent_ink'),
    '--on-accent': pair('on_accent'),
    '--header-bg': pair('header_bg'),
    '--header-ink': pair('header_ink'),
  };
}

/** The only variables a look may set on a page; every other colour is never themed. */
export const LOOK_VARIABLES = Object.freeze(['--accent', '--accent-ink', '--on-accent', '--header-bg', '--header-ink']);

const TOKEN = /^light-dark\(#[0-9a-fA-F]{6},#[0-9a-fA-F]{6}\)$/;

/**
 * Puts an effective look's colours on the page: the variables of [`lookTokens`] are set on
 * `root` (the `<html>` element) through `style.setProperty`, which the page's Content Security
 * Policy allows, never through markup or a `style` attribute. Each value is checked again to be
 * `light-dark(#rrggbb,#rrggbb)` and only the five known variables are set. If anything fails
 * the check (or there is no theme), nothing is set and an earlier look's variables are taken
 * away, so the page shows its own colours. Answers whether the look was applied.
 */
export function applyLook(root, effective) {
  const style = root?.style;
  if (typeof style?.setProperty !== 'function') return false;
  let tokens = null;
  try {
    tokens = lookTokens(effective);
  } catch {
    tokens = null;
  }
  const good =
    tokens !== null &&
    LOOK_VARIABLES.every((name) => typeof tokens[name] === 'string' && TOKEN.test(tokens[name]));
  if (!good) {
    for (const name of LOOK_VARIABLES) style.removeProperty?.(name);
    return false;
  }
  for (const name of LOOK_VARIABLES) style.setProperty(name, tokens[name]);
  return true;
}

// ---- Fonts ----
//
// A page never takes a font family from user text. A font file is loaded under a generated
// family name, `sc-` and the first 12 hexadecimal characters of its hash, and the four
// `--font-*` variables hold that name followed by one of two fixed fallback stacks.

/** The system sans-serif stack: today's look, and the fallback of every role. */
export const SYSTEM = "ui-sans-serif, system-ui, 'Segoe UI', sans-serif";

/** The system serif stack, used by the built-in Classic package. */
export const SYSTEM_SERIF = "ui-serif, Georgia, 'Times New Roman', serif";

/** The only font variables a look may set on a page, one for each text role. */
export const FONT_VARIABLES = Object.freeze(['--font-title', '--font-heading', '--font-body', '--font-interactive']);

const GENERATED_FAMILY = /^sc-[0-9a-f]{12}$/;

/**
 * The font variables of an effective look's `fonts` (`{title, heading, body, interactive}`,
 * each `{family, url?}` as `GET /api/sites/{id}/look` answers): always all four, as
 * `{'--font-title': value, ...}`. A family `system-serif` gives the fixed serif stack; a
 * generated name (`sc-` and 12 lowercase hexadecimal characters) gives that name before the
 * fixed system stack; anything else, or nothing, gives the system stack. No other text can
 * reach a value.
 */
export function fontTokens(effectiveFonts) {
  const tokens = {};
  for (const name of FONT_VARIABLES) {
    const family = effectiveFonts?.[name.slice('--font-'.length)]?.family;
    let value = SYSTEM;
    if (family === 'system-serif') value = SYSTEM_SERIF;
    else if (typeof family === 'string' && GENERATED_FAMILY.test(family)) value = `${family}, ${SYSTEM}`;
    tokens[name] = value;
  }
  return tokens;
}

/** A brand font's address: the only place a page loads a font from (the same origin). */
const FONT_URL = /^\/api\/brand\/fonts\/([0-9a-f]{64})$/;

/** The faces asked for so far, for each `document.fonts`: family to a promise of whether it loaded. */
const asked = new WeakMap();

/**
 * Loads one brand font into the page under its generated family name, and answers a promise of
 * whether it loaded (it never rejects). `family` must be `sc-` and 12 lowercase hexadecimal
 * characters, and `url` `/api/brand/fonts/` with the 64-character hash whose first 12 characters
 * are those of `family`; anything else loads nothing. The face is made with the `FontFace` API
 * (`display: swap`: text shows in the fallback stack until the font arrives) and added to
 * `document.fonts`, so no markup, `<style>` or Content Security Policy change is needed. A face
 * that fails to load is taken out of `document.fonts` again, so its roles fall back to the
 * system stack. A family asked for again answers the first promise. `env` replaces `FontFace`
 * and `document.fonts` (tests).
 */
export function loadFontFace(family, url, env = {}) {
  const Face = 'FontFace' in env ? env.FontFace : globalThis.FontFace;
  const fonts = 'fonts' in env ? env.fonts : globalThis.document?.fonts;
  const sha = typeof url === 'string' ? FONT_URL.exec(url)?.[1] : null;
  const good = typeof family === 'string' && GENERATED_FAMILY.test(family) && sha && family === `sc-${sha.slice(0, 12)}`;
  if (!good || typeof Face !== 'function' || typeof fonts?.add !== 'function') return Promise.resolve(false);
  let known = asked.get(fonts);
  if (!known) asked.set(fonts, (known = new Map()));
  if (known.has(family)) return known.get(family);
  let face;
  try {
    face = new Face(family, `url(${url})`, { display: 'swap' });
    fonts.add(face);
  } catch {
    return Promise.resolve(false);
  }
  const done = Promise.resolve()
    .then(() => face.load())
    .then(
      () => true,
      () => {
        try {
          fonts.delete?.(face);
        } catch {
          // nothing to take away
        }
        return false;
      },
    );
  known.set(family, done);
  return done;
}

const FONT_VALUES = new Set([SYSTEM, SYSTEM_SERIF]);
const FONT_VALUE = /^sc-[0-9a-f]{12}, /;

/** Whether `value` may be a `--font-*` variable: a fixed stack, or a generated name before the system stack. */
function fontValueOk(value) {
  return typeof value === 'string' && (FONT_VALUES.has(value) || (FONT_VALUE.test(value) && value.slice(17) === SYSTEM));
}

/** What each root last had set by [`applyFonts`], so that a late failure changes only its own roles. */
const appliedFonts = new WeakMap();

/**
 * Puts an effective look's fonts (`{title, heading, body, interactive}`, each `{family, url?}`)
 * on `root` (`<html>`, or the editor frame's bar and navigation) through `style.setProperty`:
 * the four variables of [`fontTokens`], each checked again against the fixed forms. A role
 * whose family is generated takes it only when its address is that font's brand-font address,
 * and the font is loaded with [`loadFontFace`]; when it fails the role goes back to the system
 * stack, and nobody is told (readers see the system font). Without fonts (the look could not be
 * read) the variables are taken away, so the page shows its own. Answers a promise that settles
 * when every font has loaded or failed.
 */
export function applyFonts(root, effectiveFonts, env) {
  const style = root?.style;
  if (typeof style?.setProperty !== 'function') return Promise.resolve();
  if (effectiveFonts == null || typeof effectiveFonts !== 'object') {
    for (const name of FONT_VARIABLES) style.removeProperty?.(name);
    appliedFonts.delete(root);
    return Promise.resolve();
  }
  const tokens = fontTokens(effectiveFonts);
  const loads = [];
  for (const name of FONT_VARIABLES) {
    const font = effectiveFonts[name.slice('--font-'.length)];
    if (!FONT_VALUE.test(tokens[name])) continue;
    const sha = typeof font?.url === 'string' ? FONT_URL.exec(font.url)?.[1] : null;
    if (!sha || font.family !== `sc-${sha.slice(0, 12)}`) {
      tokens[name] = SYSTEM;
      continue;
    }
    const value = tokens[name];
    loads.push(
      loadFontFace(font.family, font.url, env).then((ok) => {
        if (ok || appliedFonts.get(root)?.[name] !== value) return;
        style.setProperty(name, SYSTEM);
        appliedFonts.get(root)[name] = SYSTEM;
      }),
    );
  }
  for (const name of FONT_VARIABLES) if (!fontValueOk(tokens[name])) tokens[name] = SYSTEM;
  for (const name of FONT_VARIABLES) style.setProperty(name, tokens[name]);
  appliedFonts.set(root, { ...tokens });
  return Promise.all(loads).then(() => undefined);
}

// ---- Change the look: the draft, what Apply sends, the preview and the session ----
//
// A draft is a look as the server stores it (`GET /api/sites/{id}/look/settings` answers
// `look`), with each image slot holding one of: an image the look stores (`{sha256, name,
// from_doc, ...}`), a brand-centre image chosen from the picker (`{brand_image, sha256, name}`)
// or a document chosen from the picker (`{doc, name}`); `null` for none. Nothing in a draft is
// saved until Apply.

/** The look of a site with none recorded: the built-in ShareCraft theme and the defaults. */
export const DEFAULT_LOOK = Object.freeze({
  theme: { name: 'ShareCraft', primary: '#3156c9', secondary: '#233239' },
  from: 'builtin:sharecraft',
  logo: null,
  thumbnail: null,
  logo_alt: '',
  header: { layout: 'compact', background: 'none', image: null, align: 'left', hide_title: false },
  navigation: 'horizontal',
  footer: { enabled: false, layout: 'simple', logo: null, name: '', background: 'none', groups: [] },
});

/** The theme colour each check is about, which "Use suggested" sets. */
export const CHECK_FIELD = Object.freeze({
  primary_on_light: 'primary',
  button_on_primary: 'primary',
  primary_dark_on_dark: 'primary_dark',
  button_on_primary_dark: 'primary_dark',
  header_on_secondary: 'secondary',
  header_on_secondary_dark: 'secondary_dark',
});

/** Each check in words, for the live contrast check. */
export const CHECK_LABELS = Object.freeze({
  primary_on_light: 'Links and accents on light pages',
  button_on_primary: 'White button text on the primary colour',
  primary_dark_on_dark: 'Links and accents on dark pages',
  button_on_primary_dark: 'Button text on the dark primary colour',
  header_on_secondary: 'Header text on the secondary colour',
  header_on_secondary_dark: 'Header text on the dark secondary colour',
});

/** The colours of a theme, in the order the forms show them. */
export const THEME_FIELDS = Object.freeze(['primary', 'secondary', 'primary_dark', 'secondary_dark']);
const SHA256 = /^[0-9a-f]{64}$/;
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

const clone = (value) => (value === undefined ? undefined : JSON.parse(JSON.stringify(value)));

/** A copy of the image of a slot as a draft holds it, or null. */
function draftImage(image) {
  return image && typeof image === 'object' ? clone(image) : null;
}

/** Only the name and the colours a theme has; dark colours left out are derived. */
export function themeOf(theme) {
  const out = { name: String(theme?.name ?? '') };
  for (const field of THEME_FIELDS) {
    const value = theme?.[field];
    if (typeof value === 'string' && value !== '') out[field] = parseColour(value) ? value.toLowerCase() : value;
  }
  return out;
}

/** A draft of `look` (a look as stored, or null for the default look), with every field set. */
export function draftFromLook(look) {
  const base = look && typeof look === 'object' ? look : DEFAULT_LOOK;
  const header = { ...DEFAULT_LOOK.header, ...base.header };
  const footer = { ...DEFAULT_LOOK.footer, ...base.footer };
  return {
    theme: themeOf(base.theme ?? DEFAULT_LOOK.theme),
    from: typeof base.from === 'string' ? base.from : 'custom',
    logo: draftImage(base.logo),
    thumbnail: draftImage(base.thumbnail),
    logo_alt: String(base.logo_alt ?? ''),
    header: {
      layout: header.layout,
      background: header.background,
      image: draftImage(header.image),
      align: header.align,
      hide_title: Boolean(header.hide_title),
    },
    navigation: base.navigation === 'vertical' ? 'vertical' : 'horizontal',
    footer: {
      enabled: Boolean(footer.enabled),
      layout: footer.layout === 'extended' ? 'extended' : 'simple',
      logo: draftImage(footer.logo),
      name: String(footer.name ?? ''),
      background: footer.background,
      groups: clone(Array.isArray(footer.groups) ? footer.groups : []),
    },
  };
}

/** Whether two themes have the same name and colours (a dark colour left out is derived). */
export function sameTheme(a, b) {
  const x = themeOf(a);
  const y = themeOf(b);
  return x.name === y.name && THEME_FIELDS.every((field) => (x[field] ?? null) === (y[field] ?? null));
}

/** The content hash of a brand image address `/api/brand/images/{sha256}`, or null. */
export function shaOfUrl(url) {
  const match = typeof url === 'string' ? /^\/api\/brand\/images\/([0-9a-f]{64})$/.exec(url) : null;
  return match ? match[1] : null;
}

/**
 * Where the preview shows a draft image from: a stored or brand-centre image at its brand
 * image address, a document chosen from the picker at its preview; null for anything else.
 */
export function draftImageUrl(image) {
  if (!image || typeof image !== 'object') return null;
  if (typeof image.sha256 === 'string' && SHA256.test(image.sha256)) return `/api/brand/images/${image.sha256}`;
  if (typeof image.doc === 'string' && UUID.test(image.doc)) return `/api/docs/${image.doc}/preview`;
  return null;
}

/**
 * The colours the preview shows for `theme`, even one that fails a check (so the person sees
 * what fails): the checked colours when it passes, else each colour as given, a missing dark
 * colour derived where it can be, and the better text colour on each.
 */
export function previewPalettes(theme) {
  const checked = checkTheme(themeOf(theme));
  if (checked.ok) return checked.resolved;
  const p = parseColour(theme?.primary) ?? parseColour(DEFAULT_LOOK.theme.primary);
  const s = parseColour(theme?.secondary) ?? parseColour(DEFAULT_LOOK.theme.secondary);
  const pd = parseColour(theme?.primary_dark) ?? deriveDark(p, 'primary') ?? p;
  const sd = parseColour(theme?.secondary_dark) ?? deriveDark(s, 'secondary') ?? s;
  const palette = (accent, header) => ({
    accent: hexOf(accent),
    accent_ink: hexOf(accent),
    on_accent: bestInk(accent).ink,
    header_bg: hexOf(header),
    header_ink: bestInk(header).ink,
  });
  return { light: { ...palette(p, s), on_accent: WHITE }, dark: palette(pd, sd) };
}

/**
 * The draft as `GET /api/sites/{id}/look` would answer it, for the preview: the theme's colours
 * (see [`previewPalettes`]), image addresses (see [`draftImageUrl`]) and the site's name as the
 * logo description when it is empty.
 */
export function previewEffective(draft, siteName) {
  const put = (target, key, image) => {
    const url = draftImageUrl(image);
    if (url) target[key] = url;
    return target;
  };
  const header = put(
    {
      layout: draft.header.layout,
      background: draft.header.background,
      align: draft.header.align,
      hide_title: draft.header.hide_title,
    },
    'image_url',
    draft.header.image,
  );
  const footer = put(
    {
      enabled: draft.footer.enabled,
      layout: draft.footer.layout,
      name: draft.footer.name,
      background: draft.footer.background,
      groups: draft.footer.groups,
    },
    'logo_url',
    draft.footer.logo,
  );
  const out = {
    theme: previewPalettes(draft.theme),
    logo_alt: draft.logo_alt.trim() || siteName,
    header,
    navigation: draft.navigation,
    footer,
    theme_from: 'site',
  };
  put(out, 'logo_url', draft.logo);
  put(out, 'thumbnail_url', draft.thumbnail);
  return out;
}

/**
 * How a draft image is sent: `{doc}` or `{brand_image}` as chosen in the picker; an image the
 * draft took from a stored look (the current one or a history entry) as `{keep}`, so its exact
 * snapshot is kept and never taken again from its document; null for none. The server keeps
 * an image the look holds now or one its history names, and answers `409 image_gone` when that
 * content is no longer stored or available.
 */
function slotInput(image) {
  if (!image) return null;
  if (typeof image.doc === 'string') return { doc: image.doc };
  if (typeof image.brand_image === 'string') return { brand_image: image.brand_image };
  return typeof image.sha256 === 'string' ? { keep: image.sha256 } : null;
}

/**
 * The body Apply sends as `look` (`PUT /api/sites/{id}/look`) for `draft`. `themes` is every
 * theme the page offers (`{from, name, primary, ...}`). A theme still the same as its source is
 * sent by its source; otherwise (custom, or changed or gone at its source) as the colours the
 * draft holds. Images are sent as [`slotInput`] says.
 */
export function lookInput(draft, { themes = [] } = {}) {
  const source = draft.from !== 'custom' ? themes.find((t) => t.from === draft.from) : null;
  const theme = source && sameTheme(source, draft.theme) ? { from: draft.from } : themeOf(draft.theme);
  // The draft keeps every choice while the person tries layouts; what is sent is only what the
  // chosen layout uses: an image and an alignment only for an extended header, a hidden title
  // only with a logo, and the footer's groups as its layout allows them.
  const extended = draft.header.layout === 'extended';
  const band = extended && draft.header.background === 'image' && draft.header.image ? draft.header.image : null;
  const out = {
    theme,
    logo_alt: draft.logo_alt,
    header: {
      layout: draft.header.layout,
      background: draft.header.background === 'image' && !band ? 'none' : draft.header.background,
      align: extended ? draft.header.align : 'left',
      hide_title: Boolean(draft.header.hide_title && draft.logo),
    },
    navigation: draft.navigation,
    footer: {
      enabled: draft.footer.enabled,
      layout: draft.footer.layout,
      name: draft.footer.name,
      background: draft.footer.background,
      groups: footerGroups(draft.footer),
    },
  };
  const slot = (target, key, image) => {
    const value = slotInput(image);
    if (value) target[key] = value;
  };
  slot(out, 'logo', draft.logo);
  slot(out, 'thumbnail', draft.thumbnail);
  slot(out.header, 'image', band);
  slot(out.footer, 'logo', draft.footer.logo);
  return out;
}

/** The most groups of links a footer of each layout has. */
export const FOOTER_GROUP_LIMIT = Object.freeze({ simple: 1, extended: 4 });
/** The most links in a footer group. */
export const FOOTER_LINK_LIMIT = 8;

/**
 * The footer's groups as sent: links left wholly empty are dropped, and then groups with
 * neither a heading nor a link; a simple footer has its first group only and no heading; an
 * extended one at most 4 groups.
 */
export function footerGroups(footer) {
  const simple = footer.layout !== 'extended';
  const groups = (Array.isArray(footer.groups) ? footer.groups : []).slice(0, simple ? 1 : FOOTER_GROUP_LIMIT.extended);
  return groups
    .map((group) => ({
      heading: simple ? '' : String(group?.heading ?? ''),
      links: (Array.isArray(group?.links) ? group.links : [])
        .filter((link) => String(link?.label ?? '').trim() !== '' || String(link?.url ?? '').trim() !== '')
        .map((link) => ({ label: String(link.label ?? ''), url: String(link.url ?? '') })),
    }))
    .filter((group) => group.heading.trim() !== '' || group.links.length > 0);
}

/** The draft as it would be sent, to tell whether it differs from the saved look. */
function comparable(draft) {
  return { ...draft, footer: { ...draft.footer, groups: footerGroups(draft.footer) } };
}

/**
 * `draft` without the images of `slots` (`logo`, `thumbnail`, `header.image`, `footer.logo`),
 * as a rollback refused with `image_gone` names them; an image band without its image has no
 * background then, and a hidden title shows again without a logo.
 */
export function withoutSlots(draft, slots) {
  const out = clone(draft);
  for (const slot of slots ?? []) {
    if (slot === 'logo') out.logo = null;
    if (slot === 'thumbnail') out.thumbnail = null;
    if (slot === 'header.image') out.header.image = null;
    if (slot === 'footer.logo') out.footer.logo = null;
  }
  if (out.header.background === 'image' && !out.header.image) out.header.background = 'none';
  if (!out.logo) out.header.hide_title = false;
  return out;
}

/** A text form of `value` with the keys of every object sorted, to compare drafts. */
function canonical(value) {
  if (Array.isArray(value)) return `[${value.map(canonical).join(',')}]`;
  if (value && typeof value === 'object') {
    return `{${Object.keys(value)
      .sort()
      .map((key) => `${JSON.stringify(key)}:${canonical(value[key])}`)
      .join(',')}}`;
  }
  return JSON.stringify(value ?? null);
}

/** One image slot's change in words: added, removed or changed; null when it is the same. */
function imageChange(name, from, to) {
  const key = (image) => (image ? (image.sha256 ?? image.doc ?? image.brand_image ?? canonical(image)) : null);
  if (key(from) === key(to)) return null;
  if (!from) return `${name} added`;
  if (!to) return `${name} removed`;
  return `${name} changed`;
}

/** A field that went from one choice to another in words, or null when it did not. */
function choiceChange(name, from, to) {
  return from === to ? null : `${name} ${from} to ${to}`;
}

/**
 * What changed between two looks as stored (`null` is the default look), for a history entry:
 * for example "Theme: ShareCraft to Plum; logo added; header: compact to extended; footer turned
 * on". "No visible change" when nothing a person sees differs.
 */
export function describeLookChange(before, after) {
  const a = draftFromLook(before);
  const b = draftFromLook(after);
  const parts = [];
  if (a.theme.name !== b.theme.name) parts.push(`Theme: ${a.theme.name} to ${b.theme.name}`);
  else if (!sameTheme(a.theme, b.theme)) parts.push('Theme colours changed');
  for (const text of [imageChange('Logo', a.logo, b.logo), imageChange('Thumbnail', a.thumbnail, b.thumbnail)]) {
    if (text) parts.push(text);
  }
  if (a.logo_alt !== b.logo_alt) parts.push('Logo description changed');
  const layout = a.header.layout === b.header.layout ? null : `${a.header.layout} to ${b.header.layout}`;
  const header = [
    choiceChange('background', a.header.background, b.header.background),
    imageChange('image', a.header.image, b.header.image),
    choiceChange('alignment', a.header.align, b.header.align),
    a.header.hide_title !== b.header.hide_title ? (b.header.hide_title ? 'title hidden' : 'title shown') : null,
  ].filter(Boolean);
  if (layout) header.unshift(header.length ? `layout ${layout}` : layout);
  if (header.length) parts.push(`Header: ${header.join(', ')}`);
  if (a.navigation !== b.navigation) parts.push(`Navigation: ${a.navigation} to ${b.navigation}`);
  if (a.footer.enabled !== b.footer.enabled) parts.push(`Footer turned ${b.footer.enabled ? 'on' : 'off'}`);
  else if (b.footer.enabled) {
    const footer = [
      choiceChange('layout', a.footer.layout, b.footer.layout),
      choiceChange('background', a.footer.background, b.footer.background),
      imageChange('logo', a.footer.logo, b.footer.logo),
      a.footer.name !== b.footer.name ? 'name changed' : null,
      canonical(footerGroups(a.footer)) !== canonical(footerGroups(b.footer)) ? 'links changed' : null,
    ].filter(Boolean);
    if (footer.length) parts.push(`Footer: ${footer.join(', ')}`);
  }
  if (parts.length === 0) return 'No visible change';
  const text = parts.join('; ');
  return text[0].toUpperCase() + text.slice(1);
}

/**
 * The Change the look page's state for site `siteId`, without any DOM: the saved look (from
 * `settings`, the answer of `GET /api/sites/{id}/look/settings`) and a draft. Changes go to the
 * draft only; `apply` sends it (`PUT`), `cancel` drops it, `preview` loads a history entry as
 * the draft and `restore` rolls back to it, loading it as a draft without the images whose
 * content is gone when the server says so. `send(method, path, body)` is `api` from common.js
 * (paths without `/api`). `readOnly` (an archived site) never applies or restores.
 */
export function lookSession({ siteId, settings, themes = [], brandImages = [], send, readOnly = false }) {
  let saved = settings;
  let draft = draftFromLook(saved?.look);
  const path = `/sites/${encodeURIComponent(siteId)}`;
  const take = (answer) => {
    saved = answer;
    draft = draftFromLook(saved?.look);
  };
  const session = {
    get draft() {
      return draft;
    },
    get settings() {
      return saved;
    },
    /** Changes the draft with `edit(draft)`. */
    change(edit) {
      edit(draft);
    },
    /** Copies `theme` (`{from, name, primary, ...}`) into the draft. */
    chooseTheme(theme) {
      draft.theme = themeOf(theme);
      draft.from = theme.from;
    },
    /** Sets a theme colour as typed (empty leaves a dark colour to be derived); the theme becomes custom. */
    setColour(field, value) {
      if (!THEME_FIELDS.includes(field)) return;
      const text = String(value ?? '').trim();
      if (text === '' && field.endsWith('_dark')) delete draft.theme[field];
      else draft.theme[field] = parseColour(text) ? text.toLowerCase() : text;
      draft.from = 'custom';
    },
    /** Sets the theme's name; the theme becomes custom. */
    setName(name) {
      draft.theme.name = String(name ?? '');
      draft.from = 'custom';
    },
    /** Sets the colour a failing `pair` is about to its suggestion, if it has one. */
    useSuggested(pair) {
      const field = CHECK_FIELD[pair?.check];
      if (!field || typeof pair.suggestion !== 'string' || !parseColour(pair.suggestion)) return;
      session.setColour(field, pair.suggestion);
    },
    /** Drops the draft: back to the saved look. */
    cancel() {
      draft = draftFromLook(saved?.look);
    },
    /** `{dirty, check, canApply}`: Apply needs a change, every pair passing, a name and a live site. */
    status() {
      const check = checkTheme(draft.theme);
      const dirty = canonical(comparable(draft)) !== canonical(comparable(draftFromLook(saved?.look)));
      const named = draft.theme.name.trim() !== '';
      return { dirty, check, canApply: dirty && check.ok && named && !readOnly };
    },
    /** Loads the look of history `entry` (with `which` `'before'`, the look before it) as the draft. */
    preview(entry, which = 'after') {
      draft = draftFromLook((which === 'before' ? entry?.before : entry?.look) ?? null);
    },
    /** Sends the draft. Answers the `api` result; on success the answer is the saved look. */
    async apply() {
      if (!session.status().canApply) return { ok: false, error: null };
      const body = { look: lookInput(draft, { themes }) };
      const result = await send('PUT', `${path}/look`, body);
      if (result.ok) take(result.data);
      return result;
    },
    /**
     * Rolls back to history `entry`. On success the answer is the saved look; when its images'
     * content is gone (`409 image_gone`), the entry becomes the draft without them, and the
     * answer carries `gone`, the slots.
     */
    async restore(entry, which = 'after') {
      if (readOnly) return { ok: false, error: null };
      const before = which === 'before';
      const body = before ? { event_id: entry.event_id, which: 'before' } : { event_id: entry.event_id };
      const result = await send('POST', `${path}/look/rollback`, body);
      if (result.ok) {
        take(result.data);
        return result;
      }
      if (result.error === 'image_gone') {
        const gone = Array.isArray(result.data?.slots) ? result.data.slots : [];
        draft = withoutSlots(draftFromLook((before ? entry.before : entry.look) ?? null), gone);
        return { ...result, gone };
      }
      return result;
    },
    /** The themes and brand images the session knows, for the page to replace after a change. */
    setChoices(next) {
      if (Array.isArray(next.themes)) themes = next.themes;
      if (Array.isArray(next.brandImages)) brandImages = next.brandImages;
    },
  };
  return session;
}

/**
 * Whether the draft of `session` may be replaced (by a history entry's Preview or Restore):
 * at once when it has no changes that are not applied, else only when `ask()` (a confirmation
 * the page shows) resolves true.
 */
export async function mayReplaceDraft(session, ask) {
  if (!session.status().dirty) return true;
  return Boolean(await ask());
}

/**
 * What the brand centre page shows `me` for `brand` (`GET /api/brand`): `edit` for brand
 * managers and administrators (themes and images), `managers` for administrators only.
 */
export function brandCentreSections(brand, me) {
  return {
    edit: Boolean(brand?.can_manage),
    managers: Boolean(me?.is_admin) && Array.isArray(brand?.managers),
  };
}
