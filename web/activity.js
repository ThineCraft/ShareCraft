// A file's activity: who changed it, and when. The server keeps a document's latest events
// (`GET /api/docs/{id}/events`, oldest first, at most 200, only for those who manage access to
// it); this module turns them into plain sentences ("Ann Tester renamed it from Budget.xlsx to
// Budget 2026.xlsx"), folds a run of saves by one person into one line with an expander, and
// draws them newest first under day headings (Today, Yesterday, Monday, 3 Nov) in the
// organisation's time zone.
//
// It is shown in three places: the Activity part of the library's details pane (the latest
// five), and the full Activity panel opened from the editor bar's "⋯" menu on file.html (also
// over the viewer) and doc.html, and from "See all activity" in the pane. The panel is a sheet
// at the right on wide screens and a bottom sheet on phones (sheet.js).
//
// `describeEvent`, `collapseSaves`, `groupByDay`, `peopleDirectory`, `activityError` and
// `visibleCount` are pure and tested (activity.test.mjs); the rest needs a page.

import { avatarColour, initials } from './account.js';
import { api, formatDate, formatSize, h, messageFor, regionalSettings, seg } from './common.js';
import { relativeTime } from './files.js';
import { dragToClose, phoneShellNow } from './sheet.js';

/** Saves by one person this close together (milliseconds) fold into one line. */
export const SAVE_RUN_MS = 10 * 60 * 1000;

/** How many entries the panel shows at first, and how many more each "Show older" adds. */
export const PAGE_SIZE = 30;

/** How many entries the details pane shows. */
export const PANE_ENTRIES = 5;

/** The most events the server answers; when that many come back, older ones were left out. */
export const SERVER_LIMIT = 200;

/**
 * Kinds that say who had the file open (joining and leaving an editing session, taking and
 * freeing the edit seat). They are not changes, and they would bury the ones that are.
 */
export const QUIET_KINDS = Object.freeze(['editing.joined', 'editing.left', 'seat.taken', 'seat.freed']);

const SAVE_KINDS = ['revision.saved', 'upload.completed'];
const BUILTIN_ROLES = Object.freeze({ viewer: 'Viewer', editor: 'Editor', owner: 'Owner' });
const DAY = 24 * 60 * 60 * 1000;

/**
 * Whom the sentences can name, from what the page has: `me` (the signed-in account), `access`
 * (`GET /api/nodes/{id}/access`: its people, groups and role names) and `users` (`GET
 * /api/admin/users`, for administrators). Answers `{ me, users: {id: name}, groups: {id: name},
 * roles: {id: name} }`.
 */
export function peopleDirectory({ me = null, access = null, users = [] } = {}) {
  const directory = { me: me?.id ?? null, users: {}, groups: {}, roles: { ...BUILTIN_ROLES } };
  for (const user of Array.isArray(users) ? users : []) {
    if (user?.id && user.name) directory.users[user.id] = user.name;
  }
  for (const assignment of access?.assignments ?? []) {
    const principal = assignment?.principal ?? {};
    if (principal.kind === 'user' && principal.id && principal.name) directory.users[principal.id] = principal.name;
    if (principal.kind === 'group' && principal.id && principal.name && !principal.hidden) directory.groups[principal.id] = principal.name;
    if (assignment.role?.id && assignment.role.name) directory.roles[assignment.role.id] = assignment.role.name;
  }
  if (me?.id && me.name) directory.users[me.id] = me.name;
  return directory;
}

const own = (map, key) => (map && key != null && Object.hasOwn(map, key) ? map[key] : null);

/** The person `id` as the start of a sentence: "You", their name, or "Someone". */
function personName(id, people = {}) {
  if (!id) return null;
  if (people.me && id === people.me) return 'You';
  return own(people.users, id) ?? 'Someone';
}

/** The person `id` inside a sentence: "you", their name, or "someone". */
function personObject(id, people = {}) {
  const name = personName(id, people);
  if (name === 'You') return 'you';
  if (name === 'Someone') return 'someone';
  return name;
}

/** Who acted in `event`: the actor, or for a save its first author. */
function actorOf(event) {
  if (event?.actor) return String(event.actor);
  const authors = event?.detail?.authors;
  if (Array.isArray(authors) && authors.length && typeof authors[0] === 'string') return authors[0];
  return null;
}

