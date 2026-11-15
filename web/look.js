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
