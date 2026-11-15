import test from 'node:test';
import assert from 'node:assert/strict';
import {
  QUIET_KINDS,
  activityError,
  collapseSaves,
  describeEvent,
  groupByDay,
  peopleDirectory,
  visibleCount,
  whenText,
} from './activity.js';

const ANN = '11111111-1111-1111-1111-111111111111';
const BOB = '22222222-2222-2222-2222-222222222222';
const ME = '33333333-3333-3333-3333-333333333333';
const GROUP = '44444444-4444-4444-4444-444444444444';
const people = peopleDirectory({
  me: { id: ME, name: 'Me Myself' },
  access: {
    assignments: [
      { principal: { kind: 'user', id: ANN, name: 'Ann Tester' }, role: { id: 'owner', name: 'Owner' } },
      { principal: { kind: 'group', id: GROUP, name: 'Sales team' }, role: { id: 'approver', name: 'Approver' } },
    ],
  },
  users: [{ id: BOB, name: 'Bob Tester' }],
});
const UTC = { timeZone: 'UTC' };
const ev = (id, kind, at, more = {}) => ({ id, kind, at, actor: ANN, subject: null, detail: {}, ...more });

test('peopleDirectory names people, groups and roles from the access list, the admin list and me', () => {
  assert.equal(people.users[ANN], 'Ann Tester');
  assert.equal(people.users[BOB], 'Bob Tester');
  assert.equal(people.users[ME], 'Me Myself');
  assert.equal(people.groups[GROUP], 'Sales team');
  assert.equal(people.roles.approver, 'Approver');
  assert.equal(people.roles.editor, 'Editor');
  assert.equal(people.me, ME);
  assert.deepEqual(peopleDirectory().users, {});
});

test('describeEvent: renames, saves, access and the scanner as plain sentences with an actor', () => {
  const renamed = describeEvent(ev(1, 'node.renamed', '2026-11-15T10:00:00Z', { detail: { from: 'Budget.xlsx', to: 'Budget 2026.xlsx' } }), people, UTC);
  assert.equal(renamed.text, 'Ann Tester renamed it from Budget.xlsx to Budget 2026.xlsx');
  assert.deepEqual(renamed.actor, { id: ANN, name: 'Ann Tester', label: 'Ann Tester', system: false });
  assert.equal(renamed.at, '2026-11-15T10:00:00Z');

  // saves carry their authors in the detail, with no actor
  const saved = describeEvent(ev(2, 'revision.saved', '2026-11-15T10:00:00Z', { actor: null, detail: { revision: 'r', authors: [ANN] } }), people, UTC);
  assert.equal(saved.text, 'Ann Tester saved a new version');
  assert.equal(saved.actor.id, ANN);
  const two = describeEvent(ev(2, 'revision.saved', null, { actor: null, detail: { authors: [ANN, BOB] } }), people, UTC);
  assert.equal(two.text, 'Ann Tester and Bob Tester saved a new version');
  const nobody = describeEvent(ev(2, 'revision.saved', null, { actor: null, detail: { authors: [] } }), people, UTC);
  assert.equal(nobody.text, 'A new version was saved');
  assert.equal(nobody.actor.system, true);

  const given = describeEvent(ev(3, 'access.granted', null, { subject: BOB, detail: { role: 'editor' } }), people, UTC);
  assert.equal(given.text, 'Ann Tester gave Bob Tester Editor access');
  const group = describeEvent(ev(3, 'access.granted', null, { detail: { role: 'approver', group: GROUP } }), people, UTC);
  assert.equal(group.text, 'Ann Tester gave Sales team Approver access');
  const changed = describeEvent(ev(3, 'access.changed', null, { subject: ME, detail: { from: 'viewer', to: 'owner' } }), people, UTC);
  assert.equal(changed.text, 'Ann Tester changed your access from Viewer to Owner');
  const revoked = describeEvent(ev(3, 'access.revoked', null, { subject: BOB, detail: { role: 'viewer' } }), people, UTC);
  assert.equal(revoked.text, "Ann Tester took away Bob Tester's Viewer access");

  const clean = describeEvent(ev(4, 'content.scanned', null, { actor: null, detail: { result: 'clean' } }), people, UTC);
  assert.equal(clean.text, 'Checked for malware — clean');
  assert.equal(clean.actor.system, true);

  const expired = describeEvent(ev(5, 'link.revoked', null, { actor: null, detail: { reason: 'expired' } }), people, UTC);
  assert.equal(expired.text, 'A link to it expired');
  const link = describeEvent(ev(6, 'link.created', null, { detail: { kind: 'anyone', role: 'viewer', expires_at: Date.UTC(2026, 10, 20) / 1000 } }), people, UTC);
  assert.equal(link.text, 'Ann Tester made a link for anyone with the address with Viewer access, until Friday 20 November');
});

test('describeEvent: me is "You", unknown people "Someone", unknown kinds a neutral sentence', () => {
  assert.equal(describeEvent(ev(1, 'node.moved', null, { actor: ME }), people, UTC).text, 'You moved it to another folder');
  const stranger = describeEvent(ev(1, 'document.created', null, { actor: '99999999-0000-0000-0000-000000000000' }), people, UTC);
  assert.equal(stranger.text, 'Someone created it');
  assert.equal(stranger.actor.label, '');
  const odd = describeEvent(ev(1, 'node.locked', null, { detail: { secret: { a: 1 } } }), people, UTC);
  assert.equal(odd.text, 'Ann Tester did something recorded as node locked');
  assert.doesNotMatch(odd.text, /[{}]/);
  const oddSystem = describeEvent(ev(1, 'weird_thing', null, { actor: null }), people, UTC);
  assert.equal(oddSystem.text, 'Something was recorded: weird thing');
  assert.equal(describeEvent({}, people, UTC).actor.system, true);
});

