import test from 'node:test';
import assert from 'node:assert/strict';
import { buttonKey, menuKey, typeahead } from './menu.js';

test('Down and Up move between entries and wrap around', () => {
  assert.deepEqual(menuKey('ArrowDown', 0, 3), { focus: 1 });
  assert.deepEqual(menuKey('ArrowDown', 2, 3), { focus: 0 });
  assert.deepEqual(menuKey('ArrowUp', 0, 3), { focus: 2 });
  assert.deepEqual(menuKey('ArrowUp', 2, 3), { focus: 1 });
  // Old key names (some browsers) do the same.
  assert.deepEqual(menuKey('Down', 1, 3), { focus: 2 });
  assert.deepEqual(menuKey('Up', 1, 3), { focus: 0 });
});

test('Home and End go to the first and last entry', () => {
  assert.deepEqual(menuKey('Home', 2, 4), { focus: 0 });
  assert.deepEqual(menuKey('End', 0, 4), { focus: 3 });
  assert.deepEqual(menuKey('PageUp', 2, 4), { focus: 0 });
  assert.deepEqual(menuKey('PageDown', 0, 4), { focus: 3 });
});

test('with focus on no entry, Down starts at the first and Up at the last', () => {
  assert.deepEqual(menuKey('ArrowDown', -1, 3), { focus: 0 });
  assert.deepEqual(menuKey('ArrowUp', -1, 3), { focus: 2 });
  assert.deepEqual(menuKey('ArrowDown', 7, 3), { focus: 0 });
});

test('Escape closes and returns focus; Tab closes and moves on', () => {
  assert.deepEqual(menuKey('Escape', 1, 3), { close: 'refocus' });
  assert.deepEqual(menuKey('Esc', 1, 3), { close: 'refocus' });
  assert.deepEqual(menuKey('Tab', 1, 3), { close: 'leave' });
  // Even an empty menu closes.
  assert.deepEqual(menuKey('Escape', -1, 0), { close: 'refocus' });
});

test('other keys and an empty menu are left alone', () => {
  assert.equal(menuKey('Enter', 0, 3), null);
  assert.equal(menuKey(' ', 0, 3), null);
  assert.equal(menuKey('a', 0, 3), null);
  assert.equal(menuKey('ArrowDown', -1, 0), null);
  assert.equal(menuKey('ArrowLeft', 0, 3), null);
});

test('on the button, Down opens on the first entry and Up on the last', () => {
  assert.equal(buttonKey('ArrowDown'), 'first');
  assert.equal(buttonKey('Down'), 'first');
  assert.equal(buttonKey('ArrowUp'), 'last');
  assert.equal(buttonKey('Up'), 'last');
  assert.equal(buttonKey('Enter'), null);
  assert.equal(buttonKey(' '), null);
  assert.equal(buttonKey('Escape'), null);
});

test('a typed letter jumps to the next entry that starts with it, wrapping around', () => {
  const labels = ['Rename', 'Move…', 'Copy…', 'Delete', 'Access', 'More'];
  assert.equal(typeahead(labels, 0, 'm'), 1);
  assert.equal(typeahead(labels, 1, 'm'), 5);
  assert.equal(typeahead(labels, 5, 'M'), 1);
  assert.equal(typeahead(labels, -1, 'r'), 0);
  assert.equal(typeahead(labels, 0, 'r'), 0); // the only match is where focus already is
  assert.equal(typeahead([' Folder', 'File'], -1, 'f'), 0);
});

test('typeahead ignores keys that are not one printable character, and misses', () => {
  const labels = ['Rename', 'Delete'];
  assert.equal(typeahead(labels, 0, 'z'), -1);
  assert.equal(typeahead(labels, 0, ' '), -1);
  assert.equal(typeahead(labels, 0, 'Shift'), -1);
  assert.equal(typeahead(labels, 0, 'ArrowDown'), -1);
  assert.equal(typeahead([], -1, 'a'), -1);
});
