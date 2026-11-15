import { test } from 'node:test';
import assert from 'node:assert/strict';

import {
  accessDiff,
  diffBadge,
  diffWords,
  exceptionCounts,
  foldText,
  mapRows,
  summariseFilters,
  whoText,
} from './permission-map.js';

const VIEWER = { id: 'r-view', name: 'Viewer' };
const EDITOR = { id: 'r-edit', name: 'Editor' };
const OWNER = { id: 'r-own', name: 'Owner' };
const view = ['view'];
const edit = ['view', 'add', 'edit', 'delete'];
const own = ['view', 'add', 'edit', 'delete', 'manage_access', 'manage_site'];

const bob = { kind: 'user', id: 'u-bob', name: 'Bob Tester' };
const ann = { kind: 'user', id: 'u-ann', name: 'Ann Tester' };
const members = { kind: 'group', id: 'g-mem', name: 'Home Members', site: null };
const owners = { kind: 'group', id: 'g-own', name: 'Home Owners', site: null };

test('accessDiff finds added, removed and changed entries, in words', () => {
  const above = [
    { principal: owners, role: OWNER, perms: own },
    { principal: members, role: EDITOR, perms: edit },
    { principal: bob, role: VIEWER, perms: view },
  ];
  const here = [
    { principal: owners, role: OWNER, perms: own },
    { principal: bob, role: EDITOR, perms: edit },
    { principal: ann, role: VIEWER, perms: view },
  ];
  const diff = accessDiff(above, here);
  assert.equal(diff.same.length, 1);
  assert.deepEqual(diff.items.map((item) => item.change), ['changed', 'added', 'removed']);
  assert.equal(diff.changed[0].text, 'Bob Tester can edit here; above, they can only view.');
  assert.equal(diff.changed[0].before.role.name, 'Viewer');
  assert.equal(diff.added[0].text, 'Ann Tester can view here; the place above does not list them.');
  assert.equal(diff.removed[0].text, 'Everyone in Home Members can edit above, but is not listed here.');
  assert.equal(diffBadge(diff), '+1 −1 →1');
  assert.equal(diffWords(diff), '1 added, 1 removed and 1 role changed');
});

test('accessDiff words a weaker role here, and a different role at the same level', () => {
  const weaker = accessDiff([{ principal: bob, role: EDITOR, perms: edit }], [{ principal: bob, role: VIEWER, perms: view }]);
  assert.equal(weaker.changed[0].text, 'Bob Tester can only view here; above, they can edit.');
  const contributor = { id: 'r-con', name: 'Contributor' };
  const level = accessDiff(
    [{ principal: bob, role: EDITOR, perms: edit }],
    [{ principal: bob, role: contributor, perms: ['view', 'add', 'edit'] }],
  );
  assert.match(level.changed[0].text, /^Bob Tester is Contributor here \(can view, add, edit\); above, Editor \(can view, add, edit, delete\)\.$/);
});

test('accessDiff with nothing changed, and with empty or missing lists', () => {
  const same = accessDiff([{ principal: bob, role: VIEWER, perms: view }], [{ principal: bob, role: VIEWER, perms: ['view'] }]);
  assert.equal(same.items.length, 0);
  assert.equal(diffBadge(same), '');
  assert.equal(diffWords(same), '');
  assert.equal(accessDiff(undefined, undefined).items.length, 0);
  assert.equal(diffBadge(null), '');
  const fresh = accessDiff([], [{ principal: members, role: VIEWER, perms: view }]);
  assert.equal(diffBadge(fresh), '+1');
});

test('whoText names people, groups, hidden groups and links', () => {
  assert.equal(whoText(bob), 'Bob Tester');
  assert.equal(whoText(members), 'Everyone in Home Members');
  assert.equal(whoText({ kind: 'group', id: 'g', name: null, hidden: true }), 'A group you cannot see');
  assert.equal(whoText({ kind: 'link', id: 'l', link_kind: 'signed_in' }), 'A signed-in link');
  assert.equal(whoText({ kind: 'link', id: 'l', link_kind: 'anyone' }), 'An anyone link');
  assert.equal(whoText(null), 'Someone');
});

const row = (id, { unique = false, count, expandable = true } = {}) => ({ id, name: id, kind: 'folder', unique, count, expandable });

