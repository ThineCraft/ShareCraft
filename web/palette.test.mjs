import test from 'node:test';
import assert from 'node:assert/strict';
import {
  contextIdFrom,
  countText,
  fuzzyScore,
  highlightParts,
  isTextField,
  moveActive,
  paletteGroups,
  paletteShortcut,
  rankResults,
} from './palette.js';

const SITE = '11111111-1111-4111-8111-111111111111';
const OTHER = '22222222-2222-4222-8222-222222222222';

test('fuzzyScore matches letters in order, ignoring case, spaces and accents', () => {
  assert.deepEqual(fuzzyScore('bt', 'Budget.xlsx').positions, [0, 5]);
  assert.deepEqual(fuzzyScore('new site', 'New site').positions, [0, 1, 2, 4, 5, 6, 7]);
  assert.deepEqual(fuzzyScore('cafe', 'Café plans').positions, [0, 1, 2, 3]);
  assert.equal(fuzzyScore('tb', 'Budget.xlsx'), null);
  assert.equal(fuzzyScore('budgets!', 'Budget'), null);
  assert.deepEqual(fuzzyScore('', 'Anything'), { score: 0, positions: [] });
});

test('fuzzyScore prefers word starts and letters in a row', () => {
  // "t" at the start of "Team" beats the "t" inside "Best".
  assert.deepEqual(fuzzyScore('bt', 'Best Team').positions, [0, 5]);
  assert.ok(fuzzyScore('ns', 'New site').score > fuzzyScore('ns', 'Lens').score);
  assert.ok(fuzzyScore('bud', 'Budget').score > fuzzyScore('bud', 'Big update').score);
  // A capital after a small letter starts a word too.
  assert.deepEqual(fuzzyScore('sc', 'ShareCraft').positions, [0, 5]);
  // The same match in a shorter text wins.
  assert.ok(fuzzyScore('plan', 'Plan').score > fuzzyScore('plan', 'Plan for next year').score);
});

test('highlightParts groups matched letters into runs', () => {
  assert.deepEqual(highlightParts('Budget', [0, 1, 5]), [
    { text: 'Bu', hit: true },
    { text: 'dge', hit: false },
    { text: 't', hit: true },
  ]);
  assert.deepEqual(highlightParts('Plain', []), [{ text: 'Plain', hit: false }]);
});

test('rankResults sorts each group best first, caps it and drops empty groups', () => {
  const groups = [
    { id: 'files', heading: 'Files', limit: 3, items: [{ label: 'Notes.txt' }, { label: 'Budget.xlsx' }, { label: 'Big table.xlsx' }, { label: 'Best team.docx' }] },
    { id: 'none', heading: 'Nothing', items: [{ label: 'Zebra' }] },
    { id: 'actions', heading: 'Actions', items: [{ label: 'Recycle bin', keywords: 'deleted restore trash' }] },
  ];
  const ranked = rankResults(groups, 'bt');
  assert.deepEqual(ranked.map((group) => group.id), ['files']);
  // Word starts first ("Big table", "Best team"), then "Budget"; "Notes" does not match; at most 3.
  assert.deepEqual(ranked[0].items.map((item) => item.label), ['Big table.xlsx', 'Best team.docx', 'Budget.xlsx']);
  assert.deepEqual(ranked[0].items[0].positions, [0, 4]);
  assert.deepEqual(ranked[0].items[2].positions, [0, 5]);
  // "trash" is not in "Recycle bin", but in its keywords: a match that highlights nothing.
  const byWords = rankResults(groups, 'trash');
  assert.deepEqual(byWords.map((group) => group.id), ['actions']);
  assert.deepEqual(byWords[0].items[0].positions, []);
});

test('rankResults without a query keeps every group in order, capped', () => {
  const groups = [{ id: 'a', heading: 'A', limit: 2, items: [{ label: 'One' }, { label: 'Two' }, { label: 'Three' }] }];
  const ranked = rankResults(groups, '  ');
  assert.deepEqual(ranked[0].items.map((item) => item.label), ['One', 'Two']);
  assert.equal('order' in ranked[0].items[0], false);
});

test('rankResults lifts boosted items among equals', () => {
  const groups = [{ id: 'l', heading: 'Libraries', items: [{ label: 'Documents' }, { label: 'Documents', boost: 1, detail: 'here' }] }];
  assert.equal(rankResults(groups, 'doc')[0].items[0].detail, 'here');
});

const sites = [
  { id: SITE, name: 'Marketing', kind: 'team', access: { perms: ['view', 'edit', 'manage_site'], roles: [] } },
  { id: OTHER, name: 'Projects', kind: 'communication', access: { perms: ['view'], roles: [] } },
];
const keysOf = (groups) => groups.find((group) => group.id === 'actions').items.map((item) => item.key);

test('paletteGroups offers actions by rights and place', () => {
  // Someone who manages the current site, not an administrator.
  assert.deepEqual(keysOf(paletteGroups({ me: { is_admin: false }, sites, siteId: SITE })), ['look', 'site-settings', 'bin', 'start']);
  // On a site they only view: none of its management.
  assert.deepEqual(keysOf(paletteGroups({ me: { is_admin: false }, sites, siteId: OTHER })), ['start']);
  // A brand manager gets the brand centre.
  assert.deepEqual(keysOf(paletteGroups({ me: {}, sites, brandManager: true })), ['brand', 'start']);
  // An administrator on a site they do not manage: New site, Site settings, Brand centre, People.
  assert.deepEqual(keysOf(paletteGroups({ me: { is_admin: true }, sites, siteId: OTHER })), ['new-site', 'site-settings', 'brand', 'people', 'start']);
  const look = paletteGroups({ me: {}, sites, siteId: SITE })[3].items[0];
  assert.equal(look.href, `/site-look.html?id=${SITE}`);
  assert.equal(look.detail, 'Marketing');
});

