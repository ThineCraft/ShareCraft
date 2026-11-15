// The library's folder tree, dragging items to move them, and cut and paste.
//
// Pure parts (tested in tree.test.mjs): the tree model (`treeModel`, `insertChildren`,
// `setExpanded`, `ancestry`, `flattenTree`), the keys of the WAI-ARIA tree pattern over the
// flattened tree (`treeKey`), whether a drop is allowed (`canDrop`), which items a drag carries
// (`dragList`), telling an item drag from a desktop file drag (`isItemDrag`, `isFileDrag`), the
// cut keys (`clipboardKey`) and the words of the cut bar (`cutText`). The page part,
// `mountFolderTree`, draws the tree into a `ul[role=tree]` and loads a folder's children only
// when it is expanded.

import { api, can, h, messageFor, pageFor, seg } from './common.js';
import { fileIcon } from './files.js';
import { isTextField } from './palette.js';

/** The type an item drag carries, so that a drop can tell it from files dragged in from the desktop. */
export const ITEM_TYPE = 'application/x-sharecraft-items';

const KINDS = ['library', 'folder'];
const itemsText = (n) => `${n} ${n === 1 ? 'item' : 'items'}`;
const byName = (a, b) => String(a.name).localeCompare(String(b.name), undefined, { numeric: true, sensitivity: 'base' });

// ---- the model ----

/**
 * A tree holding only `root` (`{ id, name, kind, access }`, the library). Every node is
 * `{ id, name, kind, access, parent, children, expanded, loading }`; `children` is null until
 * loaded, then the ids of its folders by name.
 */
export function treeModel(root) {
  return {
    rootId: root.id,
    nodes: { [root.id]: node(root, null) },
  };
}

function node(entry, parent, old = null) {
  return {
    id: entry.id,
    name: entry.name,
    kind: entry.kind,
    access: entry.access ?? null,
    parent,
    children: old?.children ?? null,
    expanded: old?.expanded ?? false,
    loading: false,
  };
}

/**
 * The model with `children` (a listing: any kinds, only libraries and folders are kept) as the
 * children of `id`, sorted by name. A child already known keeps its own children and whether it
 * is expanded; a child that is no longer there goes, with everything below it.
 */
export function insertChildren(model, id, children) {
  const parent = model.nodes[id];
  if (!parent) return model;
  const nodes = { ...model.nodes };
  const kept = (children ?? []).filter((child) => child && KINDS.includes(child.kind) && child.id !== id).sort(byName);
  const ids = kept.map((child) => child.id);
  for (const gone of parent.children ?? []) if (!ids.includes(gone)) dropSubtree(nodes, gone);
  for (const child of kept) nodes[child.id] = node(child, id, nodes[child.id]?.parent === id ? nodes[child.id] : null);
  nodes[id] = { ...parent, children: ids, loading: false };
  return { ...model, nodes };
}

function dropSubtree(nodes, id) {
  const gone = nodes[id];
  if (!gone) return;
  for (const child of gone.children ?? []) dropSubtree(nodes, child);
  delete nodes[id];
}

/** The model with node `id` expanded or collapsed (and, with `loading`, marked as loading). */
export function setExpanded(model, id, expanded, { loading = false } = {}) {
  const at = model.nodes[id];
  if (!at) return model;
  return { ...model, nodes: { ...model.nodes, [id]: { ...at, expanded, loading } } };
}

/** The model with node `id`'s children forgotten, so that they are fetched again when it opens. */
export function forgetChildren(model, id) {
  const at = model.nodes[id];
  if (!at || at.children === null) return model;
  const nodes = { ...model.nodes };
  for (const child of at.children) dropSubtree(nodes, child);
  nodes[id] = { ...at, children: null, expanded: false, loading: false };
  return { ...model, nodes };
}

/** Whether expanding node `id` needs its children fetched first. */
export const needsLoad = (model, id) => model.nodes[id]?.children === null;

/** The ids from the root down to `id` (both included), or [] when `id` is not in the tree. */
export function ancestry(model, id) {
  const out = [];
  for (let at = model.nodes[id]; at; at = at.parent ? model.nodes[at.parent] : null) {
    out.unshift(at.id);
    if (out.length > 10_000) break; // never loops on a broken model
  }
  return out;
}

/**
 * The visible rows, top to bottom: `{ id, name, kind, level, parent, expandable, expanded,
 * loading, posinset, setsize }`. A node whose children are not loaded yet is expandable; one
 * that has none is not. Children of a collapsed node are not visible.
 */
