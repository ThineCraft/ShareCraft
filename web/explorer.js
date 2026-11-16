// The inheritance explorer as a permission map: a tree from one place down, with connector lines,
// each place marked as inheriting (quiet) or having permissions of its own (an accent pill with a
// key and how many entries differ from the place above). Inheriting branches with no exception
// below fold into one row. Choosing a place opens a side panel (a bottom sheet on phones): who has
// access there, and how that differs from the place above, in words. Reads only places with their
// own access (never the files between them) for "only exceptions" and the filters; the full tree
// opens folders on demand. Shows only what the person manages access on; the server decides.

import {
  anyoneLinksText,
  api,
  can,
  exceptionTree,
  explorerQuery,
  explorerRows,
  filterChoices,
  formatDate,
  h,
  inheritAgainMessage,
  messageFor,
  noSourceText,
  offerRequest,
  pageFor,
  previewView,
  principalParts,
  queryId,
  requireMe,
  seg,
  setupTopbar,
  showCrumbs,
  showMessage,
  siteChrome,
  toggleOpen,
  withChosen,
} from './common.js';
import { avatarColour, initials } from './account.js';
import { fileIcon, fileKind } from './files.js';
import { accessSummary, comboboxFilter, permsSentence, sortEntries } from './share.js';
import { dragToClose, phoneShellNow } from './sheet.js';
import { treeKey } from './tree.js';
import {
  accessDiff,
  diffBadge,
  diffWords,
  exceptionCounts,
  foldText,
  mapRows,
  summariseFilters,
} from './permission-map.js';

const $ = (id) => document.getElementById(id);
const error = $('error');
const MISSING = 'This place does not exist, or you do not have access to it.';
const CHILD_PAGE = 200;
const BREAK_PAGE = 200;
const KINDS = { site: 'Site', library: 'Library', folder: 'Folder', document: 'File' };

const me = await requireMe();
setupTopbar(me, error);
const rootId = queryId(location.search, 'root');
if (!rootId) fail(MISSING);
const here = await showCrumbs($('crumbs'), rootId, { linkLast: false });
if (!here) {
  offerRequest(error, rootId);
  fail(MISSING);
}
await siteChrome(here.path[0].id, { currentId: rootId });
const rootNode = here.path[here.path.length - 1];
if (!can(here.access, 'manage_access')) fail('Only people who manage access here can use the explorer.');
$('title').textContent = `Inheritance explorer: ${rootNode.name}`;
document.title = `Inheritance explorer: ${rootNode.name} — ShareCraft`;

// ---- state ----

let filter = { principal: '', kinds: [] };
let whoLabel = ''; // the name of the person or group filtered by
let only = false;
let version = 0; // the latest reload; answers to older ones are dropped

// The lazy tree (nothing filtered): rows by parent, the cursor for more children, what is open.
let lazy = { root: null, loaded: new Map(), more: new Map(), counts: new Map(), open: new Set() };
// The tree of breaks (only exceptions, or a filter): the breaks loaded, the cursor for more.
let broken = { items: [], next: null, tree: null, open: new Set(), seen: new Set() };
// Every break, unfiltered, loaded as far as the map needed: for the rows' differences and details.
let everything = { items: [], next: undefined };
// How each break differs from the place above it, by the break's id (null: nothing above to compare).
const diffs = new Map();
// The assignments of places above breaks, by id, as promises (each asked for once).
const aboveEntries = new Map();
let revealed = new Set(); // places whose folded inheriting children are shown
let rows = []; // the rows on screen, from mapRows
let selectedId = null;
let focusId = null;

const filtering = () => filter.principal !== '' || filter.kinds.length > 0;
const useBreaks = () => only || filtering();

/** The break the server described for `id`, from the filtered or the unfiltered list. */
const breakOf = (id) => broken.tree?.breaks.get(id) ?? everything.items.find((item) => item.node.id === id) ?? null;

/**
 * Whether `row` has permissions of its own. With a filter, a place on the path to a match is in
 * the tree without matching itself; it may still be a break (the site, say), which the unfiltered
 * list knows.
 */
const ownOf = (row) => row.unique || (useBreaks() && everything.items.some((item) => item.node.id === row.id));

const treeItem = (id) => [...$('tree').querySelectorAll('li[role=treeitem]')].find((li) => li.dataset.id === id) ?? null;
const SVG = 'http://www.w3.org/2000/svg';
const keyIcon = () => strokeIcon(['M10.5 13.5a4.5 4.5 0 1 1 3-3L21 18v3h-3v-2h-2v-2h-2z', 'M7.5 8.5h.01'], 'pmap-key');
let asked = 0;
let sheetOpener = null;
let stopDrag = null;
const SIGNS = { added: '+', removed: '−', changed: '→' };
const SR_WORD = { added: 'Added: ', removed: 'Removed: ', changed: 'Changed: ' };
let restoring = null;

