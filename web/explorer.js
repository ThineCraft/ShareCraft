// The inheritance explorer: a tree from one place down, each place marked as inheriting or
// having access of its own, with the paths to those that do. Reads only places with their own
// access (never the files between them) for "only exceptions" and the filters; the tree opens
// folders on demand. Shows only what the person manages access on; the server decides.

import {
  api,
  breaksBelow,
  can,
  exceptionTree,
  explorerQuery,
  explorerRows,
  filterChoices,
  formatTime,
  h,
  inheritAgainMessage,
  messageFor,
  pageFor,
  permsText,
  previewView,
  principalParts,
  queryId,
  noSourceText,
  requireMe,
  seg,
  setupTopbar,
  showCrumbs,
  showMessage,
  toggleOpen,
  visibleTree,
} from './common.js';

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
if (!here) fail(MISSING);
const rootNode = here.path[here.path.length - 1];
if (!can(here.access, 'manage_access')) fail('Only people who manage access here can use the explorer.');
$('title').textContent = `Inheritance explorer: ${rootNode.name}`;
document.title = `Inheritance explorer: ${rootNode.name} — ShareCraft`;

// ---- state ----

let filter = { principal: '', kinds: [] };
let only = false;
let version = 0; // the latest reload; answers to older ones are dropped

// The lazy tree (nothing filtered): rows by parent, the cursor for more children, what is open.
let lazy = { root: null, loaded: new Map(), more: new Map(), counts: new Map(), open: new Set() };
// The tree of breaks (only exceptions, or a filter): the breaks loaded, the cursor for more.
let broken = { items: [], next: null, tree: null, open: new Set(), seen: new Set() };
// Every break, unfiltered, loaded as far as a detail needed: to find the one for a row.
const everything = { items: [], next: undefined };

const filtering = () => filter.principal !== '' || filter.kinds.length > 0;
const useBreaks = () => only || filtering();

// ---- the controls ----

$('content').hidden = false;
const accessResult = await api('GET', `/nodes/${seg(rootId)}/access`);
const rootUnique = accessResult.ok ? !accessResult.data.inherits : true;

fillPlaces();
// What the "who" filter is built from: the root's assignments, the groups listed and the members
// of those the person manages; every break loaded adds the people and groups named on it.
const known = { assignments: accessResult.ok ? accessResult.data.assignments : [], groups: [], members: [] };
await loadGroups();
fillPrincipals();
$('filters').addEventListener('submit', (event) => event.preventDefault());
$('place').addEventListener('change', () => location.assign(`/explorer.html?root=${seg($('place').value)}`));
$('principal').addEventListener('change', changeFilter);
for (const box of document.querySelectorAll('input[name=kind]')) box.addEventListener('change', changeFilter);
$('only').addEventListener('change', changeFilter);
$('more-breaks').addEventListener('click', async () => {
  await loadBreaks();
  render();
});
$('restore-cancel').addEventListener('click', () => $('restore-dialog').close());
await reload();

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

/**
 * Offers the people and groups the person can name: those on this place, groups, their members,
 * and everyone named on a break loaded so far. Keeps the current choice.
 */
function fillPrincipals() {
  const { people, groups } = filterChoices({ ...known, breaks: broken.items.concat(everything.items) });
  const chosen = $('principal').value;
  const group = (label, entries, kind) =>
    entries.length ? h('optgroup', { label }, ...entries.map(([id, name]) => h('option', { value: `${kind}:${id}` }, name))) : null;
  $('principal').replaceChildren(
    h('option', { value: '' }, 'Anyone'),
    group('People', people, 'user'),
    group('Groups', groups, 'group'),
  );
  $('principal').value = chosen;
}

async function changeFilter() {
  filter = {
    principal: $('principal').value,
    kinds: [...document.querySelectorAll('input[name=kind]:checked')].map((box) => box.value),
  };
  only = $('only').checked;
  await reload();
}

// ---- loading ----

async function reload() {
  const mine = ++version;
  note('', false);
  $('only').checked = only || filtering();
  $('only').disabled = filtering();
  $('filter-note').hidden = !filtering();
  $('export').href = `/api/explorer/${seg(rootId)}/export.csv${explorerQuery({ principal: filter.principal, kinds: filter.kinds })}`;
  $('detail').hidden = true;
  if (useBreaks()) {
    broken = { items: [], next: null, tree: null, open: new Set(), seen: new Set() };
    await loadBreaks();
  } else {
    lazy = { root: null, loaded: new Map(), more: new Map(), counts: new Map(), open: new Set([rootId]) };
    await startLazy();
  }
  if (mine === version) render();
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
  fillPrincipals();
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
  const rows = explorerRows(result.data.items, known);
  lazy.loaded.set(row.id, (lazy.loaded.get(row.id) ?? []).concat(rows));
  lazy.more.set(row.id, result.data.next);
}

// ---- the tree ----

async function toggle(row) {
  if (useBreaks()) {
    broken.open = toggleOpen(broken.open, row.id);
  } else {
    lazy.open = toggleOpen(lazy.open, row.id);
    if (lazy.open.has(row.id) && !lazy.loaded.has(row.id)) await loadChildren(row);
  }
  render();
}

function render() {
  const breaksMode = useBreaks();
  const root = breaksMode ? broken.tree?.root : lazy.root;
  $('more-breaks').hidden = !(breaksMode && broken.next);
  $('tree-empty').hidden = !(breaksMode && !root);
  if (!root) return $('tree').replaceChildren();
  const loaded = breaksMode ? broken.tree.loaded : lazy.loaded;
  const open = breaksMode ? broken.open : lazy.open;
  $('tree').replaceChildren(...visibleTree(root, loaded, open).map((entry) => treeRow(entry, breaksMode)));
}

