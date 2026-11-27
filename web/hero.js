// A library's banner picture: while someone is in a library that has one, it slides in over
// the site header's banner picture, and when they leave for another page of the same site it
// slides back out to show the site's own again. Only a header that already shows a banner
// picture takes it. What the last page showed is kept for the tab in sessionStorage, so moving
// between the pages of one library shows it at once with no motion. The decision is pure and
// tested; drawing it is a few lines over the header the page already has.
//
// A library's banner picture may be framed: `{x, y, zoom}`, the point of the picture kept in
// view (thousandths across and down) and how far it is enlarged beyond covering the header
// (percent). The header covers itself with the picture placed at that point
// (`object-position`) and enlarges it around the same point (`scale`), so the header never
// shows past the picture's edges. `x` is equally where the visible part sits within the room
// the picture has to move across: 0 its left edge, 1000 its right.

/** The sessionStorage key of what the last page of a site showed. */
export const HERO_KEY = 'sharecraft.hero';

const BRAND_IMAGE = /^\/api\/brand\/images\/[0-9a-f]{64}$/;

/** `url` if it is the address of a brand image, else null. */
export const heroHref = (url) => (typeof url === 'string' && BRAND_IMAGE.test(url) ? url : null);

/** The framing of the middle at its smallest. */
export const HERO_FRAME_DEFAULT = Object.freeze({ x: 500, y: 500, zoom: 100 });
/** The lowest and highest `zoom`, in percent. */
export const HERO_ZOOM_MIN = 100;
export const HERO_ZOOM_MAX = 400;

const clamp = (value, low, high) => Math.min(high, Math.max(low, value));

/** `frame` as whole numbers in range; the default for anything that is not a frame. */
export function heroFrame(frame) {
  const number = (value) => (typeof value === 'number' && Number.isFinite(value) ? value : null);
  const x = number(frame?.x);
  const y = number(frame?.y);
  const zoom = number(frame?.zoom);
  if (x === null || y === null || zoom === null) return { ...HERO_FRAME_DEFAULT };
  return {
    x: Math.round(clamp(x, 0, 1000)),
    y: Math.round(clamp(y, 0, 1000)),
    zoom: Math.round(clamp(zoom, HERO_ZOOM_MIN, HERO_ZOOM_MAX)),
  };
}

/** Whether two frames are the same once each is in range. */
export function sameFrame(a, b) {
  const one = heroFrame(a);
  const two = heroFrame(b);
  return one.x === two.x && one.y === two.y && one.zoom === two.zoom;
}

/** The style that draws a picture framed by `frame` in a box it covers (`object-fit: cover`). */
export function heroFrameStyle(frame) {
  const { x, y, zoom } = heroFrame(frame);
  const point = `${x / 10}% ${y / 10}%`;
  return { objectPosition: point, transformOrigin: point, scale: String(zoom / 100) };
}

/** Draws `img` framed by `frame` (null for the middle at its smallest). */
export function applyHeroFrame(img, frame) {
  if (!img) return;
  const style = heroFrameStyle(frame);
  img.style.objectPosition = style.objectPosition;
  img.style.transformOrigin = style.transformOrigin;
  img.style.scale = style.scale;
}

/**
 * The part of a `picture` (`{width, height}`) a box of `box` (`{width, height}`) shows framed by
 * `frame`, as fractions of the picture: `{left, top, width, height}`.
 */
export function heroWindow(frame, box, picture) {
  const { x, y, zoom } = heroFrame(frame);
  const cover = Math.max(box.width / picture.width, box.height / picture.height);
  const width = Math.min(1, box.width / (picture.width * cover * (zoom / 100)));
  const height = Math.min(1, box.height / (picture.height * cover * (zoom / 100)));
  return { left: (x / 1000) * (1 - width), top: (y / 1000) * (1 - height), width, height };
}

/**
 * `frame` with the part shown moved by `dx` and `dy` (fractions of the whole picture, as
 * [`heroWindow`] measures with the same `box` and `picture`); kept within the picture.
 */
