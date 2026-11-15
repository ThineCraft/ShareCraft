// The command palette: Ctrl+K (Cmd+K on a Mac), or "/" outside a text field, opens one search
// field from any signed-in page. It jumps to recently opened files, sites and libraries, and
// runs the actions the person may take where they are (New site, Brand centre, People, Change
// the look, Site settings, Recycle bin, and what the page itself offers, such as Upload files).
//
// The matching, ranking, rights and key logic are pure (`fuzzyScore`, `rankResults`,
// `paletteGroups`, `paletteShortcut`, `moveActive`, `contextIdFrom`) and tested without a page;
// `setupPalette` wires them to the page. Everything is fetched on the first open and kept for
// the life of the page.

import { api, can, h, kindLabel, messageFor, pageFor, seg, siteMark } from './common.js';
import { fileIcon, fileKind } from './files.js';
import { addShortcut, isTextField, openShortcutSheet, setupShortcutSheet } from './shortcuts.js';
import { readRecent } from './start.js';

export { isTextField };

// ---- matching ----

/** One character folded for matching: lower case, accents dropped ("É" matches "e"). */
const fold = (ch) => ch.normalize('NFD').replace(/\p{M}/gu, '').toLocaleLowerCase();
const isWordChar = (ch) => /[\p{L}\p{N}]/u.test(ch ?? '');

/** Whether character `i` of `chars` starts a word: the first, after a space or mark, or a capital after a small letter. */
function wordStart(chars, i) {
  if (i === 0) return true;
  if (!isWordChar(chars[i - 1])) return true;
  return /\p{Ll}/u.test(chars[i - 1]) && /\p{Lu}/u.test(chars[i]);
}

/**
 * How well `query` matches `text`: its letters must appear in `text` in order (case and accents
 * ignored, spaces in the query ignored). Returns null for no match, otherwise `{ score,
 * positions }`, where `positions` are the matched characters' indexes in `Array.from(text)`
 * (for highlighting) and a higher `score` is a better match: letters at word starts and letters
 * in a row count more, skipped letters before the first match and longer texts count a little
 * less. "bt" matches "Budget.xlsx" at B and t. An empty query matches everything with score 0.
 */
export function fuzzyScore(query, text) {
  const q = Array.from(String(query ?? '')).filter((ch) => ch.trim() !== '').map(fold);
  const chars = Array.from(String(text ?? ''));
  if (q.length === 0) return { score: 0, positions: [] };
  const t = chars.map(fold);
  const m = q.length;
  const n = t.length;
  if (m > n) return null;
  const NONE = -Infinity;
  const bonus = (j) => 1 + (wordStart(chars, j) ? (j === 0 ? 10 : 8) : 0);
  // score[i][j]: the best score with query letter i matched at text letter j; from[i][j]: where letter i-1 went.
  const score = [];
  const from = [];
  for (let i = 0; i < m; i += 1) {
    score.push(new Array(n).fill(NONE));
    from.push(new Array(n).fill(-1));
    let best = NONE; // the best score[i-1][k] for k < j - 1
    let bestAt = -1;
    for (let j = i; j < n; j += 1) {
      if (i > 0 && j >= 2 && score[i - 1][j - 2] > best) {
        best = score[i - 1][j - 2];
        bestAt = j - 2;
      }
      if (t[j] !== q[i]) continue;
      if (i === 0) {
        score[0][j] = bonus(j) - Math.min(j, 10) * 0.2;
        continue;
      }
      const together = score[i - 1][j - 1] === NONE ? NONE : score[i - 1][j - 1] + 6;
      const apart = best === NONE ? NONE : best - 3;
      if (together === NONE && apart === NONE) continue;
      if (together >= apart) {
        score[i][j] = bonus(j) + together;
        from[i][j] = j - 1;
      } else {
        score[i][j] = bonus(j) + apart;
        from[i][j] = bestAt;
      }
    }
  }
  let end = -1;
  for (let j = 0; j < n; j += 1) if (score[m - 1][j] > (end < 0 ? NONE : score[m - 1][end])) end = j;
  if (end < 0) return null;
  const positions = new Array(m);
  for (let i = m - 1, j = end; i >= 0; i -= 1) {
    positions[i] = j;
    j = from[i][j];
  }
  return { score: score[m - 1][end] - (n - m) * 0.01, positions };
}

