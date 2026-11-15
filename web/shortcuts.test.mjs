import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

import {
  ariaKeys,
  comboMatches,
  contextOf,
  createRegistry,
  findShortcut,
  isMacPlatform,
  isTextField,
  keyParts,
  parseKeys,
  sheetGroups,
} from './shortcuts.js';
import { extendSelection, moveIndex, selectionText, toggleSelection } from './selection.js';
import { splitName } from './files.js';
import { isTextField as paletteIsTextField } from './palette.js';

const key = (k, extra = {}) => ({ key: k, ctrlKey: false, metaKey: false, shiftKey: false, altKey: false, target: { tagName: 'BODY' }, ...extra });

// ---- matching ----

test('parseKeys reads modifiers and key names', () => {
  assert.deepEqual(parseKeys('Mod+K'), { key: 'k', mod: true, shift: false, alt: false });
  assert.deepEqual(parseKeys('Ctrl+Shift+F'), { key: 'f', mod: true, shift: true, alt: false });
  assert.deepEqual(parseKeys('Alt+ArrowUp'), { key: 'arrowup', mod: false, shift: false, alt: true });
  assert.deepEqual(parseKeys('Space'), { key: ' ', mod: false, shift: false, alt: false });
  assert.deepEqual(parseKeys('?'), { key: '?', mod: false, shift: false, alt: false });
  assert.deepEqual(parseKeys('Mod++'), { key: '+', mod: true, shift: false, alt: false });
  assert.equal(parseKeys('Esc').key, 'escape');
});

test('comboMatches: Mod is Ctrl or Cmd; Alt and Shift must match; letters ignore case', () => {
  const modK = parseKeys('Mod+K');
  assert.equal(comboMatches(modK, key('k', { ctrlKey: true })), true);
  assert.equal(comboMatches(modK, key('K', { metaKey: true })), true);
  assert.equal(comboMatches(modK, key('k')), false);
  assert.equal(comboMatches(modK, key('k', { ctrlKey: true, altKey: true })), false);
  assert.equal(comboMatches(modK, key('K', { ctrlKey: true, shiftKey: true })), false);
  const n = parseKeys('n');
  assert.equal(comboMatches(n, key('n')), true);
  assert.equal(comboMatches(n, key('N')), true, 'caps lock');
  assert.equal(comboMatches(n, key('N', { shiftKey: true })), false);
  assert.equal(comboMatches(n, key('n', { ctrlKey: true })), false);
});

test('comboMatches: symbols ignore Shift; letters fall back to the physical key with a modifier', () => {
  assert.equal(comboMatches(parseKeys('?'), key('?', { shiftKey: true })), true);
  assert.equal(comboMatches(parseKeys('/'), key('/', { shiftKey: true })), true);
  assert.equal(comboMatches(parseKeys('/'), key('/', { ctrlKey: true })), false);
  const focus = parseKeys('Mod+Shift+F');
  assert.equal(comboMatches(focus, key('Ƒ', { code: 'KeyF', ctrlKey: true, shiftKey: true })), true);
  assert.equal(comboMatches(focus, key('f', { metaKey: true, shiftKey: true })), true);
  assert.equal(comboMatches(focus, key('f', { ctrlKey: true })), false);
  assert.equal(comboMatches(parseKeys('n'), key('x', { code: 'KeyN' })), false, 'no fallback without a modifier');
  assert.equal(comboMatches(parseKeys('Shift+ArrowDown'), key('ArrowDown', { shiftKey: true })), true);
  assert.equal(comboMatches(parseKeys('ArrowDown'), key('ArrowDown', { shiftKey: true })), false);
  assert.equal(comboMatches(parseKeys('Space'), key(' ')), true);
  assert.equal(comboMatches(parseKeys('F2'), key('F2')), true);
});

test('findShortcut leaves text fields, menus and open dialogs alone unless a shortcut says otherwise', () => {
  const plain = { keys: ['n'], run() {} };
  const anywhere = { keys: ['Mod+K'], inFields: true, inMenus: true, inDialogs: true, run() {} };
  const escape = { keys: ['Escape'], run() {} };
  const list = [plain, anywhere, escape];
  assert.equal(findShortcut(list, key('n'), {}), plain);
  assert.equal(findShortcut(list, key('n'), { field: true }), null);
  assert.equal(findShortcut(list, key('n'), { menu: true }), null);
  assert.equal(findShortcut(list, key('n'), { dialog: true }), null);
  assert.equal(findShortcut(list, key('k', { ctrlKey: true }), { field: true, dialog: true, menu: true }), anywhere);
  assert.equal(findShortcut(list, key('Escape'), { dialog: true }), escape, 'Escape passes an open dialog');
  assert.equal(findShortcut(list, key('Escape'), { field: true }), null);
  assert.equal(findShortcut(list, key('n', { defaultPrevented: true }), {}), null);
  assert.equal(findShortcut(list, key('n', { isComposing: true }), {}), null);
});