export function moveHeroFrame(frame, dx, dy, box, picture) {
  const now = heroFrame(frame);
  const shown = heroWindow(now, box, picture);
  const along = (point, delta, size) => (size >= 1 ? point : clamp(point + (delta / (1 - size)) * 1000, 0, 1000));
  return heroFrame({ ...now, x: along(now.x, dx, shown.width), y: along(now.y, dy, shown.height) });
}

/** A kept state as read back: `{site, library, url, frame}` with a brand image url, else null. */
export function readHero(text) {
  try {
    const value = JSON.parse(text ?? 'null');
    if (!value || typeof value !== 'object') return null;
    const url = heroHref(value.url);
    if (typeof value.site !== 'string' || typeof value.library !== 'string' || !url) return null;
    return { site: value.site, library: value.library, url, frame: heroFrame(value.frame) };
  } catch {
    return null;
  }
}

/**
 * What the banner does on this page, from what the last page showed (`previous`, as
 * [`readHero`] answers) and this page (`here`: `{site, library?, url?, frame?}`, where
 * `library` is the library the page is in, if any, `url` its banner picture or null and
 * `frame` its framing). Answers `{url, frame, motion, keep}`: the picture to draw (null for
 * none) and its framing, `'in'` (slides in), `'out'` (drawn, then slides out) or `'none'`, and
 * the state to keep for the next page (null to clear). A picture moved but not changed shows
 * at once in its new place.
 */
export function heroStep(previous, here) {
  const url = heroHref(here?.url);
  if (here?.library && url) {
    const frame = heroFrame(here.frame);
    const keep = { site: here.site, library: here.library, url, frame };
    const same = previous && previous.site === here.site && previous.library === here.library && previous.url === url;
    return { url, frame, motion: same ? 'none' : 'in', keep };
  }
  if (previous && previous.site === here?.site) {
    return { url: previous.url, frame: heroFrame(previous.frame), motion: 'out', keep: null };
  }
  return { url: null, frame: null, motion: 'none', keep: null };
}

/** Reads the kept state, or null where sessionStorage cannot be used. */
function loadHero() {
  try {
    return readHero(globalThis.sessionStorage?.getItem(HERO_KEY));
  } catch {
    return null;
  }
}

/** Keeps `state` for the next page (or forgets it with null). */
function storeHero(state) {
  try {
    if (state) globalThis.sessionStorage?.setItem(HERO_KEY, JSON.stringify(state));
    else globalThis.sessionStorage?.removeItem(HERO_KEY);
  } catch {
    // Without storage every visit slides it in and no page slides it out.
  }
}

/**
 * Shows the banner picture of this page on `header` (the site header drawn by `headerView`), as
 * [`heroStep`] decides, and keeps the state for the next page. A header with no banner picture
 * of its own takes none. `onShow({src, position, motion, reduced})` runs as the picture starts
 * to show (so the banner's bleed, bleed.js, can move with it). Answers the picture's element, or
 * null.
 */
export function showHero(header, here, { onShow } = {}) {
  const step = heroStep(loadHero(), here);
  storeHero(step.keep);
  const band = header?.querySelector?.('.band-image');
  if (!step.url || !band) return null;
  const img = document.createElement('img');
  img.className = 'band-image band-hero';
  img.alt = '';
  img.decoding = 'async';
  img.hidden = true;
  // The server's banner-sized picture, as the site's own banner is drawn (see bannerSrc).
  img.src = `${step.url}?size=header`;
  applyHeroFrame(img, step.frame);
  band.after(img);
  const reduced = globalThis.matchMedia?.('(prefers-reduced-motion: reduce)')?.matches;
  const start = () => {
    img.hidden = false;
    onShow?.({ src: img.src, position: img.style.objectPosition, motion: step.motion, reduced: Boolean(reduced) });
    if (step.motion === 'none') return;
    if (step.motion === 'out' && reduced) return img.remove();
    img.dataset.motion = step.motion;
    if (step.motion === 'out') img.addEventListener('animationend', () => img.remove(), { once: true });
  };
  img.addEventListener('load', start, { once: true });
  img.addEventListener('error', () => img.remove(), { once: true });
  return img;
}