/**
 * The groups of results for `query`: each group of `groups` (`{ id, heading, items, limit }`,
 * items being `{ label, keywords?, boost? }`) keeps the items that match, best first (ties keep
 * their order), at most `limit` of them (8 by default), each with its `score` and the
 * `positions` of its label to highlight. An item may match by its `keywords` instead of its
 * label, scoring half and highlighting nothing. Without a query every item is kept in order.
 * Groups left empty are dropped.
 */
export function rankResults(groups, query) {
  const searching = String(query ?? '').trim() !== '';
  const ranked = [];
  for (const group of groups) {
    const limit = group.limit ?? 8;
    let items = group.items.map((item, order) => {
      if (!searching) return { ...item, score: 0, positions: [], order };
      const byLabel = fuzzyScore(query, item.label);
      if (byLabel) return { ...item, score: byLabel.score + (item.boost ?? 0), positions: byLabel.positions, order };
      const byWords = item.keywords ? fuzzyScore(query, item.keywords) : null;
      if (byWords) return { ...item, score: byWords.score / 2 + (item.boost ?? 0), positions: [], order };
      return null;
    });
    items = items.filter(Boolean);
    if (searching) items.sort((a, b) => b.score - a.score || a.order - b.order);
    items = items.slice(0, limit).map(({ order, ...item }) => item);
    if (items.length > 0) ranked.push({ id: group.id, heading: group.heading, items });
  }
  return ranked;
}

// ---- what is offered ----

/**
 * The palette's groups, before matching: Recently opened (`recent`, as start.js reads it),
 * Sites (`sites`, as `GET /api/sites` lists them), Libraries (`libraries`: `{ id, name,
 * siteId, siteName }`; those of the current site `siteId` first) and Actions. The actions follow
 * the person's rights (`me`, `brandManager`, and their access to the current site as `sites`
 * lists it) and what the page offers (`pageActions`: `{ key, label, detail?, keywords?, run }`,
 * first, and replacing a general action with the same key).
 */
