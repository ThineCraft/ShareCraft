// The Admin centre: one layout shared by the administration pages (People, Groups, Roles, Sites,
// Templates, Sharing settings, Brand centre, Quarantine, Access requests). On wide screens a left
// column lists the sections with icons; below 800 pixels a "Section" menu (round 1's menu.js)
// takes its place. Each section is shown only to those who may use it; the current one carries
// aria-current="page" and a visible marker. Each page keeps its own address and logic.
//
// Also the search-as-you-type filter header and empty states the directory-like lists share.
//
// Pure and tested: `adminSections`, `filterRows`, `peopleSummary`, `filterStatus`, `passwordCheck`.
// `mountAdminShell` and `listFilter` need a page.

import { api, h } from './common.js';
import { setupMenu } from './menu.js';

/**
 * Every section, in the order shown. `who`: 'admin' (administrators only), 'brand'
 * (administrators and brand managers) or 'anyone' (every signed-in person: groups they can see,
 * requests for access on items they manage).
 */
export const ADMIN_SECTIONS = Object.freeze([
  { key: 'people', label: 'People', href: '/people.html', who: 'admin' },
  { key: 'groups', label: 'Groups', href: '/groups.html', who: 'anyone' },
  { key: 'roles', label: 'Roles', href: '/roles.html', who: 'admin' },
  { key: 'sites', label: 'Sites', href: '/admin-sites.html', who: 'admin' },
  { key: 'templates', label: 'Templates', href: '/templates.html', who: 'admin' },
  { key: 'settings', label: 'Sharing settings', href: '/sharing.html', who: 'admin' },
  { key: 'brand', label: 'Brand centre', href: '/brand-centre.html', who: 'brand' },
  { key: 'quarantine', label: 'Quarantine', href: '/quarantine.html', who: 'admin' },
  { key: 'requests', label: 'Access requests', href: '/requests.html', who: 'anyone' },
]);

/**
 * The sections `viewer` ({ is_admin, brand_manager }) may use, in order: administrators see all,
 * brand managers also the Brand centre, everyone Groups and Access requests.
 */
export function adminSections(viewer) {
  const admin = Boolean(viewer?.is_admin);
  const brand = admin || Boolean(viewer?.brand_manager);
  return ADMIN_SECTIONS.filter((section) => section.who === 'anyone' || (section.who === 'brand' ? brand : admin));
}

/** Lower case without accents, so "Zoë" is found by "zoe". */
function fold(text) {
  return String(text ?? '')
    .normalize('NFD')
    .replace(/\p{M}/gu, '')
    .toLocaleLowerCase();
}

/** The value of `field` (a property name, a dotted path like "hub.name", or a function) on `row`. */
function valueOf(row, field) {
  if (typeof field === 'function') return field(row);
  let value = row;
  for (const part of String(field).split('.')) value = value?.[part];
  return value;
}

/**
 * The rows of `rows` that match `query`: every word of the query (ignoring case and accents) must
 * be found in at least one of `fields` (names, dotted paths or functions). An empty query keeps
 * every row. The order is kept; the rows themselves are returned, not copies.
 */
export function filterRows(query, rows, fields) {
  const words = fold(query).split(/\s+/).filter(Boolean);
  const list = Array.isArray(rows) ? rows : [];
  if (words.length === 0) return [...list];
  return list.filter((row) => {
    const texts = fields.map((field) => fold(valueOf(row, field)));
    return words.every((word) => texts.some((text) => text.includes(word)));
  });
}

/** "1 person", "2 groups": `count` with the right one of `[one, many]`. */
export function countText(count, [one, many]) {
  return `${count} ${count === 1 ? one : many}`;
}

/**
 * The line at the top of People: "2 people, 1 administrator", with ", 1 disabled" when some
 * accounts are disabled; "No people yet." for none.
 */
export function peopleSummary(people) {
  const list = Array.isArray(people) ? people : [];
  if (list.length === 0) return 'No people yet.';
  const admins = list.filter((person) => person.is_admin).length;
  const disabled = list.filter((person) => person.disabled).length;
  const parts = [
    countText(list.length, ['person', 'people']),
    admins === 0 ? 'no administrators' : countText(admins, ['administrator', 'administrators']),
  ];
  if (disabled > 0) parts.push(`${disabled} disabled`);
  return parts.join(', ');
}

/**
 * What the filter header says: "5 groups" with no query, "2 of 5 groups" while filtering, and
 * "No groups match “x”." when nothing does.
 */
