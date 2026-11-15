import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import {
  ITEM_TYPE,
  ancestry,
  canDrop,
  clipboardKey,
  cutText,
  dragList,
  flattenTree,
  forgetChildren,
  insertChildren,
  isFileDrag,
  isItemDrag,
  needsLoad,
  parseCut,
  setExpanded,
  treeKey,
  treeModel,
} from './tree.js';

const ADD = { perms: ['view', 'add', 'edit'] };
const VIEW = { perms: ['view'] };
const lib = { id: 'lib', name: 'Documents', kind: 'library', access: ADD };

/** Documents › (Plans › (Old, Q1), Budget) with a file beside them. */
function sample() {
  let model = treeModel(lib);
  model = insertChildren(model, 'lib', [
    { id: 'plans', name: 'Plans', kind: 'folder', access: ADD },
    { id: 'doc', name: 'a.txt', kind: 'document', access: ADD },
    { id: 'budget', name: 'Budget', kind: 'folder', access: VIEW },
  ]);
  model = setExpanded(model, 'lib', true);
  model = insertChildren(model, 'plans', [
    { id: 'q1', name: 'Q1', kind: 'folder', access: ADD },
    { id: 'old', name: 'Old', kind: 'folder', access: ADD },
  ]);
  model = setExpanded(model, 'plans', true);
  return model;
}

test('insertChildren keeps folders by name and leaves files out', () => {
  const model = sample();
  assert.deepEqual(model.nodes.lib.children, ['budget', 'plans']);
  assert.equal(model.nodes.doc, undefined);
  assert.deepEqual(model.nodes.plans.children, ['old', 'q1']);
  assert.equal(model.nodes.q1.parent, 'plans');
  assert.equal(needsLoad(model, 'q1'), true);
  assert.equal(needsLoad(model, 'plans'), false);
});

test('insertChildren again keeps what was expanded and drops folders that went, with what was below', () => {
  let model = sample();
  model = insertChildren(model, 'lib', [
    { id: 'plans', name: 'Plans (renamed)', kind: 'folder', access: ADD },
    { id: 'new', name: 'Archive', kind: 'folder', access: ADD },
  ]);
  assert.deepEqual(model.nodes.lib.children, ['new', 'plans']);
  assert.equal(model.nodes.plans.name, 'Plans (renamed)');
  assert.equal(model.nodes.plans.expanded, true);
  assert.deepEqual(model.nodes.plans.children, ['old', 'q1']);
  assert.equal(model.nodes.budget, undefined);
  // Moving Plans out of the library drops it and its folders.
  model = insertChildren(model, 'lib', [{ id: 'new', name: 'Archive', kind: 'folder' }]);
  assert.equal(model.nodes.plans, undefined);
  assert.equal(model.nodes.q1, undefined);
  // Insert does not change the model it was given.
  assert.ok(sample().nodes.budget);
});

test('expand, collapse and forgetting children', () => {
  let model = sample();
  model = setExpanded(model, 'plans', false);
  assert.equal(model.nodes.plans.expanded, false);
  assert.deepEqual(model.nodes.plans.children, ['old', 'q1']);
  model = setExpanded(model, 'q1', true, { loading: true });
  assert.equal(model.nodes.q1.loading, true);
  model = forgetChildren(model, 'plans');
  assert.equal(model.nodes.plans.children, null);
  assert.equal(model.nodes.q1, undefined);
  assert.equal(setExpanded(model, 'nope', true), model);
});

test('ancestry runs from the library down', () => {
  const model = sample();
  assert.deepEqual(ancestry(model, 'q1'), ['lib', 'plans', 'q1']);
  assert.deepEqual(ancestry(model, 'lib'), ['lib']);
  assert.deepEqual(ancestry(model, 'missing'), []);
});