test('exceptionCounts adds up exceptions at and below every row', () => {
  const root = row('site', { unique: true });
  const a = row('a', { unique: true });
  const a1 = row('a1', { unique: true, expandable: false });
  const b = row('b', { count: 2 }); // not loaded: the server's count stands
  const c = row('c', { count: 0 });
  const loaded = new Map([
    ['site', [a, b, c]],
    ['a', [a1]],
  ]);
  const counts = exceptionCounts({ root, loaded });
  assert.deepEqual(counts.get('site'), { exceptions: 5, below: 4, quiet: 1 });
  assert.deepEqual(counts.get('a'), { exceptions: 2, below: 1, quiet: 0 });
  assert.deepEqual(counts.get('b'), { exceptions: 2, below: 2, quiet: 0 });
  assert.deepEqual(counts.get('c'), { exceptions: 0, below: 0, quiet: 0 });
  assert.equal(exceptionCounts({ root: null }).size, 0);
});

test('mapRows folds three or more inheriting children into one row, and reveals them', () => {
  const root = row('site', { unique: true });
  const own = row('own', { unique: true, expandable: false });
  const quiet = ['q1', 'q2', 'q3'].map((id) => row(id, { count: 0 }));
  const loaded = new Map([['site', [quiet[0], own, quiet[1], quiet[2]]]]);
  const open = new Set(['site']);
  const folded = mapRows({ root, loaded, open });
  assert.deepEqual(folded.map((r) => r.id), ['site', 'own', 'fold:site']);
  assert.equal(folded[2].count, 3);
  assert.equal(folded[2].revealed, false);
  assert.deepEqual([folded[1].posinset, folded[1].setsize, folded[2].posinset], [1, 2, 2]);
  const shown = mapRows({ root, loaded, open, revealed: new Set(['site']) });
  assert.deepEqual(shown.map((r) => r.id), ['site', 'q1', 'own', 'q2', 'q3', 'fold:site']);
  assert.equal(shown[5].revealed, true);
  assert.equal(shown[1].quiet, true);
  assert.equal(shown[2].quiet, false);
  // two quiet children stay in the tree as they are
  const two = mapRows({ root, loaded: new Map([['site', [quiet[0], own, quiet[1]]]]), open });
  assert.deepEqual(two.map((r) => r.id), ['site', 'q1', 'own', 'q2']);
});

test('mapRows: closed rows hide their children, depth grows, more rows come last', () => {
  const root = row('site', { unique: true });
  const lib = row('lib', { count: 1 });
  const deep = row('deep', { unique: true });
  const loaded = new Map([
    ['site', [lib]],
    ['lib', [deep]],
  ]);
  assert.deepEqual(mapRows({ root, loaded, open: new Set(['site']) }).map((r) => r.id), ['site', 'lib']);
  const rows = mapRows({ root, loaded, open: new Set(['site', 'lib']), more: new Set(['lib']) });
  assert.deepEqual(rows.map((r) => [r.id, r.depth]), [['site', 0], ['lib', 1], ['deep', 2], ['more:lib', 2]]);
  assert.equal(rows[3].type, 'more');
  assert.equal(rows[3].setsize, 2);
  assert.deepEqual(mapRows({ root: null }), []);
});

test('foldText', () => {
  assert.equal(foldText(12, false), '12 places inherit — show');
  assert.equal(foldText(1, false), '1 place inherits — show');
  assert.equal(foldText(12, true), 'Hide the 12 places that inherit');
});

test('summariseFilters says what is shown', () => {
  assert.equal(summariseFilters({ only: true, who: 'Bob Tester', exceptions: 3 }), 'Showing 3 exceptions for Bob Tester.');
  assert.equal(summariseFilters({ exceptions: 2 }), 'Showing every place: 2 have permissions of their own.');
  assert.equal(summariseFilters({ exceptions: 1 }), 'Showing every place: 1 has permissions of its own.');
  assert.equal(summariseFilters({}), 'Showing every place: none has permissions of its own.');
  assert.equal(summariseFilters({ only: true, exceptions: 1 }), 'Showing 1 exception.');
  assert.equal(
    summariseFilters({ kinds: ['document', 'folder'], who: 'Home Members', exceptions: 4 }),
    'Showing 4 folder and file exceptions for Home Members.',
  );
  assert.equal(summariseFilters({ kinds: ['library'], exceptions: 0 }), 'No library exceptions.');
  assert.equal(summariseFilters({ only: true, exceptions: 200, more: true }), 'Showing the first 200 exceptions (more to load).');
});
