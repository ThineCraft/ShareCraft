// Instant navigation (round 25): pointing at a link to another ShareCraft page (hover, or a finger
// touching it) asks for that page's first API answers early, and keeps them for a few seconds so
// the page that opens finds them waiting instead of asking again. `api` in common.js looks here
// before it fetches.
//
// The rules: only GET answers are kept, only 200 answers in JSON, never the byte routes (file
// content, previews, thumbnails, downloads) and never an answer marked `no-store` that is not
// JSON. An answer lives 15 seconds and is used once. Anything that changes data (a non-GET
// request) empties the cache, so nothing kept from before the change is shown after it.
// Only page addresses this file knows are planned: a link that is not a page (a download, an
// `/api` route, a sharing link) is never asked for.

/** How long a kept answer stays usable, in milliseconds. */
export const PREFETCH_TTL = 15_000;

/** The sessionStorage prefix of kept answers (they must outlive the page that asked for them). */
export const PREFETCH_PREFIX = 'sharecraft-prefetch:';

/** The routes whose answers are bytes, not JSON: never kept. */
const BYTE_ROUTE = /\/(content|preview|thumbnail|download|pdf)(\/|\?|$)|^\/brand\/images\//;

const ID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;

/** The cache key of a request to `/api{path}`: the method and the path; null for anything but GET. */
export function prefetchKey(method, path) {
  if (method !== 'GET' || typeof path !== 'string' || !path.startsWith('/')) return null;
  return `GET ${path}`;
}

/**
 * Whether an answer may be kept: GET, status 200, JSON, not a byte route, and not `no-store`
 * unless it is a JSON answer of the API (the server marks every `/api` answer `no-store` by
 * default so that no shared or disk cache holds it; the hand-off here is to the very next page
 * in this tab, within 15 seconds, once).
 */
export function mayKeep({ method, path, status, contentType = '', cacheControl = '' }) {
  if (method !== 'GET' || status !== 200) return false;
  if (typeof path !== 'string' || BYTE_ROUTE.test(path)) return false;
  const json = /^application\/(json|[\w.+-]+\+json)\b/i.test(contentType.trim());
  const noStore = /(^|,)\s*no-store\b/i.test(cacheControl);
  if (noStore && !json) return false; // the byte routes: never kept
  return json;
}

/**
 * A cache of kept answers over `store` (`get(key)`, `set(key, text)`, `remove(key)`, `keys()`),
 * with the clock `now` (injectable for tests). Entries are `{ status, data }`.
 */
export function createPrefetchCache({ store, now = () => Date.now(), ttl = PREFETCH_TTL } = {}) {
  const read = (key) => {
    let text;
    try {
      text = store.get(key);
    } catch {
      return null;
    }
    if (text == null) return null;
    try {
      const entry = JSON.parse(text);
      return entry && typeof entry.at === 'number' ? entry : null;
    } catch {
      return null;
    }
  };
  const drop = (key) => {
    try {
      store.remove(key);
    } catch {
      // nothing kept, nothing to drop
    }
  };
  const fresh = (entry) => Boolean(entry) && now() - entry.at >= 0 && now() - entry.at < ttl;
  return {
    /** Keeps `entry` ({ status, data }) under `key`; refused silently when the store is full. */
    put(key, entry) {
      if (!key) return false;
      try {
        store.set(key, JSON.stringify({ at: now(), status: entry.status, data: entry.data }));
        return true;
      } catch {
        return false;
      }
    },
    /** The entry under `key` when it is fresh, taken out of the cache (used once); else null. */
    take(key) {
      if (!key) return null;
      const entry = read(key);
      if (entry === null) return null;
      drop(key);
      return fresh(entry) ? { status: entry.status, data: entry.data } : null;
    },
    /** Whether a fresh entry is kept under `key` (it stays kept). */
    has(key) {
      return fresh(read(key));
    },
    /** Drops the entries that are no longer fresh. */
    sweep() {
      for (const key of safeKeys(store)) if (!fresh(read(key))) drop(key);
    },
    /** Drops everything. */
    clear() {
      for (const key of safeKeys(store)) drop(key);
    },
  };
}

function safeKeys(store) {
  try {
    return [...store.keys()];
  } catch {
    return [];
  }
}