test('flattenTree lists the visible rows with level, position and expandability', () => {
  const rows = flattenTree(sample());
  assert.deepEqual(rows.map((r) => [r.id, r.level, r.posinset, r.setsize]), [
    ['lib', 1, 1, 1],
    ['budget', 2, 1, 2],
    ['plans', 2, 2, 2],
    ['old', 3, 1, 2],
    ['q1', 3, 2, 2],
  ]);
  const plans = rows.find((r) => r.id === 'plans');
  assert.equal(plans.expanded, true);
  assert.equal(rows.find((r) => r.id === 'q1').expandable, true); // not loaded yet
  // A folder loaded with no folders inside is a leaf.
  const model = insertChildren(sample(), 'q1', []);
  const q1 = flattenTree(model).find((r) => r.id === 'q1');
  assert.equal(q1.expandable, false);
  assert.equal(q1.expanded, false);
  // Collapsed: its children are not visible.
  assert.deepEqual(flattenTree(setExpanded(sample(), 'plans', false)).map((r) => r.id), ['lib', 'budget', 'plans']);
});

test('treeKey follows the WAI-ARIA tree pattern', () => {
  const rows = flattenTree(sample());
  assert.deepEqual(treeKey(rows, 'lib', 'ArrowDown'), { focus: 'budget' });
  assert.deepEqual(treeKey(rows, 'q1', 'ArrowDown'), { focus: 'q1' }); // the last row stays
  assert.deepEqual(treeKey(rows, 'old', 'ArrowUp'), { focus: 'plans' });
  assert.deepEqual(treeKey(rows, 'lib', 'ArrowUp'), { focus: 'lib' });
  // Right: open an unopened folder, go into an open one; Left: close an open one, go to the parent.
  assert.deepEqual(treeKey(rows, 'q1', 'ArrowRight'), { expand: 'q1' });
  assert.deepEqual(treeKey(rows, 'plans', 'ArrowRight'), { focus: 'old' });
  assert.deepEqual(treeKey(rows, 'plans', 'ArrowLeft'), { collapse: 'plans' });
  assert.deepEqual(treeKey(rows, 'old', 'ArrowLeft'), { focus: 'plans' });
  assert.deepEqual(treeKey(rows, 'lib', 'ArrowLeft'), { collapse: 'lib' });
  const closed = flattenTree(setExpanded(sample(), 'lib', false));
  assert.deepEqual(treeKey(closed, 'lib', 'ArrowLeft'), { focus: 'lib' });
  // A leaf: Right does nothing.
  const leaf = flattenTree(insertChildren(sample(), 'q1', []));
  assert.deepEqual(treeKey(leaf, 'q1', 'ArrowRight'), { focus: 'q1' });
  assert.deepEqual(treeKey(rows, 'plans', 'Home'), { focus: 'lib' });
  assert.deepEqual(treeKey(rows, 'plans', 'End'), { focus: 'q1' });
  assert.deepEqual(treeKey(rows, 'old', 'Enter'), { open: 'old' });
  assert.equal(treeKey(rows, 'old', 'a'), null);
  assert.deepEqual(treeKey(rows, 'gone', 'ArrowDown'), { focus: 'lib' });
});

test('canDrop refuses the folder itself, folders inside it, no add and the place it is in', () => {
  const plans = { id: 'plans', name: 'Plans', movable: true };
  const file = { id: 'f', name: 'a.txt', movable: true };
  const q1 = { id: 'q1', name: 'Q1', kind: 'folder', access: ADD };
  assert.deepEqual(canDrop([file, plans], q1, ['lib', 'plans', 'q1'], { from: 'lib' }), {
    ok: false,
    reason: 'Plans cannot go into a folder inside it.',
  });
  assert.deepEqual(canDrop([plans], { id: 'plans', name: 'Plans', kind: 'folder', access: ADD }, ['lib', 'plans']), {
    ok: false,
    reason: 'Plans cannot go into itself.',
  });
  assert.equal(canDrop([file], { id: 'b', name: 'Budget', kind: 'folder', access: VIEW }, ['lib', 'b']).reason, 'You cannot add to Budget.');
  assert.equal(canDrop([file], { id: 's', name: 'Site', kind: 'site', access: ADD }, ['s']).reason, 'Items can go only into a library or a folder.');
  assert.equal(canDrop([file], { id: 'd', name: 'x.txt', kind: 'document', access: ADD }, []).ok, false);
  assert.equal(canDrop([{ ...file, movable: false }], q1, ['lib', 'q1']).reason, 'You cannot move a.txt.');
  assert.equal(canDrop([file], { ...lib }, ['lib'], { from: 'lib' }).reason, 'a.txt is already in Documents.');
  assert.equal(canDrop([file, plans], { ...lib }, ['lib'], { from: 'lib' }).reason, 'They are already in Documents.');
  assert.deepEqual(canDrop([file, { id: 'g', name: 'b.txt' }], q1, ['lib', 'plans', 'q1'], { from: 'lib' }), {
    ok: true,
    message: 'Move 2 items to Q1',
  });
  assert.equal(canDrop([], q1, []).ok, false);
});