export function paletteGroups({ me = {}, recent = [], sites = [], libraries = [], siteId = null, brandManager = false, pageActions = [] } = {}) {
  const current = siteId ? sites.find((site) => site.id === siteId) : null;
  const manages = can(current?.access, 'manage_site');
  const general = [];
  const add = (key, label, href, extra = {}) => general.push({ key, kind: 'action', label, href, ...extra });
  if (me.is_admin) add('new-site', 'New site', '/#new-site', { keywords: 'create add make site' });
  if (current && manages) add('look', 'Change the look', `/site-look.html?id=${seg(current.id)}`, { detail: current.name, keywords: 'theme colours colors brand header logo' });
  if (current && (manages || me.is_admin)) add('site-settings', 'Site settings', `/site-settings.html?id=${seg(current.id)}`, { detail: current.name, keywords: 'quota hub description archive' });
  if (current && manages) add('bin', 'Recycle bin', `/bin.html?site=${seg(current.id)}`, { detail: current.name, keywords: 'deleted restore trash' });
  if (me.is_admin || brandManager) add('brand', 'Brand centre', '/brand-centre.html', { keywords: 'themes colours colors images logos' });
  if (me.is_admin) add('people', 'People', '/people.html', { keywords: 'users accounts administrators admin centre add a person' });
  add('start', 'Start page', '/', { keywords: 'home sites' });
  const keys = new Set(pageActions.map((action) => action.key));
  const actions = [...pageActions.map((action) => ({ kind: 'action', ...action })), ...general.filter((action) => !keys.has(action.key))];

  const libs = [...libraries].sort((a, b) => Number(b.siteId === siteId) - Number(a.siteId === siteId));
  return [
    {
      id: 'recent',
      heading: 'Recently opened',
      limit: 5,
      items: recent.map((file) => ({
        key: `file:${file.id}`,
        kind: 'file',
        label: file.name,
        detail: file.site || '',
        href: pageFor('document', file.id, file.editor),
        icon: fileKind(file.name, file.editor),
      })),
    },
    {
      id: 'sites',
      heading: 'Sites',
      limit: 6,
      items: sites.map((site) => ({
        key: `site:${site.id}`,
        kind: 'site',
        label: String(site.name ?? ''),
        detail: site.id === siteId ? `${kindLabel(site.kind)} · You are here` : kindLabel(site.kind),
        href: pageFor('site', site.id),
        site,
        boost: site.id === siteId ? 1 : 0,
      })),
    },
    {
      id: 'libraries',
      heading: 'Libraries',
      limit: 6,
      items: libs.map((library) => ({
        key: `library:${library.id}`,
        kind: 'library',
        label: String(library.name ?? ''),
        detail: library.siteName ?? '',
        // The site's name before or after the library's: "projects documents" and "documents projects" both find it.
        keywords: `${library.siteName ?? ''} ${library.name ?? ''} ${library.siteName ?? ''}`,
        href: pageFor('library', library.id),
        icon: 'folder',
        boost: library.siteId === siteId ? 1 : 0,
      })),
    },
    { id: 'actions', heading: 'Actions', limit: 8, items: actions },
  ];
}

// ---- keys ----

/**
 * Whether a key press opens the palette: Ctrl+K or Cmd+K anywhere but inside rich text being
 * edited (where editors use it for links), and "/" with no Ctrl, Cmd or Alt, outside any text
 * field or rich text. Returns 'open' or null.
 */
export function paletteShortcut(event) {
  if (!event || event.defaultPrevented || event.isComposing) return null;
  const key = String(event.key ?? '');
  if ((event.ctrlKey || event.metaKey) && !event.altKey && key.toLowerCase() === 'k') {
    return event.target?.isContentEditable ? null : 'open';
  }
  if (key === '/' && !event.ctrlKey && !event.metaKey && !event.altKey) {
    return isTextField(event.target) ? null : 'open';
  }
  return null;
}

/**
 * The option a key moves to from `index` among `count` options: Down and Up wrap, Home and End
 * go to the ends (only with Ctrl, as plain Home and End move in the field); null when the key
 * does not move.
 */
export function moveActive(key, index, count, { ctrlKey = false } = {}) {
  if (count <= 0) return null;
  const at = index >= 0 && index < count ? index : -1;
  if (key === 'ArrowDown' || key === 'Down') return at === -1 ? 0 : (at + 1) % count;
  if (key === 'ArrowUp' || key === 'Up') return at === -1 ? count - 1 : (at - 1 + count) % count;
  if (key === 'PageDown' || (ctrlKey && key === 'End')) return count - 1;
  if (key === 'PageUp' || (ctrlKey && key === 'Home')) return 0;
  return null;
}

/** The sentence announced after the results change. */
export function countText(count, loading = false) {
  const found = count === 0 ? 'No results' : count === 1 ? '1 result' : `${count} results`;
  return loading ? `${found} so far. Still loading sites and libraries.` : `${found}.`;
}

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;

/**
 * The id of the place a page shows, from its address (`{ pathname, search, hash }`): `?id=`,
 * `?site=`, `?doc=` or `?root=`, or the `#id` of a text document; '' for none.
 */