// ---- the controls ----

$('content').hidden = false;
const accessResult = await api('GET', `/nodes/${seg(rootId)}/access`);
const rootUnique = accessResult.ok ? !accessResult.data.inherits : true;

fillPlaces();
// What the "who" filter is built from: the root's assignments, the groups listed and the members
// of those the person manages; every break loaded adds the people and groups named on it.
const known = { assignments: accessResult.ok ? accessResult.data.assignments : [], groups: [], members: [] };
await loadGroups();
$('place').addEventListener('change', () => location.assign(`/explorer.html?root=${seg($('place').value)}`));
for (const chip of document.querySelectorAll('.pmap-kinds [data-kind]')) {
  chip.addEventListener('click', () => {
    chip.setAttribute('aria-pressed', String(chip.getAttribute('aria-pressed') !== 'true'));
    changeFilter();
  });
}
$('only').addEventListener('click', () => {
  if (filtering()) return; // a filter always shows only exceptions
  only = !only;
  changeFilter();
});
$('more-breaks').addEventListener('click', async () => {
  await loadBreaks();
  await diffsFor(broken.items);
  render();
});
$('restore-cancel').addEventListener('click', () => $('restore-dialog').close());
$('detail-close').addEventListener('click', () => clearSelection(true));

function note(text, isError) {
  error.classList.toggle('ok', !isError);
  showMessage(error, text);
}

function fillPlaces() {
  const places = here.path.filter((step) => can(step.access, 'manage_access'));
  $('place').replaceChildren(
    ...places.map((step) => {
      const option = h('option', { value: step.id }, `${KINDS[step.kind] ?? step.kind}: ${step.name}`);
      option.selected = step.id === rootId;
      return option;
    }),
  );
}

/** Loads the groups the person can see, and the members of those they manage. */
async function loadGroups() {
  const listed = await api('GET', '/groups');
  if (!listed.ok) return;
  known.groups = listed.data;
  for (const group of listed.data) {
    if (!group.can_manage) continue;
    const detail = await api('GET', `/groups/${seg(group.id)}`);
    if (detail.ok) known.members = known.members.concat(detail.data.members);
  }
}

async function changeFilter() {
  filter = {
    principal: filter.principal,
    kinds: [...document.querySelectorAll('.pmap-kinds [data-kind][aria-pressed=true]')].map((chip) => chip.dataset.kind),
  };
  await reload();
}

// ---- the Person or group box (a combobox, as on the Share screen) ----

