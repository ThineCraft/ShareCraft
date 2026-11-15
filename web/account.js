// The account bar's right-hand end: a round avatar with the person's initials that opens the
// account menu (round 1's menu.js): who is signed in, Admin centre and Brand centre for those who may
// use them, Appearance (System, Light, Dark: appearance.js keeps and applies it) and Sign out.
//
// The bar is never themed by a site's look, and neither is the avatar: its colour comes from the
// person's id, out of a fixed palette that reads in light and dark.
//
// `initials` and `avatarColour` are pure and tested; `setupAccountMenu` needs a page. This module
// does not import common.js (common.js imports it).

import { setupMenu } from './menu.js';

/**
 * The avatar colours: a background and the ink of the initials on it, for light and for dark.
 * Every pair is at least 4.5:1 (checked in account.test.mjs).
 */
export const AVATAR_PALETTE = Object.freeze([
  { name: 'blue', light: { bg: '#2f5bd3', ink: '#ffffff' }, dark: { bg: '#a9c1ff', ink: '#141c20' } },
  { name: 'green', light: { bg: '#1d7046', ink: '#ffffff' }, dark: { bg: '#8fdcb0', ink: '#141c20' } },
  { name: 'purple', light: { bg: '#6d47c2', ink: '#ffffff' }, dark: { bg: '#c9b3ff', ink: '#141c20' } },
  { name: 'magenta', light: { bg: '#a82d76', ink: '#ffffff' }, dark: { bg: '#f5a8d0', ink: '#141c20' } },
  { name: 'teal', light: { bg: '#0a6c77', ink: '#ffffff' }, dark: { bg: '#85dbe5', ink: '#141c20' } },
  { name: 'orange', light: { bg: '#a84600', ink: '#ffffff' }, dark: { bg: '#ffb27a', ink: '#141c20' } },
  { name: 'red', light: { bg: '#b3261e', ink: '#ffffff' }, dark: { bg: '#ffa69e', ink: '#141c20' } },
  { name: 'brown', light: { bg: '#76553a', ink: '#ffffff' }, dark: { bg: '#e0bf9c', ink: '#141c20' } },
]);

const LETTER = /[\p{L}\p{N}]/u;

/** The first letter or digit of `word` (a whole character, however many code units), or ''. */
function firstLetter(word) {
  for (const char of word) if (LETTER.test(char)) return char;
  return '';
}

/**
 * Up to two initials for a name or an email address, in capitals: "Ann Tester" gives "AT",
 * "Mary Ann de Vries" gives "MV", "ann.tester@example.com" gives "AT", "ann" gives "A". Words
 * without a letter or digit are skipped; nothing usable gives "?".
 */
export function initials(text) {
  let source = String(text ?? '').trim();
  const at = source.indexOf('@');
  if (at > 0 && !/\s/.test(source)) source = source.slice(0, at).replace(/[._+-]+/g, ' ');
  const letters = source
    .split(/\s+/)
    .map(firstLetter)
    .filter(Boolean);
  if (letters.length === 0) return '?';
  const picked = letters.length === 1 ? letters : [letters[0], letters[letters.length - 1]];
  return picked.join('').toLocaleUpperCase();
}

/** A stable colour from AVATAR_PALETTE for `id` (FNV-1a hash of the text): the same id, the same colour. */
export function avatarColour(id) {
  let hash = 0x811c9dc5;
  for (const char of String(id ?? '')) {
    hash ^= char.codePointAt(0);
    hash = Math.imul(hash, 0x01000193) >>> 0;
  }
  return AVATAR_PALETTE[hash % AVATAR_PALETTE.length];
}

/** The appearance choices in the menu, in order. */
export const APPEARANCE_CHOICES = Object.freeze([
  { value: 'system', label: 'System' },
  { value: 'light', label: 'Light' },
  { value: 'dark', label: 'Dark' },
]);

// ---- wiring (needs a page) ----

const SVG = 'http://www.w3.org/2000/svg';

function el(tag, attributes = {}, ...children) {
  const element = document.createElement(tag);
  for (const [name, value] of Object.entries(attributes)) {
    if (value === false || value == null) continue;
    element.setAttribute(name, value === true ? '' : String(value));
  }
  element.append(...children.filter((child) => child != null && child !== false));
  return element;
}

function svg(tag, attributes) {
  const element = document.createElementNS(SVG, tag);
  for (const [name, value] of Object.entries(attributes)) element.setAttribute(name, String(value));
  return element;
}

