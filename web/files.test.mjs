import test from 'node:test';
import assert from 'node:assert/strict';
import {
  DEFAULT_SORT,
  FILE_KINDS,
  fileIcon,
  fileKind,
  nextSort,
  parseSort,
  relativeTime,
  sortDescription,
  sortEntries,
  typeLabel,
} from './files.js';

const UTC = { language: 'en-GB', timeZone: 'UTC' };
const NOW = Date.parse('2026-11-15T15:00:00Z');

test('relativeTime says just now, minutes and hours ago', () => {
  const ago = (ms) => relativeTime(new Date(NOW - ms), NOW, UTC);
  assert.equal(ago(0), 'just now');
  assert.equal(ago(59_000), 'just now');
  assert.equal(ago(60_000), '1 minute ago');
  assert.equal(ago(5 * 60_000), '5 minutes ago');
  assert.equal(ago(59 * 60_000), '59 minutes ago');
  assert.equal(ago(60 * 60_000), '1 hour ago');
  assert.equal(ago(14 * 60 * 60_000), '14 hours ago');
  // A little in the future (clocks differ) is still just now.
  assert.equal(ago(-2 * 60_000), 'just now');
});

test('relativeTime says yesterday, then the day and month, with the year when it differs', () => {
  assert.equal(relativeTime('2026-11-14T09:00:00Z', NOW, UTC), 'yesterday');
  assert.equal(relativeTime('2026-11-12T09:00:00Z', NOW, UTC), '12 Nov');
  assert.equal(relativeTime('2025-11-12T09:00:00Z', NOW, UTC), '12 Nov 2025');
  // Within six hours across midnight stays in hours.
  const early = Date.parse('2026-11-15T02:00:00Z');
  assert.equal(relativeTime('2026-11-14T23:00:00Z', early, UTC), '3 hours ago');
  assert.equal(relativeTime('2026-11-14T18:00:00Z', early, UTC), 'yesterday');
  // The calendar day is the time zone's: 23:30 UTC is already the 15th in Auckland.
  assert.equal(
    relativeTime('2026-11-14T10:00:00Z', Date.parse('2026-11-14T23:30:00Z'), { language: 'en-GB', timeZone: 'Pacific/Auckland' }),
    'yesterday',
  );
  assert.equal(relativeTime('2026-11-12T09:00:00Z', NOW, { language: 'en-US', timeZone: 'UTC' }), 'Nov 12');
  assert.equal(relativeTime(null, NOW, UTC), '');
  assert.equal(relativeTime('soon', NOW, UTC), 'soon');
});

test('fileKind goes by folder, editor, then the ending of the name', () => {
  assert.equal(fileKind('Plans', undefined, 'folder'), 'folder');
  assert.equal(fileKind('Shared', undefined, 'library'), 'folder');
  assert.equal(fileKind('notes', 'text'), 'text');
  assert.equal(fileKind('Report.docx', 'wordcraft'), 'word');
  assert.equal(fileKind('Budget.xlsx', 'gridcraft'), 'sheet');
  assert.equal(fileKind('Scan.pdf', 'pdfcraft'), 'pdf');
  assert.equal(fileKind('Bracket.solidcraft.json', 'solidcraft'), 'model');
  assert.equal(fileKind('Photo.JPG', 'other'), 'image');
  assert.equal(fileKind('Song.mp3', 'other'), 'audio');
  assert.equal(fileKind('Clip.mov', 'other'), 'video');
  assert.equal(fileKind('Backup.tar.gz', 'other'), 'archive');
  assert.equal(fileKind('Old.doc', 'other'), 'word');
  assert.equal(fileKind('data.csv', 'other'), 'sheet');
  assert.equal(fileKind('setup.exe', 'other'), 'file');
  assert.equal(fileKind('README', 'other'), 'file');
  assert.equal(fileKind('.hidden', 'other'), 'file');
});

test('typeLabel names the type in words', () => {
  assert.equal(typeLabel({ kind: 'folder', name: 'A' }), 'Folder');
  assert.equal(typeLabel({ kind: 'library', name: 'A' }), 'Library');
  assert.equal(typeLabel({ kind: 'document', name: 'R.docx', editor: 'wordcraft' }), 'Document');
  assert.equal(typeLabel({ kind: 'document', name: 'B.xlsx', editor: 'gridcraft' }), 'Spreadsheet');
  assert.equal(typeLabel({ kind: 'document', name: 'p.png', editor: 'other' }), 'PNG image');
  assert.equal(typeLabel({ kind: 'document', name: 's.flac', editor: 'other' }), 'FLAC audio');
  assert.equal(typeLabel({ kind: 'document', name: 'a.zip', editor: 'other' }), 'ZIP archive');
  assert.equal(typeLabel({ kind: 'document', name: 'x.bin', editor: 'other' }), 'BIN file');
  assert.equal(typeLabel({ kind: 'document', name: 'README', editor: 'other' }), 'File');
});