function setupWho() {
  const find = $('who');
  const listbox = $('who-options');
  let options = [];
  let active = -1;

  /** Every person and group the person can name: on this place, groups, members, on breaks. */
  const candidates = () => {
    const { people, groups } = withChosen(
      filterChoices({ ...known, breaks: broken.items.concat(everything.items) }),
      filter.principal,
      whoLabel,
    );
    return [
      ...groups.map(([id, name]) => ({ kind: 'group', id, name, detail: 'group' })),
      ...people.map(([id, name]) => ({ kind: 'user', id, name, detail: 'person' })),
    ];
  };
  const openList = (show) => {
    listbox.hidden = !show;
    find.setAttribute('aria-expanded', String(show));
    if (!show) {
      active = -1;
      find.removeAttribute('aria-activedescendant');
    }
  };
  const markActive = () => {
    for (const [index, row] of [...listbox.querySelectorAll('[role=option]')].entries()) {
      row.setAttribute('aria-selected', String(index === active));
      if (index === active) row.scrollIntoView?.({ block: 'nearest' });
    }
    if (active >= 0 && !listbox.hidden) find.setAttribute('aria-activedescendant', `who-option-${active}`);
    else find.removeAttribute('aria-activedescendant');
  };
  const refresh = (show = !listbox.hidden) => {
    options = comboboxFilter(find.value, candidates(), { limit: 10 }).filter((option) => option.kind !== 'email');
    if (active >= options.length) active = options.length - 1;
    listbox.replaceChildren(
      ...(options.length
        ? options.map((option, index) => {
            const row = h(
              'li',
              { role: 'option', id: `who-option-${index}`, 'aria-selected': String(index === active), class: 'combo-option' },
              avatar(option.name, option.id, { group: option.kind === 'group', small: true }),
              h('span', { class: 'combo-text' }, h('span', { class: 'combo-name' }, option.name), h('small', {}, option.detail)),
            );
            row.addEventListener('mousedown', (event) => event.preventDefault());
            row.addEventListener('click', () => choose(option));
            return row;
          })
        : [h('li', { class: 'combo-empty', role: 'presentation' }, 'No person or group matches.')]),
    );
    openList(show && document.activeElement === find);
    markActive();
  };
  const choose = (option) => {
    filter.principal = `${option.kind}:${option.id}`;
    whoLabel = option.name;
    find.value = '';
    openList(false);
    showChip();
    $('who-clear')?.focus();
    reload();
  };
  const showChip = () => {
    const chip = $('who-chip');
    const chosen = filter.principal !== '';
    chip.hidden = !chosen;
    $('who-combo').hidden = chosen;
    if (!chosen) return chip.replaceChildren();
    const [kind, id] = filter.principal.split(':');
    const clear = h('button', { type: 'button', id: 'who-clear', class: 'chip-remove', 'aria-label': `Stop showing only ${whoLabel}` }, '×');
    clear.addEventListener('click', () => {
      filter.principal = '';
      whoLabel = '';
      showChip();
      find.focus();
      reload();
    });
    chip.replaceChildren(avatar(whoLabel, id, { group: kind === 'group', small: true }), h('span', {}, `For ${whoLabel}`), clear);
  };
  find.addEventListener('input', () => {
    active = -1;
    refresh(true);
  });
  find.addEventListener('click', () => refresh(true));
  find.addEventListener('blur', () => openList(false));
  find.addEventListener('keydown', (event) => {
    const open = !listbox.hidden;
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault();
      if (!open) {
        refresh(true);
        active = options.length ? (event.key === 'ArrowDown' ? 0 : options.length - 1) : -1;
      } else if (options.length) {
        const step = event.key === 'ArrowDown' ? 1 : -1;
        active = active === -1 ? (step === 1 ? 0 : options.length - 1) : (active + step + options.length) % options.length;
      }
      markActive();
    } else if (event.key === 'Enter') {
      event.preventDefault();
      if (open && active >= 0 && options[active]) choose(options[active]);
      else if (options.length === 1 && find.value.trim()) choose(options[0]);
    } else if (event.key === 'Escape' && open) {
      event.preventDefault();
      openList(false);
    }
  });
}

/** A round avatar with initials, coloured from `key`; a group's is a rounded square. */
function avatar(name, key, { group = false, small = false } = {}) {
  const colour = avatarColour(key);
  const element = h('span', { class: `avatar${small ? ' small' : ''}${group ? ' group' : ''}`, 'aria-hidden': 'true' }, initials(name));
  element.style.setProperty('--avatar-light-bg', colour.light.bg);
  element.style.setProperty('--avatar-light-ink', colour.light.ink);
  element.style.setProperty('--avatar-dark-bg', colour.dark.bg);
  element.style.setProperty('--avatar-dark-ink', colour.dark.ink);
  return element;
}

// ---- loading ----

async function reload() {
  const mine = ++version;
  note('', false);
  const filtered = filtering();
  $('only').setAttribute('aria-pressed', String(only || filtered));
  if (filtered) $('only').setAttribute('aria-disabled', 'true');
  else $('only').removeAttribute('aria-disabled');
  $('filter-note').hidden = !filtered;
  $('export').href = `/api/explorer/${seg(rootId)}/export.csv${explorerQuery({ principal: filter.principal, kinds: filter.kinds })}`;
  $('tree').setAttribute('aria-busy', 'true');
  if (everything.next === undefined) await loadEverything();
  if (useBreaks()) {
    broken = { items: [], next: null, tree: null, open: new Set(), seen: new Set() };
    await loadBreaks();
    await diffsFor(broken.items);
  } else {
    lazy = { root: null, loaded: new Map(), more: new Map(), counts: new Map(), open: new Set([rootId]) };
    await startLazy();
  }
  await diffsFor(everything.items);
  if (mine !== version) return;
  $('tree').removeAttribute('aria-busy');
  render();
  if (selectedId && rows.some((row) => row.id === selectedId)) showDetail(selectedId, { quiet: true });
  else clearSelection(false);
}

/** Loads the next page of every break, unfiltered: what the rows' differences come from. */
async function loadEverything() {
  if (everything.next === null) return;
  const result = await api('GET', `/explorer/${seg(rootId)}${explorerQuery({ limit: 500, after: everything.next })}`);
  if (!result.ok) {
    everything.next = null;
    return note(messageFor(result.error), true);
  }
  everything.items = everything.items.concat(result.data.items);
  everything.next = result.data.next;
}

