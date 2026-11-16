import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';
import { AVATAR_PALETTE, avatarColour, initials, versionText } from './account.js';
import { DARK_BACKGROUND, DARK_PAPER, LIGHT_BACKGROUND, LIGHT_PAPER, MIN_RATIO, contrast } from './look.js';
import { menuItems } from './menu.js';

test('initials take the first and last word of a name, in capitals', () => {
  assert.equal(initials('Ann Tester'), 'AT');
  assert.equal(initials('mary ann de vries'), 'MV');
  assert.equal(initials('  Ann  '), 'A');
  assert.equal(initials('Émile Zola'), 'ÉZ');
  assert.equal(initials('Ann (Finance) Tester'), 'AT');
  assert.equal(initials('李 小龙'), '李小');
});

test('initials of an email address come from the part before the @', () => {
  assert.equal(initials('ann.tester@example.com'), 'AT');
  assert.equal(initials('ann@example.com'), 'A');
  assert.equal(initials('first_last+tag@example.com'), 'FT');
});

test('initials of nothing usable are a question mark', () => {
  assert.equal(initials(''), '?');
  assert.equal(initials(null), '?');
  assert.equal(initials('-- !!'), '?');
});

test('avatarColour is stable for an id and spreads ids over the palette', () => {
  const id = '11111111-1111-4111-8111-111111111111';
  assert.equal(avatarColour(id), avatarColour(id));
  assert.ok(AVATAR_PALETTE.includes(avatarColour(id)));
  const used = new Set();
  for (let n = 0; n < 200; n += 1) used.add(avatarColour(`00000000-0000-4000-8000-${String(n).padStart(12, '0')}`).name);
  assert.equal(used.size, AVATAR_PALETTE.length);
  assert.ok(AVATAR_PALETTE.includes(avatarColour(undefined)));
});

test('every avatar colour reads at 4.5:1 or more in light and in dark', () => {
  for (const colour of AVATAR_PALETTE) {
    for (const scheme of ['light', 'dark']) {
      const { bg, ink } = colour[scheme];
      const ratio = contrast(bg, ink);
      assert.ok(ratio >= MIN_RATIO, `${colour.name} ${scheme}: ${ratio.toFixed(2)}`);
    }
    // and the circle stands out from the bar it sits on (3:1 for a shape)
    for (const page of [LIGHT_PAPER, LIGHT_BACKGROUND]) assert.ok(contrast(colour.light.bg, page) >= 3, `${colour.name} light on ${page}`);
    for (const page of [DARK_PAPER, DARK_BACKGROUND]) assert.ok(contrast(colour.dark.bg, page) >= 3, `${colour.name} dark on ${page}`);
  }
});

test('menu entries include radio items, which a menu keeps among its keyboard stops', () => {
  const item = (role, hidden = false) => ({ role, hidden, disabled: false, closest: () => null });
  const entries = [item('menuitem'), item('menuitemradio'), item('menuitemradio', true)];
  const menu = { querySelectorAll: (selector) => entries.filter((e) => selector.includes(`"${e.role}"`)) };
  assert.equal(menuItems(menu).length, 2);
});

// appearance.js is a plain script for <head>; it is run here in a small fake page.
function loadAppearance({ stored = null, prefersDark = false, storageThrows = false } = {}) {
  const store = new Map(stored == null ? [] : [['sharecraft-appearance', stored]]);
  const props = new Map();
  const attrs = new Map();
  const listeners = [];
  const media = { matches: prefersDark, addEventListener: (_, fn) => listeners.push(fn) };
  const localStorage = {
    getItem(key) {
      if (storageThrows) throw new Error('blocked');
      return store.has(key) ? store.get(key) : null;
    },
    setItem(key, value) {
      if (storageThrows) throw new Error('blocked');
      store.set(key, value);
    },
    removeItem(key) {
      if (storageThrows) throw new Error('blocked');
      store.delete(key);
    },
  };
  const root = {
    style: { setProperty: (k, v) => props.set(k, v), removeProperty: (k) => props.delete(k) },
    setAttribute: (k, v) => attrs.set(k, v),
  };
  const context = { localStorage, matchMedia: () => media, document: { documentElement: root }, addEventListener() {} };
  context.globalThis = context;
  vm.runInNewContext(readFileSync(new URL('./appearance.js', import.meta.url), 'utf8'), context);
  const change = (dark) => {
    media.matches = dark;
    for (const fn of listeners) fn();
  };
  return { api: context.sharecraftAppearance, props, attrs, store, change };
}

test('resolveAppearance: System follows the device, Light and Dark do not', () => {
  const { resolveAppearance } = loadAppearance().api;
  assert.deepEqual({ ...resolveAppearance('system', true) }, { choice: 'system', scheme: 'dark' });
  assert.deepEqual({ ...resolveAppearance('system', false) }, { choice: 'system', scheme: 'light' });
  assert.deepEqual({ ...resolveAppearance('light', true) }, { choice: 'light', scheme: 'light' });
  assert.deepEqual({ ...resolveAppearance('dark', false) }, { choice: 'dark', scheme: 'dark' });
  assert.deepEqual({ ...resolveAppearance(null, true) }, { choice: 'system', scheme: 'dark' });
  assert.deepEqual({ ...resolveAppearance('purple', false) }, { choice: 'system', scheme: 'light' });
});

test('appearance.js applies the stored choice at load through CSSOM', () => {
  const dark = loadAppearance({ stored: 'dark' });
  assert.equal(dark.props.get('color-scheme'), 'dark');
  assert.equal(dark.attrs.get('data-appearance'), 'dark');
  const system = loadAppearance({ prefersDark: true });
  assert.equal(system.props.has('color-scheme'), false);
  assert.equal(system.attrs.get('data-appearance'), 'dark');
  system.change(false);
  assert.equal(system.attrs.get('data-appearance'), 'light');
});

test('appearance.js keeps a new choice, and still applies it when storage is blocked', () => {
  const page = loadAppearance();
  page.api.set('light');
  assert.equal(page.store.get('sharecraft-appearance'), 'light');
  assert.equal(page.props.get('color-scheme'), 'light');
  assert.equal(page.api.get(), 'light');
  page.api.set('system');
  assert.equal(page.store.has('sharecraft-appearance'), false);
  assert.equal(page.props.has('color-scheme'), false);

  const blocked = loadAppearance({ storageThrows: true });
  assert.equal(blocked.api.get(), 'system');
  blocked.api.set('dark');
  assert.equal(blocked.props.get('color-scheme'), 'dark');
});

test('version_shown_at_the_foot_of_signed_in_pages', () => {
  assert.equal(versionText('0.1.0-6a18271'), 'ShareCraft 0.1.0-6a18271');
  assert.equal(versionText('0.1.0-dev'), 'ShareCraft 0.1.0-dev');
  assert.equal(versionText('0.1.0-6a18271-modified'), 'ShareCraft 0.1.0-6a18271-modified');
  // Nothing that is not a plain version is shown.
  for (const bad of [undefined, null, '', 42, '<b>1</b>', '1.0 beta', 'x'.repeat(41)]) {
    assert.equal(versionText(bad), null, String(bad));
  }
});