const roleName = (id, people) => (id ? own(people.roles, id) ?? String(id) : null);

/** Who an access event names: the person (subject), else the group in its detail. */
function principalOf(event, people) {
  if (event.subject) return personObject(event.subject, people);
  const group = event.detail?.group;
  if (group) return own(people.groups, group) ?? 'a group';
  return 'someone';
}

/** "your", "Bob Tester's", "a group's": the possessive of a `principalOf` answer. */
const possessive = (whom) => (whom === 'you' ? 'your' : `${whom}'s`);

/** "Tuesday 3 November", for a link's expiry (unix seconds), in the page's zone. */
function expiryWords(seconds, timeZone) {
  if (typeof seconds !== 'number') return '';
  const options = { weekday: 'long', day: 'numeric', month: 'long', timeZone };
  try {
    return new Intl.DateTimeFormat('en-GB', options).format(new Date(seconds * 1000));
  } catch {
    return new Intl.DateTimeFormat('en-GB', { ...options, timeZone: undefined }).format(new Date(seconds * 1000));
  }
}

/** A kind such as `node.locked` as words: "node locked". */
const kindWords = (kind) => String(kind ?? 'event').replace(/[._]+/g, ' ').trim() || 'event';

/** "Ann Tester and Bob Tester", "Ann, Bob and Cat". */
function listText(names) {
  if (names.length < 2) return names.join('');
  return `${names.slice(0, -1).join(', ')} and ${names[names.length - 1]}`;
}

/**
 * One event (a row of `GET /api/docs/{id}/events`, or an entry of `collapseSaves`) as a plain
 * English sentence, with whom it is about and when: `{ id, at, kind, text, actor: { id, name,
 * system } }`. `people` is a `peopleDirectory`. A person the page cannot name is "Someone"; an
 * event nobody did (a scan, an expiry, a repair) has a system actor. An entry with `count` above
 * 1 (folded saves) says how many times. Kinds this does not know get a neutral sentence with
 * the kind's name, never the raw detail.
 */
