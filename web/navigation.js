// The navigation editor, shared by a site's own navigation, a hub's bar and the organisation's
// global navigation. It edits the lists as `GET ...navigation?edit=1` answers them and sends
// them back as the server stores them. Items move with buttons (up, down, in, out), never by
// dragging; links go one level deep and a list holds at most 100 items. The server checks
// everything again; this only spares a round trip and says what is wrong in a sentence.

import { api, groupLabel, h, safeHref, seg } from './common.js';

/** The most items one navigation may hold, children included. */
export const MAX_ITEMS = 100;
/** The longest label. */
export const MAX_LABEL = 60;
/** The most groups an item may be aimed at. */
export const MAX_AUDIENCE = 10;

const TOO_DEEP = 'Links go one level deep: an item with items under it cannot go under another.';
const TOO_MANY = `A navigation holds at most ${MAX_ITEMS} items, counting the ones under others.`;

/** A copy of one target as the editor holds it: `{node, name}`, `{url}` or null. */
function editableTarget(target) {
  if (!target) return null;
  if (typeof target.node === 'string') return { node: target.node, name: target.name ?? null };
  if (typeof target.url === 'string') return { url: target.url };
  return null;
}

/**
 * The items of an edit answer (`[{id, label, target: {node, name} | {url} | null, audience:
 * [{id, name}], children}]`) as the editor holds them: a deep copy, so editing never changes the
 * answer. Null (automatic) is an empty list.
 */
export function editableItems(items) {
  return (items ?? []).map((item) => ({
    id: item.id,
    label: item.label ?? '',
    target: editableTarget(item.target),
    audience: (item.audience ?? []).map((group) =>
      typeof group === 'string' ? { id: group, name: null } : { id: group.id, name: group.name ?? null },
    ),
    children: editableItems(item.children),
  }));
}

/** The items as the server stores them: `{id, label, target: {node} | {url} | null, audience: [id], children}`. */
export function storedItems(items) {
  return items.map((item) => ({
    id: item.id,
    label: item.label,
    target: item.target == null ? null : typeof item.target.node === 'string' ? { node: item.target.node } : { url: item.target.url },
    audience: item.audience.map((group) => group.id),
    children: storedItems(item.children ?? []),
  }));
}

/** How many items `items` holds, children included. */
export function countItems(items) {
  return items.reduce((sum, item) => sum + 1 + countItems(item.children ?? []), 0);
}

// ---- the mega menu: helpers for drawing a site's horizontal navigation as panels (common.js) ----

/**
 * The columns of the mega panel of a top-level item whose children are `children` (resolved
 * navigation items, as the server sent them), as `[{heading, items}]` in their order: children
 * without children of their own run together in one column (`heading` null), and each child
 * with children heads a column of its own children. Nothing is added or left out.
 */
export function megaColumns(children) {
  const columns = [];
  for (const child of children ?? []) {
    if (child.children?.length) columns.push({ heading: child, items: child.children });
    else if (columns.at(-1)?.heading === null) columns.at(-1).items.push(child);
    else columns.push({ heading: null, items: [child] });
  }
  return columns;
}

/**
 * Where focus goes among a mega menu's `count` top-level buttons and links for the key `key`,
 * from the one at `index`: ArrowRight and ArrowLeft move to the next and previous (wrapping),
 * Home and End to the first and last. Null for any other key, or when focus is not on one of
 * them (inside a panel, Tab moves through the links and the arrows are left alone).
 */
export function megaStep(key, index, count) {
  if (!(index >= 0 && index < count)) return null;
  if (key === 'ArrowRight') return (index + 1) % count;
  if (key === 'ArrowLeft') return (index - 1 + count) % count;
  if (key === 'Home') return 0;
  if (key === 'End') return count - 1;
  return null;
}

/** A version-4 UUID for a new item. */
function freshId() {
  if (globalThis.crypto?.randomUUID) return globalThis.crypto.randomUUID();
  const bytes = new Uint8Array(16);
  globalThis.crypto.getRandomValues(bytes);
  bytes[6] = (bytes[6] & 0x0f) | 0x40;
  bytes[8] = (bytes[8] & 0x3f) | 0x80;
  const hex = [...bytes].map((b) => b.toString(16).padStart(2, '0')).join('');
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}

