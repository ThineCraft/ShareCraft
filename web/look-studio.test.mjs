import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  STUDIO_PARTS,
  changedParts,
  chosenCard,
  partFromHash,
  previewPanes,
  selectTab,
  studioState,
  studioView,
  studioWide,
  swatchModel,
  tabKey,
  themeGallery,
  toggleSection,
} from './look-studio.js';
import { draftFromLook } from './look.js';

test('the studio starts on the theme, or on the part the address names', () => {
  assert.deepEqual(studioState(), { tab: 'theme', open: ['theme'] });
  assert.deepEqual(studioState('footer'), { tab: 'footer', open: ['footer'] });
  assert.deepEqual(studioState('nonsense'), { tab: 'theme', open: ['theme'] });
  assert.equal(partFromHash('#history'), 'history');
  assert.equal(partFromHash('header'), 'header');
  assert.equal(partFromHash('#preview'), null);
  assert.equal(partFromHash(undefined), null);
});

test('choosing a tab opens its section alone; toggling a section makes it the tab only when it opens', () => {
  let state = studioState();
  state = toggleSection(state, 'header');
  assert.deepEqual(state, { tab: 'header', open: ['theme', 'header'] });
  state = selectTab(state, 'header');
  assert.deepEqual(state, { tab: 'header', open: ['header'] });
  assert.equal(selectTab(state, 'nope'), state);
  state = toggleSection(state, 'theme');
  state = toggleSection(state, 'theme');
  assert.deepEqual(state, { tab: 'theme', open: ['header'] });
  state = toggleSection(state, 'footer');
  assert.deepEqual(state, { tab: 'footer', open: ['header', 'footer'] });
  assert.equal(toggleSection(state, 'nope'), state);
});

test('wide screens show one part as a tab panel; narrow ones every part as an accordion', () => {
  const state = { tab: 'navigation', open: ['navigation', 'footer'] };
  const wide = studioView(state, true);
  assert.deepEqual(Object.keys(wide), STUDIO_PARTS);
  assert.deepEqual(wide.navigation, { hidden: false, bodyHidden: false, selected: true, expanded: true });
  assert.deepEqual(wide.theme, { hidden: true, bodyHidden: false, selected: false, expanded: true });
  const narrow = studioView(state, false);
  assert.deepEqual(narrow.footer, { hidden: false, bodyHidden: false, selected: false, expanded: true });
  assert.deepEqual(narrow.theme, { hidden: false, bodyHidden: true, selected: false, expanded: false });
});

test('arrow keys move along the tabs and wrap; Home and End go to the ends', () => {
  assert.equal(tabKey('ArrowRight', 4, 5), 0);
  assert.equal(tabKey('ArrowDown', 1, 5), 2);
  assert.equal(tabKey('ArrowLeft', 0, 5), 4);
  assert.equal(tabKey('ArrowUp', 3, 5), 2);
  assert.equal(tabKey('Home', 3, 5), 0);
  assert.equal(tabKey('End', 0, 5), 4);
  assert.equal(tabKey('Enter', 0, 5), null);
  assert.equal(tabKey('ArrowRight', 0, 0), null);
});

test('the preview switch: light, dark, or light in a phone frame', () => {
  assert.deepEqual(previewPanes('light'), { light: true, dark: false, phone: false });
  assert.deepEqual(previewPanes('dark'), { light: false, dark: true, phone: false });
  assert.deepEqual(previewPanes('phone'), { light: true, dark: false, phone: true });
  assert.deepEqual(previewPanes('other'), previewPanes('light'));
});

test('a theme card takes the checked colours when the server sent them', () => {
  const card = swatchModel({
    from: 'builtin:ocean',
    name: 'Ocean',
    primary: '#000000',
    secondary: '#000000',
    resolved: {
      light: { accent: '#3156c9', header_bg: '#1f2d6b' },
      dark: { accent: '#8fa8ff', header_bg: '#24305e' },
    },
  });
  assert.deepEqual(card, {
    from: 'builtin:ocean',
    name: 'Ocean',
    light: { primary: '#3156c9', secondary: '#1f2d6b' },
    dark: { primary: '#8fa8ff', secondary: '#24305e' },
  });
});

