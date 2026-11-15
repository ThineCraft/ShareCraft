// The phone-first shell's tab bar (round 20): the tabs, the current one, the Recent rows.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { SHELL_TABS, currentTab, recentRow } from './shell.js';

test('five tabs in thumb order, each with a label and an icon', () => {
  assert.deepEqual(SHELL_TABS.map((tab) => tab.label), ['Home', 'Search', 'Recent', 'Inbox', 'You']);
  for (const tab of SHELL_TABS) assert.ok(tab.icon.length > 0 && tab.icon.every((d) => /^[Mm]/.test(d)), tab.key);
});

test('Home is current on the start page only', () => {
  assert.equal(currentTab('/'), 'home');
  assert.equal(currentTab('/index.html'), 'home');
  assert.equal(currentTab('/library.html'), null);
  assert.equal(currentTab('/site.html'), null);
  assert.equal(currentTab(), null);
});

test('a recent file opens in its page, with its site and when', () => {
  const now = Date.parse('2026-11-15T12:00:00Z');
  const row = recentRow({ id: '0f0e0d0c-0b0a-4908-8706-050403020100', name: 'Budget.xlsx', editor: 'sheet', site: 'Home', at: now - 5 * 60_000 }, now);
  assert.equal(row.href, '/file.html?doc=0f0e0d0c-0b0a-4908-8706-050403020100');
  assert.equal(row.name, 'Budget.xlsx');
  assert.equal(row.kind, 'sheet');
  assert.match(row.detail, /^Home · 5 minutes ago$/);
  const text = recentRow({ id: '0f0e0d0c-0b0a-4908-8706-050403020100', name: 'Notes', editor: 'text', site: '', at: now }, now);
  assert.equal(text.href, '/doc.html#0f0e0d0c-0b0a-4908-8706-050403020100');
  assert.ok(!text.detail.startsWith(' · '));
});
