// Skeleton screens (round 25): every page that fills itself after its fetches shows, the moment
// it arrives, grey placeholders shaped like what is coming (the site's header band and navigation,
// the title, cards, rows, avatars), in the very elements and classes the real content uses so
// that nothing moves when it comes. Counts and heights are remembered per page (in this browser)
// from the last visit, so a site with two libraries shows two placeholder cards next time.
//
// While a region waits it has aria-busy="true"; screen readers hear one polite "Loading…" (and
// only when loading takes longer than a moment). A region is done when the page draws into it
// (the placeholders are replaced), or when the page says so (`settle`); `settleAll` puts back
// what the page never drew (after a failure, say). The placeholders shimmer gently, never under
// prefers-reduced-motion (style.css).
//
// This module is loaded by a script tag of its own before the page's module, so it runs as soon
// as the page's HTML is in, without waiting for the page's other modules.

// ---- the shapes (pure) ----

/** An element of a shape: `tag.class.class` and its children. */
export const el = (selector, ...children) => {
  const [tag, ...classes] = selector.split('.');
  return { tag: tag || 'span', classes, children };
};
/** A grey bar of text `width` wide (a CSS length), standing on the text line it replaces. */
export const bar = (width) => ({ bar: width });
/** A grey block: 'icon' (24 px), 'check' (20 px) or 'control' (a menu button). */
export const box = (size) => ({ box: size });

/** The placeholder of one item of each kind. */
export const SHAPES = Object.freeze({
  title: el('span', bar('14ch')),
  line: el('span', bar('32%')),
  crumbs: el('ol', el('li', bar('7ch')), el('li', bar('10ch'))),
  'site-card': el(
    'li.site-card',
    el('span.card-band'),
    el('div.card-body', el('h3', bar('62%')), el('p.card-kind', bar('45%')), el('p.card-description', bar('88%')), el('p.card-role', bar('28%'))),
  ),
  'recent-file': el('li.recent-file', box('icon'), el('span.recent-text', el('span', bar('72%')), el('small', bar('46%')))),
  'library-card': el('li.library-card', el('span.lib-pic'), el('div.lib-body', el('h3', bar('58%')), el('p.lib-facts', bar('42%')))),
  'file-row': el(
    'tr',
    el('td.c-select', box('check')),
    el('td.c-name', el('div.name-cell', el('span.file-pic', box('icon')), el('div.name-text', el('span', bar('18ch')), el('span.sub', bar('12ch'))))),
    el('td.c-modified', bar('70%')),
    el('td.c-type', bar('60%')),
    el('td.c-sharing'),
    el('td.c-actions', box('control')),
  ),
  'share-entry': el(
    'li.share-entry',
    el('span.avatar'),
    el('span.grow', el('span.entry-name', bar('38%')), el('small', bar('52%')), el('small.entry-perms', bar('70%'))),
    el('span.menu-wrap', box('control')),
  ),
  person: el(
    'li.person',
    el('span.avatar'),
    el('span.grow.person-text', el('span.person-name', bar('34%')), el('small.person-email', bar('50%'))),
    el('span.person-marks'),
    el('span.menu-wrap', box('control')),
  ),
  'bin-group': el(
    'section.bin-group',
    el('h2.bin-heading', bar('9ch')),
    el(
      'ul.list.bin-list',
      ...[52, 40, 60].map((width) =>
        el('li.bin-row', box('check'), box('icon'), el('span.bin-main', el('strong.bin-name', bar(`${width}%`)), el('small.bin-who', bar('30%'))), el('span.actions', box('control'))),
      ),
    ),
  ),
  about: el('div.sk-about', el('p.description', bar('80%')), el('ul.chips', el('li.chip', bar('9ch')), el('li.chip', bar('7ch')))),
});

/** Kinds whose region holds a list of items, counted and remembered. */
const LISTS = new Set(['site-card', 'recent-file', 'library-card', 'file-row', 'share-entry', 'person', 'bin-group']);
/** Kinds that remember the height of their region (and reserve it next time). */
const HEIGHTS = new Set(['about', 'reserve']);
/** Kinds that remember whether the page showed the region, and how tall (shown, inert, next time). */
const SHOWN = new Set(['shown']);
/** Kinds the page settles itself (with `settle`): nothing in them is replaced. */
const EXPLICIT = new Set(['about', 'reserve', 'shown']);

/**
 * What each page shows while it loads: the site's header band and navigation above the page
 * (`header`, `chrome`), and its regions: `{ selector, kind, count, max }`, `count` items when
 * nothing is remembered, never more than `max`.
 */