const entries = [
  { id: '1', kind: 'document', name: 'beta.txt', editor: 'text', revised_at: '2026-11-15T10:00:00Z' },
  { id: '2', kind: 'folder', name: 'Zeta' },
  { id: '3', kind: 'document', name: 'Alpha.xlsx', editor: 'gridcraft', revised_at: '2026-11-10T10:00:00Z' },
  { id: '4', kind: 'document', name: 'file10.png', editor: 'other', revised_at: '2026-11-14T10:00:00Z' },
  { id: '5', kind: 'document', name: 'file9.png', editor: 'other' },
  { id: '6', kind: 'folder', name: 'alpha' },
];
const names = (list) => list.map((entry) => entry.name);

test('sortEntries keeps folders first and sorts names naturally', () => {
  assert.deepEqual(names(sortEntries(entries, 'name', 'ascending')), ['alpha', 'Zeta', 'Alpha.xlsx', 'beta.txt', 'file9.png', 'file10.png']);
  assert.deepEqual(names(sortEntries(entries, 'name', 'descending')), ['Zeta', 'alpha', 'file10.png', 'file9.png', 'beta.txt', 'Alpha.xlsx']);
  // A new array; the one given is left as it was.
  assert.equal(entries[0].name, 'beta.txt');
});

test('sortEntries by modified puts entries with no time last either way', () => {
  assert.deepEqual(names(sortEntries(entries, 'modified', 'descending')), ['alpha', 'Zeta', 'beta.txt', 'file10.png', 'Alpha.xlsx', 'file9.png']);
  assert.deepEqual(names(sortEntries(entries, 'modified', 'ascending')), ['alpha', 'Zeta', 'Alpha.xlsx', 'file10.png', 'beta.txt', 'file9.png']);
});

test('sortEntries by type groups types, then names', () => {
  assert.deepEqual(names(sortEntries(entries, 'type', 'ascending')), ['alpha', 'Zeta', 'file9.png', 'file10.png', 'Alpha.xlsx', 'beta.txt']);
  assert.deepEqual(names(sortEntries(entries, 'type', 'descending')), ['alpha', 'Zeta', 'beta.txt', 'Alpha.xlsx', 'file9.png', 'file10.png']);
});

test('the remembered sort is checked, and headers toggle or start fresh', () => {
  assert.deepEqual(parseSort('{"key":"modified","direction":"descending"}'), { key: 'modified', direction: 'descending' });
  assert.deepEqual(parseSort('{"key":"size","direction":"descending"}'), DEFAULT_SORT);
  assert.deepEqual(parseSort('not json'), DEFAULT_SORT);
  assert.deepEqual(parseSort(null), DEFAULT_SORT);
  assert.deepEqual(nextSort(DEFAULT_SORT, 'name'), { key: 'name', direction: 'descending' });
  assert.deepEqual(nextSort(DEFAULT_SORT, 'modified'), { key: 'modified', direction: 'descending' });
  assert.deepEqual(nextSort({ key: 'modified', direction: 'descending' }, 'type'), { key: 'type', direction: 'ascending' });
  assert.equal(sortDescription({ key: 'modified', direction: 'descending' }), 'Sorted by modified, newest first.');
  assert.equal(sortDescription(DEFAULT_SORT), 'Sorted by name, A to Z.');
});

test('fileIcon builds a hidden SVG for every kind', () => {
  const made = [];
  const doc = {
    createElementNS(ns, tag) {
      const element = {
        ns,
        tag,
        attributes: {},
        children: [],
        setAttribute(name, value) {
          this.attributes[name] = value;
        },
        append(...children) {
          this.children.push(...children);
        },
      };
      made.push(element);
      return element;
    },
  };
  for (const kind of FILE_KINDS) {
    const svg = fileIcon(kind, doc);
    assert.equal(svg.ns, 'http://www.w3.org/2000/svg');
    assert.equal(svg.attributes['aria-hidden'], 'true');
    assert.equal(svg.attributes.class, `file-icon k-${kind}`);
    assert.ok(svg.children.length >= 1, kind);
  }
  assert.equal(fileIcon('nonsense', doc).attributes.class, 'file-icon k-file');
});
