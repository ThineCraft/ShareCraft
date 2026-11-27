// A site's banner picture bleeding out of the banner: behind the page around and below an image
// header the picture's colours glow through, blurred, washed with a tint and fading out with
// distance from the banner. When a library's banner picture slides in over the site's (see
// hero.js), its bleed slides in with it, with the same motion.
//
// Text stays readable. The glow is the picture's colour (hue and saturation) at the brightness
// of the page's own background (`mix-blend-mode: color` over it), and text contrast depends on
// brightness only, so it can show strongly. As a check (the blend's brightness is not exactly
// WCAG's), the page's text, muted text and link colours are read from the page, the picture is
// sampled small (as the blur would average it), and the glow is shown no stronger than keeps
// each of them at 4.5:1 on any sampled part of it, in the colour scheme the page is in. The
// settings (`look.header.bleed`) are upper bounds.
//
// The maths is pure and tested (bleed.test.mjs); drawing it is a layer inside the header, under
// the page (z-index -1), sized to the header plus the room around it.

/** The settings of a look with none recorded (as `revisions::brand::Bleed::default`). */
export const BLEED_DEFAULT = Object.freeze({
  enabled: true,
  distance: 240,
  blur: 48,
  transparency: 30,
  tint_light: '#ffffff',
  tint_dark: '#000000',
});
/** The farthest a banner picture bleeds, in pixels. */
export const BLEED_DISTANCE_MAX = 400;
/** The most it is blurred, in pixels. */
export const BLEED_BLUR_MAX = 96;
/** How much the tint washes the picture (0 none, 1 only the tint). */
export const BLEED_TINT = 0.3;
/** The least contrast text keeps on the bleed (WCAG AA for normal text). */
export const BLEED_MIN_CONTRAST = 4.5;

const HEX = /^#[0-9a-f]{6}$/i;

/** `value` as settings: every field in range, the default for anything missing or wrong. */
export function bleedOf(value) {
  const v = value && typeof value === 'object' ? value : {};
  const whole = (x, max, fallback) =>
    typeof x === 'number' && Number.isFinite(x) ? Math.round(Math.min(max, Math.max(0, x))) : fallback;
  const colour = (x, fallback) => (typeof x === 'string' && HEX.test(x) ? x.toLowerCase() : fallback);
  return {
    enabled: typeof v.enabled === 'boolean' ? v.enabled : BLEED_DEFAULT.enabled,
    distance: whole(v.distance, BLEED_DISTANCE_MAX, BLEED_DEFAULT.distance),
    blur: whole(v.blur, BLEED_BLUR_MAX, BLEED_DEFAULT.blur),
    transparency: whole(v.transparency, 100, BLEED_DEFAULT.transparency),
    tint_light: colour(v.tint_light, BLEED_DEFAULT.tint_light),
    tint_dark: colour(v.tint_dark, BLEED_DEFAULT.tint_dark),
  };
}

/** Whether settings show anything: on, reaching past the banner and not wholly transparent. */
export function bleedShows(value) {
  const b = bleedOf(value);
  return b.enabled && b.distance > 0 && b.transparency < 100;
}

/**
 * How far the bleed reaches from each side of the header (`{top, right, bottom, left}` in
 * pixels): `distance`, or less where `bounds` (the page) ends sooner. `header` and `bounds` are
 * rectangles in the same coordinates (`{top, right, bottom, left}`); without `bounds`, the
 * distance on every side.
 */
export function bleedReach(distance, header, bounds) {
  const d = Math.max(0, Math.round(distance));
  if (!bounds || !header) return { top: d, right: d, bottom: d, left: d };
  const room = (n) => Math.max(0, Math.min(d, Math.floor(n)));
  return {
    top: room(header.top - bounds.top),
    right: room(bounds.right - header.right),
    bottom: room(bounds.bottom - header.bottom),
    left: room(header.left - bounds.left),
  };
}

/** The fade with distance from the banner: [fraction of the distance, how much shows]. Eased: (1 - f)². */
const FADE = Object.freeze([
  [0, 1],
  [0.25, 0.5625],
  [0.5, 0.25],
  [0.75, 0.0625],
  [1, 0],
]);

const px = (n) => `${Math.round(n * 100) / 100}px`;

/**
 * The mask of a bleed reaching `reach` past the header on each side and fading over `distance`:
 * two gradients, across and down, to be intersected (`mask-composite: intersect`). Full where
 * the header is, nothing at `distance` from it; a side cut short by the page fades no sooner.
 */
