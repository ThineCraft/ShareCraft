import test from 'node:test';
import assert from 'node:assert/strict';
import {
  MAX_ITEMS,
  addItem,
  countItems,
  editableItems,
  moveItem,
  navigationBody,
  navigationEditor,
  newItem,
  problemWith,
  storedItems,
} from './navigation.js';

const LIB = '33333333-3333-4333-8333-333333333333';
const DIR = '44444444-4444-4444-8444-444444444444';
const GROUP = '55555555-5555-4555-8555-555555555555';
const HIDDEN = '66666666-6666-4666-8666-666666666666';

const id = (n) => `00000000-0000-4000-8000-${String(n).padStart(12, '0')}`;
const link = (n, children = []) => ({ id: id(n), label: `Item ${n}`, target: { url: `/page-${n}` }, audience: [], children });
const ids = (items) => items.map((item) => [item.id, ...(item.children ?? []).map((child) => child.id)].join('>'));

/** The lists as `GET /api/sites/{id}/navigation?edit=1` answers them. */
const EDIT_ANSWER = [
  {
    id: id(1),
    label: '',
    target: { node: LIB, name: 'Documents' },
    audience: [{ id: GROUP, name: 'Sales Members' }, { id: HIDDEN, name: 'A group you cannot see' }],
    children: [
      { id: id(2), label: 'Plans', target: { node: DIR, name: null }, audience: [], children: [] },
      { id: id(3), label: 'Intranet', target: { url: 'https://intranet.example/' }, audience: [], children: [] },
    ],
  },
  { id: id(4), label: 'Help', target: null, audience: [], children: [{ id: id(5), label: 'Mail us', target: { url: 'mailto:help@example.org' }, audience: [], children: [] }] },
];

test('editor_round_trips_stored_lists', () => {
  const stored = storedItems(editableItems(EDIT_ANSWER));
  assert.deepEqual(stored, [
    {
      id: id(1),
      label: '',
      target: { node: LIB },
      audience: [GROUP, HIDDEN],
      children: [
        { id: id(2), label: 'Plans', target: { node: DIR }, audience: [], children: [] },
        { id: id(3), label: 'Intranet', target: { url: 'https://intranet.example/' }, audience: [], children: [] },
      ],
    },
    { id: id(4), label: 'Help', target: null, audience: [], children: [{ id: id(5), label: 'Mail us', target: { url: 'mailto:help@example.org' }, audience: [], children: [] }] },
  ]);
  // Every field the server's NavItem takes, and no other (it refuses unknown fields).
  const fields = (item) => Object.keys(item).sort().join(',');
  for (const item of [...stored, ...stored.flatMap((i) => i.children)]) assert.equal(fields(item), 'audience,children,id,label,target');
  // Editing works on a copy: the answer itself is never changed.
  const editable = editableItems(EDIT_ANSWER);
  editable[0].children.pop();
  assert.equal(EDIT_ANSWER[0].children.length, 2);
  // Stored again unchanged, and the empty answer stays empty.
  assert.deepEqual(storedItems(editableItems(stored.map((item) => ({ ...item, audience: item.audience.map((g) => ({ id: g, name: 'x' })) })))), stored);
  assert.deepEqual(storedItems(editableItems(null)), []);
});