/** Loads the next page of breaks for the current filter, and rebuilds the tree from them. */
async function loadBreaks() {
  const query = explorerQuery({
    principal: filter.principal,
    kinds: filter.kinds,
    limit: BREAK_PAGE,
    after: broken.next,
  });
  const result = await api('GET', `/explorer/${seg(rootId)}${query}`);
  if (!result.ok) return note(messageFor(result.error), true);
  broken.items = broken.items.concat(result.data.items);
  broken.next = result.data.next;
  broken.tree = exceptionTree(broken.items);
  const fresh = broken.tree.root ? [broken.tree.root.id, ...broken.tree.loaded.keys()] : [];
  for (const id of fresh) {
    if (!broken.seen.has(id)) {
      broken.seen.add(id);
      broken.open.add(id);
    }
  }
}

async function startLazy() {
  const counts = await loadCounts(rootId);
  const total = [...counts.values()].reduce((sum, n) => sum + n, 0);
  lazy.root = {
    id: rootId,
    kind: rootNode.kind,
    name: rootNode.name,
    access: here.access,
    unique: rootUnique,
    count: total + (rootUnique ? 1 : 0),
    expandable: rootNode.kind !== 'document',
  };
  await loadChildren(lazy.root, counts);
}

/** The break counts of the children of `parent`, as a Map of id to count. */
async function loadCounts(parent) {
  const query = parent === rootId ? '' : `?parent=${seg(parent)}`;
  const result = await api('GET', `/explorer/${seg(rootId)}/counts${query}`);
  if (!result.ok) {
    note(messageFor(result.error), true);
    return new Map();
  }
  return new Map(result.data.map((item) => [item.id, item.count]));
}

/** Loads a page of the children of `row` (the first page with their counts). */
async function loadChildren(row, counts) {
  const after = lazy.more.get(row.id);
  const query = `limit=${CHILD_PAGE}${after ? `&after=${encodeURIComponent(after)}` : ''}`;
  const result = await api('GET', `/nodes/${seg(row.id)}/children?${query}`);
  if (!result.ok) return note(messageFor(result.error), true);
  const known = counts ?? (after ? lazy.counts.get(row.id) : await loadCounts(row.id));
  lazy.counts.set(row.id, known);
  const children = explorerRows(result.data.items, known);
  lazy.loaded.set(row.id, (lazy.loaded.get(row.id) ?? []).concat(children));
  lazy.more.set(row.id, result.data.next);
}

/** The assignments of the place `id` above a break: from a break loaded, or asked for once. */
function entriesAbove(id) {
  const loaded = breakOf(id);
  if (loaded) return Promise.resolve(loaded.assignments);
  if (!aboveEntries.has(id)) {
    aboveEntries.set(
      id,
      api('GET', `/nodes/${seg(id)}/access`).then((result) => (result.ok ? result.data.assignments : null)),
    );
  }
  return aboveEntries.get(id);
}

/** Works out how each break in `items` differs from the place it would inherit from. */
async function diffsFor(items) {
  await Promise.all(
    items
      .filter((item) => !diffs.has(item.node.id))
      .map(async (item) => {
        const above = item.inherited_from ? await entriesAbove(item.inherited_from.id) : null;
        diffs.set(item.node.id, above ? accessDiff(above, item.assignments) : null);
      }),
  );
}

// ---- the map ----

function currentTree() {
  if (useBreaks()) return { root: broken.tree?.root ?? null, loaded: broken.tree?.loaded ?? new Map(), open: broken.open, more: new Set() };
  const more = new Set([...lazy.more].filter(([, next]) => next).map(([id]) => id));
  return { root: lazy.root, loaded: lazy.loaded, open: lazy.open, more };
}

async function toggle(id) {
  if (useBreaks()) {
    broken.open = toggleOpen(broken.open, id);
  } else {
    lazy.open = toggleOpen(lazy.open, id);
    const row = rows.find((item) => item.id === id)?.row;
    if (row && lazy.open.has(id) && !lazy.loaded.has(id)) {
      const li = treeItem(id);
      li?.setAttribute('aria-busy', 'true');
      await loadChildren(row);
    }
  }
  render();
}