/** A store over a Map (tests, or when sessionStorage cannot be used). */
export function mapStore(map = new Map()) {
  return {
    get: (key) => map.get(key) ?? null,
    set: (key, value) => map.set(key, value),
    remove: (key) => map.delete(key),
    keys: () => map.keys(),
  };
}

/** A store over a Web Storage object, its keys under PREFETCH_PREFIX. */
export function storageStore(storage, prefix = PREFETCH_PREFIX) {
  return {
    get: (key) => storage.getItem(prefix + key),
    set: (key, value) => storage.setItem(prefix + key, value),
    remove: (key) => storage.removeItem(prefix + key),
    keys: () => {
      const keys = [];
      for (let i = 0; i < storage.length; i += 1) {
        const name = storage.key(i);
        if (name?.startsWith(prefix)) keys.push(name.slice(prefix.length));
      }
      return keys;
    },
  };
}

/** What the site's own pages ask for around their content: the site, its navigation and its look (siteChrome). */
export const chromePaths = (siteId) => [`/sites/${siteId}`, `/sites/${siteId}/navigation`, `/sites/${siteId}/look`];

/**
 * What the page at `href` asks for first, in the form its own code asks for it so that the keys
 * match: `{ paths, follow }`, `paths` being API paths (after `/api`) and `follow` naming a node
 * path whose answer leads to more (see [`followPaths`]), or null. Nothing for anything that is
 * not a known ShareCraft page on `origin`, or for the page `current` (same path and query) itself.
 *
 * - every signed-in page starts with GET /me
 * - start page: the organisation's regional settings and navigation, the hubs, then GET /sites
 *   (with ?kind= and ?hub= as home.js builds them)
 * - site home: GET /nodes/{id} (the breadcrumb), the site's chrome and its libraries
 * - library or folder: GET /nodes/{id} and the first page of the list (50, by name); then the
 *   library and the site's chrome
 * - Share: GET /nodes/{id} and /nodes/{id}/access; then the site's chrome
 * - recycle bin: GET /nodes/{site}, the site's chrome and the first 500 entries of the stage
 * - People: the organisation's settings and navigation, and GET /admin/users
 */
export function prefetchPlan(href, origin, current = null) {
  const none = { paths: [], follow: null };
  let url;
  try {
    url = new URL(href, origin);
  } catch {
    return none;
  }
  if (url.origin !== origin) return none;
  if (current && url.pathname + url.search === current) return none;
  const id = (name = 'id') => {
    const value = (url.searchParams.get(name) ?? '').toLowerCase();
    return ID.test(value) ? value : '';
  };
  const page = url.pathname === '/' ? '/index.html' : url.pathname;
  const organisation = ['/settings/regional', '/navigation'];
  switch (page) {
    case '/index.html': {
      const query = new URLSearchParams();
      const kind = url.searchParams.get('kind');
      if (kind === 'team' || kind === 'communication') query.set('kind', kind);
      const hub = id('hub');
      if (hub) query.set('hub', hub);
      const text = query.toString();
      return { paths: ['/me', ...organisation, '/hubs', `/sites${text ? `?${text}` : ''}`], follow: null };
    }
    case '/site.html': {
      const site = id();
      if (!site) return none;
      return { paths: ['/me', `/nodes/${site}`, ...chromePaths(site), `/nodes/${site}/children?limit=500`], follow: null };
    }
    case '/library.html': {
      const node = id();
      if (!node) return none;
      return {
        paths: ['/me', `/nodes/${node}`, `/nodes/${node}/children?limit=50`],
        follow: { from: `/nodes/${node}`, library: true },
      };
    }
    case '/access.html': {
      const node = id();
      if (!node) return none;
      return { paths: ['/me', `/nodes/${node}`, `/nodes/${node}/access`], follow: { from: `/nodes/${node}`, library: false } };
    }
    case '/bin.html': {
      const site = id('site');
      if (!site) return none;
      const stage = url.hash === '#second' ? 'second' : 'first';
      return {
        paths: ['/me', `/nodes/${site}`, ...chromePaths(site), `/sites/${site}/bin?stage=${stage}&limit=500`],
        follow: null,
      };
    }
    case '/people.html':
      return { paths: ['/me', ...organisation, '/admin/users'], follow: null };
    case '/admin-sites.html':
    case '/groups.html':
    case '/roles.html':
    case '/requests.html':
    case '/sharing.html':
    case '/templates.html':
    case '/quarantine.html':
    case '/brand-centre.html':
    case '/explorer.html':
    case '/site-settings.html':
    case '/site-look.html':
    case '/library-settings.html':
      return { paths: ['/me'], follow: null };
    default:
      return none;
  }
}

