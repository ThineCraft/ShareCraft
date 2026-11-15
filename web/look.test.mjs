import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

import { checkTheme, contrast, deriveDark, hexOf, lookTokens, parseColour } from './look.js';

// The same table the Rust tests read (crates/revisions/src/brand.rs).
const vectors = JSON.parse(readFileSync(new URL('./look-vectors.json', import.meta.url), 'utf8'));
const round2 = (x) => Math.round(x * 100) / 100;

test('contrast_matches_shared_vectors', () => {
  assert.ok(vectors.contrast.length > 10);
  for (const { a, b, ratio } of vectors.contrast) {
    assert.equal(round2(contrast(a, b)), ratio, `${a} on ${b}`);
    assert.equal(round2(contrast(b, a)), ratio, `${b} on ${a}`);
  }
  assert.equal(contrast('#000000', '#ffffff'), 21);
});

test('derive_dark_matches_shared_vectors', () => {
  for (const { from, role, dark } of vectors.derive_dark) {
    assert.equal(hexOf(deriveDark(from, role)), dark, `${role} ${from}`);
  }
});

test('built_in_themes_pass_every_check', () => {
  assert.equal(vectors.built_in_themes.length, 6);
  for (const t of vectors.built_in_themes) {
    const result = checkTheme({ primary: t.primary, secondary: t.secondary });
    assert.equal(result.ok, true, t.id);
    assert.deepEqual(result.resolved, t.resolved, t.id);
    // The worse of the two dark page colours is the dark paper: 4.51 to 4.70 for these themes.
    const dark = parseColour(result.resolved.dark.accent);
    const ratio = Math.min(contrast(dark, '#202a2e'), contrast(dark, '#141c20'));
    assert.ok(ratio >= 4.5 && ratio <= 4.7, `${t.id} dark accent: ${ratio}`);
  }
});

test('every_check_on_every_path', () => {
  const names = new Set();
  for (const { note, theme, pairs } of vectors.failing_themes) {
    const result = checkTheme(theme);
    assert.equal(result.ok, false, note);
    assert.deepEqual(result.pairs, pairs, note);
    for (const p of result.pairs) names.add(p.check);
  }
  // Five checks fail in these cases; the sixth is covered below.
  assert.deepEqual(
    [...names].sort(),
    [
      'button_on_primary',
      'header_on_secondary',
      'header_on_secondary_dark',
      'primary_dark_on_dark',
      'primary_on_light',
    ],
  );
  // button_on_primary_dark never fails beside a passing primary_dark_on_dark: a colour that
  // reads on the dark paper always carries the dark ink. So a dark accent that fails only the
  // paper check shows just that pair.
  const dim = checkTheme({
    primary: '#3156c9',
    secondary: '#233239',
    primary_dark: '#3a4a52',
  });
  assert.deepEqual(
    dim.pairs.map((p) => p.check),
    ['primary_dark_on_dark'],
  );
});

test('colour_parsing_refuses_anything_but_hex', () => {
  assert.deepEqual(parseColour('#3156C9'), { r: 0x31, g: 0x56, b: 0xc9 });
  assert.equal(hexOf(parseColour('#3156C9')), '#3156c9');
  for (const bad of [
    'red',
    '#fff',
    '3156c9',
    '#3156c9 ',
    ' #3156c9',
    '#3156c9ff',
    '#3156cg',
    'red;background:url(x)',
    '#3156c9;x',
    'rgb(1,2,3)',
    '',
    null,
    undefined,
    42,
  ]) {
    assert.equal(parseColour(bad), null, String(bad));
  }
  assert.equal(checkTheme({ primary: 'red', secondary: '#233239' }).ok, false);
  assert.equal(checkTheme({ primary: '#3156c9', secondary: '#233239', primary_dark: 'x' }).ok, false);
});

test('look_tokens_use_light_dark', () => {
  const effective = {
    theme: checkTheme({ primary: '#3156c9', secondary: '#233239' }).resolved,
  };
  assert.deepEqual(lookTokens(effective), {
    '--accent': 'light-dark(#3156c9,#748ddd)',
    '--accent-ink': 'light-dark(#3156c9,#748ddd)',
    '--on-accent': 'light-dark(#ffffff,#141c20)',
    '--header-bg': 'light-dark(#233239,#233239)',
    '--header-ink': 'light-dark(#ffffff,#ffffff)',
  });
});