test('findShortcut asks the condition, and the first agreeing shortcut wins', () => {
  const off = { keys: ['d'], when: () => false, run() {} };
  const on = { keys: ['d'], when: (event, context) => context.extra === 1, run() {} };
  assert.equal(findShortcut([off, on], key('d'), { extra: 1 }), on);
  assert.equal(findShortcut([off, on], key('d'), { extra: 2 }), null);
});

test('contextOf tells fields, menus and open dialogs', () => {
  const doc = (open) => ({ querySelector: (selector) => (selector === 'dialog[open]' && open ? {} : null) });
  const input = { tagName: 'INPUT', type: 'text', closest: () => null };
  assert.deepEqual(contextOf({ target: input }, doc(false)), { field: true, menu: false, dialog: false });
  const inMenu = { tagName: 'BUTTON', closest: (s) => (s.includes('[role=menu]') ? {} : null) };
  assert.deepEqual(contextOf({ target: inMenu }, doc(true)), { field: false, menu: true, dialog: true });
});

test('isTextField is shared with the palette and knows checkboxes from text', () => {
  assert.equal(paletteIsTextField, isTextField);
  assert.equal(isTextField({ tagName: 'INPUT', type: 'checkbox' }), false);
  assert.equal(isTextField({ tagName: 'INPUT' }), true);
  assert.equal(isTextField({ tagName: 'SELECT' }), true);
  assert.equal(isTextField({ tagName: 'DIV', isContentEditable: true }), true);
  assert.equal(isTextField(null), false);
});

test('a registry runs the matching shortcut, prevents the default and can be told "not handled"', () => {
  const registry = createRegistry();
  const ran = [];
  registry.add({ keys: 'z', run: () => ran.push('z') });
  const remove = registry.add({ keys: ['x'], run: () => false });
  const event = (k) => ({ ...key(k), prevented: false, preventDefault() { this.prevented = true; }, stopPropagation() {} });
  const doc = { querySelector: () => null };
  const z = event('z');
  assert.equal(registry.dispatch(z, { doc }), true);
  assert.equal(z.prevented, true);
  assert.deepEqual(ran, ['z']);
  const x = event('x');
  assert.equal(registry.dispatch(x, { doc }), false);
  assert.equal(x.prevented, false);
  remove();
  assert.equal(registry.entries().length, 1);
  assert.equal(registry.entries()[0].scope, 'global', 'global by default');
  // Only frame shortcuts run inside an editor's frame.
  registry.add({ keys: ['q'], frames: true, run: () => ran.push('q') });
  assert.equal(registry.dispatch(event('z'), { doc, only: (entry) => entry.frames === true }), false);
  assert.equal(registry.dispatch(event('q'), { doc, only: (entry) => entry.frames === true }), true);
});

// ---- the sheet ----

test('keyParts shows Ctrl elsewhere and ⌘ on a Mac, with names for signs', () => {
  assert.deepEqual(keyParts('Mod+K'), [{ text: 'Ctrl', spoken: 'Ctrl' }, { text: 'K', spoken: 'K' }]);
  assert.deepEqual(keyParts('Mod+Shift+F', { mac: true }).map((p) => p.text), ['⌘', '⇧', 'F']);
  assert.equal(keyParts('Mod+K', { mac: true })[0].spoken, 'Command');
  assert.deepEqual(keyParts('Alt+ArrowUp').map((p) => p.text), ['Alt', '↑']);
  assert.equal(keyParts('Alt+ArrowUp')[1].spoken, 'Up arrow');
  assert.deepEqual(keyParts('Space').map((p) => p.text), ['Space']);
  assert.deepEqual(keyParts('F2').map((p) => p.text), ['F2']);
  assert.deepEqual(keyParts('?').map((p) => p.spoken), ['Question mark']);
  assert.equal(isMacPlatform('MacIntel'), true);
  assert.equal(isMacPlatform('Win32'), false);
});

test('ariaKeys gives aria-keyshortcuts values', () => {
  assert.equal(ariaKeys(['Mod+K', '/']), 'Control+K /');
  assert.equal(ariaKeys(['Mod+K'], { mac: true }), 'Meta+K');
  assert.equal(ariaKeys(['Alt+ArrowUp', 'Space', 'F2']), 'Alt+ArrowUp Space F2');
});

test('sheetGroups groups by scope in order, leaves out hidden ones and joins equal labels', () => {
  const entries = [
    { scope: 'library', keys: ['F2'], label: 'Rename' },
    { scope: 'global', keys: ['Mod+K', '/'], label: 'Search' },
    { scope: 'library', keys: ['n'], label: 'New folder', shown: () => false },
    { scope: 'library', keys: ['Escape'], label: 'Secret', listed: false },
    { scope: 'library', keys: ['i'], label: 'Details' },
    { scope: 'library', keys: ['d'], label: 'Details' },
  ];
  const groups = sheetGroups(entries);
  assert.deepEqual(groups.map((g) => g.heading), ['Everywhere', 'In a library']);
  assert.deepEqual(groups[1].rows.map((r) => r.label), ['Rename', 'Details']);
  assert.equal(groups[1].rows[1].keys.length, 2);
  assert.deepEqual(groups[0].rows[0].keys.map((parts) => parts.map((p) => p.text).join('+')), ['Ctrl+K', '/']);
  assert.equal(sheetGroups(entries, { mac: true })[0].rows[0].keys[0][0].text, '⌘');
  assert.deepEqual(sheetGroups([]), []);
});