export function describeEvent(event, people = {}, { timeZone = regionalSettings().timeZone } = {}) {
  const detail = event?.detail && typeof event.detail === 'object' ? event.detail : {};
  const kind = String(event?.kind ?? '');
  const actorId = actorOf(event);
  const name = personName(actorId, people);
  const who = name ?? 'Someone';
  const system = !actorId;
  let text;
  switch (kind) {
    case 'document.created':
      text = `${who} created it`;
      break;
    case 'upload.completed': {
      const size = typeof detail.bytes === 'number' ? ` (${formatSize(detail.bytes)})` : '';
      if (detail.new_version) text = event.count > 1 ? `${who} uploaded ${event.count} new versions` : `${who} uploaded a new version${size}`;
      else text = `${who} uploaded it${size}`;
      break;
    }
    case 'revision.saved': {
      const authors = Array.isArray(detail.authors) ? [...new Set(detail.authors.filter((a) => typeof a === 'string'))] : [];
      const names = authors.length > 1 ? listText(authors.map((id, index) => (index === 0 ? personName(id, people) : personObject(id, people)))) : who;
      if (event.count > 1) text = `${names} saved ${event.count} times`;
      else if (authors.length === 0 && !event.actor) text = 'A new version was saved';
      else text = `${names} saved a new version`;
      break;
    }
    case 'revision.conflict':
      text = 'A save was stopped: the file had been changed somewhere else';
      break;
    case 'revision.save_failed':
      text = 'A version could not be saved';
      break;
    case 'revision.index_failed':
      text = 'Search could not read this version';
      break;
    case 'node.renamed':
      text = detail.from && detail.to ? `${who} renamed it from ${detail.from} to ${detail.to}` : `${who} renamed it`;
      break;
    case 'node.moved':
      text = `${who} moved it to another folder`;
      break;
    case 'node.copied':
      text = detail.with ? `${who} made it as a copy, with the folder it is in` : `${who} made it as a copy of another file`;
      break;
    case 'node.created':
      text = system ? 'Its record was put right' : `${who} created it`;
      break;
    case 'node.deleted':
      if (detail.repaired) text = 'Its record was put right: it is in the recycle bin';
      else text = detail.with ? `${who} deleted the folder it is in, so it went to the recycle bin too` : `${who} deleted it (it went to the recycle bin)`;
      break;
    case 'node.binned':
      text = `${who} removed it from the recycle bin; it can still be brought back from the second stage`;
      break;
    case 'node.restored':
      text = detail.from_name && detail.name && detail.from_name !== detail.name
        ? `${who} restored it from the recycle bin as ${detail.name}`
        : `${who} restored it from the recycle bin`;
      break;
    case 'node.purged':
      text = detail.quarantined ? `${who} deleted it for good after the scanner found a threat` : system ? 'It was deleted for good' : `${who} deleted it for good`;
      break;
    case 'access.granted': {
      const role = roleName(detail.role, people);
      text = `${who} gave ${principalOf(event, people)} ${role ? `${role} access` : 'access'}`;
      break;
    }
    case 'access.changed': {
      const from = roleName(detail.from, people);
      const to = roleName(detail.to, people);
      const whom = principalOf(event, people);
      text = from && to ? `${who} changed ${possessive(whom)} access from ${from} to ${to}` : `${who} changed ${possessive(whom)} access`;
      break;
    }
    case 'access.revoked': {
      const role = roleName(detail.role, people);
      const whom = principalOf(event, people);
      const what = role ? `${role} access` : 'access';
      text = system ? `${whom.charAt(0).toUpperCase()}${whom.slice(1)} lost ${what}` : `${who} took away ${possessive(whom)} ${what}`;
      break;
    }
    case 'inheritance.broken':
      text = `${who} gave it permissions of its own`;
      break;
    case 'inheritance.restored':
      text = `${who} made it share the permissions of the place above again`;
      break;
    case 'link.created': {
      const role = roleName(detail.role, people);
      const audience = detail.kind === 'anyone' ? 'anyone with the address' : 'people in the organisation';
      const expiry = typeof detail.expires_at === 'number' ? `, until ${expiryWords(detail.expires_at, timeZone)}` : '';
      text = `${who} made a link for ${audience}${role ? ` with ${role} access` : ''}${expiry}`;
      break;
    }
    case 'link.revoked':
      text = detail.reason === 'expired' ? 'A link to it expired' : system ? 'A link to it was turned off' : `${who} turned off a link to it`;
      break;
    case 'link.used':
      text = `${who} opened it through a link for the first time`;
      break;
    case 'request.created':
      text = `${who} asked for access`;
      break;
    case 'request.approved': {
      const role = roleName(detail.role, people);
      text = `${who} said yes to ${possessive(principalOf(event, people))} request${role ? ` and gave ${role} access` : ''}`;
      break;
    }
    case 'request.declined':
      text = `${who} said no to ${possessive(principalOf(event, people))} request for access`;
      break;
    case 'content.scanned':
      text = scanText(detail);
      break;
    case 'editing.removed':
      text = `${personName(event.subject, people) ?? 'Someone'} was taken out of the editing session`;
      break;
    case 'editing.joined':
      text = `${who} opened it`;
      break;
    case 'editing.left':
      text = `${who} closed it`;
      break;
    default:
      text = system ? `Something was recorded: ${kindWords(kind)}` : `${who} did something recorded as ${kindWords(kind)}`;
  }
  return {
    id: event?.id ?? null,
    at: event?.at ?? null,
    kind,
    text,
    // `label` is the name the avatar's initials come from: the person's own, also for "You".
    actor: system ? { id: null, name: 'ShareCraft', label: '', system: true } : { id: actorId, name: who, label: own(people.users, actorId) ?? '', system: false },
  };
}

/** The scanner's verdict as a sentence. */
function scanText(detail) {
  switch (detail.result) {
    case 'clean':
      return 'Checked for malware — clean';
    case 'infected':
      return detail.detail ? `Checked for malware — a threat was found (${detail.detail})` : 'Checked for malware — a threat was found';
    case 'released':
      return 'An administrator released it after a malware check';
    case 'too_large':
      return 'Too large to check for malware';
    case 'failed':
      return 'The malware check could not finish';
    case 'pending':
      return 'Waiting for the malware check';
    default:
      return 'Checked for malware';
  }
}

const timeOf = (event) => {
  const ms = event?.at ? new Date(event.at).getTime() : Number.NaN;
  return Number.isNaN(ms) ? null : ms;
};