export function flattenTree(model) {
  const rows = [];
  const visit = (id, level, posinset, setsize) => {
    const at = model.nodes[id];
    if (!at) return;
    const expandable = at.children === null || at.children.length > 0;
    rows.push({
      id,
      name: at.name,
      kind: at.kind,
      level,
      parent: at.parent,
      expandable,
      expanded: expandable && at.expanded,
      loading: at.loading,
      posinset,
      setsize,
    });
    if (at.expanded && at.children) at.children.forEach((child, i) => visit(child, level + 1, i + 1, at.children.length));
  };
  visit(model.rootId, 1, 1, 1);
  return rows;
}

/**
 * What a key does in the tree with focus on row `id` of `rows` (from `flattenTree`), as the
 * WAI-ARIA tree pattern has it: `{ focus: id }`, `{ expand: id }`, `{ collapse: id }`,
 * `{ open: id }` (Enter: go to the folder) or null (not the tree's key).
 * Down and Up move to the next and previous visible row; Right expands a closed row or moves to
 * the first child of an open one; Left collapses an open row or moves to the parent; Home and
 * End go to the first and last visible row.
 */
export function treeKey(rows, id, key) {
  const at = rows.findIndex((row) => row.id === id);
  if (at === -1) return rows.length && ['ArrowDown', 'ArrowUp', 'Home', 'End'].includes(key) ? { focus: rows[0].id } : null;
  const row = rows[at];
  switch (key) {
    case 'ArrowDown':
      return at + 1 < rows.length ? { focus: rows[at + 1].id } : { focus: row.id };
    case 'ArrowUp':
      return at > 0 ? { focus: rows[at - 1].id } : { focus: row.id };
    case 'ArrowRight':
      if (row.expandable && !row.expanded) return { expand: row.id };
      if (row.expanded && rows[at + 1]?.parent === row.id) return { focus: rows[at + 1].id };
      return { focus: row.id };
    case 'ArrowLeft':
      if (row.expanded) return { collapse: row.id };
      return row.parent && rows.some((r) => r.id === row.parent) ? { focus: row.parent } : { focus: row.id };
    case 'Home':
      return { focus: rows[0].id };
    case 'End':
      return { focus: rows[rows.length - 1].id };
    case 'Enter':
      return { open: row.id };
    default:
      return null;
  }
}

// ---- dragging and dropping ----

/**
 * The items a drag of the row `id` carries: every selected item when that row is selected,
 * otherwise only that row. `items` are the listed items (`{ id, ... }`).
 */
export function dragList(id, selected, items) {
  const chosen = selected?.has?.(id) ? items.filter((item) => selected.has(item.id)) : items.filter((item) => item.id === id);
  return chosen.length ? chosen : items.filter((item) => item.id === id);
}

/** Whether a drag's types (`dataTransfer.types`) are items dragged inside the page. */
export const isItemDrag = (types) => [...(types ?? [])].includes(ITEM_TYPE);

/** Whether a drag's types are files from the desktop (and not items dragged inside the page). */
export const isFileDrag = (types) => [...(types ?? [])].includes('Files') && !isItemDrag(types);

/**
 * Whether `dragged` (`[{ id, name, movable }]`) may be dropped on `target` (`{ id, name, kind,
 * access }`), whose ids from the top down to itself are `ancestry`; `from` is the place the
 * items are in now. Answers `{ ok: true, message }` or `{ ok: false, reason }` in plain words.
 * Refused: anything but a library or folder, the folder itself or a folder inside it, a place
 * the person may not add to, an item the person may not move, and the place they are already in.
 */
export function canDrop(dragged, target, ancestry = [], { from = null } = {}) {
  const list = Array.isArray(dragged) ? dragged : [];
  if (list.length === 0 || !target) return { ok: false, reason: 'There is nothing to move.' };
  const what = list.length === 1 ? list[0].name : itemsText(list.length);
  if (!KINDS.includes(target.kind)) return { ok: false, reason: 'Items can go only into a library or a folder.' };
  const self = list.find((item) => item.id === target.id);
  if (self) return { ok: false, reason: `${self.name} cannot go into itself.` };
  const above = list.find((item) => (ancestry ?? []).includes(item.id));
  if (above) return { ok: false, reason: `${above.name} cannot go into a folder inside it.` };
  if (!can(target.access, 'add')) return { ok: false, reason: `You cannot add to ${target.name}.` };
  const stuck = list.find((item) => item.movable === false);
  if (stuck) return { ok: false, reason: `You cannot move ${stuck.name}.` };
  if (from !== null && from === target.id) {
    return { ok: false, reason: `${list.length === 1 ? `${what} is` : 'They are'} already in ${target.name}.` };
  }
  return { ok: true, message: `Move ${what} to ${target.name}` };
}

// ---- cut and paste ----

/**
 * What a key does for cut and paste: 'cut' (Ctrl+X or Command+X), 'paste' (Ctrl+V or
 * Command+V), 'cancel' (Escape) or null. Not in text fields, where the keys are their own,
 * and never with Shift or Alt.
 */