test('paletteGroups puts page actions first and lets them replace a general one', () => {
  const run = () => {};
  const groups = paletteGroups({
    me: { is_admin: true },
    pageActions: [
      { key: 'upload-files', label: 'Upload files', run },
      { key: 'new-site', label: 'New site', run },
    ],
  });
  const actions = groups.find((group) => group.id === 'actions').items;
  assert.deepEqual(actions.map((item) => item.key), ['upload-files', 'new-site', 'brand', 'people', 'start']);
  assert.equal(actions[1].run, run);
  assert.equal(actions[1].href, undefined);
});

test('paletteGroups lists recent files, sites and libraries with links and icons', () => {
  const groups = paletteGroups({
    sites,
    siteId: OTHER,
    recent: [{ id: SITE, name: 'Budget.xlsx', editor: 'gridcraft', site: 'Marketing', at: 1 }],
    libraries: [
      { id: 'a', name: 'Documents', siteId: SITE, siteName: 'Marketing' },
      { id: 'b', name: 'Documents', siteId: OTHER, siteName: 'Projects' },
    ],
  });
  const [recent, siteGroup, libraries] = groups;
  assert.equal(recent.items[0].icon, 'sheet');
  assert.equal(recent.items[0].href, `/file.html?doc=${SITE}`);
  assert.equal(siteGroup.items[1].detail, 'Communication site · You are here');
  assert.deepEqual(libraries.items.map((item) => item.detail), ['Projects', 'Marketing'], 'the current site first');
  assert.equal(libraries.items[0].href, '/library.html?id=b');
});

const key = (k, extra = {}) => ({ key: k, target: { tagName: 'BODY' }, ...extra });

test('paletteShortcut: Ctrl+K and Cmd+K open it, even in a field, but not in rich text', () => {
  assert.equal(paletteShortcut(key('k', { ctrlKey: true })), 'open');
  assert.equal(paletteShortcut(key('K', { metaKey: true })), 'open');
  assert.equal(paletteShortcut(key('k', { ctrlKey: true, target: { tagName: 'INPUT', type: 'text' } })), 'open');
  assert.equal(paletteShortcut(key('k', { ctrlKey: true, target: { tagName: 'DIV', isContentEditable: true } })), null);
  assert.equal(paletteShortcut(key('k', { ctrlKey: true, altKey: true })), null);
  assert.equal(paletteShortcut(key('k')), null);
  assert.equal(paletteShortcut(key('k', { ctrlKey: true, defaultPrevented: true })), null);
});

test('paletteShortcut: "/" opens it only outside text fields and rich text', () => {
  assert.equal(paletteShortcut(key('/')), 'open');
  assert.equal(paletteShortcut(key('/', { shiftKey: true })), 'open', 'keyboards that need Shift for /');
  assert.equal(paletteShortcut(key('/', { target: { tagName: 'INPUT', type: 'checkbox' } })), 'open');
  assert.equal(paletteShortcut(key('/', { target: { tagName: 'BUTTON' } })), 'open');
  for (const target of [
    { tagName: 'INPUT' },
    { tagName: 'INPUT', type: 'search' },
    { tagName: 'TEXTAREA' },
    { tagName: 'SELECT' },
    { tagName: 'DIV', isContentEditable: true },
  ]) {
    assert.equal(paletteShortcut(key('/', { target })), null, JSON.stringify(target));
  }
  assert.equal(paletteShortcut(key('/', { ctrlKey: true })), null);
  assert.equal(paletteShortcut(key('/', { isComposing: true })), null);
  assert.equal(isTextField(null), false);
});

test('moveActive moves and wraps', () => {
  assert.equal(moveActive('ArrowDown', -1, 3), 0);
  assert.equal(moveActive('ArrowDown', 2, 3), 0);
  assert.equal(moveActive('ArrowUp', 0, 3), 2);
  assert.equal(moveActive('ArrowUp', -1, 3), 2);
  assert.equal(moveActive('End', 0, 3), null, 'plain End moves in the field');
  assert.equal(moveActive('End', 0, 3, { ctrlKey: true }), 2);
  assert.equal(moveActive('PageUp', 2, 3), 0);
  assert.equal(moveActive('ArrowDown', -1, 0), null);
  assert.equal(moveActive('a', 0, 3), null);
});

test('contextIdFrom finds the place a page shows', () => {
  assert.equal(contextIdFrom({ pathname: '/site.html', search: `?id=${SITE.toUpperCase()}` }), SITE);
  assert.equal(contextIdFrom({ pathname: '/bin.html', search: `?site=${SITE}` }), SITE);
  assert.equal(contextIdFrom({ pathname: '/file.html', search: `?doc=${SITE}` }), SITE);
  assert.equal(contextIdFrom({ pathname: '/explorer.html', search: `?root=${SITE}` }), SITE);
  assert.equal(contextIdFrom({ pathname: '/doc.html', hash: `#${SITE}` }), SITE);
  assert.equal(contextIdFrom({ pathname: '/site.html', search: '?id=nonsense' }), '');
  assert.equal(contextIdFrom({ pathname: '/index.html', hash: `#${SITE}` }), '');
});

test('countText', () => {
  assert.equal(countText(0), 'No results.');
  assert.equal(countText(1), '1 result.');
  assert.equal(countText(4, true), '4 results so far. Still loading sites and libraries.');
});