function render() {
  const breaksMode = useBreaks();
  const tree = currentTree();
  const counts = exceptionCounts(tree);
  rows = mapRows({ ...tree, revealed, counts });
  $('more-breaks').hidden = !(breaksMode && broken.next);
  $('tree-empty').hidden = !(breaksMode && !tree.root);
  $('summary').textContent = summariseFilters({
    who: whoLabel,
    kinds: filter.kinds,
    only: breaksMode,
    exceptions: breaksMode ? broken.items.length : (counts.get(tree.root?.id)?.exceptions ?? 0),
    more: breaksMode && Boolean(broken.next),
  });
  const list = $('tree');
  const hadFocus = list.contains(document.activeElement);
  if (!rows.some((row) => row.id === focusId)) focusId = rows.some((row) => row.id === selectedId) ? selectedId : (rows[0]?.id ?? null);
  // The rows nest: each open place holds its children in a group, so the connector lines can be
  // drawn by the stylesheet from the nesting alone.
  const top = document.createDocumentFragment();
  const groups = [top];
  rows.forEach((entry, index) => {
    const li = entry.type === 'place' ? placeItem(entry, counts, index) : extraItem(entry, index);
    groups[entry.depth].append(li);
    if (entry.type === 'place' && entry.open) {
      const group = h('ul', { role: 'group', class: 'pmap-group' });
      li.append(group);
      groups[entry.depth + 1] = group;
    }
  });
  list.replaceChildren(top);
  if (hadFocus) treeItem(focusId)?.focus();
}

function placeItem(entry, counts, index) {
  const { row } = entry;
  const own = ownOf(row);
  const found = own ? breakOf(row.id) : null;
  const diff = own ? diffs.get(row.id) : undefined;
  const below = counts.get(row.id)?.below ?? 0;
  const marks = [];
  if (own) {
    marks.push(h('span', { class: 'pmap-own' }, keyIcon(), 'Own permissions'));
    const badge = diffBadge(diff);
    if (badge) marks.push(diffBadgeView(diff));
    else if (diff) marks.push(h('span', { class: 'pmap-same' }, 'same as above'));
  } else {
    marks.push(h('span', { class: 'pmap-inherits' }, 'Inherits'));
  }
  if (below > 0) marks.push(h('span', { class: 'pmap-below' }, `${below} below`));
  const anyone = anyoneLinksText(found?.anyone_links ?? 0);
  if (anyone) marks.push(h('span', { class: 'mark warn' }, anyone));
  const twisty = h('span', { class: 'twisty', 'aria-hidden': 'true' }, entry.expandable ? (entry.open ? '▾' : '▸') : '');
  twisty.addEventListener('click', (event) => {
    event.stopPropagation();
    if (entry.expandable) {
      focusId = row.id;
      toggle(row.id);
    }
  });
  const line = h(
    'div',
    { class: ['pmap-row', index % 2 ? 'band' : '', own ? 'own' : '', entry.quiet ? 'quiet' : ''].filter(Boolean).join(' ') },
    twisty,
    h('span', { class: 'pmap-icon', 'aria-hidden': 'true' }, kindIcon(row)),
    h('span', { class: 'pmap-name' }, row.name),
    h('span', { class: 'pmap-marks' }, ...marks),
  );
  const words = [
    row.name,
    KINDS[row.kind] ?? row.kind,
    own ? 'own permissions' : 'inherits',
    diff ? diffWords(diff) || 'same as the place above' : '',
    below > 0 ? `${below} ${below === 1 ? 'exception' : 'exceptions'} below` : '',
    anyone,
  ].filter(Boolean);
  const li = h(
    'li',
    {
      role: 'treeitem',
      class: 'pmap-item',
      'data-id': row.id,
      'aria-label': words.join(', '),
      'aria-level': entry.depth + 1,
      'aria-posinset': entry.posinset,
      'aria-setsize': entry.setsize,
      'aria-expanded': entry.expandable ? String(entry.open) : null,
      'aria-selected': String(row.id === selectedId),
      tabindex: row.id === focusId ? '0' : '-1',
    },
    line,
  );
  return li;
}

function extraItem(entry, index) {
  const text = entry.type === 'fold' ? foldText(entry.count, entry.revealed) : 'Show more places';
  const li = h(
    'li',
    {
      role: 'treeitem',
      class: `pmap-item pmap-extra ${entry.type}`,
      'data-id': entry.id,
      'aria-level': entry.depth + 1,
      'aria-posinset': entry.posinset,
      'aria-setsize': entry.setsize,
      'aria-selected': 'false',
      tabindex: entry.id === focusId ? '0' : '-1',
    },
    h(
      'div',
      { class: `pmap-row extra${index % 2 ? ' band' : ''}` },
      h('span', { class: 'twisty', 'aria-hidden': 'true' }, entry.type === 'fold' ? (entry.revealed ? '−' : '+') : '…'),
      h('span', { class: 'pmap-name' }, text),
    ),
  );
  return li;
}