export function filterStatus(query, shown, total, [one, many]) {
  const text = String(query ?? '').trim();
  if (text === '' || shown === total) return countText(total, [one, many]);
  if (shown === 0) return `No ${many} match “${text}”.`;
  return `${shown} of ${countText(total, [one, many])}`;
}

/** The server's password rule: 10 to 256 characters. */
export const PASSWORD_MIN = 10;
export const PASSWORD_MAX = 256;

/**
 * Whether `password` meets the rule, and a plain sentence for under the field: how many more
 * characters it needs, that it is too long, or that it is long enough.
 */
export function passwordCheck(password) {
  const length = [...String(password ?? '')].length;
  if (length === 0) return { ok: false, text: `At least ${PASSWORD_MIN} characters.` };
  if (length < PASSWORD_MIN) {
    const more = PASSWORD_MIN - length;
    return { ok: false, text: `At least ${PASSWORD_MIN} characters: ${more} more to go.` };
  }
  if (length > PASSWORD_MAX) return { ok: false, text: `At most ${PASSWORD_MAX} characters: ${length - PASSWORD_MAX} too many.` };
  return { ok: true, text: 'Long enough.' };
}

// ---- wiring (needs a page) ----

const SVG = 'http://www.w3.org/2000/svg';

/** Line icons (24 by 24, drawn with the text colour), one per section. */
const ICONS = {
  people: ['M9 11a3 3 0 1 0 0-6 3 3 0 0 0 0 6z', 'M3 20c0-3.3 2.7-6 6-6s6 2.7 6 6', 'M16 5.2a3 3 0 0 1 0 5.6', 'M18 14.3c1.8.8 3 2.7 3 5.7'],
  groups: ['M12 10a2.6 2.6 0 1 0 0-5.2 2.6 2.6 0 0 0 0 5.2z', 'M6 19.5a2.6 2.6 0 1 0 0-5.2 2.6 2.6 0 0 0 0 5.2z', 'M18 19.5a2.6 2.6 0 1 0 0-5.2 2.6 2.6 0 0 0 0 5.2z', 'M10.6 9.8 7.4 14.6M13.4 9.8l3.2 4.8M8.6 17h6.8'],
  roles: ['M12 3l7 3v5c0 4.6-3 8.2-7 10-4-1.8-7-5.4-7-10V6z', 'M9 12l2 2 4-4'],
  sites: ['M4 5h16v14H4z', 'M4 9h16', 'M8 13h4M8 16h7'],
  templates: ['M4 4h16v6H4z', 'M4 14h7v6H4z', 'M14 14h6v6h-6z'],
  settings: ['M4 7h9M17 7h3', 'M15 5v4', 'M4 17h3M11 17h9', 'M9 15v4'],
  brand: ['M12 3.5s6 6.4 6 10.7a6 6 0 0 1-12 0C6 9.9 12 3.5 12 3.5z', 'M9.5 15a2.5 2.5 0 0 0 2.5 2.5'],
  quarantine: ['M6 11h12v9H6z', 'M9 11V8a3 3 0 0 1 6 0v3', 'M12 14.5v2'],
  requests: ['M4 13l2.2-8h11.6l2.2 8v6H4z', 'M4 13h5l1 2h4l1-2h5'],
};

/** The icon of section `key` as inline SVG, hidden from screen readers. */
export function sectionIcon(key) {
  const icon = document.createElementNS(SVG, 'svg');
  for (const [name, value] of Object.entries({ class: 'admin-icon', viewBox: '0 0 24 24', width: 20, height: 20, 'aria-hidden': 'true', focusable: 'false' })) {
    icon.setAttribute(name, String(value));
  }
  for (const d of ICONS[key] ?? []) {
    const path = document.createElementNS(SVG, 'path');
    path.setAttribute('d', d);
    icon.append(path);
  }
  return icon;
}

/**
 * Fills the page's `#admin-nav` for `me` with the sections they may use, `current` (a key of
 * ADMIN_SECTIONS) marked. `brandManager` (a promise or a boolean) says whether a person who is not
 * an administrator manages the brand; without it, it is asked of the server. Returns a promise of
 * the sections shown.
 */
export async function mountAdminShell({ me, current, brandManager } = {}) {
  const nav = document.getElementById('admin-nav');
  if (!nav) return [];
  const draw = (viewer) => {
    const sections = adminSections(viewer);
    // A page the person cannot use still shows where they are, so they can find their way out.
    const here = ADMIN_SECTIONS.find((section) => section.key === current);
    const shown = here && !sections.includes(here) ? ADMIN_SECTIONS.filter((s) => s === here || sections.includes(s)) : sections;
    renderNav(nav, shown, current);
    return shown;
  };
  if (me?.is_admin) return draw({ is_admin: true, brand_manager: true });
  let shown = draw({ is_admin: false, brand_manager: false });
  let manages = brandManager;
  if (manages === undefined) manages = api('GET', '/brand').then((result) => Boolean(result.ok && result.data?.can_manage));
  try {
    if (await manages) shown = draw({ is_admin: false, brand_manager: true });
  } catch {
    // without the answer the Brand centre link stays out; the page itself still works
  }
  return shown;
}