test('editor_moves_items_without_drag', () => {
  const items = [link(1), link(2, [link(21), link(22)]), link(3)];
  // Up and down within one list; the first cannot go up, the last cannot go down.
  assert.deepEqual(ids(moveItem(items, id(3), 'up').items), [id(1), `${id(3)}`, `${id(2)}>${id(21)}>${id(22)}`]);
  assert.deepEqual(ids(moveItem(items, id(1), 'down').items), [`${id(2)}>${id(21)}>${id(22)}`, id(1), id(3)]);
  assert.ok(moveItem(items, id(1), 'up').error);
  assert.ok(moveItem(items, id(3), 'down').error);
  assert.deepEqual(ids(moveItem(items, id(22), 'up').items), [id(1), `${id(2)}>${id(22)}>${id(21)}`, id(3)]);
  // In: under the item above, as its last child. Out: back to the top, just after its parent.
  assert.deepEqual(ids(moveItem(items, id(3), 'in').items), [id(1), `${id(2)}>${id(21)}>${id(22)}>${id(3)}`]);
  assert.deepEqual(ids(moveItem(items, id(21), 'out').items), [id(1), `${id(2)}>${id(22)}`, id(21), id(3)]);
  assert.ok(moveItem(items, id(1), 'in').error, 'nothing above to go under');
  assert.ok(moveItem(items, id(1), 'out').error, 'already at the top');
  // The list given is never changed.
  assert.deepEqual(ids(items), [id(1), `${id(2)}>${id(21)}>${id(22)}`, id(3)]);

  // The drawn editor moves with buttons, named for their item; nothing is draggable.
  const all = editorDocument();
  try {
    const editor = navigationEditor({ items: [link(1), link(2)], groups: [] });
    assert.ok(!all.some((element) => 'draggable' in element.attributes), 'no element is draggable');
    const buttons = all.filter((element) => element.tag === 'button');
    for (const name of ['Move Item 1 up', 'Move Item 1 down', 'Move Item 1 in', 'Move Item 1 out']) {
      assert.ok(buttons.some((b) => b.attributes['aria-label'] === name), name);
    }
    const down = buttons.find((b) => b.attributes['aria-label'] === 'Move Item 1 down');
    down.listeners.click();
    assert.deepEqual(editor.items().map((item) => item.id), [id(2), id(1)]);
    const latest = all.filter((element) => element.tag === 'button');
    latest.findLast((b) => b.attributes['aria-label'] === 'Move Item 1 in').listeners.click();
    assert.deepEqual(ids(editor.items()), [`${id(2)}>${id(1)}`]);
    assert.equal(editor.element.tag, 'div');
  } finally {
    delete globalThis.document;
  }
});

test('editor_refuses_two_levels_and_101_items', () => {
  // A parent cannot go under another item: that would make a second level.
  const items = [link(1), link(2, [link(21)])];
  const refused = moveItem(items, id(2), 'in');
  assert.match(refused.error, /one level/);
  assert.equal(refused.items, undefined);
  // A list that holds a second level is refused before it is sent.
  const deep = [link(1, [link(11, [link(111)])])];
  assert.match(problemWith(deep), /one level/);
  assert.ok(navigationBody({ automatic: false, items: deep }).error);

  // 100 items, children included, is the most; the 101st is refused when added and when sent.
  const hundred = Array.from({ length: 50 }, (_, n) => link(n * 2 + 1, [link(n * 2 + 2)]));
  assert.equal(countItems(hundred), MAX_ITEMS);
  assert.equal(problemWith(hundred), null);
  assert.ok(navigationBody({ automatic: false, items: hundred }).items);
  const added = addItem(hundred, newItem({ label: 'One more', target: { url: '/more' } }));
  assert.match(added.error, /100/);
  const tooMany = [...hundred, link(999)];
  assert.match(problemWith(tooMany), /100/);
  assert.match(navigationBody({ automatic: false, items: tooMany }).error, /100/);

  // The other rules the server applies are checked first too.
  assert.match(problemWith([{ ...link(1), label: 'x'.repeat(61) }]), /60/);
  assert.match(problemWith([{ ...link(1), label: '' }]), /label/i);
  assert.equal(problemWith([{ ...link(1), label: '', target: { node: LIB, name: 'Documents' } }]), null, 'a library link may take its name');
  assert.match(problemWith([{ ...link(1), target: { url: 'javascript:alert(1)' } }]), /address/);
  assert.match(problemWith([{ ...link(1), audience: Array.from({ length: 11 }, (_, n) => ({ id: id(n), name: 'g' })) }]), /10/);
});

test('automatic_choice_sends_null', () => {
  assert.deepEqual(navigationBody({ automatic: true, items: [link(1)] }), { items: null });
  assert.deepEqual(navigationBody({ automatic: true, items: [] }), { items: null });
  // Custom sends the list as stored, even an empty one.
  assert.deepEqual(navigationBody({ automatic: false, items: [] }), { items: [] });
  assert.deepEqual(navigationBody({ automatic: false, items: editableItems([EDIT_ANSWER[1]]) }), {
    items: [storedItems(editableItems([EDIT_ANSWER[1]]))[0]],
  });
  // Even a list the editor would refuse is not checked when Automatic is chosen.
  assert.deepEqual(navigationBody({ automatic: true, items: [link(1, [link(2, [link(3)])])] }), { items: null });
});