export const PAGE_SKELETONS = Object.freeze({
  '/index.html': {
    header: false,
    chrome: true,
    regions: [
      { selector: '#greeting', kind: 'title' },
      { selector: '#recent', kind: 'recent-file', count: 3, max: 8 },
      { selector: '#sites', kind: 'site-card', count: 6, max: 12 },
    ],
  },
  '/site.html': {
    header: true,
    chrome: true,
    regions: [
      { selector: '#crumbs', kind: 'crumbs' },
      { selector: '#title', kind: 'title' },
      { selector: '#about', kind: 'about' },
      { selector: '#get-started', kind: 'shown' },
      { selector: '#libraries', kind: 'library-card', count: 3, max: 12 },
      { selector: '#recent', kind: 'recent-file', count: 4, max: 8 },
    ],
  },
  '/library.html': {
    header: true,
    chrome: true,
    regions: [
      { selector: '#crumbs', kind: 'crumbs' },
      { selector: '#title', kind: 'title' },
      { selector: '#kind', kind: 'line' },
      { selector: '#file-rows', kind: 'file-row', count: 8, max: 14 },
    ],
  },
  '/access.html': {
    header: true,
    chrome: true,
    regions: [
      { selector: '#title', kind: 'title' },
      { selector: '#crumbs', kind: 'crumbs' },
      { selector: '.share-head', kind: 'reserve' },
      { selector: '#assignments', kind: 'share-entry', count: 4, max: 10 },
    ],
  },
  '/people.html': {
    header: false,
    chrome: true,
    regions: [
      { selector: '#summary', kind: 'line' },
      { selector: '#users', kind: 'person', count: 8, max: 14 },
    ],
  },
  '/bin.html': {
    header: true,
    chrome: true,
    regions: [
      { selector: '#crumbs', kind: 'crumbs' },
      { selector: '#bin-head', kind: 'reserve' },
      { selector: '#bin-bar', kind: 'shown' },
      { selector: '#groups', kind: 'bin-group', count: 2, max: 4 },
    ],
  },
});

/** The skeleton of the page at `pathname` ('/' is the start page), or null. */
export function pageSkeleton(pathname) {
  return PAGE_SKELETONS[pathname === '/' ? '/index.html' : pathname] ?? null;
}

/** How many items region `region` shows: what was remembered (0 included), else its count; at most max. */
export function skeletonCount(region, remembered) {
  if (!LISTS.has(region.kind)) return 1;
  const max = region.max ?? 10;
  const n = Number.isInteger(remembered) && remembered >= 0 ? remembered : region.count ?? 3;
  return Math.min(n, max);
}

/** Whether `kind` remembers how many items it had, or how tall it was. */
export const remembersCount = (kind) => LISTS.has(kind);
export const remembersHeight = (kind) => HEIGHTS.has(kind);
export const remembersShown = (kind) => SHOWN.has(kind);

/**
 * The key a shape is remembered under: the page (path and query, without the one-off welcome
 * parameters), the part, and whether the window was wide (heights differ on phones).
 */
export function shapeKey(pathname, search, part, wide) {
  const params = new URLSearchParams(search);
  for (const name of ['welcome', 'unset']) params.delete(name);
  const query = params.toString();
  return `${pathname === '/' ? '/index.html' : pathname}${query ? `?${query}` : ''}|${part}|${wide ? 'wide' : 'narrow'}`;
}

/** `memory` with `key` set to `value`, last used last, keeping the newest `limit` keys. */
export function rememberShape(memory, key, value, limit = 150) {
  const next = {};
  const keys = Object.keys(memory ?? {}).filter((k) => k !== key);
  for (const k of keys.slice(Math.max(0, keys.length - (limit - 1)))) next[k] = memory[k];
  next[key] = value;
  return next;
}

/** Reads remembered shapes from stored text; anything unreadable is forgotten. */
export function parseShapes(text) {
  try {
    const value = JSON.parse(text ?? '{}');
    if (!value || typeof value !== 'object' || Array.isArray(value)) return {};
    const shapes = {};
    for (const [key, n] of Object.entries(value)) if (Number.isFinite(n) && n >= 0 && n < 5000) shapes[key] = Math.round(n);
    return shapes;
  } catch {
    return {};
  }
}

// ---- in the browser ----

const STORE = 'sharecraft-skeleton';
const states = new Map(); // region element -> what it was before, and its observer
let memory = {};
let live = null;
let liveTimer = 0;
let mutating = false; // this module is changing a region: its observer ignores it

const wide = () => (globalThis.innerWidth ?? 1024) >= 700;
const keyOf = (part) => shapeKey(location.pathname, location.search, part, wide());

