// The permission map's pure parts (explorer.html): how a place's access differs from the place
// above it, how many exceptions lie at or below each place, which rows of the tree are on screen
// (quiet runs of inheriting places folded into one row), and the one-line summary of the filters.
// No page needed; tested in permission-map.test.mjs.

import { HIDDEN_GROUP, permsText } from './common.js';
import { accessLevel } from './share.js';

/** The key an access entry is matched on between two places: `user:<id>`, `group:<id>`, `link:<id>`. */
export const entryKey = (entry) => `${entry?.principal?.kind ?? '?'}:${entry?.principal?.id ?? '?'}`;

const VERB = Object.freeze({ manage: 'manage access', edit: 'edit', add: 'add', view: 'view' });
const RANK = Object.freeze({ manage: 3, edit: 2, add: 1, view: 0 });

/** Who an entry names, as a sentence starts: `Bob Tester`, `Everyone in Home Members`. */
export function whoText(principal) {
  if (!principal) return 'Someone';
  if (principal.kind === 'link') return principal.link_kind === 'anyone' ? 'An anyone link' : 'A signed-in link';
  if (principal.hidden) return HIDDEN_GROUP;
  if (principal.kind === 'group') return `Everyone in ${principal.name}`;
  return principal.name || principal.email || 'Someone';
}

const samePerms = (a = [], b = []) => a.length === b.length && a.every((perm) => b.includes(perm));

function changedText(who, entry, before) {
  const here = accessLevel(entry.perms);
  const above = accessLevel(before.perms);
  if (RANK[here] > RANK[above]) return `${who} can ${VERB[here]} here; above, they can only ${VERB[above]}.`;
  if (RANK[here] < RANK[above]) return `${who} can only ${VERB[here]} here; above, they can ${VERB[above]}.`;
  return `${who} is ${entry.role?.name ?? 'listed'} here (can ${permsText(entry.perms)}); above, ${
    before.role?.name ?? 'listed'
  } (can ${permsText(before.perms)}).`;
}

/**
 * How the access entries of a place (`entries`, as `GET /api/explorer` or `GET
 * /api/nodes/{id}/access` list assignments) differ from those of the place above it
 * (`parentEntries`, what it would inherit): `{ added, removed, changed, same, items }`, each item
 * `{ change, key, principal, role, perms, before, text }` with `before` the entry above (`{ role,
 * perms }`, null for an added one) and `text` one plain sentence. Matched by person, group or
 * link; a change is a different role or different permissions. `items` lists changes, then
 * additions, then removals, each in the order given.
 */
export function accessDiff(parentEntries = [], entries = []) {
  const above = new Map();
  for (const entry of parentEntries ?? []) above.set(entryKey(entry), entry);
  const here = new Map();
  for (const entry of entries ?? []) here.set(entryKey(entry), entry);
  const added = [];
  const removed = [];
  const changed = [];
  const same = [];
  for (const [key, entry] of here) {
    const before = above.get(key);
    const who = whoText(entry.principal);
    const item = { key, principal: entry.principal, role: entry.role ?? null, perms: entry.perms ?? [] };
    if (!before) {
      added.push({
        ...item,
        change: 'added',
        before: null,
        text: `${who} can ${VERB[accessLevel(entry.perms)]} here; the place above does not list them.`,
      });
    } else if ((before.role?.id ?? null) !== (entry.role?.id ?? null) || !samePerms(before.perms, entry.perms)) {
      changed.push({
        ...item,
        change: 'changed',
        before: { role: before.role ?? null, perms: before.perms ?? [] },
        text: changedText(who, entry, before),
      });
    } else {
      same.push({ ...item, change: 'same', before: { role: before.role ?? null, perms: before.perms ?? [] }, text: '' });
    }
  }
  for (const [key, entry] of above) {
    if (here.has(key)) continue;
    removed.push({
      key,
      principal: entry.principal,
      role: null,
      perms: [],
      change: 'removed',
      before: { role: entry.role ?? null, perms: entry.perms ?? [] },
      text: `${whoText(entry.principal)} can ${VERB[accessLevel(entry.perms)]} above, but is not listed here.`,
    });
  }
  return { added, removed, changed, same, items: [...changed, ...added, ...removed] };
}

/** A diff's counts on a row: `+2 −1 →1` (only the parts that are not zero); '' for no difference. */
export function diffBadge(diff) {
  if (!diff) return '';
  const parts = [];
  if (diff.added.length) parts.push(`+${diff.added.length}`);
  if (diff.removed.length) parts.push(`−${diff.removed.length}`);
  if (diff.changed.length) parts.push(`→${diff.changed.length}`);
  return parts.join(' ');
}

/** A diff's counts in words, for screen readers: `2 added, 1 removed and 1 role changed`; '' for none. */
export function diffWords(diff) {
  if (!diff) return '';
  const parts = [];
  if (diff.added.length) parts.push(`${diff.added.length} added`);
  if (diff.removed.length) parts.push(`${diff.removed.length} removed`);
  if (diff.changed.length) parts.push(`${diff.changed.length} ${diff.changed.length === 1 ? 'role' : 'roles'} changed`);
  if (parts.length <= 1) return parts.join('');
  return `${parts.slice(0, -1).join(', ')} and ${parts[parts.length - 1]}`;
}

/**
 * For every row of a tree (`{ root, loaded }`: `root` a row, `loaded` a Map of a row's id to its
 * child rows; rows `{ id, unique, count }` as the explorer makes them, `count` the exceptions at or
 * below the row when the server or the breaks said so), `{ exceptions, below, quiet }`:
 * `exceptions` at or below the row (own included), `below` those under it only, `quiet` how many
 * of its loaded children inherit and have no exception below them. A Map of id to counts; empty
 * for no root.
 */