/** A new item: a link to `{node, name}` or `{url}`, or a heading (`target` null). */
export function newItem({ label = '', target = null, audience = [] } = {}) {
  return { id: freshId(), label, target: editableTarget(target), audience, children: [] };
}

/** A deep copy of `items`, so every change answers a new list. */
const copy = (items) => items.map((item) => ({ ...item, audience: [...item.audience], children: copy(item.children ?? []) }));

/** Where item `id` is: the list holding it, its index there and its parent (null at the top). */
function locate(items, id) {
  for (const [at, item] of items.entries()) {
    if (item.id === id) return { list: items, at, parent: null };
    const child = (item.children ?? []).findIndex((c) => c.id === id);
    if (child >= 0) return { list: item.children, at: child, parent: item };
  }
  return null;
}

/**
 * Item `id` moved `direction`: `up` or `down` within its list, `in` (under the item above it, as
 * its last child) or `out` (back to the top, just after its parent). Answers `{items}` (a new
 * list; `items` is not changed) or `{error}` when the move cannot be made.
 */
export function moveItem(items, id, direction) {
  const next = copy(items);
  const where = locate(next, id);
  if (!where) return { error: 'That item is no longer in the list.' };
  const { list, at, parent } = where;
  const item = list[at];
  switch (direction) {
    case 'up':
      if (at === 0) return { error: 'That item is already first.' };
      [list[at - 1], list[at]] = [list[at], list[at - 1]];
      return { items: next };
    case 'down':
      if (at === list.length - 1) return { error: 'That item is already last.' };
      [list[at], list[at + 1]] = [list[at + 1], list[at]];
      return { items: next };
    case 'in':
      if (parent) return { error: TOO_DEEP };
      if (at === 0) return { error: 'There is no item above it to go under.' };
      if (item.children.length > 0) return { error: TOO_DEEP };
      list.splice(at, 1);
      list[at - 1].children.push(item);
      return { items: next };
    case 'out': {
      if (!parent) return { error: 'That item is already at the top.' };
      list.splice(at, 1);
      next.splice(next.indexOf(parent) + 1, 0, item);
      return { items: next };
    }
    default:
      return { error: 'That move is not known.' };
  }
}

/** `item` added at the end of `items`, or `{error}` when the list is full. */
export function addItem(items, item) {
  if (countItems(items) + 1 + countItems(item.children ?? []) > MAX_ITEMS) return { error: TOO_MANY };
  return { items: [...copy(items), ...copy([item])] };
}

/** `items` without item `id` (and what is under it). */
export function removeItem(items, id) {
  return copy(items)
    .filter((item) => item.id !== id)
    .map((item) => ({ ...item, children: item.children.filter((child) => child.id !== id) }));
}

/** What is wrong with `items` as the server would see it, in a sentence; null when nothing is. */
export function problemWith(items) {
  if (countItems(items) > MAX_ITEMS) return TOO_MANY;
  const check = (item, depth) => {
    if (depth > 0 && item.children?.length) return TOO_DEEP;
    const label = item.label ?? '';
    const named = typeof item.target?.node === 'string';
    if (label.trim() === '' && !named) return 'Every heading and web address needs a label.';
    if ([...label].length > MAX_LABEL) return `A label is at most ${MAX_LABEL} characters: "${label.slice(0, 20)}…" is longer.`;
    if (item.target && !named && safeHref(item.target.url) === null) {
      return `"${label}" has an address that is not allowed. Use one starting https:, http: or mailto:, or a path starting with one /.`;
    }
    if (item.audience.length > MAX_AUDIENCE) return `"${label || item.target?.name || 'An item'}" is aimed at more than ${MAX_AUDIENCE} groups.`;
    for (const child of item.children ?? []) {
      const problem = check(child, depth + 1);
      if (problem) return problem;
    }
    return null;
  };
  for (const item of items) {
    const problem = check(item, 0);
    if (problem) return problem;
  }
  return null;
}