function diffBadgeView(diff) {
  const part = (n, sign, kind) => (n ? h('span', { class: `pmap-${kind}` }, `${sign}${n}`) : null);
  return h(
    'span',
    { class: 'pmap-diff', title: `Compared with the place above: ${diffWords(diff)}` },
    part(diff.added.length, '+', 'added'),
    part(diff.removed.length, '−', 'removed'),
    part(diff.changed.length, '→', 'changed'),
  );
}

/** Acts on a row: a place is shown in the panel, a fold opens or closes, "more" loads more. */
async function activate(id) {
  const entry = rows.find((row) => row.id === id);
  if (!entry) return;
  focusId = id;
  if (entry.type === 'fold') {
    revealed = toggleOpen(revealed, entry.parent);
    render();
    $('summary').textContent = `${$('summary').textContent} ${entry.revealed ? 'Inheriting places folded.' : 'Inheriting places shown.'}`;
    return;
  }
  if (entry.type === 'more') {
    const parent = rows.find((row) => row.id === entry.parent)?.row;
    if (parent) {
      await loadChildren(parent);
      render();
    }
    return;
  }
  selectedId = id;
  for (const li of $('tree').querySelectorAll('li[role=treeitem]')) li.setAttribute('aria-selected', String(li.dataset.id === id));
  showDetail(id);
}

function setupTree() {
  const list = $('tree');
  list.addEventListener('click', (event) => {
    const li = event.target.closest?.('li[role=treeitem]');
    if (!li || !list.contains(li)) return;
    focusId = li.dataset.id;
    for (const other of list.querySelectorAll('li[role=treeitem]')) other.tabIndex = other === li ? 0 : -1;
    li.focus();
    activate(li.dataset.id);
  });
  list.addEventListener('keydown', (event) => {
    const li = event.target.closest?.('li[role=treeitem]');
    if (!li) return;
    const key = event.key === ' ' ? 'Enter' : event.key;
    const flat = rows.map((row) => ({
      id: row.id,
      parent: row.parent,
      expandable: row.type === 'place' && row.expandable,
      expanded: row.type === 'place' && row.open,
    }));
    const action = treeKey(flat, li.dataset.id, key);
    if (!action) return;
    event.preventDefault();
    if (action.focus) {
      focusId = action.focus;
      for (const other of list.querySelectorAll('li[role=treeitem]')) other.tabIndex = other.dataset.id === focusId ? 0 : -1;
      treeItem(focusId)?.focus();
    } else if (action.expand || action.collapse) {
      focusId = action.expand ?? action.collapse;
      toggle(focusId);
    } else if (action.open) {
      activate(action.open);
    }
  });
}

// ---- icons ----

function strokeIcon(paths, className) {
  const svg = document.createElementNS(SVG, 'svg');
  for (const [name, value] of Object.entries({ viewBox: '0 0 24 24', width: 18, height: 18, class: className, 'aria-hidden': 'true', focusable: 'false' })) {
    svg.setAttribute(name, String(value));
  }
  for (const d of paths) {
    const path = document.createElementNS(SVG, 'path');
    path.setAttribute('d', d);
    svg.append(path);
  }
  return svg;
}

function kindIcon(row) {
  if (row.kind === 'site') return strokeIcon(['M3 11 12 4l9 7', 'M5 9.5V20h5v-6h4v6h5V9.5'], 'pmap-svg');
  if (row.kind === 'library') return strokeIcon(['M4 4h4v16H4z', 'M10 4h4v16h-4z', 'm16 5.5 3.5-1 3 14.5-3.5 1z'], 'pmap-svg');
  const icon = fileIcon(fileKind(row.name, null, row.kind));
  icon.setAttribute('width', '18');
  icon.setAttribute('height', '18');
  return icon;
}

// ---- the panel: who has access to one place, and how it differs from above ----

function setupSheet() {
  const sheet = $('detail-sheet');
  $('sheet-close').addEventListener('click', () => sheet.close());
  sheet.addEventListener('click', (event) => {
    if (event.target === sheet) sheet.close();
  });
  sheet.addEventListener('close', () => {
    stopDrag?.();
    stopDrag = null;
    if (sheetOpener?.isConnected) sheetOpener.focus();
    else treeItem(selectedId)?.focus();
    sheetOpener = null;
  });
}

