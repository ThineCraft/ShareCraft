import test from 'node:test';
import assert from 'node:assert/strict';
import { applyChunks, brandLookCard, bulkApplySummary, effectiveLookCard, templateLookCard, templateLookReport } from './look-cards.js';

const SHA = 'f'.repeat(64);
const FAMILY = `sc-${'f'.repeat(12)}`;
const BRAND_FONTS = [{ id: 'font-1', name: 'Display.woff2', family: FAMILY, url: `/api/brand/fonts/${SHA}` }];
const PLUM = { name: 'Plum', primary: '#7a3e8f', secondary: '#3b2244' };

test('bulk_apply_summary', () => {
  const names = new Map([
    ['s1', 'Sales'],
    ['s2', 'Marketing'],
    ['s3', 'Old site'],
    ['s4', 'Legal'],
  ]);
  const results = [
    { site: 's1', ok: true },
    { site: 's2', ok: false, error: 'invalid_look' },
    { site: 's3', ok: false, error: 'archived' },
    { site: 's4', ok: true, unresolved: [{ slot: 'logo', id: 'x' }] },
  ];
  const summary = bulkApplySummary(results, names);
  assert.equal(summary.applied, 2);
  assert.equal(summary.skipped, 1);
  assert.equal(summary.failed, 1);
  assert.equal(summary.kind, 'error', 'a failure makes the summary an error toast');
  assert.equal(summary.text, 'The look was applied to 2 sites. 1 archived site was skipped. 1 site could not be changed.');
  assert.deepEqual(
    summary.rows.map((r) => [r.name, r.ok, r.text]),
    [
      ['Sales', true, 'Applied.'],
      ['Marketing', false, 'Not changed: the look could not be used.'],
      ['Old site', false, 'Skipped: the site is archived.'],
      ['Legal', true, 'Applied, without the logo, which the brand centre does not hold.'],
    ],
  );
  // All applied: a success toast in one sentence.
  const fine = bulkApplySummary([{ site: 's1', ok: true }], names);
  assert.equal(fine.text, 'The look was applied to 1 site.');
  assert.equal(fine.kind, 'ok');
  // A site not named shows as an unknown site, never as its id alone.
  assert.equal(bulkApplySummary([{ site: 'zz', ok: true }], names).rows[0].name, 'A site');
  // A summary of nothing says so.
  assert.equal(bulkApplySummary([], names).text, 'No site was changed.');
});

test('a bulk apply is sent in rounds of at most 200 sites, in order', () => {
  const ids = Array.from({ length: 450 }, (_, i) => `s${i}`);
  const rounds = applyChunks(ids, 200);
  assert.deepEqual(rounds.map((r) => r.length), [200, 200, 50]);
  assert.deepEqual(rounds.flat(), ids);
  assert.deepEqual(applyChunks(['a', 'a', 'b'], 10), [['a', 'b']], 'a site is applied once');
  assert.deepEqual(applyChunks([], 10), []);
  assert.deepEqual(applyChunks(['a', 'b', 'c'], 0), [['a'], ['b'], ['c']], 'a round holds at least one site');
});

test('a template look card resolves brand-centre fonts by id', () => {
  const look = {
    theme: PLUM,
    from: 'builtin:plum',
    logo_alt: '',
    header: { layout: 'compact', background: 'none', align: 'left', hide_title: false },
    navigation: 'horizontal',
    footer: { enabled: false, layout: 'simple', name: '', background: 'none', groups: [] },
    fonts: { from: 'org:p1', package: { name: 'Pair', display: { brand_font: 'font-1' }, roles: { title: 'display', heading: 'display', body: 'system', interactive: 'system' } } },
  };
  const card = templateLookCard(look, BRAND_FONTS);
  assert.equal(card.name, 'Plum');
  assert.equal(card.light.primary, '#7a3e8f');
  assert.equal(card.light.secondary, '#3b2244');
  assert.match(card.dark.primary, /^#[0-9a-f]{6}$/);
  assert.equal(card.fontName, 'Pair');
  assert.deepEqual(card.fonts.title, { family: FAMILY, url: `/api/brand/fonts/${SHA}` });
  assert.equal(card.text, 'Look: the theme Plum, with the fonts Pair.');
  // A reference the organisation cannot fill falls back to the system fonts.
  assert.deepEqual(templateLookCard(look, []).fonts.title, { family: 'system' });
  // A template without a look has no card.
  assert.equal(templateLookCard(null, BRAND_FONTS), null);
  const plain = templateLookCard({ ...look, fonts: undefined }, BRAND_FONTS);
  assert.equal(plain.fontName, 'System');
  assert.equal(plain.text, 'Look: the theme Plum, with the system fonts.');
});

test('a site look card comes from its effective look', () => {
  const effective = {
    theme: { light: { accent: '#3156c9', header_bg: '#233239' }, dark: { accent: '#8fa8ff', header_bg: '#1b262b' } },
    fonts: { title: { family: FAMILY, url: `/api/brand/fonts/${SHA}` }, heading: { family: 'system' }, body: { family: 'system' }, interactive: { family: 'system' } },
  };
  const card = effectiveLookCard(effective, 'Sales');
  assert.deepEqual(card.light, { primary: '#3156c9', secondary: '#233239' });
  assert.deepEqual(card.dark, { primary: '#8fa8ff', secondary: '#1b262b' });
  assert.equal(card.fonts.title.family, FAMILY);
  assert.equal(card.text, 'The look of Sales.');
  assert.equal(effectiveLookCard(null, 'Sales'), null);
});

test('saving and importing a template say what its look left out', () => {
  assert.equal(templateLookReport({}), '');
  assert.equal(
    templateLookReport({ dropped: [{ slot: 'logo', name: 'Logo.png' }, { slot: 'header.image', name: 'Band.jpg' }] }),
    'The template keeps the site’s look, without these pictures from the site’s own libraries (only brand-centre images go into a template): the logo (Logo.png), the header image (Band.jpg).',
  );
  assert.equal(
    templateLookReport({ unresolved: [{ slot: 'fonts.display', id: 'x' }, { slot: 'icon', id: 'y' }] }),
    'Its look refers to brand-centre items this organisation does not have, so they were left out: the display font, the tab icon.',
  );
});

test('a brand-centre theme and package make a card', () => {
  const card = brandLookCard({ name: 'Plum', primary: '#7a3e8f', secondary: '#3b2244' }, { name: 'Pair', display: BRAND_FONTS[0], roles: { title: 'display', heading: 'system', body: 'system', interactive: 'system' } });
  assert.equal(card.light.primary, '#7a3e8f');
  assert.equal(card.fonts.title.family, FAMILY);
  assert.equal(card.text, 'The theme Plum, with the fonts Pair.');
  assert.equal(brandLookCard({ name: 'Plum', primary: '#7a3e8f', secondary: '#3b2244' }).text, 'The theme Plum, with the system fonts.');
  assert.equal(brandLookCard(null), null);
});
