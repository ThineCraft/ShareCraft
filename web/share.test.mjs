import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

import { accessLevel, accessSummary, comboboxFilter, looksLikeEmail, permsSentence, sortEntries } from './share.js';

const OWNER = ['view', 'add', 'edit', 'delete', 'manage_access', 'manage_site'];
const EDITOR = ['view', 'add', 'edit', 'delete'];
const VIEWER = ['view'];
const group = (name, perms, extra = {}) => ({ principal: { kind: 'group', id: name, name, ...extra }, role: { id: 'r', name: 'R' }, perms });
const person = (name, perms) => ({ principal: { kind: 'user', id: name, name, email: `${name.toLowerCase().replace(/ /g, '.')}@example.com` }, role: { id: 'r', name: 'R' }, perms });
const link = (perms = VIEWER) => ({ principal: { kind: 'link', id: 'l1', link_kind: 'signed_in', expires_at: null }, role: { id: 'viewer', name: 'Viewer' }, perms });

test('accessLevel ranks permissions', () => {
  assert.equal(accessLevel(OWNER), 'manage');
  assert.equal(accessLevel(['view', 'manage_access']), 'manage');
  assert.equal(accessLevel(EDITOR), 'edit');
  assert.equal(accessLevel(['view', 'add']), 'add');
  assert.equal(accessLevel(VIEWER), 'view');
  assert.equal(accessLevel(undefined), 'view');
});

test('permsSentence says the permissions in plain words', () => {
  assert.equal(permsSentence(EDITOR), 'Can view, add, edit and delete');
  assert.equal(permsSentence(['view']), 'Can view');
  assert.equal(permsSentence(['manage_access', 'view']), 'Can view and manage access');
  assert.equal(permsSentence([]), 'Can do nothing');
});

test('accessSummary: the example of the brief', () => {
  const entries = [
    group('Home Owners', OWNER, { member_count: 1 }),
    group('Home Members', EDITOR, { member_count: 3 }),
    link(),
  ];
  assert.equal(
    accessSummary(entries, [{ id: 'l1', kind: 'signed_in' }]),
    'Everyone in Home Members (3 people) can edit. 1 owner group. Shared by 1 link.',
  );
});

test('accessSummary names up to two, counts more, and says nobody for none', () => {
  assert.equal(accessSummary([], []), 'Nobody has access here yet.');
  assert.equal(accessSummary([person('Ann Tester', VIEWER)], []), 'Ann Tester can view.');
  assert.equal(
    accessSummary([group('Sales', EDITOR, { member_count: 1 }), person('Bob', EDITOR)], []),
    'Everyone in Sales (1 person) and Bob can edit.',
  );
  assert.equal(
    accessSummary([group('A', VIEWER), group('B', VIEWER), person('C', VIEWER)], []),
    '2 groups and 1 person can view.',
  );
  assert.equal(accessSummary([group('X', EDITOR, { hidden: true, name: null })], []), 'A group you cannot see can edit.');
  assert.equal(
    accessSummary([group('O', OWNER), person('P', OWNER), person('Q', OWNER)], []),
    '1 owner group and 2 owners.',
  );
  assert.equal(accessSummary([person('Ann', ['view', 'add'])], []), 'Ann can add.');
  assert.equal(accessSummary([group('Home Visitors', VIEWER, { member_count: 0 })], []), 'Home Visitors (no members yet) can view.');
});

test('accessSummary counts links and says which are open to anyone', () => {
  assert.equal(accessSummary([], [{ kind: 'anyone' }]), 'Shared by 1 link, open to anyone.');
  assert.equal(accessSummary([], [{ kind: 'anyone' }, { kind: 'anyone' }]), 'Shared by 2 links, all open to anyone.');
  assert.equal(accessSummary([link(), link()], [{ kind: 'anyone' }, { kind: 'signed_in' }]), 'Shared by 2 links, 1 open to anyone.');
  // A link inherited from above shows as an entry only.
  assert.equal(accessSummary([link()], []), 'Shared by 1 link.');
});