test('describeEvent: a folded run of saves says how many', () => {
  const run = { ...ev(9, 'revision.saved', '2026-11-15T10:00:00Z', { actor: null, detail: { authors: [ANN] } }), count: 6 };
  assert.equal(describeEvent(run, people, UTC).text, 'Ann Tester saved 6 times');
});

test('collapseSaves folds saves by one person within ten minutes of each other, newest first', () => {
  const save = (id, at, author = ANN) => ev(id, 'revision.saved', at, { actor: null, detail: { authors: [author] } });
  const entries = collapseSaves([
    save(1, '2026-11-15T09:00:00Z'),
    save(2, '2026-11-15T09:30:00Z'), // 30 minutes after: a run of its own
    save(3, '2026-11-15T09:38:00Z'),
    save(4, '2026-11-15T09:47:00Z'),
    save(5, '2026-11-15T09:50:00Z', BOB), // someone else breaks the run
    save(6, '2026-11-15T09:52:00Z'),
    ev(7, 'editing.joined', '2026-11-15T09:53:00Z'), // quiet: left out
  ]);
  assert.deepEqual(entries.map((entry) => [entry.id, entry.count]), [[6, 1], [5, 1], [4, 3], [1, 1]]);
  assert.deepEqual(entries[2].members.map((member) => member.id), [4, 3, 2]);
  assert.ok(QUIET_KINDS.includes('editing.joined'));
});

test('collapseSaves folds an upload\'s own created/saved events and link access into one line', () => {
  const entries = collapseSaves([
    ev(1, 'document.created', '2026-11-15T09:00:00Z', { detail: { name: 'a.pdf' } }),
    ev(2, 'upload.completed', '2026-11-15T09:00:01Z', { detail: { bytes: 2048, new_version: false } }),
    ev(3, 'revision.saved', '2026-11-15T10:00:00Z', { actor: null, detail: { authors: [ANN] } }),
    ev(4, 'upload.completed', '2026-11-15T10:00:01Z', { detail: { bytes: 4096, new_version: true } }),
    ev(5, 'access.granted', '2026-11-15T11:00:00Z', { detail: { role: 'viewer', link: 'l', group: 'g' } }),
    ev(6, 'link.created', '2026-11-15T11:00:00Z', { detail: { kind: 'signed_in', role: 'viewer', expires_at: null } }),
  ]);
  assert.deepEqual(entries.map((entry) => entry.id), [6, 4, 2]);
  assert.equal(describeEvent(entries[1], people, UTC).text, 'Ann Tester uploaded a new version (4.0 KB)');
  assert.equal(describeEvent(entries[2], people, UTC).text, 'Ann Tester uploaded it (2.0 KB)');
  assert.equal(describeEvent(entries[0], people, UTC).text, 'Ann Tester made a link for people in the organisation with Viewer access');
  assert.deepEqual(collapseSaves(null), []);
});

test('groupByDay: Today, Yesterday, weekday names, then dates, in the time zone given', () => {
  const now = Date.parse('2026-11-15T12:00:00Z'); // a Sunday
  const items = [
    { id: 1, at: '2026-11-15T23:30:00Z' }, // a little ahead: today
    { id: 2, at: '2026-11-15T01:00:00Z' },
    { id: 3, at: '2026-11-14T22:00:00Z' },
    { id: 4, at: '2026-11-10T08:00:00Z' },
    { id: 5, at: '2026-11-03T08:00:00Z' },
    { id: 6, at: '2025-12-24T08:00:00Z' },
    { id: 7, at: null },
  ];
  const groups = groupByDay(items, now, 'UTC');
  assert.deepEqual(
    groups.map((group) => [group.heading, group.items.map((item) => item.id)]),
    [
      ['Today', [1, 2]],
      ['Yesterday', [3]],
      ['Tuesday', [4]],
      ['3 November', [5]],
      ['24 December 2025', [6]],
      ['Date not known', [7]],
    ],
  );
  // 22:00 UTC on Saturday is Sunday morning in Auckland: today there, at 22:00 on Sunday
  const auckland = groupByDay([{ id: 3, at: '2026-11-14T22:00:00Z' }], Date.parse('2026-11-15T09:00:00Z'), 'Pacific/Auckland');
  assert.equal(auckland[0].heading, 'Today');
  assert.deepEqual(groupByDay([], now, 'UTC'), []);
});

test('activityError and visibleCount', () => {
  assert.match(activityError(403, 'forbidden'), /manage access/);
  assert.match(activityError(404, 'not_found'), /could not be found/);
  assert.match(activityError(0, 'network'), /Could not reach the server/);
  assert.equal(visibleCount(30, 100), 60);
  assert.equal(visibleCount(90, 100), 100);
});

test('whenText: relative today, the time of day on earlier days', () => {
  const now = Date.parse('2026-11-15T15:00:00Z');
  const utc = { language: 'en-GB', timeZone: 'UTC' };
  assert.equal(whenText('2026-11-15T13:00:00Z', now, utc), '2 hours ago');
  assert.equal(whenText('2026-11-11T03:58:00Z', now, utc), '3:58');
  assert.equal(whenText(null, now, utc), '');
});
