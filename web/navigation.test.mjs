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