/**
 * What a Save sends: `{items: null}` for Automatic (a site's own navigation lists its libraries),
 * otherwise `{items}` as stored, or `{error}` when the list breaks a rule and is not sent.
 */
export function navigationBody({ automatic, items }) {
  if (automatic) return { items: null };
  const problem = problemWith(items);
  return problem ? { error: problem } : { items: storedItems(items) };
}

/** What an item is, in words: "Links to Documents", "Web address", "Heading". */
function describe(item) {
  if (!item.target) return 'Heading';
  if (typeof item.target.node === 'string') return item.target.name == null ? 'Links to a place you cannot see' : `Links to ${item.target.name}`;
  return 'Web address';
}

/** The name an item is called by on its buttons. */
const nameOf = (item) => (item.label.trim() || item.target?.name || 'Untitled item');

/** Whether `button` is disabled (in a page, or in the tests' stand-in document). */
const isDisabled = (button) => button.disabled === true || (button.hasAttribute ? button.hasAttribute('disabled') : 'disabled' in button.attributes);

let editors = 0; // makes the ids of each drawn editor unique on the page

/**
 * A drawn navigation editor for `items` (an edit answer, or editor items). `groups` are the
 * groups an item may be aimed at (`[{id, name, site}]`, as `GET /api/groups` lists them).
 * `picker` is an optional place picker (see [`placePicker`]) for adding links to libraries
 * and folders. Answers `{element, items()}`: `items()` is the list as now edited.
 */
export function navigationEditor({ items, groups = [], picker = null, label = 'Navigation items' }) {
  editors += 1;
  const prefix = `nav-edit-${editors}`;
  let current = editableItems(items);
  const status = h('p', { class: 'meta', role: 'status', 'aria-live': 'polite' });
  const list = h('ul', { class: 'list nav-items', 'aria-label': label });
  const empty = h('p', { class: 'meta' }, 'No items yet. Add a link or a heading below.');
  const say = (text) => status.replaceChildren(text);

  /** Draws the list again, then puts the focus on `focus` (`{id, direction}`) if given. */
  const draw = (focus) => {
    const found = new Map();
    list.replaceChildren(...current.map((item, at) => drawItem(item, at, current, false, found)));
    list.hidden = current.length === 0;
    empty.hidden = current.length > 0;
    // The focus stays on the item moved: on the same button, or the opposite one when the same
    // can no longer be used (an item moved in cannot move in again, but can move out).
    if (focus) {
      const opposite = { up: 'down', down: 'up', in: 'out', out: 'in' }[focus.direction];
      const button = [focus.direction, opposite]
        .map((direction) => found.get(`${focus.id}:${direction}`))
        .find((candidate) => candidate && !isDisabled(candidate));
      button?.focus?.();
    }
  };

  const move = (item, direction) => {
    const result = moveItem(current, item.id, direction);
    if (result.error) return say(result.error);
    current = result.items;
    const words = { up: 'up', down: 'down', in: 'under the item above', out: 'out to the top' };
    say(`Moved ${nameOf(item)} ${words[direction]}.`);
    draw({ id: item.id, direction });
  };

  function drawItem(item, at, siblings, isChild, found) {
    const labelId = `${prefix}-label-${item.id}`;
    const name = nameOf(item);
    const button = (direction, text, disabled) => {
      const b = h(
        'button',
        { type: 'button', class: 'plain', 'aria-label': `Move ${name} ${direction}`, disabled: disabled || null },
        text,
      );
      b.addEventListener('click', () => move(item, direction));
      found.set(`${item.id}:${direction}`, b);
      return b;
    };
    const labelInput = h('input', {
      type: 'text',
      id: labelId,
      maxlength: String(MAX_LABEL),
      autocomplete: 'off',
      placeholder: item.target?.name ?? null,
    });
    labelInput.value = item.label;
    labelInput.addEventListener('input', () => {
      item.label = labelInput.value;
    });
    const fields = [h('div', { class: 'field' }, h('label', { for: labelId }, item.target?.node ? 'Label (empty for the name of the place)' : 'Label'), labelInput)];
    if (item.target && typeof item.target.url === 'string') {
      const urlId = `${prefix}-url-${item.id}`;
      const urlInput = h('input', { type: 'text', id: urlId, maxlength: '2000', autocomplete: 'off', inputmode: 'url' });
      urlInput.value = item.target.url;
      urlInput.addEventListener('input', () => {
        item.target = { url: urlInput.value.trim() };
      });
      fields.push(h('div', { class: 'field' }, h('label', { for: urlId }, 'Web address'), urlInput));
    }
    const remove = h('button', { type: 'button', class: 'plain', 'aria-label': `Remove ${name}` }, 'Remove');
    remove.addEventListener('click', () => {
      current = removeItem(current, item.id);
      say(`Removed ${name}${item.children.length ? ' and the items under it' : ''}.`);
      draw();
    });
    const actions = h(
      'div',
      { class: 'actions' },
      button('up', '↑ Up', at === 0),
      button('down', '↓ Down', at === siblings.length - 1),
      button('in', '→ In', isChild || at === 0),
      button('out', '← Out', !isChild),
      remove,
    );
    const children = item.children.length
      ? h('ul', { class: 'list nav-children', 'aria-label': `Under ${name}` }, ...item.children.map((child, n) => drawItem(child, n, item.children, true, found)))
      : null;
    return h(
      'li',
      { class: isChild ? 'nav-item child' : 'nav-item' },
      h('div', { class: 'row' }, ...fields, h('small', { class: 'mark' }, describe(item))),
      actions,
      audienceChooser(item, groups, `${prefix}-aud-${item.id}`, say),
      children,
    );
  }

  const add = addForm(prefix, picker, (item) => {
    const result = addItem(current, item);
    if (result.error) return say(result.error);
    current = result.items;
    say(`Added ${nameOf(item)}.`);
    draw();
  });

  draw();
  const element = h('div', { class: 'nav-editor' }, list, empty, status, add);
  return { element, items: () => current };
}

