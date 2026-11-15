import test from 'node:test';
import assert from 'node:assert/strict';
import { LIBRARY_ICON_KINDS, itemCountText, libraryCardModel, ownerColour, recentAcrossLibraries } from './site-home.js';

const id = (n) => `00000000-0000-4000-8000-${String(n).padStart(12, '0')}`;
const doc = (n, at, extra = {}) => ({ id: id(n), kind: 'document', name: `File ${n}.docx`, editor: 'wordcraft', revised_at: at, ...extra });

test('recentAcrossLibraries merges the libraries, newest first, up to the limit', () => {
  const lists = [
    { library: { id: id(100), name: 'Documents' }, items: [doc(1, '2026-11-10T10:00:00Z'), doc(2, '2026-11-14T09:00:00Z')] },
    { library: { id: id(200), name: 'Brand' }, items: [doc(3, '2026-11-12T08:00:00Z'), doc(4, '2026-11-15T08:00:00Z')] },
  ];
  const recent = recentAcrossLibraries(lists, 3);
  assert.deepEqual(
    recent.map((file) => file.id),
    [id(4), id(2), id(3)],
  );
  assert.equal(recent[0].libraryName, 'Brand');
  assert.equal(recent[0].library, id(200));
  assert.equal(recent[0].at, '2026-11-15T08:00:00Z');
  assert.equal(recent[0].href, `/file.html?doc=${id(4)}`);
  assert.equal(recent[0].icon, 'word');
  assert.equal('ms' in recent[0], false);
});

test('recentAcrossLibraries skips folders, never-saved files, bad times and repeats', () => {
  const lists = [
    {
      library: { id: id(100), name: 'Documents' },
      items: [
        { id: id(1), kind: 'folder', name: 'Plans' },
        doc(2, undefined),
        doc(3, 'not a time'),
        doc(4, '2026-11-01T00:00:00Z', { name: 'notes.txt', editor: 'text' }),
        null,
      ],
    },
    { library: { id: id(100), name: 'Documents' }, items: [doc(4, '2026-11-01T00:00:00Z', { name: 'notes.txt', editor: 'text' })] },
    { library: { id: id(300), name: 'Empty' } },
  ];
  const recent = recentAcrossLibraries(lists);
  assert.equal(recent.length, 1);
  assert.equal(recent[0].href, `/doc.html#${id(4)}`);
  assert.equal(recent[0].icon, 'text');
  assert.deepEqual(recentAcrossLibraries(undefined), []);
  assert.deepEqual(recentAcrossLibraries([], 8), []);
  assert.deepEqual(recentAcrossLibraries(lists, 0), []);
});

test('recentAcrossLibraries breaks ties by name and defaults to eight', () => {
  const at = '2026-11-15T12:00:00Z';
  const items = Array.from({ length: 12 }, (_, i) => doc(i + 1, at, { name: `${String.fromCharCode(76 - i)}.png`, editor: undefined }));
  const recent = recentAcrossLibraries([{ library: { id: id(1), name: 'Images' }, items }]);
  assert.equal(recent.length, 8);
  assert.deepEqual(
    recent.map((file) => file.name),
    ['A.png', 'B.png', 'C.png', 'D.png', 'E.png', 'F.png', 'G.png', 'H.png'],
  );
  assert.equal(recent[0].icon, 'image');
});

test('libraryCardModel names the kind, the count and the access', () => {
  const card = libraryCardModel({
    id: id(5),
    name: 'Brand images',
    kind: 'image',
    count: 12,
    access: { perms: ['view'], roles: [{ name: 'Reader' }] },
    inherits: true,
  });
  assert.equal(card.href, `/library.html?id=${id(5)}`);
  assert.equal(card.name, 'Brand images');
  assert.equal(card.icon, 'image');
  assert.equal(card.kind, 'Image library');
  assert.equal(card.count, '12 items');
  assert.equal(card.limited, false);
  assert.equal(card.ownPermissions, false);
  assert.match(card.role, /Reader/);
});

test('libraryCardModel copes with an unknown kind, no count and limited access', () => {
  const card = libraryCardModel({ id: id(6), name: 'Old', kind: 'something', access: null, inherits: false });
  assert.equal(card.icon, 'library');
  assert.equal(card.kind, 'Library');
  assert.equal(card.count, null);
  assert.equal(card.role, 'Limited access');
  assert.equal(card.limited, true);
  assert.equal(card.ownPermissions, true);
  for (const kind of LIBRARY_ICON_KINDS) assert.equal(libraryCardModel({ id: id(1), name: 'x', kind }).icon, kind);
  assert.equal(libraryCardModel(undefined).name, '');
});

test('itemCountText says how many items in words', () => {
  assert.equal(itemCountText(0), 'Empty');
  assert.equal(itemCountText(1), '1 item');
  assert.equal(itemCountText(7), '7 items');
  assert.equal(itemCountText(100, true), '100 or more items');
  assert.equal(itemCountText(undefined), null);
  assert.equal(itemCountText(-1), null);
  assert.equal(itemCountText(2.5), null);
});

test('an owner keeps the same avatar colour', () => {
  assert.deepEqual(ownerColour('Ann Tester'), ownerColour('Ann Tester'));
  assert.ok(ownerColour(undefined).light.bg);
});
