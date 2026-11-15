// The notifications inbox (round 23): requests in words, what counts as new, the bell's label,
// and the remembered "last opened" time.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { badgeText, bellLabel, describeRequest, inboxModel, nextSeen, readSeen, refreshOnShow, seenKey, unreadCount, writeSeen, REFRESH_ON_SHOW_AFTER } from './inbox.js';

const ID = '0f0e0d0c-0b0a-4908-8706-050403020100';
const HOME = { id: 's1', kind: 'site', name: 'Home', access: { perms: ['view'], roles: [] } };
const DOCS = { id: 'l1', kind: 'library', name: 'Documents', access: { perms: ['view'], roles: [] } };
const manage = { perms: ['view', 'add', 'edit', 'delete', 'manage_access'], roles: [] };

const openRequest = (over = {}) => ({
  id: 'r1',
  node: { id: ID, kind: 'document', name: 'Budget.xlsx', access: manage },
  path: [HOME, DOCS],
  requester: { id: 'u2', name: 'Bob Tester' },
  message: 'For the quarterly review, please.',
  created_at: '2026-11-15T10:00:00Z',
  ...over,
});

const myRequest = (over = {}) => ({
  id: 'm1',
  node: { id: ID, kind: 'document', name: 'Budget.xlsx', access: { perms: ['view', 'edit'], roles: [] } },
  path: [HOME, DOCS],
  decision: 'approved',
  role: { id: 'editor', name: 'Editor' },
  reply: null,
  created_at: '2026-11-14T09:00:00Z',
  decided_at: '2026-11-15T09:30:00Z',
  ...over,
});

test('an open request names the person, the item with its place and icon, and their message', () => {
  const item = describeRequest(openRequest());
  assert.equal(item.section, 'decide');
  assert.equal(item.status, 'open');
  assert.equal(item.headline, 'Bob Tester asked for access');
  assert.deepEqual(item.person, { id: 'u2', name: 'Bob Tester' });
  assert.equal(item.name, 'Budget.xlsx');
  assert.equal(item.icon, 'sheet');
  assert.equal(item.where, 'Home › Documents');
  assert.equal(item.href, `/file.html?doc=${ID}`);
  assert.equal(item.message, 'For the quarterly review, please.');
  assert.equal(item.at, '2026-11-15T10:00:00Z');
  assert.equal(item.label, 'Bob Tester asked for access to the file Budget.xlsx in Home › Documents. Their message: For the quarterly review, please.');
  const folder = describeRequest(openRequest({ node: { id: 'f1', kind: 'folder', name: 'Plans', access: manage }, message: '  ' }));
  assert.equal(folder.icon, 'folder');
  assert.equal(folder.href, '/library.html?id=f1');
  assert.equal(folder.message, '');
  assert.match(folder.label, /the folder Plans in Home › Documents\.$/);
});

test('your requests say approved with the role and link the item, declined with the reply, or waiting', () => {
  const approved = describeRequest(myRequest());
  assert.equal(approved.section, 'yours');
  assert.equal(approved.status, 'approved');
  assert.equal(approved.statusText, 'Approved as Editor');
  assert.equal(approved.headline, 'You can now open Budget.xlsx as Editor');
  assert.equal(approved.href, `/file.html?doc=${ID}`);
  assert.equal(approved.at, '2026-11-15T09:30:00Z');

  const declined = describeRequest(myRequest({ decision: 'declined', role: null, reply: 'Ask Ann instead.', node: null, path: [] }));
  assert.equal(declined.status, 'declined');
  assert.equal(declined.statusText, 'Declined');
  assert.equal(declined.headline, 'Your request for an item you cannot see was declined');
  assert.equal(declined.href, null);
  assert.equal(declined.reply, 'Ask Ann instead.');
  assert.match(declined.label, /Their reply: Ask Ann instead\.$/);

  // a declined item the person can still see is not linked as if access was given
  assert.equal(describeRequest(myRequest({ decision: 'declined', role: null })).href, null);

  const waiting = describeRequest(myRequest({ decision: undefined, role: null, decided_at: null }));
  assert.equal(waiting.status, 'waiting');
  assert.equal(waiting.statusText, 'Waiting for a decision');
  assert.equal(waiting.at, '2026-11-14T09:00:00Z');
});

test('the inbox model marks what changed after the last opening, newest first', () => {
  const open = [openRequest({ id: 'old', created_at: '2026-11-13T08:00:00Z' }), openRequest({ id: 'new', created_at: '2026-11-15T10:00:00Z' })];
  const mine = [myRequest({ id: 'seen', decided_at: '2026-11-14T08:00:00Z' }), myRequest({ id: 'fresh', decided_at: '2026-11-15T11:00:00Z' })];
  const lastSeen = Date.parse('2026-11-14T12:00:00Z');
  const model = inboxModel(open, mine, lastSeen);
  assert.deepEqual(model.decide.map((item) => [item.id, item.isNew]), [['new', true], ['old', false]]);
  assert.deepEqual(model.yours.map((item) => [item.id, item.isNew]), [['fresh', true], ['seen', false]]);
  assert.equal(model.unread, 2);
  assert.equal(unreadCount(model), 2);
  assert.equal(model.newest, Date.parse('2026-11-15T11:00:00Z'));
  // never opened: everything that happened is new; nothing waiting ever is
  const first = inboxModel(open, [...mine, myRequest({ id: 'w', decision: undefined })], null);
  assert.equal(first.unread, 4);
  // empty or broken lists
  assert.deepEqual(inboxModel(null, undefined, null), { decide: [], yours: [], newest: null, unread: 0 });
  assert.equal(unreadCount(null), 0);
});

test('the bell says how many are new, the badge caps at 99+', () => {
  assert.equal(bellLabel(0), 'Notifications');
  assert.equal(bellLabel(2), 'Notifications, 2 new');
  assert.equal(badgeText(0), '');
  assert.equal(badgeText(7), '7');
  assert.equal(badgeText(100), '99+');
});

test('seen is never earlier than the newest item, and the tab refetches only after a while', () => {
  const now = Date.parse('2026-11-15T12:00:00Z');
  assert.equal(nextSeen({ newest: now - 1000 }, now), now);
  assert.equal(nextSeen({ newest: now + 60_000 }, now), now + 60_000); // a server clock ahead
  assert.equal(nextSeen({ newest: null }, now), now);
  assert.equal(refreshOnShow(null, now), true);
  assert.equal(refreshOnShow(now - 1000, now), false);
  assert.equal(refreshOnShow(now - REFRESH_ON_SHOW_AFTER, now), true);
});

test('the last-opened time is kept per person and storage trouble never throws', () => {
  const map = new Map();
  const store = { getItem: (key) => map.get(key) ?? null, setItem: (key, value) => map.set(key, value) };
  assert.equal(readSeen('u1', store), null);
  writeSeen('u1', 1763200000000.4, store);
  assert.equal(map.get(seenKey('u1')), '1763200000000');
  assert.equal(readSeen('u1', store), 1763200000000);
  assert.equal(readSeen('u2', store), null);
  map.set(seenKey('u3'), 'nonsense');
  assert.equal(readSeen('u3', store), null);
  const broken = { getItem: () => { throw new Error('blocked'); }, setItem: () => { throw new Error('full'); } };
  assert.equal(readSeen('u1', broken), null);
  assert.doesNotThrow(() => writeSeen('u1', 1, broken));
  assert.equal(readSeen('u1', null), null);
});