export function bleedMask(distance, reach) {
  const d = Math.max(1, distance);
  const stop = (alpha, at) => `rgb(0 0 0/${alpha}) ${at}`;
  const axis = (direction, near, far) => {
    const start = [...FADE].reverse().map(([f, alpha]) => stop(alpha, px(near - f * d)));
    const end = FADE.map(([f, alpha]) => stop(alpha, `calc(100% + ${px(f * d - far)})`));
    return `linear-gradient(${direction},${[...start, ...end].join(',')})`;
  };
  return [axis('to right', reach.left, reach.right), axis('to bottom', reach.top, reach.bottom)];
}

// ---- contrast ----

const channel = (v) => {
  const c = v / 255;
  return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
};
/** WCAG relative luminance of `[r, g, b]` (0 to 255). */
export const luminance = ([r, g, b]) => 0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b);
const ratio = (a, b) => (Math.max(a, b) + 0.05) / (Math.min(a, b) + 0.05);
const mix = (from, to, t) => from.map((v, i) => v + (to[i] - v) * t);

// The `color` blend mode of CSS compositing: the source's hue and saturation at the backdrop's
// brightness (`lum`, 0 to 255).
const lum = ([r, g, b]) => 0.3 * r + 0.59 * g + 0.11 * b;
function clipColour(c) {
  const l = lum(c);
  const low = Math.min(...c);
  const high = Math.max(...c);
  let out = c;
  if (low < 0) out = out.map((v) => l + ((v - l) * l) / (l - low));
  if (high > 255) out = out.map((v) => l + ((v - l) * (255 - l)) / (high - l));
  return out;
}
/** `source` (`[r, g, b]`) at the brightness of `backdrop`: the `color` blend mode. */
export function colourBlend(backdrop, source) {
  const d = lum(backdrop) - lum(source);
  return clipColour(source.map((v) => v + d));
}

/**
 * The most the bleed may show (0 to 1, in hundredths) so that each of `inks` (`[r, g, b]`) keeps
 * [`BLEED_MIN_CONTRAST`] (or what it has on the bare `background`, when that is less) on the
 * `background` with any of `samples` laid over it: washed with `tint` at `tintMix`, drawn at the
 * background's brightness ([`colourBlend`]).
 */
export function readableAlpha(samples, tint, background, inks, { tintMix = BLEED_TINT, min = BLEED_MIN_CONTRAST } = {}) {
  if (!samples?.length || !inks?.length) return 1;
  const washed = samples.map((s) => colourBlend(background, mix(s, tint, tintMix)));
  const needs = inks.map((ink) => {
    const l = luminance(ink);
    return { l, need: Math.min(min, ratio(l, luminance(background))) - 1e-9 };
  });
  for (let step = 1; step <= 100; step += 1) {
    const a = step / 100;
    for (const w of washed) {
      const under = luminance(mix(background, w, a));
      if (needs.some(({ l, need }) => ratio(l, under) < need)) return (step - 1) / 100;
    }
  }
  return 1;
}

/**
 * How much of the picture's own light and shade may show over the colour glow (0 to 1, in
 * hundredths) when the bleed shows at `strength`: the most that keeps each of `inks` at
 * [`BLEED_MIN_CONTRAST`] (or what it has on the bare `background`) on any of `samples`, washed as
 * in [`readableAlpha`]. The glow alone (0) is assumed to read, as `readableAlpha` checks.
 */
export function readableShade(samples, tint, background, inks, strength, { tintMix = BLEED_TINT, min = BLEED_MIN_CONTRAST, base = background } = {}) {
  if (!samples?.length || !inks?.length || strength <= 0) return 1;
  const washed = samples.map((s) => mix(s, tint, tintMix));
  const glows = washed.map((w) => colourBlend(base, w));
  const needs = inks.map((ink) => {
    const l = luminance(ink);
    return { l, need: Math.min(min, ratio(l, luminance(background))) - 1e-9 };
  });
  for (let step = 1; step <= 100; step += 1) {
    const c = step / 100;
    for (let i = 0; i < washed.length; i += 1) {
      const under = luminance(mix(background, mix(glows[i], washed[i], c), strength));
      if (needs.some(({ l, need }) => ratio(l, under) < need)) return (step - 1) / 100;
    }
  }
  return 1;
}

/**
 * The colour the glow is drawn at the brightness of: the page `background` moved toward black on
 * a light page (`dark` false) or white on a dark one, as far (in hundredths) as keeps each of
 * `inks` at [`BLEED_MIN_CONTRAST`] (or what it has on the bare background) on any of `samples`
 * glowing at that brightness at `strength`. Further from the page colour the glow can hold more
 * colour. Answers `[r, g, b]`.
 */