export function clipboardKey(event) {
  if (!event || event.defaultPrevented || isTextField(event.target)) return null;
  if (event.key === 'Escape' && !event.ctrlKey && !event.metaKey && !event.altKey) return 'cancel';
  if (!(event.ctrlKey || event.metaKey) || event.altKey || event.shiftKey) return null;
  const key = String(event.key ?? '').toLowerCase();
  if (key === 'x') return 'cut';
  if (key === 'v') return 'paste';
  return null;
}

/** The cut bar's words: "Budget.xlsx is ready to move" or "2 items ready to move". */
export function cutText(items) {
  const n = items?.length ?? 0;
  return n === 1 ? `${items[0].name} is ready to move` : `${itemsText(n)} ready to move`;
}

/**
 * The cut items kept for this tab (sessionStorage text) as `{ from, fromName, items: [{ id,
 * name }] }`, or null when there are none or the text is not that.
 */
export function parseCut(text) {
  try {
    const value = JSON.parse(text);
    if (!value || typeof value.from !== 'string' || !Array.isArray(value.items)) return null;
    const items = value.items.filter((item) => item && typeof item.id === 'string' && typeof item.name === 'string');
    if (items.length === 0) return null;
    return { from: value.from, fromName: String(value.fromName ?? ''), items: items.map(({ id, name }) => ({ id, name })) };
  } catch {
    return null;
  }
}

// ---- the tree on the page ----

/**
 * Draws the folder tree of `root` (the library step, `{ id, name, kind, access }`) into `list`
 * (a `ul[role=tree]`), expanded down to `path` (the steps from the library to the folder shown,
 * which is marked selected). A folder's children are fetched when it is expanded; which folders
 * were expanded is kept for the tab. Enter or a click opens a folder. `onDraw` runs after every
 * redraw; `onOpen(node)` may open a folder itself (answering true). Returns `{ node(id),
 * ancestry(id), refresh(), reveal(), show(id, steps) }`.
 */