function clearSelection(refocus) {
  const had = selectedId;
  selectedId = null;
  for (const li of $('tree').querySelectorAll('li[role=treeitem]')) li.setAttribute('aria-selected', 'false');
  $('detail-heading').textContent = 'Choose a place';
  $('detail-close').hidden = true;
  $('detail-body').replaceChildren(
    h('p', { class: 'meta' }, 'Choose a place on the map to see who has access there, and how it differs from the place above.'),
  );
  if (refocus && had) treeItem(had)?.focus();
}

/** Shows the place `id` in the side panel, or in a bottom sheet on a phone. */
async function showDetail(id, { quiet = false } = {}) {
  const entry = rows.find((row) => row.id === id && row.type === 'place');
  if (!entry) return;
  const mine = ++asked;
  const phone = phoneShellNow();
  const heading = phone ? $('sheet-heading') : $('detail-heading');
  const body = phone ? $('sheet-body') : $('detail-body');
  heading.textContent = entry.row.name;
  body.replaceChildren(h('p', { class: 'meta', role: 'status' }, 'Loading who has access…'));
  if (phone) openSheet();
  else $('detail-close').hidden = false;
  const view = await placeView(entry.row);
  if (mine !== asked) return;
  body.replaceChildren(view);
  if (!phone && !quiet) {
    $('detail').scrollIntoView?.({ block: 'nearest' });
  }
}

function openSheet() {
  const sheet = $('detail-sheet');
  if (!sheet.open) {
    sheetOpener = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    sheet.showModal();
    $('sheet-close').focus();
    stopDrag = dragToClose(sheet, () => sheet.close(), { handle: sheet.querySelector('.sheet-handle') });
  }
}

/** Everything the panel says about `row`: a break's details, or where an inheriting place's access comes from. */
async function placeView(row) {
  if (ownOf(row)) {
    let found = breakOf(row.id);
    while (!found && everything.next) {
      await loadEverything();
      found = breakOf(row.id);
    }
    if (!found) return h('p', { class: 'msg' }, 'That place no longer has access of its own.');
    await diffsFor([found]);
    return breakView(found, diffs.get(row.id));
  }
  const result = await api('GET', `/nodes/${seg(row.id)}/access`);
  if (!result.ok) return h('p', { class: 'msg' }, messageFor(result.error));
  return inheritsView(row, result.data);
}

function stamp(item) {
  return item ? `${item.user.name}${item.at ? `, ${formatDate(item.at)}` : ''}` : 'Not recorded';
}

function kindLine(kind, extra) {
  return h('p', { class: 'pmap-kind' }, `${KINDS[kind] ?? kind}`, extra ? ` · ${extra}` : '');
}

function pathLine(found) {
  const chain = [...found.path, found.node];
  return h(
    'ol',
    { class: 'crumbs-line', 'aria-label': 'Where it is' },
    ...chain.map((step, at) =>
      h(
        'li',
        at === chain.length - 1 ? { 'aria-current': 'page' } : {},
        step.access ? h('a', { href: pageFor(step.kind, step.id) }, step.name) : h('span', {}, step.name),
      ),
    ),
  );
}

/** One person, group or link with access: avatar, name, what they are, role and what it lets them do. */
function entryRow({ principal, role, perms }) {
  const { name, note: sub, hidden } = principalParts(principal, role);
  const isLink = principal.kind === 'link';
  const pic = isLink
    ? h('span', { class: 'avatar small link-mark', 'aria-hidden': 'true' })
    : hidden
      ? h('span', { class: 'avatar small group hidden-group', 'aria-hidden': 'true' }, '?')
      : avatar(name, principal.id, { group: principal.kind === 'group', small: true });
  const kind = isLink ? 'link' : principal.kind === 'group' ? 'group' : 'person';
  return h(
    'li',
    { class: 'share-entry pmap-entry' },
    pic,
    h(
      'span',
      { class: 'grow' },
      h('span', { class: 'entry-name' }, name),
      h('small', {}, sub ? `${kind} · ${sub}` : kind),
      h('small', { class: 'entry-perms' }, `${permsSentence(perms)}.`),
    ),
    isLink ? null : h('span', { class: 'mark' }, role?.name ?? ''),
  );
}

function peopleView(entries) {
  return [
    h('h3', {}, 'Who has access here'),
    h('p', { class: 'pmap-sentence' }, accessSummary(entries)),
    entries.length
      ? h('ul', { class: 'list share-list pmap-people' }, ...sortEntries(entries).map(entryRow))
      : h('p', { class: 'meta' }, 'Nobody is listed on this place.'),
  ];
}