test('new items get a fresh identifier and the drawn editor keeps labels as text', () => {
  const a = newItem({ label: 'A', target: null });
  const b = newItem({ label: 'B', target: null });
  assert.match(a.id, /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
  assert.notEqual(a.id, b.id);
  const all = editorDocument();
  try {
    const hostile = '<img src=x onerror="alert(1)">';
    navigationEditor({
      items: [{ id: id(1), label: hostile, target: { node: LIB, name: hostile }, audience: [{ id: GROUP, name: hostile }], children: [] }],
      groups: [{ id: GROUP, name: hostile }],
    });
    assert.ok(!all.some((element) => element.tag === 'img'));
    const strings = all.flatMap((element) => element.children.filter((child) => typeof child === 'string'));
    assert.ok(strings.some((text) => text.includes(hostile)), 'drawn as text');
  } finally {
    delete globalThis.document;
  }
});

// A document with just enough for the editor: elements keep tag, attributes, children (strings
// stay strings), listeners and a value; `innerHTML` is never used.
function editorDocument() {
  const all = [];
  globalThis.document = {
    createElement(tag) {
      const element = {
        tag,
        attributes: {},
        children: [],
        listeners: {},
        hidden: false,
        value: '',
        checked: false,
        setAttribute(name, value) {
          this.attributes[name] = value;
        },
        getAttribute(name) {
          return this.attributes[name] ?? null;
        },
        removeAttribute(name) {
          delete this.attributes[name];
        },
        append(...nodes) {
          this.children.push(...nodes);
        },
        replaceChildren(...nodes) {
          this.children = nodes;
        },
        addEventListener(type, listener) {
          this.listeners[type] = listener;
        },
        focus() {},
        set innerHTML(_) {
          throw new Error('innerHTML is never used');
        },
      };
      all.push(element);
      return element;
    },
  };
  return all;
}

// ---- the mega menu (plan 4B): a site's horizontal navigation as full-width panels ----

import { readFileSync } from 'node:fs';
import { navigationView } from './common.js';
import { megaColumns, megaStep } from './navigation.js';

const SITE = '11111111-1111-4111-8111-111111111111';

// A document for drawn navigation: elements know their parent, what they contain and focus.
function megaDocument() {
  const all = [];
  const doc = {
    activeElement: null,
    createElement(tag) {
      const element = {
        tag,
        attributes: {},
        children: [],
        listeners: {},
        hidden: false,
        parent: null,
        setAttribute(name, value) {
          this.attributes[name] = value;
        },
        getAttribute(name) {
          return this.attributes[name] ?? null;
        },
        append(...nodes) {
          for (const node of nodes) if (typeof node === 'object') node.parent = this;
          this.children.push(...nodes);
        },
        replaceChildren(...nodes) {
          this.children = [];
          this.append(...nodes);
        },
        addEventListener(type, listener) {
          this.listeners[type] = listener;
        },
        contains(other) {
          for (let at = other; at; at = at.parent) if (at === this) return true;
          return false;
        },
        focus() {
          doc.activeElement = this;
        },
        set innerHTML(_) {
          throw new Error('innerHTML is never used');
        },
      };
      all.push(element);
      return element;
    },
    getElementById: () => null,
    querySelector: () => null,
    addEventListener() {},
  };
  globalThis.document = doc;
  return all;
}

const below = (element) => (typeof element === 'string' ? [] : [element, ...element.children.flatMap(below)]);
const classed = (element, name) => below(element).filter((e) => (e.attributes.class ?? '').split(' ').includes(name));
const textOf = (element) =>
  element.children.flatMap((child) => (typeof child === 'string' ? [child] : child.attributes['aria-hidden'] === 'true' ? [] : [textOf(child)])).join('');
const keyEvent = (key, target, more = {}) => ({
  key,
  target,
  defaultPrevented: false,
  preventDefault() {
    this.defaultPrevented = true;
  },
  ...more,
});
const navLink = (n, label, children = [], more = {}) => ({ id: `i${n}`, label, target: { url: `/page-${n}` }, targeted: false, children, ...more });

/** The lists as `GET /api/sites/{id}/navigation` answers them: already cut to what the reader may see. */
const SEEN = {
  global: [navLink(9, 'Intranet', [navLink(91, 'Policies')])],
  hub: null,
  local: [
    navLink(1, 'Teams', [navLink(11, 'Sales', [], { targeted: true }), navLink(12, 'Legal')]),
    navLink(2, 'News'),
    {
      id: 'i3',
      label: 'Help',
      target: null,
      targeted: false,
      children: [
        { id: 'i31', label: 'Guides', target: null, targeted: false, children: [navLink(311, 'Start here')] },
        navLink(32, 'Mail us'),
      ],
    },
  ],
  automatic: false,
};

test('mega_menu_keyboard_and_audiences', () => {
  // The columns: links without children together, a link with children as a headed column, in order.
  const help = SEEN.local[2].children;
  assert.deepEqual(
    megaColumns(help).map((column) => [column.heading?.label ?? null, column.items.map((item) => item.label)]),
    [['Guides', ['Start here']], [null, ['Mail us']]],
  );
  assert.deepEqual(megaColumns([navLink(1, 'A'), navLink(2, 'B')]).length, 1);
  assert.deepEqual(megaColumns([]), []);
  // Arrows move between the top-level controls and wrap; Home and End go to either end.
  assert.equal(megaStep('ArrowRight', 0, 3), 1);
  assert.equal(megaStep('ArrowRight', 2, 3), 0);
  assert.equal(megaStep('ArrowLeft', 0, 3), 2);
  assert.equal(megaStep('Home', 2, 3), 0);
  assert.equal(megaStep('End', 0, 3), 2);
  assert.equal(megaStep('a', 0, 3), null);
  assert.equal(megaStep('ArrowRight', -1, 3), null);

  const all = megaDocument();
  try {
    const view = navigationView({ id: SITE, name: 'Sales site' }, SEEN, { menu: 'mega' });
    const local = below(view).find((e) => e.attributes['data-bar'] === 'local');
    const global = below(view).find((e) => e.attributes['data-bar'] === 'global');
    assert.ok(local.attributes.class.split(' ').includes('mega'), 'the site bar is a mega menu');
    assert.ok(!global.attributes.class.split(' ').includes('mega'), "the organisation's bar keeps its dropdowns");

    // Only what the server sent is drawn: every link and label, nothing more.
    const links = below(local).filter((e) => e.tag === 'a');
    assert.deepEqual(
      links.map((a) => a.attributes.href),
      [`/site.html?id=${SITE}`, '/page-1', '/page-11', '/page-12', '/page-2', '/page-311', '/page-32'],
    );
    assert.deepEqual(classed(local, 'mark').map(textOf), ['Targeted'], 'a targeted link is marked');

    // Each top-level item with children is a disclosure button for one full-width panel.
    const buttons = classed(local, 'mega-toggle');
    assert.deepEqual(buttons.map(textOf), ['Teams', 'Help'], 'the button is named by the label');
    const panels = classed(local, 'mega-panel');
    assert.equal(panels.length, 2);
    for (const [at, button] of buttons.entries()) {
      assert.equal(button.tag, 'button');
      assert.equal(button.attributes.type, 'button');
      assert.equal(button.attributes['aria-expanded'], 'false');
      assert.equal(button.attributes['aria-controls'], panels[at].attributes.id);
      assert.equal(panels[at].hidden, true);
    }
    // The link of an item with children opens from its panel; a column with children is headed.
    assert.equal(below(panels[0]).filter((e) => e.tag === 'a')[0].attributes.href, '/page-1');
    const heading = classed(panels[1], 'mega-heading')[0];
    assert.equal(textOf(heading), 'Guides');
    const headed = below(panels[1]).find((e) => e.tag === 'ul' && e.attributes['aria-labelledby'] === heading.attributes.id);
    assert.deepEqual(below(headed).filter((e) => e.tag === 'a').map((a) => a.attributes.href), ['/page-311']);

    // Opening one panel closes the other.
    buttons[0].listeners.click();
    assert.equal(buttons[0].attributes['aria-expanded'], 'true');
    assert.equal(panels[0].hidden, false);
    buttons[1].listeners.click();
    assert.equal(buttons[0].attributes['aria-expanded'], 'false');
    assert.equal(panels[0].hidden, true);
    assert.equal(panels[1].hidden, false);

    // Arrows move between the top-level buttons and links.
    const top = below(local).find((e) => e.tag === 'ul' && e.attributes['aria-label'] === 'This site');
    const news = links.find((a) => a.attributes.href === '/page-2');
    const right = keyEvent('ArrowRight', buttons[0]);
    top.listeners.keydown(right);
    assert.equal(document.activeElement, news);
    assert.ok(right.defaultPrevented);
    top.listeners.keydown(keyEvent('ArrowRight', news));
    assert.equal(document.activeElement, buttons[1]);
    top.listeners.keydown(keyEvent('ArrowRight', buttons[1]));
    assert.equal(document.activeElement, buttons[0]);
    // Inside a panel the arrows are left alone (Tab moves through its links).
    const inside = keyEvent('ArrowRight', below(panels[1]).find((e) => e.tag === 'a'));
    top.listeners.keydown(inside);
    assert.equal(inside.defaultPrevented, false);

    // Escape closes the panel and returns focus to its button.
    const helpItem = buttons[1].parent;
    const escape = keyEvent('Escape', below(panels[1]).find((e) => e.tag === 'a'));
    helpItem.listeners.keydown(escape);
    assert.equal(buttons[1].attributes['aria-expanded'], 'false');
    assert.equal(panels[1].hidden, true);
    assert.equal(document.activeElement, buttons[1]);
    assert.ok(escape.defaultPrevented, 'only the panel closes, not a drawer around it');

    // Tabbing out of an item closes its panel; moving within it does not.
    buttons[1].listeners.click();
    helpItem.listeners.focusout({ relatedTarget: below(panels[1]).find((e) => e.tag === 'a') });
    assert.equal(panels[1].hidden, false);
    helpItem.listeners.focusout({ relatedTarget: news });
    assert.equal(panels[1].hidden, true);
    assert.equal(buttons[1].attributes['aria-expanded'], 'false');
    assert.ok(!all.some((element) => element.tag === 'img'));
  } finally {
    delete globalThis.document;
  }
});

/** The rules of `css` inside an `@media` prelude matching `media` (or outside any, for null). */
function rulesIn(css, media) {
  const found = [];
  const walk = (text, at) => {
    let i = 0;
    while (i < text.length) {
      const open = text.indexOf('{', i);
      if (open < 0) break;
      const prelude = text.slice(i, open).trim();
      let depth = 1;
      let j = open + 1;
      while (j < text.length && depth) {
        if (text[j] === '{') depth++;
        else if (text[j] === '}') depth--;
        j++;
      }
      const body = text.slice(open + 1, j - 1);
      if (prelude.startsWith('@')) walk(body, prelude);
      else if (media === null ? at === null : at !== null && media.test(at)) found.push({ selector: prelude, body });
      i = j;
    }
  };
  walk(css, null);
  return found;
}

test('mega_menu_falls_back_on_phones', () => {
  megaDocument();
  try {
    // The Menu drawer button is there as on every page; vertical navigation and any other
    // menu value keep the dropdowns.
    const view = navigationView({ id: SITE, name: 'Sales site' }, SEEN, { menu: 'mega' });
    assert.equal(classed(view, 'menu-toggle').length, 1);
    assert.equal(classed(view, 'chrome-menu').length, 1);
    for (const options of [{ menu: 'mega', vertical: true }, { menu: 'cascading' }, { menu: '<b>mega</b>' }, {}]) {
      const other = navigationView({ id: SITE, name: 'Sales site' }, SEEN, options);
      assert.equal(classed(other, 'mega-panel').length, 0, JSON.stringify(options));
      assert.equal(classed(other, 'sub').length, 4, JSON.stringify(options));
    }
  } finally {
    delete globalThis.document;
  }
  // Wide: a full-width panel. In the drawer (phones): the panel is a nested list in the flow.
  const css = readFileSync(new URL('./style.css', import.meta.url), 'utf8').replace(/\/\*[\s\S]*?\*\//g, '');
  const wide = rulesIn(css, null).filter((rule) => rule.selector.split(/\s*,\s*/).includes('.navbar .mega-panel'));
  assert.ok(wide.some((rule) => /position:absolute/.test(rule.body) && /left:0/.test(rule.body) && /right:0/.test(rule.body)));
  const phone = rulesIn(css, /max-width:\s*760px/);
  const panel = phone.filter((rule) => rule.selector.split(/\s*,\s*/).includes('.navbar .mega-panel'));
  assert.ok(panel.some((rule) => /position:static/.test(rule.body) && /box-shadow:none/.test(rule.body)), 'static in the drawer');
  const columns = phone.filter((rule) => rule.selector.split(/\s*,\s*/).includes('.navbar .mega-columns'));
  assert.ok(columns.some((rule) => /display:block/.test(rule.body)), 'one column in the drawer');
});