export function contextIdFrom({ pathname = '', search = '', hash = '' } = {}) {
  const params = new URLSearchParams(search);
  for (const name of ['id', 'site', 'doc', 'root']) {
    const value = (params.get(name) ?? '').toLowerCase();
    if (UUID.test(value)) return value;
  }
  if (pathname.endsWith('/doc.html')) {
    const value = decodeURIComponent(hash.replace(/^#/, '')).toLowerCase();
    if (UUID.test(value)) return value;
  }
  return '';
}

/** The parts of `text` split for highlighting: `[{ text, hit }]`, runs of matched characters together. */
export function highlightParts(text, positions = []) {
  const chars = Array.from(String(text ?? ''));
  const hits = new Set(positions);
  const parts = [];
  chars.forEach((ch, i) => {
    const hit = hits.has(i);
    const last = parts[parts.length - 1];
    if (last && last.hit === hit) last.text += ch;
    else parts.push({ text: ch, hit });
  });
  return parts;
}

// ---- wiring (needs a page) ----

const providers = [];

/**
 * Lets a page offer its own actions: `provide()` is asked each time the palette opens and
 * returns `[{ key, label, detail?, keywords?, run }]`. `run` is called after the palette closes.
 */
export function addPaletteActions(provide) {
  providers.push(provide);
}

let installed = false;

/** Wires the palette into the page for the signed-in person `me`: its shortcut and the account bar's button. */
export function setupPalette(me) {
  if (installed || typeof document === 'undefined') return;
  installed = true;
  const palette = createPalette(me);
  const mac = /Mac|iPhone|iPad/.test(globalThis.navigator?.platform ?? '');
  const bar = document.querySelector('.topbar');
  if (bar) {
    const button = h(
      'button',
      { type: 'button', class: 'plain palette-button', 'aria-label': 'Search or jump to', 'aria-keyshortcuts': mac ? 'Meta+K /' : 'Control+K /', 'aria-haspopup': 'dialog' },
      h('span', { class: 'palette-glass', 'aria-hidden': 'true' }),
      h('span', { class: 'palette-button-text' }, 'Search or jump to…'),
      h('kbd', { 'aria-hidden': 'true' }, mac ? '⌘ K' : 'Ctrl K'),
    );
    button.addEventListener('click', () => palette.open());
    const spacer = bar.querySelector('.spacer');
    if (spacer) spacer.after(button);
    else bar.append(button);
  }
  addShortcut({
    keys: ['Mod+K', '/'],
    label: 'Search or jump to (the command palette)',
    scope: 'global',
    inFields: true,
    inMenus: true,
    inDialogs: true,
    // The shortcut again closes it ("/" is typed into its field as usual); over another open
    // dialog it waits, as that one has the person's attention.
    when: (event, context) => paletteShortcut(event) === 'open' && (palette.isOpen() ? event.key !== '/' : !context.dialog),
    run: () => (palette.isOpen() ? palette.close() : palette.open()),
  });
  // "?" lists the shortcuts of the page; the palette offers the same list.
  setupShortcutSheet();
  addPaletteActions(() => [
    { key: 'shortcuts', label: 'Keyboard shortcuts', detail: 'Press ? on any page', keywords: 'keys help hotkeys keyboard', run: () => openShortcutSheet() },
  ]);
}

function createPalette(me) {
  let dialog = null;
  let field;
  let list;
  let live;
  let note;
  let returnTo = null;
  let options = []; // [{ element, item }] in order
  let active = -1;
  let announce = 0;

  // What has been fetched, kept for the life of the page.
  const data = { sites: null, libraries: [], siteId: '', brandManager: false, loading: false, problem: '' };
  let fetched = null;

  function build() {
    field = h('input', {
      type: 'text',
      id: 'palette-field',
      role: 'combobox',
      'aria-autocomplete': 'list',
      'aria-expanded': 'true',
      'aria-controls': 'palette-list',
      autocomplete: 'off',
      spellcheck: 'false',
      placeholder: 'Search files, sites, libraries and actions',
      maxlength: '200',
    });
    list = h('div', { id: 'palette-list', role: 'listbox', 'aria-label': 'Results', class: 'palette-list' });
    live = h('p', { class: 'visually-hidden', role: 'status', 'aria-live': 'polite' });
    note = h('p', { class: 'palette-note meta' });
    dialog = h(
      'dialog',
      { class: 'palette', 'aria-labelledby': 'palette-title' },
      h('h2', { id: 'palette-title', class: 'visually-hidden' }, 'Search or jump to'),
      h('label', { for: 'palette-field', class: 'visually-hidden' }, 'Search or jump to'),
      h('div', { class: 'palette-search' }, h('span', { class: 'palette-glass', 'aria-hidden': 'true' }), field),
      list,
      note,
      live,
      h('p', { class: 'palette-keys meta', 'aria-hidden': 'true' }, h('kbd', {}, '↑'), h('kbd', {}, '↓'), ' to move · ', h('kbd', {}, 'Enter'), ' to open · ', h('kbd', {}, 'Esc'), ' to close'),
    );
    document.body.append(dialog);
    field.addEventListener('input', () => render());
    field.addEventListener('keydown', onKey);
    dialog.addEventListener('close', () => {
      const back = returnTo;
      returnTo = null;
      if (back?.isConnected && typeof back.focus === 'function') back.focus();
    });
    // A press on the backdrop (the dialog itself, outside its box) closes it.
    dialog.addEventListener('click', (event) => {
      if (event.target === dialog) close();
    });
  }

  function onKey(event) {
    const next = moveActive(event.key, active, options.length, { ctrlKey: event.ctrlKey });
    if (next !== null) {
      event.preventDefault();
      setActive(next, true);
      return;
    }
    if (event.key === 'Enter') {
      event.preventDefault();
      if (active >= 0) choose(options[active].item, event);
    }
  }

  function setActive(index, scroll) {
    options[active]?.element.setAttribute('aria-selected', 'false');
    active = index;
    const option = options[active];
    if (!option) {
      field.removeAttribute('aria-activedescendant');
      return;
    }
    option.element.setAttribute('aria-selected', 'true');
    field.setAttribute('aria-activedescendant', option.element.id);
    if (scroll) option.element.scrollIntoView({ block: 'nearest' });
  }

  function choose(item, event) {
    if (item.run) {
      // The action takes focus (its own dialog, a file picker). When its dialog closes and leaves
      // focus nowhere (inside the closed dialog, or on the page body), it goes back where it was.
      const back = returnTo;
      returnTo = null;
      dialog.close();
      item.run();
      const opened = [...document.querySelectorAll('dialog[open]')].find((other) => other !== dialog);
      const restore = (closed) => {
        const now = document.activeElement;
        if (back?.isConnected && (!now || now === document.body || closed?.contains(now))) back.focus();
      };
      if (opened) opened.addEventListener('close', () => restore(opened), { once: true });
      else restore(null);
      return;
    }
    if (item.href) {
      if (event?.ctrlKey || event?.metaKey) {
        window.open(item.href, '_blank', 'noopener');
        return;
      }
      location.assign(item.href);
    }
  }

  function close() {
    if (dialog?.open) dialog.close();
  }

  function pageActions() {
    const actions = [];
    for (const provide of providers) {
      try {
        actions.push(...(provide() ?? []));
      } catch {
        // a page whose actions cannot be read right now offers none
      }
    }
    return actions;
  }

  function render() {
    const query = field.value;
    const groups = rankResults(
      paletteGroups({
        me,
        recent: readRecent(me.id),
        sites: data.sites ?? [],
        libraries: data.libraries,
        siteId: data.siteId,
        brandManager: data.brandManager,
        pageActions: pageActions(),
      }),
      query,
    );
    const keep = options[active]?.item.key;
    options = [];
    active = -1;
    list.replaceChildren(
      ...groups.map((group) => {
        const headingId = `palette-group-${group.id}`;
        return h(
          'div',
          { role: 'group', 'aria-labelledby': headingId, class: 'palette-group' },
          h('p', { id: headingId, class: 'palette-heading', role: 'presentation' }, group.heading),
          ...group.items.map((item) => optionFor(item)),
        );
      }),
    );
    const again = keep ? options.findIndex((option) => option.item.key === keep) : -1;
    setActive(query.trim() === '' ? (again >= 0 ? again : 0) : options.length > 0 ? 0 : -1, false);
    field.setAttribute('aria-expanded', options.length > 0 ? 'true' : 'false');
    note.textContent = data.problem || (data.loading ? 'Loading sites and libraries…' : options.length === 0 ? 'Nothing matches. Try fewer letters.' : '');
    // The count is announced once typing pauses, not on every letter.
    const mine = ++announce;
    const count = options.length;
    const loading = data.loading;
    setTimeout(() => {
      if (mine === announce && dialog.open) live.textContent = countText(count, loading);
    }, 350);
  }

  function optionFor(item) {
    const id = `palette-option-${options.length}`;
    const icon =
      item.kind === 'site'
        ? siteMark(item.site)
        : item.icon
          ? fileIcon(item.icon)
          : h('span', { class: 'palette-action-mark', 'aria-hidden': 'true' }, '›');
    const name = h(
      'span',
      { class: 'palette-name' },
      ...highlightParts(item.label, item.positions).map((part) => (part.hit ? h('mark', {}, part.text) : part.text)),
    );
    const element = h(
      'div',
      { id, role: 'option', 'aria-selected': 'false', class: `palette-option is-${item.kind}` },
      icon,
      h('span', { class: 'palette-text' }, name, item.detail ? h('small', {}, item.detail) : null),
    );
    const index = options.length;
    element.addEventListener('mousemove', () => {
      if (active !== index) setActive(index, false);
    });
    // The field keeps focus while an option is pressed.
    element.addEventListener('mousedown', (event) => event.preventDefault());
    element.addEventListener('click', (event) => choose(item, event));
    options.push({ element, item });
    return element;
  }

  /** Fetches the sites, their libraries, the current place and brand rights, once. */
  function fetchAll() {
    if (fetched) return fetched;
    data.loading = true;
    fetched = (async () => {
      const contextId = contextIdFrom(location);
      const [sites, place, brand] = await Promise.all([
        api('GET', '/sites'),
        contextId ? api('GET', `/nodes/${seg(contextId)}`) : null,
        me.is_admin ? null : api('GET', '/brand'),
      ]);
      data.brandManager = Boolean(brand?.ok && brand.data?.can_manage);
      if (place?.ok && Array.isArray(place.data?.path) && place.data.path[0]?.kind === 'site') data.siteId = place.data.path[0].id;
      if (!sites.ok) {
        data.sites = [];
        data.problem = `Sites could not be loaded. ${messageFor(sites.error)}`;
      } else {
        data.sites = sites.data;
      }
      if (dialog.open) render();
      // Libraries: the current site's first, then the others, a few sites at a time.
      const order = [...data.sites].sort((a, b) => Number(b.id === data.siteId) - Number(a.id === data.siteId));
      for (let at = 0; at < order.length; at += 6) {
        const batch = order.slice(at, at + 6);
        const answers = await Promise.all(batch.map((site) => api('GET', `/nodes/${seg(site.id)}/children?limit=500`)));
        answers.forEach((answer, i) => {
          if (!answer.ok) return;
          for (const node of answer.data.items ?? []) {
            if (node.kind === 'library') data.libraries.push({ id: node.id, name: node.name, siteId: batch[i].id, siteName: batch[i].name });
          }
        });
        if (dialog.open) render();
      }
      data.loading = false;
      if (dialog.open) render();
    })();
    return fetched;
  }

  function open() {
    if (!dialog) build();
    if (dialog.open) return;
    returnTo = document.activeElement;
    field.value = '';
    live.textContent = '';
    dialog.showModal();
    field.focus();
    render();
    fetchAll();
  }

  return { open, close, isOpen: () => Boolean(dialog?.open) };
}