/** The key of `part` on the page this one was opened from, when that was a ShareCraft page. */
function referrerKey(part) {
  try {
    const from = new URL(document.referrer);
    return from.origin === location.origin ? shapeKey(from.pathname, from.search, part, wide()) : null;
  } catch {
    return null;
  }
}

function loadMemory() {
  try {
    memory = parseShapes(localStorage.getItem(STORE));
  } catch {
    memory = {};
  }
}

function remember(part, value) {
  memory = rememberShape(memory, keyOf(part), value);
  try {
    localStorage.setItem(STORE, JSON.stringify(memory));
  } catch {
    // not remembered: the defaults are used next time
  }
}

/** Builds the DOM of shape `node` (an `el`, a `bar` or a `box`). */
function build(node) {
  if (node.bar) {
    const span = document.createElement('span');
    span.className = 'sk';
    span.style.setProperty('--sk-w', node.bar);
    return span;
  }
  if (node.box) {
    const span = document.createElement('span');
    span.className = `sk sk-box sk-${node.box}`;
    return span;
  }
  const element = document.createElement(node.tag);
  if (node.classes.length) element.className = node.classes.join(' ');
  element.append(...node.children.map(build));
  return element;
}

/** One placeholder item of `kind`, hidden from screen readers. */
function item(kind) {
  const node = build(SHAPES[kind]);
  node.classList.add('sk-item');
  node.setAttribute('aria-hidden', 'true');
  return node;
}

/** A placeholder band above the page: the site's header (`header`) or navigation (`chrome`). */
function band(part, height) {
  const element = document.createElement(part === 'header' ? 'header' : 'nav');
  element.className = part === 'header' ? 'site-header sk-header sk-band' : 'chrome sk-chrome sk-band';
  element.setAttribute(part === 'header' ? 'data-site-header' : 'data-chrome', '');
  element.setAttribute('aria-hidden', 'true');
  if (height !== undefined) element.style.setProperty('--sk-h', `${height}px`);
  return element;
}

/** Shows the placeholders of the page at `pathname`. Called once, as the page arrives. */
export function showSkeleton(pathname = location.pathname) {
  const spec = pageSkeleton(pathname);
  if (!spec) return;
  loadMemory();
  const top = document.querySelector('.topbar');
  let after = top;
  for (const part of ['header', 'chrome']) {
    if (!spec[part] || !after) continue;
    // Never seen this page: the page it was opened from (in the same site, mostly) has the same bars.
    const height = memory[keyOf(part)] ?? memory[referrerKey(part)];
    if (height === 0) continue; // this page had none last time
    const placeholder = band(part, height);
    after.after(placeholder);
    after = placeholder;
  }
  for (const region of spec.regions) {
    const element = document.querySelector(region.selector);
    if (element) fill(element, region);
  }
  if (states.size === 0) return;
  live = document.createElement('p');
  live.className = 'visually-hidden';
  live.setAttribute('role', 'status');
  document.body.append(live);
  // Said only when loading takes more than a moment, and only once.
  liveTimer = setTimeout(() => {
    if (states.size) live.textContent = 'Loading…';
  }, 700);
}

function fill(element, region) {
  const { kind } = region;
  const count = skeletonCount(region, memory[keyOf(region.selector)]);
  const state = { kind, part: region.selector, saved: null, unhidden: [], reserved: false, observer: null };
  mutating = true;
  if (kind === 'shown') {
    // Shown (at its height) while loading only when the page showed it last time.
    const height = memory[keyOf(region.selector)];
    if (!(height > 0) || !element.hidden) {
      mutating = false;
      states.set(element, state);
      return;
    }
    element.style.setProperty('--sk-min', `${height}px`);
    element.classList.add('sk-reserve');
    state.reserved = true;
  } else if (kind === 'reserve') {
    const height = memory[keyOf(region.selector)];
    if (height > 0) {
      element.style.setProperty('--sk-min', `${height}px`);
      element.classList.add('sk-reserve');
      state.reserved = true;
    }
  } else if (kind === 'about') {
    const height = memory[keyOf(region.selector)];
    if (height > 0) {
      element.style.setProperty('--sk-min', `${height}px`);
      element.classList.add('sk-reserve');
      state.reserved = true;
    }
    element.append(item(kind));
  } else {
    if (count === 0) {
      mutating = false;
      return;
    }
    state.saved = [...element.childNodes];
    element.replaceChildren(...Array.from({ length: count }, () => item(kind)));
  }
  // What waits is shown (inert until it is drawn): a hidden table or section around the region.
  for (let node = element; node && node !== document.body; node = node.parentElement) {
    if (node.hidden) {
      node.hidden = false;
      node.inert = true;
      state.unhidden.push(node);
    }
  }
  element.setAttribute('aria-busy', 'true');
  mutating = false;
  if (!EXPLICIT.has(kind)) {
    state.observer = new MutationObserver((records) => {
      if (mutating) return;
      const drawn = records.some((record) => [...record.removedNodes].some((node) => node.classList?.contains('sk-item')));
      if (drawn) settle(element);
    });
    state.observer.observe(element, { childList: true });
  }
  states.set(element, state);
}

