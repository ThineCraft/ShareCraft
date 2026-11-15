import test from 'node:test';
import assert from 'node:assert/strict';
import {
  RECENT_MAX,
  addRecent,
  firstName,
  forgetOpened,
  greeting,
  hourIn,
  parseRecent,
  readRecent,
  recentKey,
  rememberOpened,
  removeRecent,
  siteCardModel,
} from './start.js';

const id = (n) => `00000000-0000-4000-8000-${String(n).padStart(12, '0')}`;
const file = (n, at = n) => ({ id: id(n), name: `File ${n}.xlsx`, editor: 'gridcraft', site: 'Sales', at });

test('greeting follows the time of day and uses the first name', () => {
  assert.equal(greeting(5, 'Ann Smith'), 'Good morning, Ann');
  assert.equal(greeting(11, 'Ann'), 'Good morning, Ann');
  assert.equal(greeting(12, 'Ann'), 'Good afternoon, Ann');
  assert.equal(greeting(17, 'Ann'), 'Good afternoon, Ann');
  assert.equal(greeting(18, 'Ann'), 'Good evening, Ann');
  assert.equal(greeting(0, 'Ann'), 'Good evening, Ann');
  assert.equal(greeting(4, 'Ann'), 'Good evening, Ann');
  assert.equal(greeting(9, ''), 'Good morning');
  assert.equal(greeting(9, null), 'Good morning');
  assert.equal(greeting(9, '  Bo   Lee '), 'Good morning, Bo');
  assert.equal(firstName(undefined), '');
});

test('hourIn reads the hour in a time zone', () => {
  const at = Date.parse('2026-11-15T07:30:00Z');
  assert.equal(hourIn(at, 'UTC'), 7);
  assert.equal(hourIn(at, 'Asia/Tokyo'), 16);
  assert.equal(hourIn(Date.parse('2026-11-15T00:10:00Z'), 'UTC'), 0);
  // An unknown zone falls back to the browser's clock rather than throwing.
  assert.equal(typeof hourIn(at, 'Not/AZone'), 'number');
});

test('siteCardModel gives the card one link, words and a checked look', () => {
  const card = siteCardModel({
    id: id(1),
    name: 'Sales',
    kind: 'communication',
    hub: { id: id(2), name: 'Commercial' },
    is_hub: false,
    description: '  Leads and offers  ',
    access: { roles: [{ name: 'Owner' }], perms: ['view'] },
    accent: '#1F5F8B',
    thumbnail_url: `/api/brand/images/${'a'.repeat(64)}`,
  });
  assert.equal(card.href, `/site.html?id=${id(1)}`);
  assert.equal(card.name, 'Sales');
  assert.equal(card.kind, 'Communication site');
  assert.equal(card.hub, 'Commercial');
  assert.equal(card.isHub, false);
  assert.equal(card.description, 'Leads and offers');
  assert.equal(card.role, 'Your role: Owner');
  assert.equal(card.accent, '#1f5f8b');
  assert.equal(card.thumbnail, `/api/brand/images/${'a'.repeat(64)}`);

  const plain = siteCardModel({ id: id(3), name: 'HQ', kind: 'team', is_hub: true, access: null, accent: 'red;x', thumbnail_url: 'https://evil.example/x.png' });
  assert.equal(plain.kind, 'Team site');
  assert.equal(plain.isHub, true);
  assert.equal(plain.hub, null);
  assert.equal(plain.role, 'Limited access');
  assert.equal(plain.accent, null);
  assert.equal(plain.thumbnail, null);
  assert.equal(plain.description, '');
  assert.equal(siteCardModel({ id: id(4), name: 'X', access: { roles: [] } }).role, 'You are a member');
});

test('addRecent puts the newest first, once, and keeps eight', () => {
  let list = [];
  for (let n = 1; n <= 10; n += 1) list = addRecent(list, file(n));
  assert.equal(list.length, RECENT_MAX);
  assert.deepEqual(list.map((item) => item.name), [10, 9, 8, 7, 6, 5, 4, 3].map((n) => `File ${n}.xlsx`));
  // Opening a file again moves it to the front instead of listing it twice.
  list = addRecent(list, { ...file(5, 99), name: 'Renamed.xlsx' });
  assert.equal(list.length, RECENT_MAX);
  assert.equal(list[0].id, id(5));
  assert.equal(list[0].name, 'Renamed.xlsx');
  assert.equal(list.filter((item) => item.id === id(5)).length, 1);
  // A bad entry changes nothing.
  assert.deepEqual(addRecent(list, { id: 'nope', name: 'x', at: 1 }), list);
});

test('parseRecent drops bad entries, dedupes and orders by time', () => {
  assert.deepEqual(parseRecent('not json'), []);
  assert.deepEqual(parseRecent('{"a":1}'), []);
  const list = parseRecent(JSON.stringify([file(1, 5), { id: 'x', name: 'y', at: 1 }, file(2, 9), file(1, 2), { ...file(3), name: '' }, null]));
  assert.deepEqual(list.map((item) => [item.id, item.at]), [[id(2), 9], [id(1), 5]]);
  assert.equal(parseRecent([{ ...file(4), editor: 7 }])[0].editor, null);
  assert.deepEqual(removeRecent(list, id(2)).map((item) => item.id), [id(1)]);
});

test('rememberOpened and forgetOpened keep each person separate and never throw', () => {
  const data = new Map();
  const store = { getItem: (k) => data.get(k) ?? null, setItem: (k, v) => data.set(k, v) };
  rememberOpened('ann', { id: id(1), name: 'Plan.docx', editor: 'wordcraft', site: 'HR' }, { now: 10, store });
  rememberOpened('ann', { id: id(2), name: 'Notes.txt', editor: 'text', site: 'HR' }, { now: 20, store });
  rememberOpened('bo', { id: id(3), name: 'Bo.txt', editor: 'text', site: 'HR' }, { now: 30, store });
  assert.deepEqual(readRecent('ann', store).map((item) => item.name), ['Notes.txt', 'Plan.docx']);
  assert.deepEqual(readRecent('bo', store).map((item) => item.name), ['Bo.txt']);
  assert.ok(data.has(recentKey('ann')));
  assert.deepEqual(forgetOpened('ann', id(2), store).map((item) => item.name), ['Plan.docx']);
  assert.deepEqual(readRecent('ann', store).map((item) => item.name), ['Plan.docx']);

  const broken = { getItem() { throw new Error('blocked'); }, setItem() { throw new Error('blocked'); } };
  assert.deepEqual(readRecent('ann', broken), []);
  assert.doesNotThrow(() => rememberOpened('ann', file(1), { store: broken }));
  assert.deepEqual(forgetOpened('ann', id(1), broken), []);
  assert.deepEqual(readRecent('ann', null), []);
});