// ---- selection ----

test('moveIndex moves without wrapping, by pages and to the ends', () => {
  assert.equal(moveIndex('ArrowDown', 0, 5), 1);
  assert.equal(moveIndex('ArrowDown', 4, 5), 4);
  assert.equal(moveIndex('ArrowUp', 0, 5), 0);
  assert.equal(moveIndex('ArrowUp', -1, 5), 4);
  assert.equal(moveIndex('ArrowDown', -1, 5), 0);
  assert.equal(moveIndex('Home', 3, 5), 0);
  assert.equal(moveIndex('End', 1, 5), 4);
  assert.equal(moveIndex('PageDown', 1, 50, 10), 11);
  assert.equal(moveIndex('PageDown', 45, 50, 10), 49);
  assert.equal(moveIndex('PageUp', 5, 50, 10), 0);
  assert.equal(moveIndex('Enter', 1, 5), null);
  assert.equal(moveIndex('ArrowDown', 0, 0), null);
});

test('extendSelection selects from the anchor, shrinks back and keeps what was chosen before', () => {
  const order = ['a', 'b', 'c', 'd', 'e'];
  const ids = (set) => [...set].sort();
  assert.deepEqual(ids(extendSelection(order, new Set(), 'b', 'd')), ['b', 'c', 'd']);
  assert.deepEqual(ids(extendSelection(order, new Set(), 'b', 'c')), ['b', 'c'], 'Shift+Up after two Shift+Downs');
  assert.deepEqual(ids(extendSelection(order, new Set(), 'c', 'a')), ['a', 'b', 'c'], 'upwards');
  assert.deepEqual(ids(extendSelection(order, new Set(['e']), 'a', 'b')), ['a', 'b', 'e']);
  assert.deepEqual(ids(extendSelection(order, new Set(), 'a', 'c', (id) => id !== 'b')), ['a', 'c'], 'a library is not selected');
  assert.deepEqual(ids(extendSelection(order, new Set(['a']), 'gone', 'c')), ['a', 'c'], 'a lost anchor starts at the target');
  assert.deepEqual(ids(extendSelection(order, new Set(['a']), 'a', 'gone')), ['a']);
});

test('toggleSelection and selectionText', () => {
  const one = toggleSelection(new Set(['a']), 'b');
  assert.deepEqual([...one].sort(), ['a', 'b']);
  assert.deepEqual([...toggleSelection(one, 'a')], ['b']);
  assert.equal(selectionText(0), 'Nothing selected');
  assert.equal(selectionText(1), '1 selected');
  assert.equal(selectionText(3), '3 selected');
});

test('splitName selects up to the ending, as desktop file managers do', () => {
  assert.deepEqual(splitName('Budget.xlsx'), { base: 'Budget', extension: '.xlsx' });
  assert.deepEqual(splitName('archive.tar.gz'), { base: 'archive.tar', extension: '.gz' });
  assert.deepEqual(splitName('.gitignore'), { base: '.gitignore', extension: '' });
  assert.deepEqual(splitName('Notes'), { base: 'Notes', extension: '' });
  assert.deepEqual(splitName('Version 2.'), { base: 'Version 2.', extension: '' });
  assert.deepEqual(splitName('Plans v1.5', { folder: true }), { base: 'Plans v1.5', extension: '' });
  assert.deepEqual(splitName('Mr. Smith letter'), { base: 'Mr. Smith letter', extension: '' }, 'an ending with a space is not one');
  assert.deepEqual(splitName(null), { base: '', extension: '' });
});

// ---- the pages ----

test('the library page has its keyboard hints and live region, and the sheet is styled', () => {
  const html = readFileSync(new URL('./library.html', import.meta.url), 'utf8');
  assert.match(html, /id="keys-status"[^>]*role="status"/);
  assert.match(html, /id="file-table"[^>]*aria-describedby="table-keys"/);
  assert.match(html, /id="rename-help"/);
  const css = readFileSync(new URL('./style.css', import.meta.url), 'utf8');
  assert.match(css, /\.shortcut-table tbody tr:nth-child\(even\)\{background:var\(--band\)\}/);
  assert.match(css, /input\.rename-inline/);
  // The old listeners are gone: the page's keys go through the registry.
  for (const file of ['library.js', 'palette.js', 'details.js', 'editor-frame.js']) {
    const script = readFileSync(new URL(`./${file}`, import.meta.url), 'utf8');
    assert.doesNotMatch(script, /addEventListener\('keydown', \(event\) => \{\s*(?:if \(!isUndoKey|const what = clipboardKey|if \(paletteShortcut|if \(!enabled\(\))/, file);
  }
});
