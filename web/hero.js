// A library's banner picture: while someone is in a library that has one, it slides in over
// the site header's banner picture, and when they leave for another page of the same site it
// slides back out to show the site's own again. Only a header that already shows a banner
// picture takes it. What the last page showed is kept for the tab in sessionStorage, so moving
// between the pages of one library shows it at once with no motion. The decision is pure and
// tested; drawing it is a few lines over the header the page already has.

/** The sessionStorage key of what the last page of a site showed. */
export const HERO_KEY = 'sharecraft.hero';

const BRAND_IMAGE = /^\/api\/brand\/images\/[0-9a-f]{64}$/;

/** `url` if it is the address of a brand image, else null. */
export const heroHref = (url) => (typeof url === 'string' && BRAND_IMAGE.test(url) ? url : null);

/** A kept state as read back: `{site, library, url}` with a brand image url, else null. */
export function readHero(text) {
  try {
    const value = JSON.parse(text ?? 'null');
    if (!value || typeof value !== 'object') return null;
    const url = heroHref(value.url);
    if (typeof value.site !== 'string' || typeof value.library !== 'string' || !url) return null;
    return { site: value.site, library: value.library, url };
  } catch {
    return null;
  }
}

/**
 * What the banner does on this page, from what the last page showed (`previous`, as
 * [`readHero`] answers) and this page (`here`: `{site, library?, url?}`, where `library` is the
 * library the page is in, if any, and `url` its banner picture or null). Answers
 * `{url, motion, keep}`: the picture to draw (null for none), `'in'` (slides in), `'out'`
 * (drawn, then slides out) or `'none'`, and the state to keep for the next page (null to clear).
 */
export function heroStep(previous, here) {
  const url = heroHref(here?.url);
  if (here?.library && url) {
    const keep = { site: here.site, library: here.library, url };
    const same = previous && previous.site === here.site && previous.library === here.library && previous.url === url;
    return { url, motion: same ? 'none' : 'in', keep };
  }
  if (previous && previous.site === here?.site) return { url: previous.url, motion: 'out', keep: null };
  return { url: null, motion: 'none', keep: null };
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
 * of its own takes none. Answers the picture's element, or null.
 */
export function showHero(header, here) {
  const step = heroStep(loadHero(), here);
  storeHero(step.keep);
  const band = header?.querySelector?.('.band-image');
  if (!step.url || !band) return null;
  const img = document.createElement('img');
  img.className = 'band-image band-hero';
  img.alt = '';
  img.decoding = 'async';
  img.hidden = true;
  img.src = step.url;
  band.after(img);
  const reduced = globalThis.matchMedia?.('(prefers-reduced-motion: reduce)')?.matches;
  const start = () => {
    img.hidden = false;
    if (step.motion === 'none') return;
    if (step.motion === 'out' && reduced) return img.remove();
    img.dataset.motion = step.motion;
    if (step.motion === 'out') img.addEventListener('animationend', () => img.remove(), { once: true });
  };
  img.addEventListener('load', start, { once: true });
  img.addEventListener('error', () => img.remove(), { once: true });
  return img;
}