function treeRow({ row, depth, open }, breaksMode) {
  const parts = [];
  if (row.expandable) {
    const button = h(
      'button',
      {
        type: 'button',
        class: 'plain twist',
        'aria-expanded': open ? 'true' : 'false',
        'aria-label': `${open ? 'Hide' : 'Show'} the contents of ${row.name}`,
      },
      open ? '▾' : '▸',
    );
    button.addEventListener('click', () => toggle(row));
    parts.push(button);
  } else {
    parts.push(h('span', { class: 'twist' }));
  }
  parts.push(
    h('span', { class: 'grow' }, row.name, ' ', h('small', {}, KINDS[row.kind] ?? row.kind)),
    h('small', { class: row.unique ? 'mark own' : 'mark' }, row.unique ? 'Own access' : 'Inherited'),
  );
  const below = breaksBelow(row);
  if (below > 0) parts.push(h('small', { class: 'mark' }, `${below} below`));
  if (row.unique) {
    const details = h('button', { type: 'button', class: 'plain', 'aria-label': `Details of ${row.name}` }, 'Details');
    details.addEventListener('click', () => showDetail(row));
    parts.push(details);
  }
  if (!breaksMode && open && lazy.more.get(row.id)) {
    const more = h('button', { type: 'button', class: 'plain' }, 'Show more');
    more.addEventListener('click', async () => {
      await loadChildren(row);
      render();
    });
    parts.push(more);
  }
  return h('li', { class: 'tree-row', style: `--depth:${depth}` }, ...parts);
}

// ---- the details of one break ----

/** The break the server described for `id`, loading more of the unfiltered list as needed. */
async function findBreak(id) {
  const shown = broken.tree?.breaks.get(id);
  if (useBreaks() && shown) return shown;
  const seen = () => everything.items.find((item) => item.node.id === id);
  while (!seen() && everything.next !== null) {
    const query = explorerQuery({ limit: 500, after: everything.next });
    const result = await api('GET', `/explorer/${seg(rootId)}${query}`);
    if (!result.ok) {
      note(messageFor(result.error), true);
      return null;
    }
    everything.items = everything.items.concat(result.data.items);
    everything.next = result.data.next;
  }
  fillPrincipals();
  return seen() ?? null;
}

async function showDetail(row) {
  const found = await findBreak(row.id);
  if (!found) return note('That place no longer has access of its own.', true);
  $('detail-heading').textContent = found.node.name;
  $('detail-body').replaceChildren(detailView(found));
  $('detail').hidden = false;
  $('detail').scrollIntoView?.({ block: 'nearest' });
}

function stamp(item) {
  return item ? `${item.user.name}${item.at ? `, ${formatTime(item.at)}` : ''}` : 'Not recorded';
}

function detailView(found) {
  const chain = [...found.path, found.node];
  const path = h(
    'ol',
    { class: 'crumbs-line' },
    ...chain.map((step, at) =>
      h(
        'li',
        at === chain.length - 1 ? { 'aria-current': 'page' } : {},
        step.access ? h('a', { href: pageFor(step.kind, step.id) }, step.name) : h('span', {}, step.name),
      ),
    ),
  );
  const table = h(
    'div',
    { class: 'scroll' },
    h(
      'table',
      { class: 'banded' },
      h('thead', {}, h('tr', {}, h('th', {}, 'Who'), h('th', {}, 'Role'), h('th', {}, 'Can'))),
      h(
        'tbody',
        {},
        ...found.assignments.map(({ principal, role, perms }) => {
          const { name, note: sub } = principalParts(principal);
          return h(
            'tr',
            {},
            h('td', {}, name, sub ? h('br') : null, sub ? h('small', {}, sub) : null),
            h('td', {}, role.name),
            h('td', {}, permsText(perms)),
          );
        }),
      ),
    ),
  );
  const source = found.inherited_from
    ? h('a', { href: `/access.html?id=${seg(found.inherited_from.id)}` }, found.inherited_from.name)
    : noSourceText(found.kind);
  const restore = h('button', { type: 'button', class: 'plain' }, 'Restore preview');
  restore.addEventListener('click', () => openRestore(found.node));
  return h(
    'div',
    {},
    h('p', { class: 'kind' }, `${KINDS[found.kind] ?? found.kind} · path`),
    path,
    h('h3', {}, 'Who has access, and why'),
    found.assignments.length ? table : h('p', { class: 'meta' }, 'Nobody is listed on this place.'),
    h(
      'dl',
      { class: 'facts' },
      h('dt', {}, 'Would inherit from'),
      h('dd', {}, source),
      h('dt', {}, 'Made its own by'),
      h('dd', {}, stamp(found.broken_by)),
      h('dt', {}, 'Last changed by'),
      h('dd', {}, stamp(found.changed_by)),
      h('dt', {}, 'Places below with their own access'),
      h('dd', {}, String(found.breaks_below)),
    ),
    h(
      'p',
      { class: 'toolbar' },
      h('a', { class: 'button plain', href: `/access.html?id=${seg(found.node.id)}` }, 'Access page'),
      found.inherited_from ? restore : null,
    ),
  );
}

// ---- restoring ----

let restoring = null;

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
  everything.items = [];
  everything.next = undefined;
  await reload();
  note(`“${node.name}” inherits its access again.`, false);
});

function fail(text) {
  showMessage(error, text);
  throw new Error(text);
}