/**
 * The groups an item is shown to: a disclosure button that opens a list of checkboxes, one per
 * group. Groups already chosen that the editor cannot see are kept, under the name the server
 * gave. At most ten.
 */
function audienceChooser(item, groups, id, say) {
  const summary = () =>
    item.audience.length === 0 ? 'Shown to everyone' : `Shown only to ${item.audience.length} group${item.audience.length === 1 ? '' : 's'}`;
  const toggle = h('button', { type: 'button', class: 'plain', 'aria-expanded': 'false', 'aria-controls': id }, summary());
  const known = new Map(groups.map((group) => [group.id, groupLabel(group)]));
  for (const group of item.audience) if (!known.has(group.id)) known.set(group.id, group.name ?? 'A group you cannot see');
  const boxes = [...known].map(([groupId, name]) => {
    const box = h('input', { type: 'checkbox' });
    box.checked = item.audience.some((group) => group.id === groupId);
    box.addEventListener('change', () => {
      if (box.checked) {
        if (item.audience.length >= MAX_AUDIENCE) {
          box.checked = false;
          return say(`An item can be aimed at ${MAX_AUDIENCE} groups at most.`);
        }
        item.audience.push({ id: groupId, name });
      } else {
        item.audience = item.audience.filter((group) => group.id !== groupId);
      }
      toggle.replaceChildren(summary());
    });
    return h('label', { class: 'check' }, box, ' ', name);
  });
  const panel = h(
    'fieldset',
    { class: 'audience', id },
    h('legend', {}, 'Show only to these groups (none for everyone). It hides the link; it never changes who has access.'),
    ...(boxes.length ? boxes : [h('p', { class: 'meta' }, 'There are no groups you can choose.')]),
  );
  panel.hidden = true;
  toggle.addEventListener('click', () => {
    const open = toggle.getAttribute('aria-expanded') !== 'true';
    toggle.setAttribute('aria-expanded', open ? 'true' : 'false');
    panel.hidden = !open;
  });
  return h('div', { class: 'audience-row' }, toggle, panel);
}