/**
 * What a node's answer (`GET /api/nodes/{id}`: `{ path }`) names for its page to ask next: the
 * site's chrome, and with `library` the library the node is in (`GET /libraries/{id}`).
 */
export function followPaths(node, { library = false } = {}) {
  const path = Array.isArray(node?.path) ? node.path : [];
  const site = path[0]?.kind === 'site' && ID.test(path[0].id ?? '') ? path[0].id : null;
  const paths = site ? chromePaths(site) : [];
  const step = library ? path.find((s) => s.kind === 'library' && ID.test(s.id ?? '')) : null;
  if (step) paths.unshift(`/libraries/${step.id}`);
  return paths;
}

/**
 * Whether pointing at link `a` may ask for its page early: a plain same-tab link (no `download`,
 * no other target, no `data-no-prefetch`), not with Save-Data on.
 */
export function mayPrefetchLink(a, { saveData = false } = {}) {
  if (!a || saveData) return false;
  if (a.hasAttribute?.('download') || a.hasAttribute?.('data-no-prefetch')) return false;
  const target = a.getAttribute?.('target');
  if (target && target !== '_self') return false;
  return true;
}

// ---- in the browser ----

let shared = null;

/** The cache of this tab (sessionStorage, or memory when that cannot be used); null outside a browser. */
export function prefetchCache() {
  if (shared) return shared;
  if (typeof window === 'undefined') return null;
  let store;
  try {
    const probe = `${PREFETCH_PREFIX}probe`;
    sessionStorage.setItem(probe, '1');
    sessionStorage.removeItem(probe);
    store = storageStore(sessionStorage);
  } catch {
    store = mapStore();
  }
  shared = createPrefetchCache({ store });
  shared.sweep();
  return shared;
}

const asking = new Set(); // keys being fetched now

/**
 * Asks for `/api{path}` now and keeps the answer if it may be kept; answers its data when it was
 * kept, else null. Failures are silent.
 */
export async function prefetch(path) {
  const cache = prefetchCache();
  const key = prefetchKey('GET', path);
  if (!cache || !key || asking.has(key) || cache.has(key)) return null;
  asking.add(key);
  try {
    const response = await fetch(`/api${path}`, { credentials: 'same-origin' });
    const meta = {
      method: 'GET',
      path,
      status: response.status,
      contentType: response.headers.get('Content-Type') ?? '',
      cacheControl: response.headers.get('Cache-Control') ?? '',
    };
    if (!mayKeep(meta)) return null;
    const data = await response.json();
    cache.put(key, { status: response.status, data });
    return data;
  } catch {
    return null; // a prefetch that fails is forgotten: the page asks again itself
  } finally {
    asking.delete(key);
  }
}

/** Hover (after a short pause) or touch on a link to a ShareCraft page asks for its first answers. */
export function setupPrefetch() {
  if (typeof window === 'undefined' || typeof document?.addEventListener !== 'function' || setupPrefetch.done) return;
  setupPrefetch.done = true;
  const saveData = Boolean(globalThis.navigator?.connection?.saveData);
  let timer = 0;
  const go = (a) => {
    if (!mayPrefetchLink(a, { saveData })) return;
    const plan = prefetchPlan(a.href, location.origin, location.pathname + location.search);
    for (const path of plan.paths) {
      const asked = prefetch(path);
      if (plan.follow?.from === path) {
        asked.then((data) => {
          if (data) for (const next of followPaths(data, plan.follow)) prefetch(next);
        });
      }
    }
  };
  document.addEventListener('pointerover', (event) => {
    if (event.pointerType !== 'mouse') return;
    const a = event.target.closest?.('a[href]');
    if (!a) return;
    clearTimeout(timer);
    timer = setTimeout(() => go(a), 80);
  });
  document.addEventListener('pointerout', (event) => {
    if (event.target.closest?.('a[href]')) clearTimeout(timer);
  });
  document.addEventListener(
    'touchstart',
    (event) => {
      const a = event.target.closest?.('a[href]');
      if (a) go(a);
    },
    { passive: true },
  );
}
