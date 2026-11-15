// The Share screen's pure parts (access.html): the one-sentence summary of who can reach an item,
// the order its entries are listed in, and the suggestions of the "Add people or groups" box.
// No page needed; tested in share.test.mjs.

import { PERMS, permLabel } from './common.js';

/**
 * How much an entry's permissions let people do, strongest first: `manage` (manages access or
 * the site: an owner), `edit` (edits or deletes), `add` (adds only) or `view`.
 */
export function accessLevel(perms) {
  const has = (perm) => Array.isArray(perms) && perms.includes(perm);
  if (has('manage_access') || has('manage_site')) return 'manage';
  if (has('edit') || has('delete')) return 'edit';
  if (has('add')) return 'add';
  return 'view';
}

const LEVEL_RANK = Object.freeze({ manage: 0, edit: 1, add: 2, view: 3 });
const LEVEL_VERB = Object.freeze({ manage: 'can manage access', edit: 'can edit', add: 'can add', view: 'can view' });

/** The permissions as a sentence: `Can view, add, edit and delete`; `Can do nothing` for none. */
export function permsSentence(perms) {
  const held = PERMS.filter((perm) => Array.isArray(perms) && perms.includes(perm)).map(permLabel);
  if (held.length === 0) return 'Can do nothing';
  if (held.length === 1) return `Can ${held[0]}`;
  return `Can ${held.slice(0, -1).join(', ')} and ${held[held.length - 1]}`;
}

/** `1 person`, `3 people`. */
const people = (n) => `${n} ${n === 1 ? 'person' : 'people'}`;
const plural = (n, one, many) => `${n} ${n === 1 ? one : many}`;

/** `A`, `A and B`, `A, B and C`. */
function joinAnd(parts) {
  if (parts.length <= 1) return parts.join('');
  return `${parts.slice(0, -1).join(', ')} and ${parts[parts.length - 1]}`;
}

/** An entry's name as the summary says it: `everyone in Home Members (3 people)`, `Ann Tester`. */
function summaryName(principal) {
  if (principal.kind === 'group') {
    if (principal.hidden) return 'a group you cannot see';
    if (principal.member_count === 0) return `${principal.name} (no members yet)`;
    const count = Number.isFinite(principal.member_count) ? ` (${people(principal.member_count)})` : '';
    return `everyone in ${principal.name}${count}`;
  }
  return principal.name || principal.email || 'someone';
}

const capitalise = (text) => (text ? text[0].toLocaleUpperCase() + text.slice(1) : text);

/**
 * Who can reach an item, in a few short sentences, from its access entries (`assignments` of
 * `GET /api/nodes/{id}/access`; a group's principal may carry `member_count` from `GET
 * /api/groups`) and its live links: `Everyone in Home Members (3 people) can edit. 1 owner group.
 * Shared by 1 link.` Up to two entries of one level are named; more are counted. Owners are
 * counted, links last.
 */
export function accessSummary(entries = [], links = []) {
  const sentences = [];
  const byLevel = { manage: [], edit: [], add: [], view: [] };
  let linkEntries = 0;
  for (const entry of entries) {
    if (entry.principal?.kind === 'link') linkEntries += 1;
    else byLevel[accessLevel(entry.perms)].push(entry.principal ?? {});
  }
  for (const level of ['edit', 'add', 'view']) {
    const list = byLevel[level];
    if (list.length === 0) continue;
    if (list.length <= 2) {
      sentences.push(`${capitalise(joinAnd(list.map(summaryName)))} ${LEVEL_VERB[level]}.`);
    } else {
      const groups = list.filter((p) => p.kind === 'group').length;
      const persons = list.length - groups;
      const parts = [];
      if (groups) parts.push(plural(groups, 'group', 'groups'));
      if (persons) parts.push(people(persons));
      sentences.push(`${joinAnd(parts)} ${LEVEL_VERB[level]}.`);
    }
  }
  const owners = byLevel.manage;
  if (owners.length) {
    const groups = owners.filter((p) => p.kind === 'group').length;
    const persons = owners.length - groups;
    const parts = [];
    if (groups) parts.push(plural(groups, 'owner group', 'owner groups'));
    if (persons) parts.push(plural(persons, 'owner', 'owners'));
    sentences.push(`${joinAnd(parts)}.`);
  }
  const live = Array.isArray(links) ? links : [];
  const linkCount = Math.max(linkEntries, live.length);
  if (linkCount > 0) {
    const anyone = live.filter((link) => link.kind === 'anyone').length;
    let open = '';
    if (anyone > 0 && anyone === linkCount) open = linkCount === 1 ? ', open to anyone' : ', all open to anyone';
    else if (anyone > 0) open = `, ${anyone} open to anyone`;
    sentences.push(`Shared by ${plural(linkCount, 'link', 'links')}${open}.`);
  }
  if (sentences.length === 0) return 'Nobody has access here yet.';
  return sentences.join(' ');
}