function renderNav(nav, sections, current) {
  const title = h('p', { id: 'admin-title', class: 'admin-title' }, 'Admin centre');
  nav.setAttribute('aria-labelledby', 'admin-title');
  const link = (section, menu) => {
    const attributes = { href: section.href, class: 'admin-link' };
    if (menu) attributes.role = 'menuitem';
    if (section.key === current) attributes['aria-current'] = 'page';
    return h('a', attributes, sectionIcon(section.key), h('span', { class: 'admin-label' }, section.label));
  };
  const list = h('ul', { class: 'admin-nav-list' }, ...sections.map((section) => h('li', {}, link(section, false))));

  // Narrow screens: one button naming the current section, opening the same links as a menu.
  const here = sections.find((section) => section.key === current);
  const button = h(
    'button',
    { type: 'button', id: 'admin-section-button', class: 'plain admin-section-button' },
    h('span', { class: 'admin-section-label' }, 'Section:'),
    ' ',
    h('span', { class: 'admin-section-name' }, here?.label ?? 'Choose'),
    h('span', { class: 'admin-caret', 'aria-hidden': 'true' }, '▾'),
  );
  const menu = h(
    'ul',
    { id: 'admin-section-menu', class: 'menu end admin-section-menu', 'aria-label': 'Admin centre sections' },
    ...sections.map((section) => h('li', { role: 'none' }, link(section, true))),
  );
  const wrap = h('div', { class: 'menu-wrap admin-section-wrap' }, button, menu);
  nav.replaceChildren(title, list, wrap);
  setupMenu(button, menu);
}

/**
 * A search-as-you-type header for a list: a labelled search field and a count ("2 of 5 groups"),
 * and an empty state after `list` (`empty` when there are none at all; "No groups match …" with
 * Clear the search when the search hides them all). `set(entries)` gives the entries as
 * `{ row, element }`; matching elements are put back in `list` in order, so alternating shading
 * stays right. `fields` are as for filterRows, applied to each entry's row. Escape in the field
 * clears it. Returns `{ element, input, set, refresh, clear }`; put `element` before `list`.
 */
export function listFilter({ id, label, nouns, fields, list, empty }) {
  const input = h('input', { type: 'search', id, autocomplete: 'off', spellcheck: 'false', enterkeyhint: 'search' });
  const count = h('p', { class: 'filter-count', role: 'status' });
  const element = h(
    'div',
    { class: 'list-filter', role: 'search' },
    h('div', { class: 'field' }, h('label', { for: id }, label), input),
    count,
  );
  const emptyText = h('p', {});
  const clearButton = h('button', { type: 'button', class: 'plain' }, 'Clear the search');
  const emptyBox = h('div', { class: 'empty-state', hidden: true }, emptyText, clearButton);
  list.after(emptyBox);
  let entries = [];
  const refresh = () => {
    const query = input.value;
    const shown = filterRows(query, entries, fields.map((field) => (entry) => valueOf(entry.row, field)));
    list.replaceChildren(...shown.map((entry) => entry.element));
    count.textContent = entries.length === 0 ? '' : filterStatus(query, shown.length, entries.length, nouns);
    // With nothing found the empty state below says so; the count is then only heard, not seen twice.
    count.classList.toggle('visually-hidden', entries.length > 0 && shown.length === 0);
    const none = entries.length === 0;
    emptyBox.hidden = shown.length > 0;
    emptyBox.classList.toggle('searching', !none);
    emptyText.textContent = none ? empty : filterStatus(query, 0, entries.length, nouns);
    clearButton.hidden = none;
    list.hidden = shown.length === 0;
    element.hidden = none;
  };
  const clear = () => {
    input.value = '';
    refresh();
    input.focus();
  };
  input.addEventListener('input', refresh);
  input.addEventListener('keydown', (event) => {
    if (event.key === 'Escape' && input.value !== '') {
      event.preventDefault();
      clear();
    }
  });
  clearButton.addEventListener('click', clear);
  return {
    element,
    input,
    refresh,
    clear,
    set(next) {
      entries = Array.isArray(next) ? next : [];
      refresh();
    },
  };
}