/** Whether `event` is a save that can fold into a run: a saved version, or an uploaded new one. */
const isSave = (event) => event?.kind === 'revision.saved' || (event?.kind === 'upload.completed' && event.detail?.new_version === true);

/** Who a save is by, as one key (all its authors). */
function saveKey(event) {
  const authors = Array.isArray(event.detail?.authors) ? event.detail.authors : [];
  return [...new Set([event.actor, ...authors].filter(Boolean))].sort().join(',');
}

/**
 * The events as the timeline shows them, newest first: the quiet kinds (who had it open) left
 * out; an upload's own "saved" and "created" events folded into the upload (same person, within
 * two minutes), and access given or taken through a link folded into the link's own event; and
 * each run of saves by the same person, every one within `SAVE_RUN_MS` of the next, made one
 * entry: the newest save with `count` and `members` (newest first). Other entries have count 1.
 */
export function collapseSaves(events, { runMs = SAVE_RUN_MS } = {}) {
  const list = (Array.isArray(events) ? events : [])
    .filter((event) => event && typeof event === 'object' && !QUIET_KINDS.includes(event.kind))
    .filter((event) => !((event.kind === 'access.granted' || event.kind === 'access.revoked') && event.detail?.link));
  // newest first: by time, then by id (the server's order)
  const sorted = list
    .map((event, index) => ({ event, index }))
    .sort((a, b) => (timeOf(b.event) ?? -Infinity) - (timeOf(a.event) ?? -Infinity) || (Number(b.event.id) || b.index) - (Number(a.event.id) || a.index))
    .map(({ event }) => event);

  // An upload records "created" (a new file) or "saved" (a new version) as well: one line each.
  const uploads = sorted.filter((event) => event.kind === 'upload.completed');
  const absorbed = new Set();
  for (const upload of uploads) {
    const at = timeOf(upload);
    const twin = sorted.find(
      (event) =>
        !absorbed.has(event) &&
        (upload.detail?.new_version ? event.kind === 'revision.saved' : event.kind === 'document.created') &&
        saveKey(event).split(',').includes(String(upload.actor)) &&
        at !== null &&
        timeOf(event) !== null &&
        Math.abs(timeOf(event) - at) <= 120_000,
    );
    if (twin) absorbed.add(twin);
  }
  const tidy = sorted.filter((event) => !absorbed.has(event));

  const entries = [];
  for (const event of tidy) {
    const last = entries[entries.length - 1];
    const lastMember = last?.members[last.members.length - 1];
    const at = timeOf(event);
    if (
      last &&
      isSave(event) &&
      isSave(last) &&
      event.kind === last.kind &&
      saveKey(event) === saveKey(last) &&
      at !== null &&
      timeOf(lastMember) !== null &&
      timeOf(lastMember) - at <= runMs
    ) {
      last.members.push(event);
      last.count = last.members.length;
      continue;
    }
    entries.push({ ...event, count: 1, members: [event] });
  }
  return entries;
}

/** The calendar day of `ms` in `timeZone`, as a day number (days since 1970). */
function dayNumber(ms, timeZone) {
  let parts;
  try {
    parts = new Intl.DateTimeFormat('en-GB', { year: 'numeric', month: 'numeric', day: 'numeric', timeZone }).formatToParts(new Date(ms));
  } catch {
    parts = new Intl.DateTimeFormat('en-GB', { year: 'numeric', month: 'numeric', day: 'numeric' }).formatToParts(new Date(ms));
  }
  const part = (type) => Number(parts.find((p) => p.type === type).value);
  return Math.round(Date.UTC(part('year'), part('month') - 1, part('day')) / DAY);
}

/**
 * `entries` (newest first, each with `at`) under day headings in `timeZone`, compared with
 * `now`: Today, Yesterday, the weekday's name for the rest of the last week ("Monday"), then the
 * date ("3 November", with the year when it is not this year's). Answers `[{ key, heading,
 * items }]` in the order given; entries without a time go last, under "Date not known". A time
 * a little ahead of `now` (clocks differ) is today.
 */