test('a theme card works out the dark colours the preview would draw', () => {
  const card = swatchModel({ from: 'site:1', name: '  ', primary: '#3156C9', secondary: '#1f2d6b' });
  assert.equal(card.name, 'Untitled');
  assert.equal(card.light.primary, '#3156c9');
  assert.equal(card.light.secondary, '#1f2d6b');
  assert.match(card.dark.primary, /^#[0-9a-f]{6}$/);
  assert.notEqual(card.dark.primary, '#3156c9');
  // a theme with a colour that is not one still gets chips
  const broken = swatchModel({ name: 'Mine', primary: 'blue', secondary: '#1f2d6b' });
  assert.equal(broken.from, 'custom');
  assert.match(broken.light.primary, /^#[0-9a-f]{6}$/);
});

test('the gallery groups built-in, organisation and site themes, leaving out empty groups', () => {
  const brand = [
    { from: 'builtin:a', name: 'A', builtin: true, primary: '#3156c9', secondary: '#1f2d6b' },
    { from: 'brand:b', name: 'B', primary: '#3156c9', secondary: '#1f2d6b' },
  ];
  const groups = themeGallery(brand, []);
  assert.deepEqual(groups.map((g) => g.label), ['Built in', 'Organisation']);
  assert.deepEqual(groups[1].cards.map((c) => c.from), ['brand:b']);
  const site = [{ from: 'site:c', name: 'C', primary: '#3156c9', secondary: '#1f2d6b' }];
  assert.deepEqual(themeGallery([], site).map((g) => g.label), ['This site']);
  assert.deepEqual(themeGallery(), []);
});

test('the marked card is the draft theme while the page offers it, else Custom colours', () => {
  const themes = [{ from: 'builtin:a' }];
  assert.equal(chosenCard({ from: 'builtin:a' }, themes), 'builtin:a');
  assert.equal(chosenCard({ from: 'brand:gone' }, themes), 'custom');
  assert.equal(chosenCard({ from: 'custom' }, themes), 'custom');
});

test('the tabs know which parts of the draft are not applied yet', () => {
  const saved = null;
  const draft = draftFromLook(saved);
  assert.deepEqual(changedParts(draft, saved), []);
  // an empty footer group the editor shows is not a change
  draft.footer.groups.push({ heading: '', links: [] });
  assert.deepEqual(changedParts(draft, saved), []);
  draft.navigation = 'vertical';
  draft.header.layout = 'extended';
  assert.deepEqual(changedParts(draft, saved), ['header', 'navigation']);
  draft.theme.primary = '#123456';
  draft.from = 'custom';
  draft.footer.groups[0].links.push({ label: 'Help', url: '/help' });
  assert.deepEqual(changedParts(draft, saved), ['theme', 'header', 'navigation', 'footer']);
  const other = draftFromLook(null);
  other.logo_alt = 'Our logo';
  assert.deepEqual(changedParts(other, null), ['header']);
});

test('two panes need a wide window and room beside the site sidebar',() => {
  assert.equal(studioWide(1400, 1100), true);
  assert.equal(studioWide(1100, 840), true);
  assert.equal(studioWide(1099, 1099), false);
  assert.equal(studioWide(1200, 800), false);
});

// ---- Plan 4B: fonts, limits and copying ----

import { STUDIO_LIMITS, choiceLimit, copyChoices, packageGallery, studioLimits } from './look-studio.js';

test('limits_disable_with_reasons', () => {
  const closed = { hide_builtin_themes: true, custom_colours: false, site_themes: false, site_images: false, site_fonts: false };
  const open = { hide_builtin_themes: false, custom_colours: true, site_themes: true, site_images: true, site_fonts: true };
  // A site owner under every limit: each choice is disabled, and each says why in words.
  const limited = studioLimits(closed, true);
  assert.deepEqual(Object.keys(limited), STUDIO_LIMITS);
  for (const key of STUDIO_LIMITS) {
    assert.equal(limited[key].disabled, true, key);
    assert.match(limited[key].reason, /turned off for site owners/, key);
    assert.match(limited[key].reason, /^[A-Z].*\.$/, `${key} is a sentence`);
  }
  // Nothing is ever disabled without a reason, and nothing has a reason when allowed.
  for (const [policy, who] of [
    [open, true],
    [closed, false],
    [undefined, true],
  ]) {
    for (const value of Object.values(studioLimits(policy, who))) assert.deepEqual(value, { disabled: false, reason: '' });
  }
  // One limit at a time disables only its own choices.
  const custom = studioLimits({ ...open, custom_colours: false }, true);
  assert.equal(custom.customColours.disabled, true);
  assert.equal(custom.builtinThemes.disabled, false);
  // The gallery's cards, the site-only packages and the copy options are matched to their limit.
  assert.equal(choiceLimit('builtin:plum', limited), limited.builtinThemes);
  assert.equal(choiceLimit('custom', limited), limited.customColours);
  assert.equal(choiceLimit('site:t1', limited), limited.siteThemes);
  assert.equal(choiceLimit('site:p1', limited, 'fonts'), limited.siteFonts);
  assert.equal(choiceLimit('builtin:classic', limited, 'fonts'), null, 'built-in font packages are never limited');
  assert.equal(choiceLimit('org:t1', limited), null, 'organisation themes are always allowed');
  // An administrator or brand manager is never limited.
  assert.equal(choiceLimit('custom', studioLimits(closed, false)), null);
});

test('the font gallery groups built-in, organisation and site packages with their specimens', () => {
  const sha = 'a'.repeat(64);
  const brand = [
    { id: 'builtin:system', from: 'builtin:system', name: 'System', builtin: true, roles: { title: 'system', heading: 'system', body: 'system', interactive: 'system' } },
    { id: 'builtin:classic', from: 'builtin:classic', name: 'Classic', builtin: true, roles: { title: 'system-serif', heading: 'system-serif', body: 'system', interactive: 'system' } },
    { id: 'p1', from: 'org:p1', name: 'Pair', builtin: false, display: { name: 'D', family: `sc-${'a'.repeat(12)}`, url: `/api/brand/fonts/${sha}` }, roles: { title: 'display', heading: 'display', body: 'system', interactive: 'system' } },
  ];
  const site = [{ id: 's1', from: 'site:s1', name: 'Ours', roles: { title: 'system', heading: 'system', body: 'system', interactive: 'system' } }];
  const groups = packageGallery(brand, site);
  assert.deepEqual(groups.map((g) => g.label), ['Built in', 'Organisation', 'This site']);
  assert.deepEqual(groups[0].cards.map((c) => c.from), ['builtin:system', 'builtin:classic']);
  assert.equal(groups[1].cards[0].fonts.title.family, `sc-${'a'.repeat(12)}`);
  assert.equal(groups[0].cards[1].fonts.heading.family, 'system-serif');
  assert.deepEqual(packageGallery([], []), []);
});

test('the copy dialog lists the other sites the person can see, by name', () => {
  const sites = [
    { id: 'b', name: 'beta' },
    { id: 'me', name: 'Sales' },
    { id: 'a', name: 'Alpha' },
    { id: 'z', name: 'Zed', archived: true },
  ];
  assert.deepEqual(copyChoices(sites, 'me').map((s) => s.id), ['a', 'b', 'z']);
  assert.deepEqual(copyChoices(null, 'me'), []);
});

test('the tabs mark fonts, the tab icon and the menu style as changes', () => {
  const draft = draftFromLook(null);
  draft.fonts = { from: 'builtin:classic', name: 'Classic', keep: false };
  draft.icon = { brand_image: 'i1', sha256: 'b'.repeat(64), name: 'Icon' };
  draft.menu = 'mega';
  assert.deepEqual(changedParts(draft, null), ['fonts', 'header', 'navigation']);
  assert.ok(STUDIO_PARTS.includes('fonts'));
  assert.equal(STUDIO_PARTS.indexOf('fonts'), 1, 'Fonts come after Theme');
});

import { FONTS_SOURCE_CHANGED, fontsChoice } from './look-studio.js';

test('a kept font copy whose package changed in the brand centre is noted and its card left unchosen', () => {
  const pkg = { name: 'Brand', roles: { title: 'display', heading: 'display', body: 'system', interactive: 'system' } };
  const look = { from: 'builtin:plum', fonts: { from: 'org:p1', package: pkg } };
  const offered = ['builtin:system', 'builtin:classic', 'org:p1'];
  const draft = draftFromLook(look);
  // The source is unchanged: the card is chosen and there is no note.
  assert.deepEqual(fontsChoice(draft, { look, fonts_source_changed: false }, offered), { checked: 'org:p1', note: null });
  // Changed at its source: the note asks to choose it again, and no card is chosen.
  const changed = { look, fonts_source_changed: true };
  assert.equal(FONTS_SOURCE_CHANGED, 'This font package changed in the brand centre — choose it again to use the new version.');
  assert.deepEqual(fontsChoice(draft, changed, offered), { checked: '', note: FONTS_SOURCE_CHANGED });
  // Once it is picked (sent as {from}), its card is chosen and the note goes.
  const picked = { ...draft, fonts: { ...draft.fonts, keep: false } };
  assert.deepEqual(fontsChoice(picked, changed, offered), { checked: 'org:p1', note: null });
  // A copy of a package no longer offered keeps the older note; the system fonts are always offered.
  const gone = fontsChoice(draft, changed, ['builtin:system']);
  assert.equal(gone.checked, '');
  assert.match(gone.note, /no longer offered/);
  assert.deepEqual(fontsChoice(draftFromLook({ from: 'builtin:plum' }), changed, offered), { checked: 'builtin:system', note: null });
});