test('sortEntries: owners, editors, viewers, links; groups before people; by name', () => {
  const entries = [
    link(),
    person('zed', VIEWER),
    group('Visitors', VIEWER),
    person('Amy', EDITOR),
    group('Owners', OWNER),
    group('hidden', EDITOR, { hidden: true, name: null }),
    group('Members', EDITOR),
    person('bob', OWNER),
    person('Ann', ['view', 'add']),
  ];
  const order = sortEntries(entries).map((e) => (e.principal.kind === 'link' ? 'link' : e.principal.name ?? 'hidden'));
  assert.deepEqual(order, ['Owners', 'bob', 'Members', 'hidden', 'Amy', 'Ann', 'Visitors', 'zed', 'link']);
  assert.equal(entries[0].principal.kind, 'link', 'the input is left as it was');
});

const CANDIDATES = [
  { kind: 'group', id: 'g1', name: 'Home Members', detail: 'group · site: Home' },
  { kind: 'group', id: 'g2', name: 'Home Owners', detail: 'group · site: Home' },
  { kind: 'group', id: 'g3', name: 'Projects Members', detail: 'group · site: Projects' },
  { kind: 'user', id: 'u1', name: 'Ann Tester', email: 'ann@example.com', detail: 'ann@example.com' },
  { kind: 'user', id: 'u2', name: 'Bob Smith', email: 'bob@example.com', detail: 'bob@example.com' },
];

test('comboboxFilter ranks a name start before a word start before anywhere', () => {
  assert.deepEqual(comboboxFilter('mem', CANDIDATES).map((c) => c.id), ['g1', 'g3']);
  assert.deepEqual(comboboxFilter('home', CANDIDATES).map((c) => c.id), ['g1', 'g2']);
  assert.deepEqual(comboboxFilter('PRO', CANDIDATES).map((c) => c.id), ['g3']);
  assert.deepEqual(comboboxFilter('smith', CANDIDATES).map((c) => c.id), ['u2']);
  assert.deepEqual(comboboxFilter('bob@', CANDIDATES).map((c) => c.id), ['u2']);
  assert.deepEqual(comboboxFilter('ester', CANDIDATES).map((c) => c.id), ['u1']);
  assert.deepEqual(comboboxFilter('site: projects', CANDIDATES).map((c) => c.id), ['g3']);
  assert.deepEqual(comboboxFilter('zzz', CANDIDATES), []);
});

test('comboboxFilter: empty query, limit, and a typed email', () => {
  assert.equal(comboboxFilter('', CANDIDATES).length, 5);
  assert.equal(comboboxFilter('  ', CANDIDATES, { limit: 2 }).length, 2);
  const typed = comboboxFilter('cat@example.com', CANDIDATES);
  assert.deepEqual(typed, [{ kind: 'email', id: 'email:cat@example.com', name: 'cat@example.com', email: 'cat@example.com', detail: 'Add by email address' }]);
  // A known person's address is not offered twice.
  assert.deepEqual(comboboxFilter('Ann@Example.com', CANDIDATES).map((c) => c.id), ['u1']);
  // With a full list the email still gets the last place.
  const many = Array.from({ length: 10 }, (_, n) => ({ kind: 'user', id: `x${n}`, name: `x${n}`, email: `x${n}@ex.org`, detail: 'x@ex.org' }));
  const full = comboboxFilter('x@ex.org', many, { limit: 3 });
  assert.equal(full.length, 3);
  assert.equal(full[2].kind, 'email');
  assert.equal(looksLikeEmail('a@b'), false);
  assert.equal(looksLikeEmail(' a@b.co '), true);
});

test('access.html has one combobox wired to a listbox, and no inline styles', () => {
  const html = readFileSync(new URL('./access.html', import.meta.url), 'utf8');
  assert.match(html, /role="combobox"[^>]*aria-controls="share-options"/);
  assert.match(html, /id="share-options"[^>]*role="listbox"/);
  assert.doesNotMatch(html, /\sstyle=/);
  assert.doesNotMatch(html, /<style/);
  assert.doesNotMatch(html, /id="group-form"/);
});
