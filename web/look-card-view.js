// Draws the small pictures of a look (see look-cards.js and look.js): a look card (two small
// pages in its light and dark colours and a font sample) and a font specimen. Colours and font
// variables are set through `style.setProperty` only, with the checked values of look.js, so no
// inline style or <style> is ever written (the page Content Security Policy stays as it is).

import { h } from './common.js';
import { applyFonts, parseColour } from './look.js';

/** A small page in one mode: the secondary colour as its band, the primary as a line and a button. */
function miniPage(scheme, colour) {
  const mini = h(
    'span',
    { class: 'tc-mini', 'data-scheme': scheme, 'aria-hidden': 'true' },
    h('span', { class: 'tc-band' }),
    h('span', { class: 'tc-line' }),
    h('span', { class: 'tc-button' }),
  );
  // Only colours that are #rrggbb reach the page.
  if (parseColour(colour?.primary ?? '')) mini.style.setProperty('--tc-primary', colour.primary);
  if (parseColour(colour?.secondary ?? '')) mini.style.setProperty('--tc-secondary', colour.secondary);
  return mini;
}

/**
 * A specimen of `fonts` (an effective look's `{title, heading, body, interactive}`): the site
 * title, a heading, a line of body text and a button, each in its role's font, loaded through
 * `FontFace` from the brand font address under its generated family (see `applyFonts`). `size`
 * `'small'` shows the title and a line only. Hidden from screen readers: the words around it
 * name the fonts.
 */
export function fontSpecimen(fonts, size = 'full') {
  const box = h(
    'span',
    { class: `font-specimen${size === 'small' ? ' small' : ''}`, 'aria-hidden': 'true' },
    h('span', { class: 'sp-title' }, 'Aa Site title'),
    size === 'small' ? null : h('span', { class: 'sp-heading' }, 'A heading'),
    h('span', { class: 'sp-body' }, 'The quick brown fox jumps over the lazy dog.'),
    size === 'small' ? null : h('span', { class: 'sp-button' }, 'Button'),
  );
  applyFonts(box, fonts);
  return box;
}

/**
 * A look card for `card` (`templateLookCard` or `effectiveLookCard`): the two small pages, a
 * font sample, the theme's name and the fonts' name; the whole card in words for screen readers.
 */
export function lookCardElement(card) {
  if (!card) return h('span', { class: 'look-card none' }, h('span', { class: 'meta' }, 'No look of its own'));
  return h(
    'span',
    { class: 'look-card' },
    h('span', { class: 'tc-pages' }, miniPage('light', card.light), miniPage('dark', card.dark)),
    fontSpecimen(card.fonts, 'small'),
    h('span', { class: 'look-card-text' }, card.text),
  );
}
