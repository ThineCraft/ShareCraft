import test from 'node:test';
import assert from 'node:assert/strict';

import {
  binKind,
  binPathText,
  binSummary,
  daysLeftLevel,
  emptyText,
  eraseText,
  filterBin,
  groupByDeletedDate,
  kindChoices,
  restoredNote,
  restoredText,
  shownText,
} from './recycle.js';
import { buildUndo, undoOutcome } from './undo.js';

// Wednesday 18 November 2026, 10:00 UTC.
const NOW = Date.parse('2026-11-18T10:00:00Z');
const at = (iso, extra = {}) => ({ id: iso, name: iso, deleted_at: iso, ...extra });

test('groupByDeletedDate puts entries under Today, Yesterday, this week, this month and Older', () => {
  const items = [
    at('2026-11-18T09:00:00Z'),
    at('2026-11-18T00:30:00Z'),
    at('2026-11-17T23:00:00Z'),
    at('2026-11-16T08:00:00Z'), // Monday of this week
    at('2026-11-15T08:00:00Z'), // Sunday, last week
    at('2026-11-01T08:00:00Z'),
    at('2026-10-31T08:00:00Z'),
  ];
  const groups = groupByDeletedDate(items, NOW, 'UTC');
  assert.deepEqual(
    groups.map((g) => [g.heading, g.items.map((i) => i.id.slice(5, 10))]),
    [
      ['Today', ['11-18', '11-18']],
      ['Yesterday', ['11-17']],
      ['Earlier this week', ['11-16']],
      ['Earlier this month', ['11-15', '11-01']],
      ['Older', ['10-31']],
    ],
  );
});

test('groupByDeletedDate counts days in the time zone given', () => {
  // 23:00 UTC on the 17th is already the 18th in Auckland.
  const groups = groupByDeletedDate([at('2026-11-17T23:00:00Z')], NOW, 'Pacific/Auckland');
  assert.equal(groups[0].heading, 'Today');
  const la = groupByDeletedDate([at('2026-11-18T05:00:00Z')], NOW, 'America/Los_Angeles');
  assert.equal(la[0].heading, 'Yesterday');
});

test('groupByDeletedDate: yesterday wins over last week, the future is today, no time is older, empty groups are left out', () => {
  const monday = Date.parse('2026-11-16T12:00:00Z');
  const groups = groupByDeletedDate(
    [at('2026-11-16T13:00:00Z'), at('2026-11-15T10:00:00Z'), { id: 'x', name: 'x' }],
    monday,
    'UTC',
  );
  assert.deepEqual(groups.map((g) => g.id), ['today', 'yesterday', 'older']);
  assert.deepEqual(groupByDeletedDate([], NOW, 'UTC'), []);
  // An unknown zone falls back to the browser's rather than throwing.
  assert.equal(groupByDeletedDate([at('2026-11-18T09:00:00Z')], NOW, 'Not/AZone').length, 1);
});

test('daysLeftLevel warns in the last seven days', () => {
  assert.equal(daysLeftLevel(93), 'ok');
  assert.equal(daysLeftLevel(8), 'ok');
  assert.equal(daysLeftLevel(7), 'warn');
  assert.equal(daysLeftLevel(1), 'warn');
  assert.equal(daysLeftLevel(0), 'due');
  assert.equal(daysLeftLevel(-2), 'due');
  assert.equal(daysLeftLevel(null), 'ok');
});

test('binSummary counts items, adds sizes and says the next removal', () => {
  const items = [
    { size: 2 * 1024 * 1024, days_left: 90 },
    { size: 300 * 1024, days_left: 3 },
    { size: 0, days_left: 50 },
  ];
  assert.equal(binSummary(items, { language: 'en' }), '3 items · 2.3 MB · next removal in 3 days');
  assert.equal(binSummary([{ size: 0, days_left: 1 }], { language: 'en' }), '1 item · 0 bytes · next removal in 1 day');
  assert.equal(binSummary([{ size: 5, days_left: 0 }], { language: 'en' }), '1 item · 5 bytes · next removal within a day');
  assert.equal(binSummary(items, { more: true, language: 'en' }).startsWith('3+ items'), true);
  assert.equal(binSummary([]), '');
});