export function mountFolderTree({ list, root, path, currentId, onDraw = () => {}, onOpen = () => false }) {
  const key = `sharecraft-tree-${root.id}`;
  let model = treeModel(root);
  let focusId = currentId;
  const requests = new Map(); // id -> the latest listing asked for; older answers are dropped

  const remembered = () => {
    try {
      const ids = JSON.parse(sessionStorage.getItem(key) ?? '[]');
      return Array.isArray(ids) ? ids.filter((id) => typeof id === 'string') : [];
    } catch {
      return [];
    }
  };
  const remember = () => {
    try {
      const open = Object.values(model.nodes).filter((n) => n.expanded).map((n) => n.id);
      sessionStorage.setItem(key, JSON.stringify(open));
    } catch {
      // not remembered; the tree still works
    }
  };

  async function fetchChildren(id) {
    const ask = (requests.get(id) ?? 0) + 1;
    requests.set(id, ask);
    let listed = [];
    let after = null;
    do {
      const query = `limit=500${after ? `&after=${encodeURIComponent(after)}` : ''}`;
      const result = await api('GET', `/nodes/${seg(id)}/children?${query}`);
      if (!result.ok) return { ok: false, error: result.error, stale: requests.get(id) !== ask };
      listed = listed.concat(result.data.items);
      after = result.data.next;
    } while (after);
    return { ok: true, items: listed, stale: requests.get(id) !== ask };
  }

  async function expand(id, { draw: redraw = true } = {}) {
    if (!model.nodes[id]) return;
    if (!needsLoad(model, id)) {
      model = setExpanded(model, id, true);
      remember();
      if (redraw) draw();
      return;
    }
    model = setExpanded(model, id, true, { loading: true });
    if (redraw) draw();
    const answer = await fetchChildren(id);
    if (answer.stale || !model.nodes[id]) return;
    if (!answer.ok) {
      model = setExpanded(model, id, false);
      list.dataset.error = messageFor(answer.error);
    } else {
      model = insertChildren(model, id, answer.items);
      model = setExpanded(model, id, true);
    }
    remember();
    if (redraw) draw();
  }

  function collapse(id) {
    model = setExpanded(model, id, false);
    remember();
    draw();
  }

  function open(id) {
    const at = model.nodes[id];
    if (!at || id === currentId) return;
    // The page may open the folder itself (it does while uploads run); otherwise it is loaded.
    if (!onOpen(at)) location.assign(pageFor(at.kind, at.id));
  }

  /** The tree drawn again from the model; focus stays on the focused row when it was in the tree. */
  function draw() {
    const rows = flattenTree(model);
    // The one Tab stop: the focused row, else the folder shown, else the top (focusId itself is
    // kept, so that the folder shown gets the stop once the path down to it is loaded).
    const shown = (id) => rows.some((row) => row.id === id);
    const stop = shown(focusId) ? focusId : shown(currentId) ? currentId : rows[0]?.id;
    const hadFocus = list.contains(document.activeElement);
    const byParent = new Map();
    const band = new Set(rows.filter((_, at) => at % 2 === 1).map((row) => row.id)); // every second visible row
    for (const row of rows) {
      if (!byParent.has(row.parent)) byParent.set(row.parent, []);
      byParent.get(row.parent).push(row);
    }
    const item = (row) => {
      const twisty = h('span', { class: 'twisty', 'aria-hidden': 'true' }, row.expandable ? (row.expanded ? '▾' : '▸') : '');
      const line = h(
        'span',
        { class: band.has(row.id) ? 'tree-row band' : 'tree-row' },
        twisty,
        h('span', { class: 'tree-icon', 'aria-hidden': 'true' }, fileIcon('folder')),
        h('span', { class: 'tree-name', title: row.name }, row.name),
        row.loading ? h('span', { class: 'meta tree-loading' }, 'Loading…') : null,
      );
      line.style.setProperty('--level', String(row.level - 1));
      const li = h('li', {
        role: 'treeitem',
        'data-id': row.id,
        'aria-level': row.level,
        'aria-posinset': row.posinset,
        'aria-setsize': row.setsize,
        'aria-selected': String(row.id === currentId),
        'aria-expanded': row.expandable ? String(row.expanded) : null,
        'aria-busy': row.loading ? 'true' : null,
        tabindex: row.id === stop ? '0' : '-1',
      }, line);
      const kids = byParent.get(row.id);
      if (row.expanded && kids?.length) li.append(h('ul', { role: 'group' }, ...kids.map(item)));
      return li;
    };
    list.replaceChildren(...(byParent.get(null) ?? []).map(item));
    if (hadFocus) rowElement(stop)?.focus();
    onDraw();
  }

  const rowElement = (id) => [...list.querySelectorAll('li[role=treeitem]')].find((li) => li.dataset.id === id) ?? null;

  list.addEventListener('keydown', (event) => {
    const li = event.target.closest?.('li[role=treeitem]');
    if (!li || event.altKey || event.ctrlKey || event.metaKey || event.shiftKey) return;
    const action = treeKey(flattenTree(model), li.dataset.id, event.key);
    if (!action) return;
    event.preventDefault();
    if (action.focus) {
      focusId = action.focus;
      for (const other of list.querySelectorAll('li[role=treeitem]')) other.tabIndex = other.dataset.id === focusId ? 0 : -1;
      rowElement(focusId)?.focus();
    } else if (action.expand) expand(action.expand);
    else if (action.collapse) collapse(action.collapse);
    else if (action.open) open(action.open);
  });
  list.addEventListener('click', (event) => {
    const li = event.target.closest?.('li[role=treeitem]');
    if (!li) return;
    const id = li.dataset.id;
    focusId = id;
    if (event.target.closest('.twisty')) {
      const at = model.nodes[id];
      if (at?.expanded) collapse(id);
      else expand(id);
      li.focus();
      return;
    }
    const at = model.nodes[id];
    if (at && (event.ctrlKey || event.metaKey)) {
      window.open(pageFor(at.kind, at.id), '_blank', 'noopener');
      return;
    }
    open(id);
  });

  /** Expands the path down to the folder shown and the folders expanded before in this tab. */
  async function reveal() {
    const steps = path.filter((step) => step.id !== currentId || step.id === root.id);
    for (const step of steps) {
      if (!model.nodes[step.id]) break;
      await expand(step.id, { draw: false });
    }
    // The folder shown: its own folders are fetched (not opened), so that it shows whether it has any.
    if (model.nodes[currentId] && needsLoad(model, currentId)) {
      const answer = await fetchChildren(currentId);
      if (answer.ok && !answer.stale) model = insertChildren(model, currentId, answer.items);
    }
    for (const id of remembered()) if (model.nodes[id] && !model.nodes[id].expanded) await expand(id, { draw: false });
    draw();
  }

  /** Fetches the children of every expanded folder again (after a move, a new folder or an undo). */
  async function refresh() {
    const loaded = Object.values(model.nodes).filter((n) => n.children !== null);
    await Promise.all(
      loaded.map(async (n) => {
        if (!n.expanded && n.id !== currentId) {
          model = forgetChildren(model, n.id);
          return;
        }
        const answer = await fetchChildren(n.id);
        if (answer.ok && !answer.stale && model.nodes[n.id]) model = insertChildren(model, n.id, answer.items);
      }),
    );
    draw();
  }

  draw();
  return {
    node: (id) => model.nodes[id] ?? null,
    ancestry: (id) => ancestry(model, id),
    expand: (id) => expand(id),
    refresh,
    reveal,
    /** Marks `id` as the folder shown (opened in the page, with `steps` its path) and reveals it. */
    show(id, steps) {
      currentId = id;
      focusId = id;
      path = steps;
      return reveal();
    },
  };
}
