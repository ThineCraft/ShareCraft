import test from 'node:test';
import assert from 'node:assert/strict';
import { POLICY_KEYS, fontForm, packageBody, policyRows, roleChoices } from './brand-fonts.js';
import { fontEntries, isFontName } from './image-picker.js';

const DOC = '0000000a-0000-4000-8000-00000000000a';

test('licence_checkbox_required', () => {
  // Nothing chosen yet.
  assert.deepEqual(fontForm({ chosen: null, name: '', confirmed: false }), { canAdd: false, reason: 'Choose a font file first.', body: null });
  // A file chosen, but the licence not confirmed: Add stays off and says why.
  const unconfirmed = fontForm({ chosen: { doc: DOC, name: 'Brand.woff2' }, name: 'Brand', confirmed: false });
  assert.equal(unconfirmed.canAdd, false);
  assert.equal(unconfirmed.body, null);
  assert.match(unconfirmed.reason, /Confirm that your organisation holds the licence/);
  // A name is needed too, 1 to 120 characters.
  assert.match(fontForm({ chosen: { doc: DOC, name: 'B.woff2' }, name: '   ', confirmed: true }).reason, /name/);
  assert.match(fontForm({ chosen: { doc: DOC, name: 'B.woff2' }, name: 'x'.repeat(121), confirmed: true }).reason, /120/);
  // Confirmed: Add sends the document, the name and the confirmation.
  assert.deepEqual(fontForm({ chosen: { doc: DOC, name: 'Brand.woff2' }, name: ' Brand ', confirmed: true }), {
    canAdd: true,
    reason: '',
    body: { doc: DOC, name: 'Brand', licence_confirmed: true },
  });
  // A confirmation that is not exactly true is not one.
  assert.equal(fontForm({ chosen: { doc: DOC, name: 'B.woff2' }, name: 'B', confirmed: 'yes' }).canAdd, false);
});

test('the picker in font mode lists only font files', () => {
  assert.equal(isFontName('Brand.WOFF2'), true);
  assert.equal(isFontName('a.otf'), true);
  assert.equal(isFontName('a.ttf'), true);
  assert.equal(isFontName('a.woff'), true);
  assert.equal(isFontName('a.svg'), false);
  assert.equal(isFontName('woff2'), false);
  const items = [
    { id: 'd1', kind: 'document', name: 'Serif.woff2' },
    { id: 'd2', kind: 'document', name: 'Logo.png' },
    { id: 'd3', kind: 'document', name: 'Held.ttf', scan: 'held' },
    { id: 'f1', kind: 'folder', name: 'Fonts.otf' },
    { id: 'd4', kind: 'document', name: 'Body.otf' },
  ];
  assert.deepEqual(fontEntries(items, { name: 'Brand assets' }), [
    { doc: 'd4', name: 'Body.otf', library: 'Brand assets', thumb: null },
    { doc: 'd1', name: 'Serif.woff2', library: 'Brand assets', thumb: null },
  ]);
});

test('each role offers only the fonts the package has, and says why the others are off', () => {
  const none = roleChoices({ display: false, content: false });
  assert.deepEqual(none.map((c) => c.value), ['system', 'system-serif', 'display', 'content']);
  const display = none.find((c) => c.value === 'display');
  assert.equal(display.disabled, true);
  assert.match(display.reason, /Choose a display font first/);
  assert.equal(none.find((c) => c.value === 'system').disabled, false);
  const both = roleChoices({ display: true, content: true });
  assert.ok(both.every((c) => !c.disabled && c.reason === ''));
});

test('a package is sent with its fonts by id and every role', () => {
  const form = { name: ' Pair ', display: 'f1', content: '', roles: { title: 'display', heading: 'display', body: 'content', interactive: 'system' } };
  // A role naming a font the package lacks falls back to the system font, so it is never refused for that.
  assert.deepEqual(packageBody(form), {
    name: 'Pair',
    display: { brand_font: 'f1' },
    content: null,
    roles: { title: 'display', heading: 'display', body: 'system', interactive: 'system' },
  });
});

test('the limits for site owners are five switches in words', () => {
  const rows = policyRows({ hide_builtin_themes: false, custom_colours: true, site_themes: false, site_images: true, site_fonts: true });
  assert.deepEqual(rows.map((r) => r.key), POLICY_KEYS);
  assert.equal(rows.length, 5);
  for (const row of rows) {
    assert.match(row.label, /^[A-Z]/);
    assert.match(row.explanation, /\.$/);
    assert.equal(typeof row.checked, 'boolean');
  }
  assert.equal(rows.find((r) => r.key === 'site_themes').checked, false);
  // A missing policy reads as every limit off.
  assert.deepEqual(policyRows(undefined).map((r) => r.checked), [false, true, true, true, true]);
});
