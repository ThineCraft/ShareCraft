import test from 'node:test';
import assert from 'node:assert/strict';
import {
  accessSummary,
  changedBy,
  countText,
  firstLines,
  isToggleKey,
  paneShown,
  paneModel,
  peopleIn,
  previewKind,
  sizeFromAnswer,
} from './details.js';

const UTC = { language: 'en-GB', timeZone: 'UTC' };
const NOW = Date.parse('2026-11-15T15:00:00Z');
const OWNER = { perms: ['view', 'add', 'edit', 'delete', 'manage_access'], roles: [{ name: 'Owner' }] };
const VIEWER = { perms: ['view'], roles: [{ name: 'Viewer' }] };
const doc = (name, more = {}) => ({ id: 'd1', kind: 'document', name, editor: 'other', access: OWNER, inherits: true, revised_at: '2026-11-15T14:55:00Z', ...more });

test('previewKind chooses the preview by kind, scan, editor and ending', () => {
  assert.equal(previewKind({ kind: 'folder', name: 'Plans' }), 'folder');
  assert.equal(previewKind({ kind: 'library', name: 'Documents' }), 'folder');
  assert.equal(previewKind(doc('cat.png')), 'image');
  assert.equal(previewKind(doc('song.mp3')), 'audio');
  assert.equal(previewKind(doc('clip.mp4')), 'video');
  assert.equal(previewKind(doc('notes.md')), 'text');
  assert.equal(previewKind(doc('page.html')), 'text', 'web pages are only ever shown as text');
  assert.equal(previewKind(doc('data.bin')), 'none');
  assert.equal(previewKind(doc('Budget.xlsx', { editor: 'gridcraft' })), 'editor');
  assert.equal(previewKind(doc('cat.png', { revised_at: null })), 'empty');
  assert.equal(previewKind(doc('cat.png', { scan: 'held' })), 'held');
  assert.equal(previewKind(doc('cat.png', { scan: 'quarantined', threat: 'EICAR' })), 'quarantined');
  assert.equal(previewKind(doc('Budget.xlsx', { editor: 'gridcraft', scan: 'held' })), 'held', 'held wins over the editor');
  assert.equal(previewKind(null), 'none');
});

test('paneModel gives a file its facts, Open, Download and the actions the person may use', () => {
  const model = paneModel(doc('cat.png'), { size: 2048, access: undefined }, { now: NOW, regional: UTC });
  assert.equal(model.kind, 'image');
  assert.equal(model.icon, 'image');
  assert.deepEqual(
    model.facts.map((fact) => [fact.label, fact.value]),
    [['Type', 'PNG image'], ['Size', '2.0 KB'], ['Modified', '5 minutes ago']],
  );
  assert.match(model.facts[2].exact, /2026/);
  assert.equal(model.open.label, 'Open');
  assert.equal(model.open.href, '/file.html?doc=d1');
  assert.deepEqual(model.actions.map((action) => action.key), ['download', 'access', 'rename', 'move', 'delete']);
  assert.equal(model.actions[0].href, '/api/docs/d1/content');
  assert.equal(model.access.state, 'loading');
  assert.equal(model.access.href, '/access.html?id=d1');
  assert.equal(model.held, null);
  assert.equal(model.announce, 'cat.png, png image.');
});

test('paneModel hides content-changing actions in a read-only connected library', () => {
  const model = paneModel(doc('cat.png'), { size: 2048, access: undefined }, { now: NOW, regional: UTC, readOnly: true });
  assert.deepEqual(model.actions.map((action) => action.key), ['download', 'access']);
});

test('paneModel says Loading for a size not yet known, and leaves out an unknown one', () => {
  const loading = paneModel(doc('a.txt'), {}, { now: NOW, regional: UTC });
  assert.deepEqual(loading.facts[1], { label: 'Size', value: 'Loading…', loading: true });
  const unknown = paneModel(doc('a.txt'), { size: null }, { now: NOW, regional: UTC });
  assert.deepEqual(unknown.facts.map((fact) => fact.label), ['Type', 'Modified']);
  const by = paneModel(doc('a.txt'), { size: 0, by: 'Ann' }, { now: NOW, regional: UTC });
  assert.deepEqual(by.facts.map((fact) => fact.value).slice(1), ['0 bytes', '5 minutes ago', 'Ann']);
});

