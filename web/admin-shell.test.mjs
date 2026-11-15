import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import {
  ADMIN_SECTIONS,
  adminSections,
  countText,
  filterRows,
  filterStatus,
  passwordCheck,
  peopleSummary,
} from './admin-shell.js';

const keys = (sections) => sections.map((section) => section.key);

test('adminSections: administrators see every section, in order', () => {
  assert.deepEqual(keys(adminSections({ is_admin: true })), [
    'people',
    'groups',
    'roles',
    'sites',
    'templates',
    'settings',
    'brand',
    'quarantine',
    'requests',
  ]);
  assert.equal(adminSections({ is_admin: true }).length, ADMIN_SECTIONS.length);
});

test('adminSections: brand managers add the Brand centre to what everyone sees', () => {
  assert.deepEqual(keys(adminSections({ is_admin: false, brand_manager: true })), ['groups', 'brand', 'requests']);
  assert.deepEqual(keys(adminSections({ is_admin: false, brand_manager: false })), ['groups', 'requests']);
  assert.deepEqual(keys(adminSections(null)), ['groups', 'requests']);
  assert.deepEqual(keys(adminSections({})), ['groups', 'requests']);
});

test('adminSections: every section has a label, an address and an icon key', () => {
  for (const section of ADMIN_SECTIONS) {
    assert.match(section.href, /^\/[a-z-]+\.html$/);
    assert.ok(section.label.length > 0);
  }
  assert.equal(ADMIN_SECTIONS.find((s) => s.key === 'settings').label, 'Sharing settings');
  assert.equal(ADMIN_SECTIONS.find((s) => s.key === 'requests').label, 'Access requests');
});

const people = [
  { name: 'Ann Tester', email: 'ann@example.com', hub: { name: 'Comms' } },
  { name: 'Zoë Brandt', email: 'zoe@example.org' },
  { name: 'Bob Builder', email: 'bob@example.com' },
];

test('filterRows: an empty query keeps every row, in order, as a new array', () => {
  const all = filterRows('', people, ['name']);
  assert.deepEqual(all, people);
  assert.notEqual(all, people);
  assert.deepEqual(filterRows('   ', people, ['name']), people);
  assert.deepEqual(filterRows(null, people, ['name']), people);
  assert.deepEqual(filterRows('x', null, ['name']), []);
});

test('filterRows: matches any field, ignoring case, accents and spaces at either end', () => {
  assert.deepEqual(filterRows('ANN', people, ['name', 'email']), [people[0]]);
  assert.deepEqual(filterRows(' example.com ', people, ['name', 'email']), [people[0], people[2]]);
  assert.deepEqual(filterRows('zoe', people, ['name']), [people[1]]);
  assert.deepEqual(filterRows('zoë', people, ['email']), [people[1]]);
  assert.deepEqual(filterRows('nobody', people, ['name', 'email']), []);
});

test('filterRows: every word must match somewhere; dotted paths and functions are fields', () => {
  assert.deepEqual(filterRows('bob com', people, ['name', 'email']), [people[2]]);
  assert.deepEqual(filterRows('bob org', people, ['name', 'email']), []);
  assert.deepEqual(filterRows('comms', people, ['hub.name']), [people[0]]);
  assert.deepEqual(filterRows('builder', people, [(row) => row.name.split(' ')[1]]), [people[2]]);
  // A missing field is no match, not an error.
  assert.deepEqual(filterRows('x', [{}], ['hub.name', 'name']), []);
});

test('peopleSummary counts people, administrators and disabled accounts', () => {
  assert.equal(
    peopleSummary([
      { is_admin: true, disabled: false },
      { is_admin: false, disabled: false },
    ]),
    '2 people, 1 administrator',
  );
  assert.equal(peopleSummary([{ is_admin: true }]), '1 person, 1 administrator');
  assert.equal(
    peopleSummary([{ is_admin: true }, { is_admin: true }, { disabled: true }]),
    '3 people, 2 administrators, 1 disabled',
  );
  assert.equal(peopleSummary([{ is_admin: false }]), '1 person, no administrators');
  assert.equal(peopleSummary([]), 'No people yet.');
  assert.equal(peopleSummary(undefined), 'No people yet.');
});

test('filterStatus and countText say how many are shown', () => {
  const nouns = ['group', 'groups'];
  assert.equal(countText(1, nouns), '1 group');
  assert.equal(countText(0, nouns), '0 groups');
  assert.equal(filterStatus('', 5, 5, nouns), '5 groups');
  assert.equal(filterStatus('  ', 5, 5, nouns), '5 groups');
  assert.equal(filterStatus('fin', 2, 5, nouns), '2 of 5 groups');
  assert.equal(filterStatus('fin', 5, 5, nouns), '5 groups');
  assert.equal(filterStatus(' xyz ', 0, 5, nouns), 'No groups match “xyz”.');
});

test('passwordCheck follows the server rule of 10 to 256 characters', () => {
  assert.deepEqual(passwordCheck(''), { ok: false, text: 'At least 10 characters.' });
  assert.deepEqual(passwordCheck('abcdefg'), { ok: false, text: 'At least 10 characters: 3 more to go.' });
  assert.deepEqual(passwordCheck('abcdefghij'), { ok: true, text: 'Long enough.' });
  assert.equal(passwordCheck('😀'.repeat(10)).ok, true, 'characters, not code units');
  assert.deepEqual(passwordCheck('a'.repeat(260)), { ok: false, text: 'At most 256 characters: 4 too many.' });
});

test('the admin pages use the shared layout and no longer have the link row', () => {
  const pages = ['people', 'groups', 'roles', 'admin-sites', 'templates', 'sharing', 'brand-centre', 'quarantine', 'requests'];
  for (const page of pages) {
    const html = readFileSync(new URL(`./${page}.html`, import.meta.url), 'utf8');
    assert.match(html, /<nav id="admin-nav" class="admin-nav" aria-label="Admin centre"><\/nav>/, page);
    assert.match(html, /<main class="admin-main">/, page);
    assert.doesNotMatch(html, /class="subnav"/, page);
    assert.doesNotMatch(html, /style=|<style/, page);
    const script = readFileSync(new URL(`./${page}.js`, import.meta.url), 'utf8');
    assert.match(script, /mountAdminShell\(\{ me, current: '/, page);
  }
});

test('the People page adds people through a dialog with a password toggle', () => {
  const html = readFileSync(new URL('./people.html', import.meta.url), 'utf8');
  assert.match(html, /<button type="button" id="add-button" hidden>Add a person<\/button>/);
  assert.match(html, /<dialog id="add-dialog" aria-labelledby="add-title">/);
  assert.match(html, /id="password-toggle"[^>]*aria-pressed="false"/);
  assert.match(html, /aria-describedby="password-hint"/);
});