test('binPathText, binKind and kindChoices', () => {
  assert.equal(binPathText({ path: ['Home', 'Documents', 'Plans'] }), 'Home › Documents › Plans');
  assert.equal(binPathText({ path: [] }), '');
  assert.equal(binKind({ kind: 'library', name: 'Docs' }), 'library');
  assert.equal(binKind({ kind: 'folder', name: 'a.zip' }), 'folder');
  assert.equal(binKind({ kind: 'document', name: 'Plan.docx', editor: 'wordcraft' }), 'word');
  assert.equal(binKind({ kind: 'document', name: 'photo.JPG', editor: 'other' }), 'image');
  assert.deepEqual(
    kindChoices([{ kind: 'document', name: 'a.png' }, { kind: 'folder', name: 'f' }, { kind: 'document', name: 'b.png' }]),
    [
      { value: 'folder', label: 'Folders' },
      { value: 'image', label: 'Pictures' },
    ],
  );
});

test('filterBin finds words in the name or path, and filters by who and type', () => {
  const items = [
    { name: 'Budget.xlsx', editor: 'gridcraft', kind: 'document', path: ['Home', 'Documents', 'Plans'], deleted_by: { id: 'ann' } },
    { name: 'Café notes.txt', kind: 'document', path: ['Home', 'Documents'], deleted_by: { id: 'bob' } },
    { name: 'Plans', kind: 'folder', path: ['Home', 'Documents'], deleted_by: null },
  ];
  const names = (list) => list.map((i) => i.name);
  assert.deepEqual(names(filterBin(items, { text: 'plans' })), ['Budget.xlsx', 'Plans']);
  assert.deepEqual(names(filterBin(items, { text: 'cafe' })), ['Café notes.txt']);
  assert.deepEqual(names(filterBin(items, { text: 'budget plans' })), ['Budget.xlsx']);
  assert.deepEqual(names(filterBin(items, { by: 'me', me: 'ann' })), ['Budget.xlsx']);
  assert.deepEqual(names(filterBin(items, { kind: 'folder' })), ['Plans']);
  assert.deepEqual(names(filterBin(items, { text: '  ' })), names(items));
  assert.equal(shownText(1, 3, { text: 'x' }), 'Showing 1 of 3');
  assert.equal(shownText(3, 3, {}), '');
});

test('restoredNote says where an item went when its folder is gone, and renames', () => {
  const entry = { name: 'Budget.xlsx', path: ['Home', 'Documents', 'Plans'] };
  assert.equal(restoredNote(entry, ['Home', 'Documents', 'Plans']), '');
  assert.equal(
    restoredNote(entry, ['Home', 'Documents']),
    '“Budget.xlsx” went to Home › Documents, because the folder “Plans” it was in is gone.',
  );
  assert.equal(restoredNote(entry, ['Home', 'Documents'], null, { alone: true }), 'It went to Home › Documents, because the folder “Plans” it was in is gone.');
  assert.equal(restoredNote(entry, null, { from: 'Budget.xlsx', to: 'Budget (2).xlsx' }), '“Budget.xlsx” is now “Budget (2).xlsx”, because its name was taken.');
  assert.equal(restoredText([{ name: 'a' }]), 'Restored “a”.');
  assert.equal(restoredText([{ name: 'a' }, { name: 'b' }, { name: 'c' }], ['', 'Note.']), 'Restored 3 items. Note.');
});

test('confirmation and empty words', () => {
  assert.match(eraseText(3), /3 items.*cannot be undone/);
  assert.equal(emptyText('first'), 'Nothing in the recycle bin. Deleted files stay here for 93 days.');
  assert.match(emptyText('second'), /second stage/);
});

test('buildUndo of a restore deletes again through the delete routes', () => {
  const one = buildUndo({ op: 'restore', done: ['a'] }, [{ id: 'a', name: 'A' }]);
  assert.deepEqual(one.requests, [{ method: 'DELETE', path: '/nodes/a', ids: ['a'], bulk: false }]);
  assert.equal(one.message, 'Restored A');
  const many = buildUndo({ op: 'restore', done: ['a', 'b'] }, [{ id: 'a', name: 'A' }, { id: 'b', name: 'B' }, { id: 'c', name: 'C' }]);
  assert.deepEqual(many.requests, [{ method: 'POST', path: '/bulk', body: { op: 'delete', items: ['a', 'b'] }, ids: ['a', 'b'], bulk: true }]);
  assert.equal(undoOutcome(many, [{ id: 'a', ok: true }, { id: 'b', ok: true }]).message, 'Moved 2 items back to the recycle bin.');
  assert.match(undoOutcome(many, [{ id: 'a', ok: true }, { id: 'b', ok: false, error: 'forbidden' }]).message, /^Only 1 of 2 items were moved back to the recycle bin\./);
});