export function groupByDay(entries, now = Date.now(), timeZone = undefined) {
  const today = dayNumber(new Date(now).getTime(), timeZone);
  const groups = [];
  const byKey = new Map();
  const unknown = [];
  for (const entry of Array.isArray(entries) ? entries : []) {
    const at = timeOf(entry);
    if (at === null) {
      unknown.push(entry);
      continue;
    }
    const day = dayNumber(at, timeZone);
    const key = String(day);
    if (!byKey.has(key)) {
      const group = { key, heading: dayHeading(day, today, at, now, timeZone), items: [] };
      byKey.set(key, group);
      groups.push(group);
    }
    byKey.get(key).items.push(entry);
  }
  if (unknown.length) groups.push({ key: 'unknown', heading: 'Date not known', items: unknown });
  return groups;
}

function dayHeading(day, today, at, now, timeZone) {
  const ago = today - day;
  if (ago <= 0) return 'Today';
  if (ago === 1) return 'Yesterday';
  const format = (options) => {
    try {
      return new Intl.DateTimeFormat('en-GB', { ...options, timeZone }).format(new Date(at));
    } catch {
      return new Intl.DateTimeFormat('en-GB', options).format(new Date(at));
    }
  };
  if (ago < 7) return format({ weekday: 'long' });
  const sameYear = format({ year: 'numeric' }) === (() => {
    try {
      return new Intl.DateTimeFormat('en-GB', { year: 'numeric', timeZone }).format(new Date(now));
    } catch {
      return new Intl.DateTimeFormat('en-GB', { year: 'numeric' }).format(new Date(now));
    }
  })();
  return format(sameYear ? { day: 'numeric', month: 'long' } : { day: 'numeric', month: 'long', year: 'numeric' });
}

/**
 * The plain words for a failed events request: who may see activity when it was refused, the
 * file being gone, else the shared message for the error code.
 */
export function activityError(status, error) {
  if (status === 403) return 'Only people who can manage access to this file can see its activity.';
  if (status === 404) return 'This file could not be found. It may have been deleted, or it is no longer shared with you.';
  return messageFor(error ?? 'unknown');
}

/** How many entries to show after `shown` were shown and "Show older" was pressed. */
export function visibleCount(shown, total, page = PAGE_SIZE) {
  return Math.min(total, Math.max(0, shown) + page);
}

// ---- on the page ----

/**
 * Fetches what the timeline of document `id` needs: its events, and the names to put in them
 * (its access list, and every account for an administrator). Answers `{ ok: true, events,
 * people, capped }` or `{ ok: false, text }`.
 */
export async function loadActivity(id, me, { access = undefined } = {}) {
  const [events, accessAnswer, users] = await Promise.all([
    api('GET', `/docs/${seg(id)}/events`),
    access === undefined ? api('GET', `/nodes/${seg(id)}/access`) : Promise.resolve({ ok: true, data: access }),
    me?.is_admin ? api('GET', '/admin/users') : Promise.resolve({ ok: false }),
  ]);
  if (!events.ok) return { ok: false, text: activityError(events.status, events.error) };
  const list = Array.isArray(events.data) ? events.data : [];
  return {
    ok: true,
    events: list,
    capped: list.length >= SERVER_LIMIT,
    people: peopleDirectory({ me, access: accessAnswer.ok ? accessAnswer.data : null, users: users.ok ? users.data : [] }),
  };
}

const SVG = 'http://www.w3.org/2000/svg';

/** The neutral mark of something ShareCraft did itself (a scan, an expiry). */
function systemIcon() {
  const svg = document.createElementNS(SVG, 'svg');
  svg.setAttribute('viewBox', '0 0 24 24');
  svg.setAttribute('width', '16');
  svg.setAttribute('height', '16');
  svg.setAttribute('aria-hidden', 'true');
  svg.setAttribute('focusable', 'false');
  const path = document.createElementNS(SVG, 'path');
  // a shield
  path.setAttribute('d', 'M12 3l7 3v5c0 4.4-3 8.3-7 9.5C8 19.3 5 15.4 5 11V6z');
  path.setAttribute('fill', 'none');
  path.setAttribute('stroke', 'currentColor');
  path.setAttribute('stroke-width', '2');
  path.setAttribute('stroke-linejoin', 'round');
  svg.append(path);
  return h('span', { class: 'avatar small activity-system', 'aria-hidden': 'true' }, svg);
}

