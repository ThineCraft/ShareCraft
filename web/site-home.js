// A site's home page (site.html): the library cards and "Recent in this site". The helpers here
// are pure, apart from `libraryIcon` and `ownerChip`, which build elements.

import { fileKind, fileIcon } from './files.js';
import { libraryKindLabel, pageFor, roleLine } from './common.js';
import { avatarColour, initials } from './account.js';

/** The library kinds that have an icon of their own; anything else gets the plain library icon. */
export const LIBRARY_ICON_KINDS = Object.freeze(['document', 'image', 'template', 'asset']);

/**
 * How many items a first page of a library's children holds, in words: `Empty`, `1 item`,
 * `12 items`, `100 or more items` when the page says there is more. null when unknown.
 */
export function itemCountText(count, more = false) {
  if (!Number.isInteger(count) || count < 0) return null;
  if (more) return `${count} or more items`;
  if (count === 0) return 'Empty';
  return count === 1 ? '1 item' : `${count} items`;
}

/**
 * What a library card shows for `library`: a child row of the site (`{id, name, access,
 * inherits}`) with what is known besides, `kind` (the library's kind, from `GET
 * /api/libraries/{id}`, absent when it could not be read) and `count` / `more` (its first page
 * of children, absent when not fetched). Gives `href`, `name`, `icon` (one of
 * LIBRARY_ICON_KINDS or `library`), `kind` in words, `count` in words or null, `role` (the
 * person's role, or `Limited access`) and whether it has `ownPermissions`.
 */
export function libraryCardModel(library) {
  const kind = LIBRARY_ICON_KINDS.includes(library?.kind) ? library.kind : null;
  return {
    id: String(library?.id ?? ''),
    href: pageFor('library', library?.id ?? ''),
    name: String(library?.name ?? ''),
    icon: kind ?? 'library',
    kind: kind ? libraryKindLabel(kind) : 'Library',
    count: itemCountText(library?.count, library?.more === true),
    role: library?.access ? roleLine(library.access) : 'Limited access',
    limited: !library?.access,
    ownPermissions: library?.inherits === false,
  };
}

/** Milliseconds of an RFC 3339 time, or null for none or nonsense. */
function timeOf(text) {
  if (typeof text !== 'string' || text === '') return null;
  const ms = new Date(text).getTime();
  return Number.isNaN(ms) ? null : ms;
}

/**
 * The `limit` most recently changed files across `lists`, newest first: each list is
 * `{ library: {id, name}, items }` with `items` as `GET /api/nodes/{id}/children` lists them.
 * Only files with a modified time count (folders and never-saved files do not); a file listed
 * twice counts once; ties go by name. Each comes back as `{id, name, editor, at, href, icon,
 * library, libraryName}`.
 */
export function recentAcrossLibraries(lists, limit = 8) {
  const seen = new Set();
  const files = [];
  for (const list of Array.isArray(lists) ? lists : []) {
    for (const item of Array.isArray(list?.items) ? list.items : []) {
      if (item?.kind !== 'document' || !item.id || seen.has(item.id)) continue;
      const at = timeOf(item.revised_at);
      if (at === null) continue;
      seen.add(item.id);
      files.push({
        id: item.id,
        name: String(item.name ?? ''),
        editor: item.editor ?? null,
        at: item.revised_at,
        ms: at,
        href: pageFor('document', item.id, item.editor),
        icon: fileKind(item.name, item.editor),
        library: list.library?.id ?? null,
        libraryName: String(list.library?.name ?? ''),
      });
    }
  }
  files.sort((a, b) => b.ms - a.ms || a.name.localeCompare(b.name));
  return files.slice(0, Math.max(0, limit)).map(({ ms, ...file }) => file);
}

// ---- elements ----

const SVG = 'http://www.w3.org/2000/svg';

// A library is a rounded tile in its kind's colour with a mark in the ink drawn on it, in the
// manner of the file icons of files.js.
const LIBRARY_MARKS = {
  library: [['line', 'path', { d: 'M8 8v8M12 8v8M16 9l2 7' }]],
  document: [
    ['ink', 'path', { d: 'M8.5 6.5h5l2.5 2.5v8.5h-7.5z' }],
    ['line', 'path', { d: 'M10 12h4.5M10 14.5h4.5' }],
  ],
  image: [
    ['ink', 'circle', { cx: 9.5, cy: 9.5, r: 1.7 }],
    ['ink', 'path', { d: 'M6.5 17.5l4-4.5 3 3 2-2 2 3.5z' }],
  ],
  template: [['line', 'path', { d: 'M7 7h10v3.5H7zM7 13h4.5v4H7zM14 13h3M14 15.5h3' }]],
  asset: [['ink', 'path', { d: 'M12 6.5l5.5 5.5-5.5 5.5-5.5-5.5z' }]],
};

/** The icon of a library of `kind` (LIBRARY_ICON_KINDS or `library`): a 24 by 24 SVG, hidden from screen readers. */
export function libraryIcon(kind, doc = globalThis.document) {
  const which = Object.hasOwn(LIBRARY_MARKS, kind) ? kind : 'library';
  const make = (tag, attributes) => {
    const element = doc.createElementNS(SVG, tag);
    for (const [name, value] of Object.entries(attributes)) element.setAttribute(name, String(value));
    return element;
  };
  const svg = make('svg', {
    viewBox: '0 0 24 24',
    width: 24,
    height: 24,
    class: `file-icon lib-icon l-${which}`,
    'aria-hidden': 'true',
    focusable: 'false',
  });
  svg.append(make('rect', { x: 2, y: 2, width: 20, height: 20, rx: 5, class: 'fill' }));
  for (const [role, tag, attributes] of LIBRARY_MARKS[which]) svg.append(make(tag, { ...attributes, class: role }));
  return svg;
}

/** The colours of an owner's avatar: the same name always gives the same colour. */
export function ownerColour(name) {
  return avatarColour(String(name ?? ''));
}

/** An owner of the site as a chip: a small round avatar with their initials, then their name. */
export function ownerChip(name, doc = globalThis.document) {
  const colour = ownerColour(name);
  const avatar = doc.createElement('span');
  avatar.className = 'avatar small';
  avatar.setAttribute('aria-hidden', 'true');
  avatar.textContent = initials(name);
  avatar.style.setProperty('--avatar-light-bg', colour.light.bg);
  avatar.style.setProperty('--avatar-light-ink', colour.light.ink);
  avatar.style.setProperty('--avatar-dark-bg', colour.dark.bg);
  avatar.style.setProperty('--avatar-dark-ink', colour.dark.ink);
  const label = doc.createElement('span');
  label.textContent = String(name ?? '');
  const chip = doc.createElement('li');
  chip.className = 'person-chip';
  chip.append(avatar, label);
  return chip;
}

/** The icon of a recent file, as files.js draws it. */
export const recentIcon = (file, doc) => fileIcon(file.icon, doc);