function diffView(found, diff) {
  const source = found.inherited_from;
  if (!source || diff === undefined) {
    return [h('h3', {}, 'Compared with the place above'), h('p', { class: 'meta' }, noSourceText(found.kind))];
  }
  const above = h('a', { href: `/access.html?id=${seg(source.id)}` }, source.name);
  if (!diff) {
    return [
      h('h3', {}, 'Compared with the place above'),
      h('p', { class: 'meta' }, 'The access of ', above, ' could not be read, so the differences are not shown.'),
    ];
  }
  if (!diff.items.length) {
    return [
      h('h3', {}, 'Compared with the place above'),
      h('p', { class: 'pmap-same-note' }, 'The same people and roles as ', above, '. Inheriting again would change nobody’s access here.'),
    ];
  }
  return [
    h('h3', {}, 'Compared with the place above'),
    h('p', { class: 'meta' }, 'Above is ', above, ` — ${diffWords(diff)}.`),
    h(
      'ul',
      { class: 'pmap-diff-list' },
      ...diff.items.map((item) =>
        h(
          'li',
          { class: `pmap-diff-item ${item.change}` },
          h('span', { class: 'pmap-sign', 'aria-hidden': 'true' }, SIGNS[item.change]),
          h('span', {}, h('span', { class: 'sr' }, SR_WORD[item.change]), item.text),
        ),
      ),
    ),
  ];
}

function breakView(found, diff) {
  const restore = h('button', { type: 'button', class: 'secondary' }, 'Inherit again…');
  restore.addEventListener('click', () => openRestore(found.node));
  return h(
    'div',
    { class: 'pmap-detail' },
    kindLine(found.kind),
    pathLine(found),
    h('p', {}, h('span', { class: 'pmap-own big' }, keyIcon(), 'Own permissions')),
    ...diffView(found, diff),
    ...peopleView(found.assignments),
    h(
      'dl',
      { class: 'facts' },
      h('dt', {}, 'Made its own by'),
      h('dd', {}, stamp(found.broken_by)),
      h('dt', {}, 'Last changed by'),
      h('dd', {}, stamp(found.changed_by)),
      h('dt', {}, 'Places below with their own access'),
      h('dd', {}, String(found.breaks_below)),
      h('dt', {}, 'Links for anyone'),
      h('dd', {}, anyoneLinksText(found.anyone_links) || 'None'),
    ),
    h(
      'p',
      { class: 'toolbar pmap-actions' },
      h('a', { class: 'button', href: `/access.html?id=${seg(found.node.id)}` }, 'Open Share screen'),
      found.inherited_from ? restore : null,
    ),
  );
}

function inheritsView(row, access) {
  const source = access.source;
  const below = exceptionCounts(currentTree()).get(row.id)?.below ?? 0;
  return h(
    'div',
    { class: 'pmap-detail' },
    kindLine(row.kind),
    h(
      'p',
      {},
      h('span', { class: 'pmap-inherits big' }, 'Inherits'),
      ' Access comes from ',
      source ? h('a', { href: `/access.html?id=${seg(source.id)}` }, source.name) : 'the place above',
      '. It is the same as there.',
    ),
    ...peopleView(access.assignments),
    below > 0 ? h('p', { class: 'meta' }, `${below} ${below === 1 ? 'place below has' : 'places below have'} access of their own.`) : null,
    h('p', { class: 'toolbar pmap-actions' }, h('a', { class: 'button', href: `/access.html?id=${seg(row.id)}` }, 'Open Share screen')),
  );
}

// ---- restoring ----

async function openRestore(node) {
  note('', false);
  const result = await api('GET', `/nodes/${seg(node.id)}/inheritance/preview`);
  if (!result.ok) return note(inheritAgainMessage(result.error), true);
  restoring = node;
  $('restore-title').textContent = `Inherit again on “${node.name}”?`;
  $('restore-text').replaceChildren(previewView(result.data));
  showMessage($('restore-error'), '');
  $('restore-dialog').showModal();
}

$('restore-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  const node = restoring;
  const result = await api('POST', `/nodes/${seg(node.id)}/inheritance`, { inherit: true });
  if (!result.ok) return showMessage($('restore-error'), inheritAgainMessage(result.error));
  $('restore-dialog').close();
  if ($('detail-sheet').open) $('detail-sheet').close();
  everything = { items: [], next: undefined };
  diffs.clear();
  aboveEntries.clear();
  await reload();
  note(`“${node.name}” inherits its access again.`, false);
});

// ---- start ----

// Last, so every declaration above is initialised before the first reload uses it.
setupWho();
setupTree();
setupSheet();
await reload();

function fail(text) {
  showMessage(error, text);
  throw new Error(text);
}
