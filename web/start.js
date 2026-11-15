// The start page (index.html): the greeting, the site cards and the files this person opened
// lately in this browser. The helpers here are pure, apart from the localStorage wrappers at the
// end, which never throw (a private window or blocked storage just remembers nothing).

import { brandImageHref, kindLabel, pageFor, rolesText } from './common.js';

/** The first name in `name` (its first word), or '' for none. */
export function firstName(name) {
  return String(name ?? '').trim().split(/\s+/)[0] ?? '';
}

/**
 * The greeting for `hour` (0 to 23) and the person's `name`: `Good morning, Ann` from 5 to 11,
 * `Good afternoon` from 12 to 17, `Good evening` otherwise; just the greeting without a name.
 */
export function greeting(hour, name) {
  const part = hour >= 5 && hour < 12 ? 'morning' : hour >= 12 && hour < 18 ? 'afternoon' : 'evening';
  const first = firstName(name);
  return first ? `Good ${part}, ${first}` : `Good ${part}`;
}

/** The hour (0 to 23) of `now` in `timeZone` (the browser's own when it is not given or unknown). */
export function hourIn(now = Date.now(), timeZone) {
  const at = new Date(now);
  try {
    const text = new Intl.DateTimeFormat('en-GB', { hour: 'numeric', hourCycle: 'h23', timeZone }).format(at);
    const hour = Number.parseInt(text, 10);
    if (Number.isInteger(hour)) return hour % 24;
  } catch {
    // an unknown time zone: the browser's own clock
  }
  return at.getHours();
}

const HEX_COLOUR = /^#[0-9a-fA-F]{6}$/;

/**
 * What a site card shows for `site` (as `GET /api/sites` lists it): `href` and `name` for its
 * one link, `kind` in words, the `hub` it joined (its name only: the card has one link),
 * whether it `isHub` or is `archived`, its `description`, the person's `role` in words, and its
 * look: `accent` (`#rrggbb` or null) and `thumbnail` (a brand image address or null).
 */
export function siteCardModel(site) {
  const roles = site?.access ? rolesText(site.access) : '';
  return {
    id: site?.id ?? '',
    href: pageFor('site', site?.id ?? ''),
    name: String(site?.name ?? ''),
    kind: kindLabel(site?.kind),
    hub: site?.hub?.name ? String(site.hub.name) : null,
    isHub: site?.is_hub === true,
    archived: site?.archived === true,
    description: String(site?.description ?? '').trim(),
    role: site?.access ? (roles ? `Your role: ${roles}` : 'You are a member') : 'Limited access',
    accent: typeof site?.accent === 'string' && HEX_COLOUR.test(site.accent) ? site.accent.toLowerCase() : null,
    thumbnail: brandImageHref(site?.thumbnail_url),
  };
}

// ---- recently opened files ----

/** How many files the Recently opened strip keeps. */
export const RECENT_MAX = 8;

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;

/** `entry` as a recent file `{ id, name, editor, site, at }` if it is a good one, else null. */
function cleanEntry(entry) {
  if (!entry || typeof entry !== 'object') return null;
  const id = typeof entry.id === 'string' ? entry.id.toLowerCase() : '';
  if (!UUID.test(id) || typeof entry.name !== 'string' || entry.name === '') return null;
  const at = Number(entry.at);
  if (!Number.isFinite(at)) return null;
  return {
    id,
    name: entry.name.slice(0, 260),
    editor: typeof entry.editor === 'string' && entry.editor ? entry.editor.slice(0, 40) : null,
    site: typeof entry.site === 'string' ? entry.site.slice(0, 120) : '',
    at,
  };
}

/**
 * The recent files in `value` (an array, or its JSON as stored): the good entries only, newest
 * first, each file once (its newest opening), at most `max`. Anything unreadable is no files.
 */
export function parseRecent(value, max = RECENT_MAX) {
  let list = value;
  if (typeof value === 'string') {
    try {
      list = JSON.parse(value);
    } catch {
      return [];
    }
  }
  if (!Array.isArray(list)) return [];
  const seen = new Set();
  const kept = [];
  for (const entry of list.map(cleanEntry).filter(Boolean).sort((a, b) => b.at - a.at)) {
    if (seen.has(entry.id)) continue;
    seen.add(entry.id);
    kept.push(entry);
    if (kept.length >= max) break;
  }
  return kept;
}

/** `list` with `entry` first (an earlier opening of the same file dropped), trimmed to `max`. */
export function addRecent(list, entry, max = RECENT_MAX) {
  const clean = cleanEntry(entry);
  const rest = parseRecent(list, Number.MAX_SAFE_INTEGER);
  if (!clean) return rest.slice(0, max);
  return [clean, ...rest.filter((item) => item.id !== clean.id)].slice(0, max);
}

/** `list` without the file `id`. */
export function removeRecent(list, id) {
  return parseRecent(list, Number.MAX_SAFE_INTEGER).filter((item) => item.id !== String(id).toLowerCase());
}

/** The localStorage key of the person `userId`'s recent files: people sharing a browser keep their own. */
export const recentKey = (userId) => `sharecraft-recent-files:${userId ?? 'anyone'}`;

const storage = () => {
  try {
    return globalThis.localStorage ?? null;
  } catch {
    return null;
  }
};

/** The person's recent files in this browser, newest first ([] when storage cannot be read). */
export function readRecent(userId, store = storage()) {
  try {
    return parseRecent(store?.getItem(recentKey(userId)) ?? '[]');
  } catch {
    return [];
  }
}

function writeRecent(userId, list, store) {
  try {
    store?.setItem(recentKey(userId), JSON.stringify(list));
  } catch {
    // full or blocked storage: the strip just does not change
  }
}

/** Remembers that the person opened a file now: `{ id, name, editor, site }`. */
export function rememberOpened(userId, file, { now = Date.now(), store = storage() } = {}) {
  writeRecent(userId, addRecent(readRecent(userId, store), { ...file, at: now }), store);
}

/** Forgets the file `id` (removed by the person, or gone). */
export function forgetOpened(userId, id, store = storage()) {
  const before = readRecent(userId, store);
  const after = removeRecent(before, id);
  if (after.length !== before.length) writeRecent(userId, after, store);
  return after;
}