export function glowBase(samples, tint, background, inks, strength, { dark = false, tintMix = BLEED_TINT, min = BLEED_MIN_CONTRAST } = {}) {
  if (!samples?.length || !inks?.length || strength <= 0) return background;
  const washed = samples.map((s) => mix(s, tint, tintMix));
  const toward = dark ? [255, 255, 255] : [0, 0, 0];
  const needs = inks.map((ink) => {
    const l = luminance(ink);
    return { l, need: Math.min(min, ratio(l, luminance(background))) - 1e-9 };
  });
  let best = background;
  for (let step = 1; step <= 100; step += 1) {
    const base = mix(background, toward, step / 100);
    for (const w of washed) {
      const under = luminance(mix(background, colourBlend(base, w), strength));
      if (needs.some(({ l, need }) => ratio(l, under) < need)) return best;
    }
    best = base;
  }
  return best;
}

/** How strongly the bleed shows: the settings' opacity, but no more than `cap`. */
export function bleedAlpha(value, cap = 1) {
  const b = bleedOf(value);
  return Math.max(0, Math.min((100 - b.transparency) / 100, cap));
}

/** `[r, g, b]` of a computed CSS colour (`rgb(...)`, `rgba(...)` or `color(srgb ...)`), or null. */
export function parseCssColour(text) {
  const rgb = /^rgba?\(\s*([\d.]+)[\s,]+([\d.]+)[\s,]+([\d.]+)/.exec(text ?? '');
  if (rgb) return [Number(rgb[1]), Number(rgb[2]), Number(rgb[3])];
  const srgb = /^color\(srgb\s+([\d.]+)\s+([\d.]+)\s+([\d.]+)/.exec(text ?? '');
  if (srgb) return [Number(srgb[1]) * 255, Number(srgb[2]) * 255, Number(srgb[3]) * 255];
  return null;
}

/** When a picture cannot be read: the darkest and lightest it could be. */
const ANY_PICTURE = Object.freeze([
  [0, 0, 0],
  [255, 255, 255],
]);

/** A loaded picture as a few hundred `[r, g, b]` samples, about as the blur averages it. */
function pictureSamples(img) {
  try {
    const canvas = document.createElement('canvas');
    canvas.width = 32;
    canvas.height = 16;
    const context = canvas.getContext('2d', { willReadFrequently: true });
    context.drawImage(img, 0, 0, canvas.width, canvas.height);
    const { data } = context.getImageData(0, 0, canvas.width, canvas.height);
    const samples = [];
    for (let i = 0; i < data.length; i += 4) if (data[i + 3] >= 16) samples.push([data[i], data[i + 1], data[i + 2]]);
    return samples.length ? samples : ANY_PICTURE;
  } catch {
    return ANY_PICTURE;
  }
}

// ---- drawing ----

/** The colours text meets on the page at `place`: `{dark, background, inks}`. */
function pageColours(place) {
  const probe = document.createElement('span');
  probe.hidden = true;
  place.append(probe);
  const read = (value) => {
    probe.style.color = '';
    probe.style.color = value;
    return parseCssColour(getComputedStyle(probe).color);
  };
  const scheme = read('light-dark(#000000,#ffffff)');
  const colours = {
    dark: Boolean(scheme && scheme[0] > 127),
    background: read('var(--bg)') ?? [255, 255, 255],
    inks: ['var(--text)', 'var(--muted)', 'var(--accent-ink)'].map(read).filter(Boolean),
  };
  probe.remove();
  return colours;
}

/** One picture of the bleed, `src` drawn at `position` (`object-position`). */
function bleedPicture(src, position) {
  const img = document.createElement('img');
  img.className = 'bleed-image';
  img.alt = '';
  img.decoding = 'async';
  img.src = src;
  if (position) img.style.objectPosition = position;
  return img;
}

/**
 * Draws the bleed of `header` (the site header from `headerView`, with its banner picture) for
 * `settings` (`look.header.bleed`). `bounds` is the element the bleed stays within (the page's
 * `<html>`; none in Change the look's preview, which clips it itself). Answers `{hero(step),
 * stop()}` (see [`showHero`] in hero.js for `step`), or null when there is nothing to draw.
 */
export function showBleed(header, settings, { bounds = null } = {}) {
  const band = header?.querySelector?.('.band-image');
  if (!band || !bleedShows(settings)) return null;
  const b = bleedOf(settings);
  const layer = document.createElement('div');
  layer.className = 'band-bleed';
  layer.setAttribute('aria-hidden', 'true');
  layer.style.setProperty('--bleed-blur', `${b.blur}px`);
  layer.style.setProperty('--bleed-tint', `light-dark(${b.tint_light},${b.tint_dark})`);
  layer.style.setProperty('--bleed-tint-mix', String(BLEED_TINT));
  // The page's own colour; over it the pictures and tint as colour only (the glow, which keeps
  // the page's brightness); over that the same as they are (the shade), only as much as text
  // contrast leaves room for (style.css).
  const base = document.createElement('span');
  base.className = 'bleed-base';
  const group = (name) => {
    const element = document.createElement('span');
    element.className = name;
    const tint = document.createElement('span');
    tint.className = 'bleed-tint';
    element.append(tint);
    return { element, tint };
  };
  const glow = group('bleed-glow');
  const shade = group('bleed-shade');
  layer.append(base, glow.element, shade.element);
  header.setAttribute('data-bleed', '');
  header.prepend(layer);

  // What each shown picture looks like, sampled once it has loaded.
  const samples = new Map();
  const update = () => {
    if (!header.isConnected) return stop();
    const place = header.parentElement ?? document.body;
    const colours = pageColours(place);
    const tintColour = hexRgb(colours.dark ? b.tint_dark : b.tint_light);
    let cap = 1;
    for (const seen of samples.values()) cap = Math.min(cap, readableAlpha(seen, tintColour, colours.background, colours.inks));
    const strength = samples.size ? bleedAlpha(b, cap) : 0;
    const all = [...samples.values()].flat();
    const tone = glowBase(all, tintColour, colours.background, colours.inks, strength, { dark: colours.dark });
    base.style.background = `rgb(${tone.map(Math.round).join(' ')})`;
    const room = readableShade(all, tintColour, colours.background, colours.inks, strength, { base: tone });
    layer.style.opacity = String(strength);
    shade.element.style.opacity = String(room);
  };
  // A picture is drawn in both groups; the glow's copy is sampled.
  const picture = (src, position, extra) => {
    const pair = [bleedPicture(src, position), bleedPicture(src, position)];
    if (extra) for (const img of pair) img.classList.add(extra);
    glow.element.insertBefore(pair[0], glow.tint);
    shade.element.insertBefore(pair[1], shade.tint);
    const [img] = pair;
    const take = () => {
      if (!img.isConnected) return;
      samples.set(img, pictureSamples(img));
      update();
    };
    if (img.complete && img.naturalWidth) take();
    else img.addEventListener('load', take, { once: true });
    img.addEventListener('error', () => forget(pair), { once: true });
    return pair;
  };
  const forget = (pair) => {
    samples.delete(pair[0]);
    for (const img of pair) img.remove();
    update();
  };

  picture(band.currentSrc || band.getAttribute('src'));

  const place = () => {
    if (!header.isConnected) return stop();
    const box = bounds?.getBoundingClientRect?.();
    const limit = box
      ? {
          top: box.top,
          left: box.left,
          right: Math.min(box.right, document.documentElement.clientWidth),
          bottom: Math.max(box.bottom, document.documentElement.clientHeight),
        }
      : null;
    const reach = bleedReach(b.distance, header.getBoundingClientRect(), limit);
    layer.style.top = `${-reach.top}px`;
    layer.style.right = `${-reach.right}px`;
    layer.style.bottom = `${-reach.bottom}px`;
    layer.style.left = `${-reach.left}px`;
    const mask = bleedMask(b.distance, reach).join(',');
    layer.style.setProperty('mask-image', mask);
    layer.style.setProperty('-webkit-mask-image', mask);
  };
  place();

  const resize = globalThis.ResizeObserver ? new ResizeObserver(() => place()) : null;
  resize?.observe(header);
  if (bounds) resize?.observe(bounds);
  globalThis.addEventListener?.('resize', place);
  const media = globalThis.matchMedia?.('(prefers-color-scheme: dark)');
  media?.addEventListener?.('change', update);
  const scheme = globalThis.MutationObserver ? new MutationObserver(update) : null;
  scheme?.observe(document.documentElement, { attributes: true, attributeFilter: ['style', 'class', 'data-theme'] });

  let stopped = false;
  function stop() {
    if (stopped) return;
    stopped = true;
    resize?.disconnect();
    scheme?.disconnect();
    globalThis.removeEventListener?.('resize', place);
    media?.removeEventListener?.('change', update);
  }

  return {
    layer,
    /** A library's banner picture over the site's (`{src, position, motion, reduced}`), moving as the banner does. */
    hero({ src, position, motion, reduced }) {
      if (stopped || !src || (motion === 'out' && reduced)) return null;
      const pair = picture(src, position, 'bleed-hero');
      if (motion === 'in' || motion === 'out') for (const img of pair) img.dataset.motion = motion;
      if (motion === 'out') pair[0].addEventListener('animationend', () => forget(pair), { once: true });
      return pair[0];
    },
    stop() {
      stop();
      layer.remove();
      header.removeAttribute('data-bleed');
    },
  };
}

/** `[r, g, b]` of `#rrggbb`. */
function hexRgb(hex) {
  return [1, 3, 5].map((i) => Number.parseInt(hex.slice(i, i + 2), 16));
}