test('dragList carries the selection when the dragged row is in it', () => {
  const items = [{ id: 'a' }, { id: 'b' }, { id: 'c' }];
  assert.deepEqual(dragList('b', new Set(['a', 'b']), items).map((i) => i.id), ['a', 'b']);
  assert.deepEqual(dragList('c', new Set(['a', 'b']), items).map((i) => i.id), ['c']);
  assert.deepEqual(dragList('a', new Set(), items).map((i) => i.id), ['a']);
});

test('item drags and desktop file drags are told apart by their types', () => {
  assert.equal(isItemDrag([ITEM_TYPE, 'text/plain']), true);
  assert.equal(isFileDrag([ITEM_TYPE, 'text/plain']), false);
  assert.equal(isFileDrag(['Files']), true);
  assert.equal(isItemDrag(['Files']), false);
  assert.equal(isFileDrag(['text/uri-list']), false);
  assert.equal(isItemDrag(undefined), false);
});

test('clipboardKey: Ctrl or Command with X and V, Escape, never in text fields', () => {
  const row = { tagName: 'TR' };
  assert.equal(clipboardKey({ key: 'x', ctrlKey: true, target: row }), 'cut');
  assert.equal(clipboardKey({ key: 'X', metaKey: true, target: row }), 'cut');
  assert.equal(clipboardKey({ key: 'v', ctrlKey: true, target: row }), 'paste');
  assert.equal(clipboardKey({ key: 'Escape', target: row }), 'cancel');
  assert.equal(clipboardKey({ key: 'x', target: row }), null);
  assert.equal(clipboardKey({ key: 'v', ctrlKey: true, shiftKey: true, target: row }), null);
  assert.equal(clipboardKey({ key: 'x', ctrlKey: true, target: { tagName: 'INPUT', type: 'text' } }), null);
  assert.equal(clipboardKey({ key: 'x', ctrlKey: true, target: { tagName: 'INPUT', type: 'checkbox' } }), 'cut');
  assert.equal(clipboardKey({ key: 'x', ctrlKey: true, defaultPrevented: true, target: row }), null);
});

test('cut words and the kept cut list', () => {
  assert.equal(cutText([{ name: 'Budget.xlsx' }]), 'Budget.xlsx is ready to move');
  assert.equal(cutText([{ name: 'a' }, { name: 'b' }]), '2 items ready to move');
  const kept = JSON.stringify({ from: 'lib', fromName: 'Documents', items: [{ id: 'a', name: 'a.txt', extra: 1 }, { id: 3 }] });
  assert.deepEqual(parseCut(kept), { from: 'lib', fromName: 'Documents', items: [{ id: 'a', name: 'a.txt' }] });
  assert.equal(parseCut('nonsense'), null);
  assert.equal(parseCut(null), null);
  assert.equal(parseCut(JSON.stringify({ from: 'lib', items: [] })), null);
});

test('the library page has the folder tree, the Folders toggle and the cut bar', () => {
  const html = readFileSync(new URL('./library.html', import.meta.url), 'utf8');
  assert.match(html, /<ul id="folder-tree" role="tree" aria-labelledby="folder-tree-title"><\/ul>/);
  assert.match(html, /id="folders-toggle"[^>]*aria-haspopup="dialog"[^>]*aria-expanded="false"/);
  assert.match(html, /id="cut-bar"/);
  assert.doesNotMatch(html, /style=/);
  const css = readFileSync(new URL('./style.css', import.meta.url), 'utf8');
  assert.match(css, /\.side-body>\.chrome\+\.folder-pane\{/);
  assert.match(css, /\.drop-ok/);
});
