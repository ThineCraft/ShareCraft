import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { DRAWER_BELOW, layoutFor, prefsText, readPrefs } from './workspace.js';

test('layoutFor: no sidebar without vertical links or folders', () => {
  assert.deepEqual(layoutFor(1280, {}, 'horizontal'), { side: 'none', label: null });
  assert.deepEqual(layoutFor(375, {}, 'horizontal', { folders: false }), { side: 'none', label: null });
});

test('layoutFor: a column from 1000 pixels, a rail when folded, a drawer below', () => {
  assert.deepEqual(layoutFor(1280, {}, 'vertical'), { side: 'column', label: 'Menu' });
  assert.deepEqual(layoutFor(1280, { collapsed: true }, 'vertical', { folders: true }), { side: 'rail', label: 'Menu' });
  assert.deepEqual(layoutFor(DRAWER_BELOW, {}, 'horizontal', { folders: true }), { side: 'column', label: 'Folders' });
  assert.deepEqual(layoutFor(DRAWER_BELOW - 1, {}, 'horizontal', { folders: true }), { side: 'drawer', label: 'Folders' });
  assert.deepEqual(layoutFor(375, { collapsed: true }, 'vertical', { folders: true }), { side: 'drawer', label: 'Menu' }, 'folding does not apply to the drawer');
  assert.equal(layoutFor(Number.NaN, {}, 'vertical').side, 'drawer');
});

test('the folded or open choice is stored as text', () => {
  assert.deepEqual(readPrefs('rail'), { collapsed: true });
  assert.deepEqual(readPrefs('column'), { collapsed: false });
  assert.deepEqual(readPrefs(null), { collapsed: false });
  assert.deepEqual(readPrefs('nonsense'), { collapsed: false });
  assert.equal(prefsText({ collapsed: true }), 'rail');
  assert.equal(prefsText({ collapsed: false }), 'column');
  assert.equal(prefsText(null), 'column');
});

test('workspace pages are wide and the sidebar has its column, rail and drawer', () => {
  for (const page of ['library.html', 'site.html', 'access.html', 'bin.html', 'explorer.html']) {
    const html = readFileSync(new URL(`./${page}`, import.meta.url), 'utf8');
    assert.match(html, /<div class="page[^"]* workspace[" ]/, page);
  }
  const css = readFileSync(new URL('./style.css', import.meta.url), 'utf8');
  assert.match(css, /\.page\.workspace\{max-width:1600px/);
  assert.match(css, /\[data-side=rail\] \.ws\{grid-template-columns:40px/);
  assert.match(css, /dialog\.side-drawer\{/);
  assert.match(css, /prefers-reduced-motion:no-preference\)\{\s*dialog\.side-drawer/);
  assert.match(css, /\.files\{width:100%;table-layout:fixed/);
  assert.match(css, /\.files a\.name-link\{[^}]*white-space:nowrap;text-overflow:ellipsis/);
  assert.doesNotMatch(css, /tree-col/);
});