/**
 * The page has drawn `element` (or, with `{ drawn: false }`, never will): its placeholders go,
 * aria-busy goes, and what it drew is remembered for next time. Without `drawn`, what this module
 * showed or emptied is put back as the page had it.
 */
export function settle(element, { drawn = true } = {}) {
  const state = element && states.get(element);
  if (!state) return;
  states.delete(element);
  state.observer?.disconnect();
  mutating = true;
  for (const child of [...element.children]) if (child.classList.contains('sk-item')) child.remove();
  mutating = false;
  element.removeAttribute('aria-busy');
  for (const node of state.unhidden) node.inert = false;
  if (state.reserved) {
    element.classList.remove('sk-reserve');
    element.style.removeProperty('--sk-min');
  }
  if (drawn) {
    if (remembersCount(state.kind)) remember(state.part, element.children.length);
    else if (remembersHeight(state.kind) && element.offsetHeight > 0) remember(state.part, element.offsetHeight);
    else if (remembersShown(state.kind)) remember(state.part, element.hidden ? 0 : element.offsetHeight);
  } else {
    for (const node of state.unhidden) node.hidden = true;
    if (state.saved && element.childNodes.length === 0) element.replaceChildren(...state.saved);
  }
  if (states.size === 0) finished();
}

/** Ends every wait still showing: the page has done what it will do (or failed). */
export function settleAll() {
  for (const element of [...states.keys()]) settle(element, { drawn: false });
  if (typeof document !== 'undefined') for (const node of document.querySelectorAll('.sk-band')) node.remove();
  finished();
}

function finished() {
  clearTimeout(liveTimer);
  if (live) live.textContent = '';
}

/**
 * Remembers the heights the site's header and navigation took on this page (siteChrome and
 * organisationChrome call it once they are drawn): 0 for a part that is not there, or not above the page (navigation
 * in a column at the side).
 */
export function rememberChrome() {
  if (typeof window === 'undefined' || typeof document?.querySelector !== 'function') return;
  const page = document.querySelector('.topbar')?.parentElement;
  const header = document.querySelector('[data-site-header]:not(.sk-band)');
  const chrome = page?.querySelector(':scope > [data-chrome]:not(.sk-band)');
  const spec = pageSkeleton(location.pathname);
  if (spec?.header) remember('header', header && !header.hidden ? header.offsetHeight : 0);
  if (spec?.chrome) remember('chrome', chrome && !chrome.hidden && !page.classList.contains('nav-vertical') ? chrome.offsetHeight : 0);
}

/**
 * Runs `update` (which changes the page) inside a view transition, a soft crossfade, where the
 * browser has them and motion is welcome; otherwise just runs it. Answers when it has run.
 */
export async function softly(update) {
  const still = globalThis.matchMedia?.('(prefers-reduced-motion: reduce)').matches;
  if (typeof document === 'undefined' || !document.startViewTransition || still) return update();
  let ran = false;
  const transition = document.startViewTransition(() => {
    ran = true;
    return update();
  });
  // A transition skipped (another one started, the tab hidden) still runs the update: no error.
  transition.ready.catch(() => {});
  transition.finished.catch(() => {});
  // The browser starts the update at its next frame; a page that is not drawing frames (in the
  // background) would wait for it, so after a moment the crossfade is dropped and the update runs.
  const guard = setTimeout(() => {
    if (!ran) transition.skipTransition();
  }, 300);
  try {
    await transition.updateCallbackDone;
  } catch {
    // the update itself failed: as without a transition
  } finally {
    clearTimeout(guard);
  }
  return undefined;
}

if (typeof document !== 'undefined' && typeof MutationObserver !== 'undefined') {
  showSkeleton();
  // The crossfade between pages (@view-transition in style.css) may be dropped, for one when the
  // page is not being drawn; that is not an error.
  const quiet = (event) => {
    event.viewTransition?.ready.catch(() => {});
    event.viewTransition?.finished.catch(() => {});
  };
  addEventListener('pagereveal', quiet);
  addEventListener('pageswap', quiet);
}