test('paneModel opens editor files in their editor and offers Download only once saved', () => {
  const sheet = paneModel(doc('Budget.xlsx', { editor: 'gridcraft' }), {}, { now: NOW, regional: UTC });
  assert.equal(sheet.open.label, 'Open in GridCraft');
  assert.ok(sheet.actions.some((action) => action.key === 'download'));
  const text = paneModel(doc('Notes', { editor: 'text' }), {}, { now: NOW, regional: UTC });
  assert.equal(text.open.label, 'Open in Text');
  assert.equal(text.open.href, '/doc.html#d1');
  assert.ok(text.actions.some((action) => action.key === 'download'));
  const unsaved = paneModel(doc('x.png', { revised_at: null }), {}, { now: NOW, regional: UTC });
  assert.deepEqual(unsaved.facts.map((fact) => fact.value), ['PNG image', 'Not saved yet']);
  assert.ok(!unsaved.actions.some((action) => action.key === 'download'));
});

test('paneModel never offers the bytes of held or quarantined content', () => {
  const held = paneModel(doc('cat.png', { scan: 'held' }), {}, { now: NOW, regional: UTC });
  assert.equal(held.kind, 'held');
  assert.equal(held.held[0], 'Being checked for malware.');
  assert.ok(!held.actions.some((action) => action.key === 'download'));
  assert.ok(!held.facts.some((fact) => fact.label === 'Size'));
  const bad = paneModel(doc('cat.png', { scan: 'quarantined', threat: 'EICAR-Test' }), {}, { now: NOW, regional: UTC });
  assert.match(bad.held[0], /EICAR-Test/);
  assert.match(bad.held[0], /quarantined/);
  assert.ok(!bad.actions.some((action) => action.key === 'download'));
});

test('paneModel gives a folder its count and its access summary instead of a preview', () => {
  const folder = { id: 'f1', kind: 'folder', name: 'Plans', access: OWNER, inherits: false };
  const loading = paneModel(folder, {}, { now: NOW, regional: UTC });
  assert.equal(loading.kind, 'folder');
  assert.deepEqual(loading.facts, [{ label: 'Type', value: 'Folder' }, { label: 'Contains', value: 'Loading…', loading: true }]);
  assert.equal(loading.open.label, 'Open folder');
  assert.equal(loading.open.href, '/library.html?id=f1');
  assert.ok(!loading.actions.some((action) => action.key === 'download'));
  const access = {
    inherits: false,
    source: { id: 'f1', name: 'Plans' },
    assignments: [
      { principal: { kind: 'group', id: 'g1', name: 'Projects Owners', site: { name: 'Projects' } }, role: { name: 'Owner' } },
      { principal: { kind: 'user', id: 'u2', name: 'Bob', email: 'bob@example.test' }, role: { name: 'Editor' } },
    ],
  };
  const done = paneModel(folder, { count: { count: 12, more: false }, access }, { now: NOW, regional: UTC });
  assert.equal(done.facts[1].value, '12 items');
  assert.equal(done.access.state, 'summary');
  assert.equal(done.access.headline, '2 people and groups');
  assert.equal(done.access.roles, 'Owner 1 · Editor 1');
  assert.equal(done.access.from, 'Has its own permissions.');
  assert.deepEqual(done.access.names, [{ name: 'Projects Owners', role: 'Owner' }, { name: 'Bob', role: 'Editor' }]);
});

test('paneModel shows a viewer their own access and only what they may do', () => {
  const model = paneModel(doc('a.pdf', { access: VIEWER }), { access: null }, { now: NOW, regional: UTC });
  assert.equal(model.access.state, 'own');
  assert.equal(model.access.text, 'You have Viewer access.');
  assert.equal(model.access.sharing, 'Shared like the place above.');
  assert.equal(model.access.href, undefined);
  assert.deepEqual(model.actions.map((action) => action.key), ['download']);
  const limited = paneModel(doc('a.pdf', { access: null, inherits: false }), { access: null }, { now: NOW, regional: UTC });
  assert.match(limited.access.text, /limited access/);
  assert.equal(limited.access.sharing, 'Has its own permissions.');
  const failed = paneModel(doc('a.pdf'), { access: { error: 'network' } }, { now: NOW, regional: UTC });
  assert.equal(failed.access.state, 'error');
});

test('accessSummary counts by role and names the first few', () => {
  const assignments = Array.from({ length: 6 }, (_, at) => ({
    principal: { kind: 'user', id: `u${at}`, name: `Person ${at}` },
    role: { name: at === 0 ? 'Owner' : 'Viewer' },
  }));
  const summary = accessSummary({ inherits: true, source: { name: 'Documents' }, assignments });
  assert.equal(summary.from, 'Shared like Documents, the place above.');
  assert.equal(summary.roles, 'Owner 1 · Viewer 5');
  assert.equal(summary.names.length, 4);
  assert.equal(summary.more, 2);
  assert.equal(accessSummary({ assignments: [] }).headline, 'Nobody is given access here.');
  const hidden = accessSummary({ assignments: [{ principal: { kind: 'group', hidden: true }, role: { name: 'Viewer' } }] });
  assert.equal(hidden.names[0].name, 'A group you cannot see');
  assert.equal(hidden.headline, '1 person or group');
});

