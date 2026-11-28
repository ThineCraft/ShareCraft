// The design system in style.css (round 18): the tokens exist, no text is smaller than 12 pixels,
// and component colours come from the colour tokens (with the documented exceptions below).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

const SOURCE = readFileSync(new URL('./style.css', import.meta.url), 'utf8').replace(/\/\*[\s\S]*?\*\//g, '');

/** Every style rule as { selector, at, declarations: [[property, value]] }; `at` lists the
    enclosing at-rule preludes (media, container), outermost first. */
export function parseRules(css) {
  const rules = [];
  const walk = (text, at) => {
    let i = 0;
    while (i < text.length) {
      const open = text.indexOf('{', i);
      if (open < 0) break;
      const prelude = text.slice(i, open).trim();
      let depth = 1;
      let j = open + 1;
      while (j < text.length && depth) {
        if (text[j] === '{') depth++;
        else if (text[j] === '}') depth--;
        j++;
      }
      const body = text.slice(open + 1, j - 1);
      if (prelude.startsWith('@keyframes')) {
        // frames are not components
      } else if (prelude.startsWith('@')) {
        walk(body, [...at, prelude]);
      } else {
        const declarations = body
          .split(';')
          .map((part) => part.trim())
          .filter(Boolean)
          .map((part) => {
            const colon = part.indexOf(':');
            return [part.slice(0, colon).trim(), part.slice(colon + 1).trim()];
          });
        rules.push({ selector: prelude, at, declarations });
      }
      i = j;
    }
  };
  walk(css, []);
  return rules;
}

const RULES = parseRules(SOURCE);
const ROOT = Object.fromEntries(
  RULES.filter((rule) => rule.selector === ':root' && rule.at.length === 0).flatMap((rule) => rule.declarations),
);

test('the design tokens are all on :root', () => {
  const expected = {
    '--text-xs': '12px', '--text-sm': '13px', '--text-md': '14px', '--text-lg': '17px', '--text-xl': '20px', '--text-2xl': '26px',
    '--space-1': '4px', '--space-2': '8px', '--space-3': '12px', '--space-4': '16px', '--space-5': '24px', '--space-6': '32px', '--space-7': '48px',
    '--radius-sm': '6px', '--radius-md': '10px', '--radius-lg': '14px', '--radius-pill': '999px',
    '--control-sm': '32px', '--control': '36px', '--control-touch': '44px',
    '--dur-fast': '120ms', '--dur': '200ms',
  };
  for (const [name, value] of Object.entries(expected)) assert.equal(ROOT[name], value, name);
  for (const name of ['--leading-tight', '--leading-snug', '--leading', '--ease', '--text-reading']) assert.ok(ROOT[name], name);
  // shadows read in dark mode: each one has a light and a dark colour
  for (const name of ['--shadow-1', '--shadow-2', '--shadow-3']) assert.match(ROOT[name] ?? '', /light-dark\(/, name);
  // reduced motion stops every transition and animation that uses the durations
  const reduced = RULES.filter((rule) => rule.selector === ':root' && rule.at.some((at) => /prefers-reduced-motion:\s*reduce/.test(at)));
  const still = Object.fromEntries(reduced.flatMap((rule) => rule.declarations));
  assert.equal(still['--dur'], '0s');
  assert.equal(still['--dur-fast'], '0s');
  // the colour tokens keep their meaning
  assert.equal(ROOT['--accent'], '#3156c9');
  assert.match(ROOT['--stop-ink'], /^light-dark\(/);
});

/** The size in pixels of a font-size value, or null when it is relative (em, %, inherit). */
function pixels(value) {
  const token = /^var\((--text-[\w-]+)\)/.exec(value);
  if (token) return Number.parseFloat(ROOT[token[1]]);
  const px = /^(\d+(?:\.\d+)?)px/.exec(value);
  return px ? Number(px[1]) : null;
}

test('no text is smaller than 12 pixels, and every size comes from the type scale', () => {
  const small = [];
  const loose = [];
  for (const rule of RULES) {
    for (const [property, value] of rule.declarations) {
      if (property !== 'font-size' && property !== 'font') continue;
      if (property === 'font' && /^(inherit|initial|unset)$/.test(value)) continue;
      const size = pixels(value);
      if (size !== null && size < 12) small.push(`${rule.selector} ${property}:${value}`);
      if (!/^var\(--text-/.test(value) && !/^(inherit|1em|100%)/.test(value)) loose.push(`${rule.selector} ${property}:${value}`);
    }
  }
  assert.deepEqual(small, []);
  assert.deepEqual(loose, []);
});

// Colours written as literals on purpose. Each needs a reason.
const COLOUR_EXCEPTIONS = [
  // a site's image band: a fixed dark shade keeps white titles at 4.5:1 on any picture
  /^\.site-header\[data-background=image\]( :focus-visible)?$/,
  // the ShareCraft wordmark is the product's own mark, the same on every site and in both modes
  /^\.wordmark-(tile|line|dot)$/,
  // initials on the site's light accent: white passes on every accent a theme may have
  /^\.site-mark\.initials$/,
  // the viewer's dark stage and the checkerboard behind transparent pictures stay dark/light in both modes
  /^\.pv:is\(\[data-kind=image\],\[data-kind=video\]\) \.pv-(main|edge)(:hover)?$/,
  /^\.pv\[data-kind=video\] \.pv-caption \.meta$/,
  /^\.pv-stage img$/,
  /^\.pv-media video$/,
  // modal backdrops and the image band's shade are plain black glass
  /::backdrop$/,
  /^\.band-shade$/,
];
// Values that are not colours of their own: keywords, system colours, inheriting.
const KEYWORDS = /^(transparent|currentColor|inherit|initial|unset|none|Canvas|CanvasText|Highlight|ButtonText|ButtonFace|GrayText)$/i;

test('component colours come from the colour tokens', () => {
  const literal = [];
  for (const rule of RULES) {
    if (rule.at.some((at) => /forced-colors/.test(at))) continue;
    if (COLOUR_EXCEPTIONS.some((pattern) => pattern.test(rule.selector))) continue;
    for (const [property, value] of rule.declarations) {
      if (!/^(color|background|background-color|border-color|fill|stroke|outline-color|accent-color)$/.test(property)) continue;
      if (KEYWORDS.test(value) || value.includes('var(')) continue;
      literal.push(`${rule.selector} { ${property}:${value} }`);
    }
  }
  assert.deepEqual(literal, []);
});

test('raised surfaces use the elevation tokens', () => {
  const raw = [];
  for (const rule of RULES) {
    if (rule.selector === ':root' || COLOUR_EXCEPTIONS.some((pattern) => pattern.test(rule.selector))) continue;
    for (const [property, value] of rule.declarations) {
      if (property !== 'box-shadow' || /inset|var\(--shadow-|^none$/.test(value)) continue;
      // rings (a spread with no blur) are focus and selection marks, not elevation
      if (/^0 0 0 /.test(value)) continue;
      raw.push(`${rule.selector} { box-shadow:${value} }`);
    }
  }
  assert.deepEqual(raw, []);
});

test('buttons have the three looks, a danger modifier and the sizes', () => {
  const selectors = RULES.map((rule) => rule.selector);
  assert.ok(selectors.includes('button,.button'), 'primary');
  assert.ok(selectors.some((s) => /\.secondary/.test(s) && /\.plain/.test(s)), 'secondary, with .plain as its alias');
  assert.ok(selectors.some((s) => /\.ghost/.test(s)), 'ghost');
  assert.ok(selectors.some((s) => /\.danger\b/.test(s) && /button/.test(s)), 'danger');
  const coarse = RULES.filter((rule) => rule.selector === ':root' && rule.at.some((at) => /pointer:\s*coarse/.test(at)));
  assert.ok(coarse.length, 'touch sizes on coarse pointers');
  assert.ok(selectors.some((s) => s.startsWith('.card,') && /\.site-card/.test(s) && /\.library-card/.test(s) && /\.link-card/.test(s) && /\.bin-list/.test(s) && /\.recent-file/.test(s)), 'one card surface');
});

test('nested folders stay indented in the folder tree and the permission map (round 23 check)', () => {
  const rules = parseRules(SOURCE);
  // The round-12 folder tree indents each row by its --level (tree.js sets it per row).
  const treeRow = rules.find((rule) => rule.selector === '.tree-row' && rule.at.length === 0);
  assert.ok(treeRow, 'a .tree-row rule');
  const padding = treeRow.declarations.find(([property]) => property === 'padding' || property === 'padding-left');
  assert.match(padding?.[1] ?? '', /var\(--level/);
  // Nothing may override that indent with !important (round 22 removed such a rule).
  for (const rule of rules) {
    if (!/\.tree-row/.test(rule.selector)) continue;
    for (const [property, value] of rule.declarations) {
      if (/^padding/.test(property)) assert.doesNotMatch(value, /!important/, `${rule.selector} { ${property}: ${value} }`);
    }
  }
  // The permission map indents nested groups, and its step outranks the plain tree group reset.
  const group = rules.find((rule) => rule.selector === '.pmap-tree .pmap-group');
  assert.match(group?.declarations.find(([property]) => property === 'padding-left')?.[1] ?? '', /var\(--pmap-step\)/);
  const reset = rules.findIndex((rule) => rule.selector.includes('[role=tree] [role=group]'));
  assert.ok(reset >= 0 && reset < rules.indexOf(group), 'the map indent comes after the tree reset');
});

test('mega menu columns are banded like the dropdowns, in light and dark', () => {
  const rules = parseRules(SOURCE);
  const banded = (selector) =>
    rules.some(
      (rule) =>
        rule.at.length === 0 &&
        rule.selector.split(',').map((s) => s.trim()).includes(selector) &&
        rule.declarations.some(([property, value]) => property === 'background' && value === 'var(--band)'),
    );
  assert.ok(banded('.navbar .sub li:nth-child(even)'));
  assert.ok(banded('.navbar .mega-panel li:nth-child(even)'));
  // The band is a mix of the text colour, so it follows the light and dark text colours.
  const band = rules.find((rule) => rule.selector === ':root' && rule.declarations.some(([p]) => p === '--band'));
  assert.match(band.declarations.find(([p]) => p === '--band')[1], /var\(--text\)/);
});

test('a paper page has fixed colours, the same as look.js, and a grain that never prints', async () => {
  const { PAPER } = await import('./look.js');
  const paper = RULES.filter((rule) => rule.selector === '[data-page=paper]' && rule.at.length === 0);
  assert.equal(paper.length, 1, 'one [data-page=paper] rule');
  const set = Object.fromEntries(paper[0].declarations);
  const pair = (key) => `light-dark(${PAPER.light[key]},${PAPER.dark[key]})`;
  for (const [token, key] of [['--bg', 'bg'], ['--paper', 'paper'], ['--sheet', 'paper'], ['--text', 'text'], ['--muted', 'muted'], ['--champagne', 'champagne']]) {
    assert.equal(set[token], pair(key), token);
  }
  // The grain on a page is one fixed layer behind everything that never takes a click.
  const grain = RULES.find((rule) => rule.selector === 'html[data-page=paper] body::before' && rule.at.length === 0);
  assert.ok(grain, 'a grain layer on the page');
  const layer = Object.fromEntries(grain.declarations);
  assert.equal(layer.position, 'fixed');
  assert.equal(layer['pointer-events'], 'none');
  // In a preview panel or a card picture the same grain sits inside the element.
  assert.ok(RULES.some((rule) => rule.selector === '[data-page=paper]:not(html)::before'), 'a grain layer in a panel');
  // Printing leaves the grain out.
  const print = RULES.filter((rule) => rule.at.some((at) => /^@media print/.test(at)) && /data-page=paper/.test(rule.selector));
  assert.ok(print.some((rule) => Object.fromEntries(rule.declarations).display === 'none'), 'no grain in print');
  // No image, filter or address anywhere in the paper rules.
  for (const rule of RULES.filter((r) => /data-page=paper/.test(r.selector))) {
    for (const [property, value] of rule.declarations) assert.doesNotMatch(value, /url\(/, `${rule.selector} { ${property} }`);
  }
});