/** The name an entry is sorted and labelled by. */
function entryName(principal) {
  if (principal?.hidden) return '';
  return String(principal?.name || principal?.email || '');
}

/**
 * The entries in the order the Share screen lists them: owners first, then editors, those who
 * add, viewers, and links last; groups before people within each, then by name (hidden groups
 * after the named ones). A new array.
 */
export function sortEntries(entries = []) {
  const rank = (entry) => (entry.principal?.kind === 'link' ? 4 : LEVEL_RANK[accessLevel(entry.perms)]);
  const kindRank = (entry) => (entry.principal?.kind === 'group' ? 0 : 1);
  return [...entries].sort((a, b) => {
    const byLevel = rank(a) - rank(b);
    if (byLevel) return byLevel;
    const byKind = kindRank(a) - kindRank(b);
    if (byKind) return byKind;
    const an = entryName(a.principal);
    const bn = entryName(b.principal);
    if (!an !== !bn) return an ? -1 : 1;
    return an.localeCompare(bn, undefined, { sensitivity: 'base' });
  });
}

/** Whether `text` reads as one email address. */
export const looksLikeEmail = (text) => /^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(String(text ?? '').trim());

/**
 * The suggestions for `query` among `candidates` (`{ kind: 'group' | 'user', id, name, email?,
 * detail? }`), best first, at most `limit`: names starting with the query, then a word of the
 * name or the email starting with it, then the query anywhere in the name, email or detail. An
 * empty query suggests the first candidates as they are. When the query is an email address no
 * candidate has, an `{ kind: 'email', id: 'email:<address>', name, email, detail }` entry for it
 * comes last (taking the last place if the list is full).
 */
export function comboboxFilter(query, candidates = [], { limit = 8 } = {}) {
  const typed = String(query ?? '').trim();
  const q = typed.toLocaleLowerCase();
  let found;
  if (q === '') {
    found = candidates.slice(0, limit);
  } else {
    const scored = [];
    for (const candidate of candidates) {
      const name = String(candidate.name ?? '').toLocaleLowerCase();
      const email = String(candidate.email ?? '').toLocaleLowerCase();
      const detail = String(candidate.detail ?? '').toLocaleLowerCase();
      let score = -1;
      if (name.startsWith(q)) score = 0;
      else if (name.split(/\s+/).some((word) => word.startsWith(q)) || (email && email.startsWith(q))) score = 1;
      else if (name.includes(q) || email.includes(q) || detail.includes(q)) score = 2;
      if (score >= 0) scored.push({ candidate, score, name });
    }
    scored.sort((a, b) => a.score - b.score || a.name.localeCompare(b.name));
    found = scored.slice(0, limit).map((entry) => entry.candidate);
  }
  if (looksLikeEmail(typed) && !candidates.some((c) => String(c.email ?? '').toLocaleLowerCase() === q)) {
    const byEmail = { kind: 'email', id: `email:${q}`, name: typed, email: typed, detail: 'Add by email address' };
    if (found.length >= limit) found = found.slice(0, Math.max(0, limit - 1));
    found = [...found, byEmail];
  }
  return found;
}