/** The form that adds an item: a link to a place (when there is a picker), a web address or a heading. */
function addForm(prefix, picker, onAdd) {
  const kindId = `${prefix}-kind`;
  const labelId = `${prefix}-new-label`;
  const urlId = `${prefix}-new-url`;
  const kind = h(
    'select',
    { id: kindId },
    picker ? h('option', { value: 'place' }, 'A library or folder') : null,
    h('option', { value: 'url' }, 'A web address'),
    h('option', { value: 'heading' }, 'A heading'),
  );
  const label = h('input', { type: 'text', id: labelId, maxlength: String(MAX_LABEL), autocomplete: 'off' });
  const url = h('input', { type: 'text', id: urlId, maxlength: '2000', autocomplete: 'off', inputmode: 'url', placeholder: 'https://' });
  const urlField = h('div', { class: 'field' }, h('label', { for: urlId }, 'Web address'), url);
  const placeField = picker ? h('div', { class: 'place' }, picker.element) : null;
  const problem = h('p', { class: 'msg', role: 'alert' });
  const show = () => {
    urlField.hidden = kind.value !== 'url';
    if (placeField) placeField.hidden = kind.value !== 'place';
  };
  kind.addEventListener('change', show);
  const button = h('button', { type: 'button' }, 'Add');
  button.addEventListener('click', () => {
    problem.replaceChildren();
    const text = label.value.trim();
    let target = null;
    if (kind.value === 'place') {
      target = picker?.value();
      if (!target) return problem.replaceChildren('Choose a library or folder first.');
    } else if (kind.value === 'url') {
      if (safeHref(url.value.trim()) === null) {
        return problem.replaceChildren('That address is not allowed. Use one starting https:, http: or mailto:, or a path starting with one /.');
      }
      target = { url: url.value.trim() };
    }
    if (kind.value !== 'place' && text === '') return problem.replaceChildren('Give it a label.');
    onAdd(newItem({ label: text, target }));
    label.value = '';
    url.value = '';
  });
  show();
  return h(
    'fieldset',
    { class: 'nav-add' },
    h('legend', {}, 'Add an item'),
    h('div', { class: 'field' }, h('label', { for: kindId }, 'Add'), kind),
    placeField,
    urlField,
    h('div', { class: 'field' }, h('label', { for: labelId }, 'Label'), label),
    h('div', {}, button),
    problem,
  );
}

let pickers = 0;

/**
 * A place picker: a chain of selects, the first listing `first()` (`[{id, name, kind}]`: a site's
 * libraries, or the sites), each later one the libraries and folders inside the place chosen
 * above it, loaded when it is chosen. `value()` is the deepest place chosen, `{node, name}`, or
 * null. Names are drawn as text only.
 */
export function placePicker({ first, label = 'Library or folder' }) {
  pickers += 1;
  const prefix = `place-${pickers}`;
  const element = h('div', { class: 'row place-picker' });
  const levels = []; // [{select, places}]

  const addLevel = (places, depth) => {
    const id = `${prefix}-${depth}`;
    const select = h(
      'select',
      { id },
      h('option', { value: '' }, depth === 0 ? 'Choose…' : '(the place above itself)'),
      ...places.map((place) => h('option', { value: place.id }, place.name)),
    );
    const field = h('div', { class: 'field' }, h('label', { for: id }, depth === 0 ? label : 'Folder inside it (optional)'), select);
    levels.push({ select, places, field });
    element.append(field);
    select.addEventListener('change', async () => {
      for (const level of levels.splice(depth + 1)) level.field.remove?.();
      if (!select.value) return;
      const result = await api('GET', `/nodes/${seg(select.value)}/children?limit=500`);
      if (!result.ok || levels.length !== depth + 1) return;
      const inside = result.data.items.filter((item) => item.kind === 'library' || item.kind === 'folder');
      if (inside.length) addLevel(inside, depth + 1);
    });
  };

  const ready = first().then((places) => addLevel(places, 0));
  const value = () => {
    for (const level of [...levels].reverse()) {
      const place = level.places.find((p) => p.id === level.select.value);
      if (place) return { node: place.id, name: place.name };
    }
    return null;
  };
  return { element, value, ready };
}