test('countText and sizeFromAnswer', () => {
  assert.equal(countText({ count: 0 }), 'Empty');
  assert.equal(countText({ count: 1 }), '1 item');
  assert.equal(countText({ count: 500, more: true }), '500 or more items');
  assert.equal(sizeFromAnswer(206, 'bytes 0-0/2048', '1'), 2048);
  assert.equal(sizeFromAnswer(416, 'bytes */0', null), 0);
  assert.equal(sizeFromAnswer(200, null, '17'), 17);
  assert.equal(sizeFromAnswer(204, null, null), null);
  assert.equal(sizeFromAnswer(423, null, '40'), null);
  assert.equal(sizeFromAnswer(206, 'nonsense', null), null);
});

test('firstLines keeps the first lines and says when more was left out', () => {
  assert.deepEqual(firstLines('a\r\nb\nc', 2), { text: 'a\nb', cut: true });
  assert.deepEqual(firstLines('a\nb', 2), { text: 'a\nb', cut: false });
  assert.deepEqual(firstLines('a', 2, { more: true }), { text: 'a', cut: true });
});

test('changedBy names the maker of the current version only when an event matches it', () => {
  const events = [
    { kind: 'document.created', actor: 'u1', at: '2026-11-10T09:00:00Z' },
    { kind: 'editing.joined', actor: 'u3', at: '2026-11-15T14:54:00Z' },
    { kind: 'upload.completed', actor: 'u2', at: '2026-11-15T14:55:10Z' },
  ];
  const people = { u1: 'Ann', u2: 'Bob' };
  assert.equal(changedBy(events, '2026-11-15T14:55:00Z', people), 'Bob');
  assert.equal(changedBy(events, '2026-11-15T14:55:00Z', people, 'u2'), 'You');
  assert.equal(changedBy(events, '2026-11-10T09:00:30Z', people), 'Ann');
  assert.equal(changedBy(events, '2026-11-12T09:00:00Z', people), null, 'an edit in an editor has no matching event');
  assert.equal(changedBy(events, '2026-11-15T14:55:00Z', {}), null, 'an unknown person is not named');
  assert.equal(changedBy(null, '2026-11-15T14:55:00Z', people), null);
  assert.deepEqual(
    peopleIn({ assignments: [{ principal: { kind: 'user', id: 'u1', name: 'Ann' } }, { principal: { kind: 'group', id: 'g', name: 'G' } }] }),
    { u1: 'Ann' },
  );
});

test('isToggleKey takes "i" alone, outside text fields, menus and other dialogs', () => {
  const target = (more = {}) => ({ tagName: 'TR', closest: (selector) => more[selector] ?? null, ...more.self });
  const key = (key, t = target(), more = {}) => ({ key, target: t, ctrlKey: false, metaKey: false, altKey: false, defaultPrevented: false, ...more });
  assert.equal(isToggleKey(key('i')), true);
  assert.equal(isToggleKey(key('I')), true);
  assert.equal(isToggleKey(key('j')), false);
  assert.equal(isToggleKey(key('i', target(), { ctrlKey: true })), false);
  assert.equal(isToggleKey(key('i', { tagName: 'INPUT', type: 'text', closest: () => null })), false);
  assert.equal(isToggleKey(key('i', target({ '[role=menu]': {} }))), false);
  const other = { classList: { contains: () => false } };
  const sheet = { classList: { contains: (name) => name === 'details-sheet' } };
  assert.equal(isToggleKey(key('i', target({ dialog: other }))), false);
  assert.equal(isToggleKey(key('i', target({ dialog: sheet })), { sheetOpen: true }), true);
});

test('the details pane shows only with something selected, and gives the room back otherwise', () => {
  assert.equal(paneShown({ open: true, selected: true, wide: true }), 'column');
  assert.equal(paneShown({ open: true, selected: true, wide: false }), 'sheet');
  assert.equal(paneShown({ open: true, selected: false, wide: true }), null, 'no empty pane');
  assert.equal(paneShown({ open: false, selected: true, wide: true }), null);
  assert.equal(paneShown({ open: true, selected: true, wide: true, enabled: false }), null, 'not in the grid');
  assert.equal(paneShown(), null);
});