export function exceptionCounts(tree) {
  const out = new Map();
  if (!tree?.root) return out;
  const loaded = tree.loaded ?? new Map();
  const visit = (row) => {
    const children = loaded.get(row.id);
    let sum = row.unique ? 1 : 0;
    let quiet = 0;
    for (const child of children ?? []) {
      const counted = visit(child);
      sum += counted.exceptions;
      if (counted.exceptions === 0) quiet += 1;
    }
    const exceptions = Math.max(sum, Number.isFinite(row.count) ? row.count : 0);
    const counts = { exceptions, below: exceptions - (row.unique ? 1 : 0), quiet };
    out.set(row.id, counts);
    return counts;
  };
  visit(tree.root);
  return out;
}

/** Fewest inheriting places, among the children of one place, that fold into one "inherit" row. */
export const QUIET_FOLD = 3;

/**
 * The rows of the permission map on screen, top to bottom, as the tree shows them:
 * `{ type: 'place', id, row, parent, depth, open, expandable, quiet }` for a place (`quiet`:
 * inherits with no exception below), `{ type: 'fold', id: 'fold:<parent>', parent, depth, count,
 * revealed }` for the inheriting children of one open place folded into one row (when there are
 * at least [`QUIET_FOLD`] of them; `revealed` lists them all and the fold row hides them again),
 * and `{ type: 'more', id: 'more:<parent>', parent, depth }` where `more` (a Set of ids) says that
 * place has more children to load. Every row has `posinset` and `setsize` among its siblings.
 */
export function mapRows({ root, loaded = new Map(), open = new Set(), revealed = new Set(), more = new Set(), counts }) {
  if (!root) return [];
  const tally = counts ?? exceptionCounts({ root, loaded });
  const quietOf = (row) => !row.unique && (tally.get(row.id)?.exceptions ?? 0) === 0;
  const out = [];
  const visit = (row, parent, depth, posinset, setsize) => {
    const isOpen = row.expandable !== false && open.has(row.id);
    out.push({
      type: 'place',
      id: row.id,
      row,
      parent,
      depth,
      open: isOpen,
      expandable: row.expandable !== false,
      quiet: quietOf(row),
      posinset,
      setsize,
    });
    if (!isOpen) return;
    const children = loaded.get(row.id) ?? [];
    const quiet = children.filter(quietOf);
    const folds = quiet.length >= QUIET_FOLD;
    const show = revealed.has(row.id);
    const kids = folds && !show ? children.filter((child) => !quietOf(child)) : children;
    const extra = [];
    if (folds) extra.push({ type: 'fold', id: `fold:${row.id}`, parent: row.id, depth: depth + 1, count: quiet.length, revealed: show });
    if (more.has(row.id)) extra.push({ type: 'more', id: `more:${row.id}`, parent: row.id, depth: depth + 1 });
    const size = kids.length + extra.length;
    kids.forEach((child, at) => visit(child, row.id, depth + 1, at + 1, size));
    extra.forEach((item, at) => out.push({ ...item, posinset: kids.length + at + 1, setsize: size }));
  };
  visit(root, null, 0, 1, 1);
  return out;
}

/** What a fold row says: `12 places inherit — show`, or `Hide the 12 places that inherit`. */
export function foldText(count, revealed) {
  const places = `${count} ${count === 1 ? 'place' : 'places'}`;
  return revealed ? `Hide the ${places} that ${count === 1 ? 'inherits' : 'inherit'}` : `${places} ${count === 1 ? 'inherits' : 'inherit'} — show`;
}

const KIND_WORDS = Object.freeze({ site: 'site', library: 'library', folder: 'folder', document: 'file' });
const KIND_ORDER = ['site', 'library', 'folder', 'document'];

/** `A`, `A and B`, `A, B and C`. */
function joinAnd(parts) {
  if (parts.length <= 1) return parts.join('');
  return `${parts.slice(0, -1).join(', ')} and ${parts[parts.length - 1]}`;
}

/**
 * The filters in one line, as the summary under the chip bar says them: `filters` is `{ who,
 * kinds, only, exceptions, more }`: `who` the name of the person or group filtered by ('' for
 * anyone), `kinds` the kinds chosen (`site`, `library`, `folder`, `document`), `only` whether only
 * exceptions are shown, `exceptions` how many are shown and `more` whether more are still to load.
 * `Showing 3 exceptions for Bob Tester`, `Showing every place: 2 have permissions of their own`.
 */
export function summariseFilters({ who = '', kinds = [], only = false, exceptions = 0, more = false } = {}) {
  const n = Number.isFinite(exceptions) ? exceptions : 0;
  const filtered = Boolean(who) || kinds.length > 0;
  if (!filtered && !only) {
    if (n === 0) return 'Showing every place: none has permissions of its own.';
    return `Showing every place: ${n} ${n === 1 ? 'has' : 'have'} permissions of ${n === 1 ? 'its' : 'their'} own.`;
  }
  const kindWords = joinAnd(KIND_ORDER.filter((kind) => kinds.includes(kind)).map((kind) => KIND_WORDS[kind]));
  const noun = `${kindWords ? `${kindWords} ` : ''}${n === 1 ? 'exception' : 'exceptions'}`;
  const forWho = who ? ` for ${who}` : '';
  if (n === 0) return `No ${kindWords ? `${kindWords} ` : ''}exceptions${forWho}.`;
  return `Showing ${more ? 'the first ' : ''}${n} ${noun}${forWho}${more ? ' (more to load)' : ''}.`;
}