/** The round initials of `actor` (round 6's colours), or the system mark. */
function actorAvatar(actor) {
  if (actor.system) return systemIcon();
  const colour = avatarColour(actor.id || actor.name);
  const element = h('span', { class: 'avatar small', 'aria-hidden': 'true' }, actor.label ? initials(actor.label) : '?');
  element.style.setProperty('--avatar-light-bg', colour.light.bg);
  element.style.setProperty('--avatar-light-ink', colour.light.ink);
  element.style.setProperty('--avatar-dark-bg', colour.dark.bg);
  element.style.setProperty('--avatar-dark-ink', colour.dark.ink);
  return element;
}

/**
 * When `at` was, short: relative today ("2 hours ago"); on an earlier day, whose heading already
 * says the date, the time of day ("15:04"). The exact date and time is in a title and, for
 * screen readers, in text.
 */
export function whenText(at, now = Date.now(), { language, timeZone } = regionalSettings()) {
  if (!at) return '';
  const ms = new Date(at).getTime();
  if (Number.isNaN(ms)) return String(at);
  const day = (value) => dayNumber(new Date(value).getTime(), timeZone);
  if (day(ms) >= day(now)) return relativeTime(at, now, { language, timeZone });
  try {
    return new Intl.DateTimeFormat(language ?? 'en-GB', { hour: 'numeric', minute: '2-digit', timeZone }).format(new Date(ms));
  } catch {
    return new Intl.DateTimeFormat('en-GB', { hour: 'numeric', minute: '2-digit' }).format(new Date(ms));
  }
}

function timeNode(at, now, regional) {
  if (!at) return null;
  const exact = formatDate(at, regional);
  return h(
    'span',
    { class: 'activity-when' },
    h('time', { datetime: at, title: exact }, whenText(at, now, regional)),
    h('span', { class: 'visually-hidden' }, `, ${exact}`),
  );
}

let uid = 0;

/**
 * Draws `entries` (from `collapseSaves`) into `list`: day sections with a heading at
 * `level`, each entry with its avatar, sentence and time; folded saves get a button that shows
 * each save. `regional` is `{ language, timeZone }`.
 */
export function drawTimeline(list, entries, people, { now = Date.now(), regional = regionalSettings(), level = 3 } = {}) {
  const days = groupByDay(entries, now, regional.timeZone);
  list.replaceChildren(
    ...days.map((day) => {
      const id = `activity-day-${(uid += 1)}`;
      return h(
        'section',
        { class: 'activity-day', 'aria-labelledby': id },
        h(`h${level}`, { id, class: 'activity-day-heading' }, day.heading),
        h('ol', { class: 'activity-list' }, ...day.items.map((entry) => entryNode(entry, people, now, regional))),
      );
    }),
  );
}

function entryNode(entry, people, now, regional) {
  const said = describeEvent(entry, people, regional);
  const body = h('div', { class: 'activity-body' }, h('p', { class: 'activity-text' }, said.text), timeNode(said.at, now, regional));
  if (entry.count > 1) {
    const id = `activity-run-${(uid += 1)}`;
    const toggle = h('button', { type: 'button', class: 'ghost small activity-expand', 'aria-expanded': 'false', 'aria-controls': id }, `Show each save`);
    const runList = h(
      'ol',
      { id, class: 'activity-run', hidden: true },
      ...entry.members.map((member) => {
        const one = describeEvent({ ...member, count: 1 }, people, regional);
        return h('li', {}, h('span', {}, one.text), timeNode(one.at, now, regional));
      }),
    );
    toggle.addEventListener('click', () => {
      const opening = runList.hidden;
      runList.hidden = !opening;
      toggle.setAttribute('aria-expanded', String(opening));
      toggle.textContent = opening ? 'Hide each save' : 'Show each save';
    });
    body.append(toggle, runList);
  }
  return h('li', { class: said.actor.system ? 'activity-entry system' : 'activity-entry' }, actorAvatar(said.actor), body);
}

/**
 * The full Activity panel: one `<dialog>` for the page, a sheet at the right on wide screens
 * and a bottom sheet on phones. `open({ id, name, me, opener, access })` loads and shows the
 * activity of document `id`; focus goes back to `opener` (or what had it) when it closes.
 */
