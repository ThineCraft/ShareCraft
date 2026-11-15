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