/** The ShareCraft mark: a rounded tile with three joined dots (sharing), drawn as inline SVG. */
export function wordmarkMark() {
  const mark = svg('svg', { class: 'wordmark-mark', viewBox: '0 0 24 24', width: 24, height: 24, 'aria-hidden': 'true', focusable: 'false' });
  mark.append(
    svg('rect', { class: 'wordmark-tile', x: 0, y: 0, width: 24, height: 24, rx: 6 }),
    svg('path', { class: 'wordmark-line', d: 'M8 12 L16 7 M8 12 L16 17' }),
    svg('circle', { class: 'wordmark-dot', cx: 7.5, cy: 12, r: 2.6 }),
    svg('circle', { class: 'wordmark-dot', cx: 16.5, cy: 6.5, r: 2.6 }),
    svg('circle', { class: 'wordmark-dot', cx: 16.5, cy: 17.5, r: 2.6 }),
  );
  return mark;
}

/** Puts the mark in the bar's `.wordmark` link (made if the page has none) and returns the link. */
export function setupWordmark(bar) {
  let link = bar.querySelector('.wordmark');
  if (!link) {
    link = el('a', { class: 'wordmark', href: '/' }, el('span', { class: 'wordmark-word' }, 'ShareCraft'));
    bar.prepend(link);
  }
  link.setAttribute('aria-label', 'ShareCraft, all sites');
  if (location.pathname === '/' || location.pathname === '/index.html') link.setAttribute('aria-current', 'page');
  if (!link.querySelector('svg')) link.prepend(wordmarkMark());
  return link;
}

function appearanceNow() {
  return globalThis.sharecraftAppearance?.get?.() ?? 'system';
}

/**
 * Adds the avatar and its account menu to the end of `bar` for `me` ({id, name, email, is_admin}).
 * `brandManager` is a promise of whether the person manages the brand (administrators always
 * see Brand centre); `signOut()` signs out. Returns the menu's controls.
 */
export function setupAccountMenu(bar, me, { brandManager = Promise.resolve(false), signOut } = {}) {
  const name = me.name || me.email || 'You';
  const colour = avatarColour(me.id ?? me.email ?? name);
  const avatar = el('span', { class: 'avatar', 'aria-hidden': 'true' }, initials(me.name || me.email));
  avatar.style.setProperty('--avatar-light-bg', colour.light.bg);
  avatar.style.setProperty('--avatar-light-ink', colour.light.ink);
  avatar.style.setProperty('--avatar-dark-bg', colour.dark.bg);
  avatar.style.setProperty('--avatar-dark-ink', colour.dark.ink);
  const button = el('button', { type: 'button', id: 'account-button', class: 'avatar-button', 'aria-label': `Account: ${name}` }, avatar);

  const head = el(
    'li',
    { role: 'none', class: 'account-head' },
    el('span', { class: 'account-name' }, name),
    me.email && me.email !== name ? el('span', { class: 'account-email' }, me.email) : null,
  );
  const people = el('li', { role: 'none', hidden: !me.is_admin }, el('a', { role: 'menuitem', href: '/people.html' }, 'Admin centre'));
  const brandItem = el('li', { role: 'none', hidden: !me.is_admin }, el('a', { role: 'menuitem', href: '/brand-centre.html' }, 'Brand centre'));
  const radios = APPEARANCE_CHOICES.map(({ value, label }) => {
    const radio = el('button', { type: 'button', role: 'menuitemradio', 'aria-checked': 'false', 'data-appearance': value }, el('span', { class: 'radio-dot', 'aria-hidden': 'true' }), label);
    radio.addEventListener('click', () => {
      globalThis.sharecraftAppearance?.set?.(value);
      markChecked();
    });
    return el('li', { role: 'none' }, radio);
  });
  const markChecked = () => {
    const now = appearanceNow();
    for (const radio of menu.querySelectorAll('[role=menuitemradio]')) {
      radio.setAttribute('aria-checked', String(radio.dataset.appearance === now));
    }
  };
  const signOutItem = el('button', { type: 'button', role: 'menuitem' }, 'Sign out');
  signOutItem.addEventListener('click', () => signOut?.());
  const separator = () => el('li', { role: 'separator', class: 'menu-separator' });

  const menu = el(
    'ul',
    { id: 'account-menu', class: 'menu end account-menu', 'aria-label': me.email && me.email !== name ? `${name}, ${me.email}` : name, hidden: true },
    head,
    separator(),
    people,
    brandItem,
    el('li', { role: 'none', class: 'menu-label', id: 'appearance-label', 'aria-hidden': 'true' }, 'Appearance'),
    el('li', { role: 'none' }, el('ul', { role: 'group', class: 'menu-group', 'aria-label': 'Appearance' }, ...radios)),
    separator(),
    el('li', { role: 'none' }, signOutItem),
  );
  markChecked();
  const wrap = el('div', { class: 'menu-wrap account-wrap' }, button, menu);
  bar.append(wrap);
  const controls = setupMenu(button, menu);
  // Opening shows the choice as it is now (another tab may have changed it).
  button.addEventListener('click', markChecked, true);
  globalThis.addEventListener?.('storage', markChecked);
  if (!me.is_admin) {
    Promise.resolve(brandManager)
      .then((yes) => {
        brandItem.hidden = !yes;
      })
      .catch(() => {});
  }
  return controls;
}