export function createActivityPanel() {
  const title = h('h2', { id: 'activity-title' }, 'Activity');
  const subtitle = h('p', { class: 'meta activity-subtitle' });
  const close = h('button', { type: 'button', class: 'ghost sheet-close' }, 'Close');
  const status = h('p', { class: 'meta activity-status', role: 'status', 'aria-live': 'polite' });
  const timeline = h('div', { class: 'activity-timeline' });
  const more = h('button', { type: 'button', class: 'secondary activity-more', hidden: true }, 'Show older');
  const end = h('p', { class: 'meta activity-end', hidden: true });
  const dialog = h(
    'dialog',
    { class: 'activity-panel', 'aria-labelledby': 'activity-title', 'aria-describedby': 'activity-subtitle' },
    h('div', { class: 'sheet-handle', 'aria-hidden': 'true' }),
    h('div', { class: 'activity-head' }, h('div', { class: 'activity-titles' }, title, subtitle), close),
    status,
    timeline,
    more,
    end,
  );
  subtitle.id = 'activity-subtitle';
  document.body.append(dialog);

  let opener = null;
  let stopDrag = null;
  let state = null; // { entries, people, shown, capped }
  let request = 0;

  close.addEventListener('click', () => dialog.close());
  dialog.addEventListener('click', (event) => {
    if (event.target === dialog) dialog.close();
  });
  dialog.addEventListener('close', () => {
    request += 1;
    stopDrag?.();
    stopDrag = null;
    if (opener?.isConnected) opener.focus();
    opener = null;
  });
  more.addEventListener('click', () => {
    if (!state) return;
    const before = state.shown;
    state.shown = visibleCount(state.shown, state.entries.length);
    draw();
    status.textContent = `Showing ${state.shown} of ${state.entries.length}.`;
    // Focus stays near where reading was: the first newly shown entry's day.
    const items = timeline.querySelectorAll('.activity-entry');
    items[before]?.setAttribute('tabindex', '-1');
    items[before]?.focus();
  });

  function draw() {
    const { entries, people, shown, capped } = state;
    drawTimeline(timeline, entries.slice(0, shown), people, { level: 3 });
    more.hidden = shown >= entries.length;
    end.hidden = !(shown >= entries.length && capped);
    end.textContent = 'Older activity is not kept here: only the latest 200 events are shown.';
    if (!entries.length) {
      timeline.replaceChildren(h('p', { class: 'activity-empty' }, 'Nothing has happened to this file yet — or nothing that is recorded here.'));
    }
  }

  async function open({ id, name = '', me = null, opener: from = null, access } = {}) {
    if (!id) return;
    opener = from ?? (document.activeElement instanceof HTMLElement ? document.activeElement : null);
    const phone = phoneShellNow();
    dialog.classList.toggle('as-bottom', phone);
    dialog.classList.toggle('as-side', !phone);
    subtitle.textContent = name;
    subtitle.hidden = !name;
    timeline.replaceChildren();
    more.hidden = true;
    end.hidden = true;
    status.textContent = 'Loading activity…';
    timeline.setAttribute('aria-busy', 'true');
    if (!dialog.open) dialog.showModal();
    close.focus();
    stopDrag?.();
    stopDrag = phone ? dragToClose(dialog, () => dialog.close(), { handle: dialog.querySelector('.sheet-handle') }) : null;
    const asked = (request += 1);
    const answer = await loadActivity(id, me, { access });
    if (asked !== request || !dialog.open) return;
    timeline.removeAttribute('aria-busy');
    if (!answer.ok) {
      state = null;
      status.textContent = '';
      timeline.replaceChildren(h('p', { class: 'msg activity-error', role: 'alert' }, answer.text));
      return;
    }
    const entries = collapseSaves(answer.events);
    state = { entries, people: answer.people, shown: Math.min(PAGE_SIZE, entries.length), capped: answer.capped };
    draw();
    status.textContent = entries.length ? `${entries.length === 1 ? '1 entry' : `${entries.length} entries`}, newest first.` : 'No activity yet.';
  }

  return { open, close: () => dialog.open && dialog.close(), element: dialog };
}

let shared = null;

/** The page's one Activity panel, made the first time it is needed. */
export function activityPanel() {
  shared ??= createActivityPanel();
  return shared;
}
