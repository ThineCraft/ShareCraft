import test from 'node:test';
import assert from 'node:assert/strict';
import {
  ANYONE_NOTICE,
  LINK_NOT_WORKING,
  REQUEST_SENT,
  ageText,
  anyoneLinksText,
  expiryFromDays,
  expiryText,
  fileOffer,
  linkLabel,
  linkRevokeHint,
  linkRoleChoices,
  liveLinks,
  requestDecisionText,
  requestAnswer,
  requestProblem,
  withChosen,
  breadcrumbParts,
  breaksBelow,
  exceptionTree,
  breadcrumbs,
  canMoveHere,
  explorerQuery,
  explorerRows,
  filterChoices,
  groupLabel,
  itemActions,
  permsText,
  previewSections,
  principalParts,
  principalText,
  roleLine,
  rolesText,
  toggleOpen,
  visibleTree,
  inheritAgainMessage,
  messageFor,
  moveChoices,
  noSourceText,
  rolesWithin,
  pageFor,
  removedMessage,
  removedReasonOf,
  safeNext,
} from './common.js';

const origin = 'http://127.0.0.1:8787';

/** Access as the server answers it for each built-in role. */
const VIEWER = Object.freeze({ perms: ['view'], roles: [{ id: 'viewer', name: 'Viewer' }] });
const EDITOR = Object.freeze({
  perms: ['view', 'add', 'edit', 'delete'],
  roles: [{ id: 'editor', name: 'Editor' }],
});
const OWNER = Object.freeze({
  perms: ['view', 'add', 'edit', 'delete', 'manage_access', 'manage_site'],
  roles: [{ id: 'owner', name: 'Owner' }],
});

test('a path on this site is kept', () => {
  assert.equal(safeNext('/doc.html#abc', origin), '/doc.html#abc');
  assert.equal(safeNext('/people.html?x=1#y', origin), '/people.html?x=1#y');
});

test('anything else goes to the home page', () => {
  for (const bad of [
    '//evil.example',
    '/\\evil.example',
    '/\t/evil.example',
    '/\n/evil.example',
    '/\r/evil.example',
    '/ x',
    'https://evil.example/',
    'javascript:alert(1)',
    'doc.html',
    '',
    undefined,
    null,
  ]) {
    assert.equal(safeNext(bad, origin), '/', JSON.stringify(bad));
  }
});

const SITE = '11111111-1111-4111-8111-111111111111';
const LIB = '22222222-2222-4222-8222-222222222222';
const DIR = '33333333-3333-4333-8333-333333333333';
const FILE = '44444444-4444-4444-8444-444444444444';

test('pages open by kind', () => {
  assert.equal(pageFor('site', SITE), `/site.html?id=${SITE}`);
  assert.equal(pageFor('library', LIB), `/library.html?id=${LIB}`);
  assert.equal(pageFor('folder', DIR), `/library.html?id=${DIR}`);
  assert.equal(pageFor('document', FILE, 'text'), `/doc.html#${FILE}`);
  assert.equal(pageFor('document', FILE, 'gridcraft'), `/file.html?doc=${FILE}`);
});

test('breadcrumbs link only the steps with access', () => {
  const path = [
    { id: SITE, kind: 'site', name: 'Site', access: null },
    { id: LIB, kind: 'library', name: 'Documents', access: null },
    { id: DIR, kind: 'folder', name: 'Plans', access: VIEWER },
    { id: FILE, kind: 'document', name: 'Plan.txt', access: VIEWER },
  ];
  const parts = breadcrumbParts(path);
  assert.deepEqual(
    parts.map((p) => p.href),
    [null, null, `/library.html?id=${DIR}`, `/file.html?doc=${FILE}`],
  );
  assert.deepEqual(
    breadcrumbParts(path, { linkLast: false }).map((p) => p.href),
    [null, null, `/library.html?id=${DIR}`, null],
  );
});

// A document with just enough in it for `h`: elements remember tag, attributes and children.
function fakeDocument() {
  globalThis.document = {
    createElement(tag) {
      const element = {
        tag,
        attributes: {},
        children: [],
        setAttribute(name, value) {
          this.attributes[name] = value;
        },
        append(...nodes) {
          this.children.push(...nodes);
        },
      };
      return element;
    },
  };
}

test('rendered breadcrumbs hold links only where there is access', () => {
  fakeDocument();
  try {
    const list = breadcrumbs([
      { id: SITE, kind: 'site', name: 'Site', access: null },
      { id: LIB, kind: 'library', name: 'Documents', access: OWNER },
      { id: DIR, kind: 'folder', name: 'Plans', access: OWNER },
    ], { linkLast: false });
    const tags = list.children.map((li) => li.children[0].tag);
    assert.deepEqual(tags, ['span', 'a', 'span']);
    assert.equal(list.children[1].children[0].attributes.href, `/library.html?id=${LIB}`);
    assert.equal(list.children[2].attributes['aria-current'], 'page');
  } finally {
    delete globalThis.document;
  }
});

test('every error code of the tree and access routes has its own sentence', () => {
  const general = messageFor('something_nobody_made_up');
  for (const code of [
    'name_taken',
    'parent_in_bin',
    'forbidden_below',
    'into_itself',
    'wrong_place',
    'is_site',
    'last_owner',
    'last_manager',
    'beyond_your_access',
    'invalid_name',
    'invalid_request',
    'no_such_user',
    'not_found',
  ]) {
    assert.notEqual(messageFor(code), general, code);
  }
  assert.match(messageFor('name_taken'), /already has that name/);
  assert.match(messageFor('parent_in_bin'), /recycle bin/);
  assert.match(messageFor('into_itself'), /itself/);
});

test('a document that was deleted has its own removal message', () => {
  assert.equal(removedMessage('deleted'), 'This document was deleted.');
  assert.notEqual(removedMessage('access_revoked'), removedMessage('deleted'));
});

test('a removed frame is read as a removal even when the client cannot read it', () => {
  assert.equal(removedReasonOf('{"type":"removed","reason":"deleted"}'), 'deleted');
  assert.equal(removedReasonOf('{"type":"removed","reason":"something_new"}'), 'something_new');
  assert.equal(removedReasonOf('{"type":"removed"}'), 'unknown');
  assert.equal(removedReasonOf('{"type":"removed","reason":7}'), 'unknown');
  assert.equal(removedReasonOf('{"type":"people","participants":[]}'), null);
  assert.equal(removedReasonOf('not json'), null);
  assert.equal(removedReasonOf('null'), null);
  // An unknown reason still gets a sentence, never the missing-document one.
  assert.equal(removedMessage('something_new'), 'Your access to this document ended.');
});

test('a refused "Inherit again" names who may do it', () => {
  assert.equal(
    inheritAgainMessage('forbidden'),
    'Only an owner of the place above can make this inherit again.',
  );
  assert.equal(inheritAgainMessage('unavailable'), messageFor('unavailable'));
});

test('the move dialog never offers the item itself', () => {
  const items = [
    { id: DIR, kind: 'folder', name: 'Plans', access: EDITOR },
    { id: LIB, kind: 'folder', name: 'Other', access: OWNER },
    { id: FILE, kind: 'document', name: 'Plan.txt', access: EDITOR },
    { id: SITE, kind: 'library', name: 'Limited', access: null },
  ];
  const offered = moveChoices(items, DIR);
  assert.deepEqual(offered.map((o) => o.id), [LIB, SITE]);
  assert.equal(offered[0].canChoose, true);
  assert.equal(offered[1].canChoose, false, 'limited access is a way through, not a place');
});

test('the move dialog never lets the item go into itself or below itself', () => {
  const site = { id: SITE, kind: 'site', name: 'S', access: OWNER };
  const lib = { id: LIB, kind: 'library', name: 'L', access: OWNER };
  const moving = { id: DIR, kind: 'folder', name: 'D', access: EDITOR };
  const below = { id: FILE, kind: 'folder', name: 'Below', access: EDITOR };
  assert.equal(canMoveHere([site, lib], DIR, FILE), true);
  assert.equal(canMoveHere([site, lib, moving], DIR, FILE), false, 'into itself');
  assert.equal(canMoveHere([site, lib, moving, below], DIR, FILE), false, 'below itself');
  assert.equal(canMoveHere([site, lib], DIR, LIB), false, 'where it already is');
  assert.equal(canMoveHere([site], DIR, LIB), false, 'a site is not a place for a folder');
  assert.equal(canMoveHere([site, { ...lib, access: VIEWER }], DIR, FILE), false);
  const editOnly = { perms: ['view', 'edit'], roles: [{ id: 'x', name: 'Edit only' }] };
  assert.equal(canMoveHere([site, { ...lib, access: editOnly }], DIR, FILE), false, 'needs add');
  assert.equal(canMoveHere([], DIR, FILE), false);
});

test('the new error codes have their own sentences', () => {
  const general = messageFor('something_nobody_made_up');
  for (const code of ['role_in_use', 'builtin_role', 'group_in_use', 'last_manager', 'site_group']) {
    assert.notEqual(messageFor(code), general, code);
  }
  assert.equal(messageFor('beyond_your_access'), 'You can only give access you have yourself.');
});

test('permissions and roles read as words', () => {
  assert.equal(permsText([]), 'none');
  assert.equal(permsText(['view']), 'view');
  assert.equal(permsText(['edit', 'view', 'manage_access']), 'view, edit, manage access');
  assert.equal(rolesText(OWNER), 'Owner');
  assert.equal(
    rolesText({ perms: ['view'], roles: [{ id: 'a', name: 'Auditor' }, { id: 'viewer', name: 'Viewer' }] }),
    'Auditor, Viewer',
  );
  assert.equal(roleLine(EDITOR), 'Your role: Editor');
  assert.match(roleLine(null), /^Limited access/);
});

test('people and groups are named without guessing', () => {
  const person = { kind: 'user', id: 'u', name: 'Ann', email: 'ann@example.com' };
  assert.deepEqual(principalParts(person), { name: 'Ann', note: 'ann@example.com', hidden: false });
  const siteGroup = { kind: 'group', id: 'g', name: 'Site Owners', site: { id: SITE, name: 'Sales' } };
  assert.equal(principalText(siteGroup), 'Site Owners (site: Sales)');
  const orgGroup = { kind: 'group', id: 'g2', name: 'Site Owners', site: null };
  assert.equal(principalText(orgGroup), 'Site Owners (organisation group)');
  assert.notEqual(principalText(siteGroup), principalText(orgGroup));
  const hidden = { kind: 'group', id: 'g3', name: null, hidden: true };
  assert.equal(principalText(hidden), 'A group you cannot see');
  assert.equal(principalParts(hidden).hidden, true);
  assert.equal(groupLabel({ name: 'Staff', site: null }), 'Staff (organisation group)');
});

test('each item offers only the actions its access allows', () => {
  const none = { rename: false, move: false, remove: false, access: false };
  assert.deepEqual(itemActions({ kind: 'folder', access: VIEWER }), none);
  assert.deepEqual(itemActions({ kind: 'folder', access: null }), none);
  assert.deepEqual(itemActions({ kind: 'folder', access: EDITOR }), {
    rename: true, move: true, remove: true, access: false,
  });
  assert.equal(itemActions({ kind: 'folder', access: OWNER }).access, true);
  const renameOnly = { perms: ['view', 'edit'], roles: [{ id: 'x', name: 'X' }] };
  assert.deepEqual(itemActions({ kind: 'document', access: renameOnly }), {
    rename: true, move: true, remove: false, access: false,
  });
  // A library is renamed with manage_site and never moved or deleted here.
  assert.deepEqual(itemActions({ kind: 'library', access: EDITOR }), none);
  assert.deepEqual(itemActions({ kind: 'library', access: OWNER }), {
    rename: true, move: false, remove: false, access: true,
  });
});

const child = (id, kind, inherits, access = OWNER) => ({ id, kind, name: id, inherits, access });

test('explorer rows show only what the viewer manages, with break counts', () => {
  const items = [
    child('a', 'folder', true),
    child('b', 'folder', false),
    child('c', 'folder', true, EDITOR),
    child('d', 'folder', true, null),
    child('e', 'document', true),
  ];
  const counts = new Map([['a', 2]]);
  const rows = explorerRows(items, counts, { onlyExceptions: false });
  assert.deepEqual(rows.map((r) => r.id), ['a', 'b', 'e']);
  assert.deepEqual(rows.map((r) => r.unique), [false, true, false]);
  assert.deepEqual(rows.map((r) => r.count), [2, 0, 0]);
  assert.deepEqual(rows.map((r) => r.expandable), [true, true, false]);
  const only = explorerRows(items, counts, { onlyExceptions: true });
  assert.deepEqual(only.map((r) => r.id), ['a', 'b'], 'the paths to breaks, and the breaks');
});

test('the visible tree follows what is open and loaded', () => {
  const root = { id: 'r', kind: 'site', name: 'r', unique: true, count: 3, expandable: true };
  const a = { id: 'a', kind: 'library', name: 'a', unique: false, count: 1, expandable: true };
  const b = { id: 'b', kind: 'folder', name: 'b', unique: true, count: 0, expandable: true };
  const loaded = new Map([['r', [a]], ['a', [b]]]);
  assert.deepEqual(visibleTree(root, loaded, new Set()).map((n) => n.row.id), ['r']);
  let open = toggleOpen(new Set(), 'r');
  assert.deepEqual(visibleTree(root, loaded, open).map((n) => [n.row.id, n.depth]), [['r', 0], ['a', 1]]);
  open = toggleOpen(open, 'a');
  assert.deepEqual(visibleTree(root, loaded, open).map((n) => n.row.id), ['r', 'a', 'b']);
  assert.equal(visibleTree(root, loaded, open)[1].open, true);
  // An open folder whose children have not loaded yet shows no children.
  assert.deepEqual(visibleTree(root, new Map(), open).map((n) => n.row.id), ['r']);
  assert.equal(toggleOpen(open, 'a').has('a'), false);
  assert.equal(open.has('a'), true, 'toggling leaves the old set alone');
});

test('explorer filters become a query', () => {
  assert.equal(explorerQuery({}), '');
  assert.equal(explorerQuery({ principal: 'user:u1' }), '?principal=user%3Au1');
  assert.equal(
    explorerQuery({ principal: 'group:g1', kinds: ['folder', 'document'] }),
    '?principal=group%3Ag1&kind=folder%2Cdocument',
  );
  assert.equal(explorerQuery({ kinds: [], limit: 50, after: 'ab' }), '?limit=50&after=ab');
});

test('the restore preview is worded as changes from entries the viewer can see', () => {
  const preview = {
    node: { id: DIR, kind: 'folder', name: 'Plans', access: OWNER },
    inherit_from: { id: LIB, kind: 'library', name: 'Documents', access: OWNER },
    people: [
      { user: { id: 'u1', name: 'Ann' }, before: ['view', 'add', 'edit'], after: ['view'] },
      { user: { id: 'u2', name: 'Bo' }, before: ['view'], after: [] },
      { user: { id: 'u3', name: 'Cy' }, before: [], after: ['view', 'edit'] },
    ],
    groups: [
      { group: { kind: 'group', id: 'g', name: null, hidden: true }, before: ['view'], after: [] },
      { group: { kind: 'group', id: 'h', name: 'Staff', site: null }, before: [], after: ['view'] },
    ],
    unchanged_breaks_below: 2,
  };
  const s = previewSections(preview);
  assert.equal(s.source, 'Documents');
  assert.match(s.peopleHeading, /who lose or gain access/);
  assert.match(s.peopleHeading, /entries you can see/);
  assert.deepEqual(s.people, [
    'Ann: view, add, edit → view',
    'Bo: view → none',
    'Cy: none → view, edit',
  ]);
  assert.deepEqual(s.groups, [
    'Members of a group you cannot see: view → none',
    'Members of Staff (organisation group): none → view',
  ]);
  assert.match(s.below, /2 places below keep their own access/);
  assert.doesNotMatch(s.peopleHeading, /all access/i);
  const quiet = previewSections({ ...preview, people: [], groups: [], unchanged_breaks_below: 0 });
  assert.match(quiet.peopleEmpty, /No one/);
  assert.match(quiet.below, /No places below/);
  assert.match(previewSections({ ...preview, unchanged_breaks_below: 1 }).below, /1 place below keeps/);
});

test('the exceptions tree is built from the breaks alone', () => {
  const crumb = (id, kind) => ({ id, kind, name: id, access: OWNER });
  const site = crumb('s', 'site');
  const lib = crumb('l', 'library');
  const brk = (node, path) => ({ node, path, assignments: [] });
  const tree = exceptionTree([
    brk(site, []),
    brk(crumb('f1', 'folder'), [site, lib]),
    brk(crumb('f2', 'folder'), [site, lib]),
    brk(crumb('d', 'document'), [site, lib, crumb('f2', 'folder')]),
  ]);
  assert.equal(tree.root.id, 's');
  assert.equal(tree.root.unique, true);
  assert.equal(tree.root.count, 4);
  assert.equal(breaksBelow(tree.root), 3);
  assert.deepEqual(tree.loaded.get('s').map((r) => r.id), ['l']);
  const lower = tree.loaded.get('l');
  assert.deepEqual(lower.map((r) => r.id), ['f1', 'f2']);
  assert.equal(lower[0].unique, true);
  assert.equal(tree.loaded.get('s')[0].unique, false, 'a path step is not a break');
  assert.equal(tree.loaded.get('s')[0].count, 3);
  assert.equal(tree.loaded.get('f2')[0].expandable, false);
  assert.equal(tree.breaks.size, 4);
  assert.equal(exceptionTree([]).root, null);
});

test('only roles within the own access of the person are offered', () => {
  const role = (id, perms) => ({ id, name: id, perms, builtin: false });
  const all = [
    role('viewer', ['view']),
    role('editor', ['view', 'add', 'edit', 'delete']),
    role('owner', ['view', 'add', 'edit', 'delete', 'manage_access', 'manage_site']),
    role('auditor', ['view', 'manage_access']),
  ];
  assert.deepEqual(rolesWithin(all, OWNER).map((r) => r.id), ['viewer', 'editor', 'owner', 'auditor']);
  const manager = { perms: ['view', 'manage_access'], roles: [{ id: 'm', name: 'M' }] };
  assert.deepEqual(rolesWithin(all, manager).map((r) => r.id), ['viewer', 'auditor']);
  assert.deepEqual(rolesWithin(all, null), []);
});

test('a missing source is a site only when it is one', () => {
  assert.match(noSourceText('site'), /site always has its own/);
  assert.doesNotMatch(noSourceText('folder'), /site/);
  assert.match(noSourceText('folder'), /do not manage access on/);
});

test('the explorer filter offers everyone named on a break loaded, once each, by name', () => {
  const ann = { kind: 'user', id: 'u1', name: 'Ann' };
  const zed = { kind: 'user', id: 'u2', name: 'Zed' };
  const bea = { kind: 'user', id: 'u3', name: 'Bea' };
  const staff = { kind: 'group', id: 'g1', name: 'Staff', site: null };
  const hidden = { kind: 'group', id: 'g2', name: null, hidden: true };
  const choices = filterChoices({
    assignments: [{ principal: zed }],
    breaks: [
      { assignments: [{ principal: bea }, { principal: hidden }] },
      { assignments: [{ principal: ann }, { principal: zed }, { principal: staff }] },
    ],
    groups: [{ id: 'g3', name: 'Alpha Owners', site: { id: 's', name: 'Alpha' } }],
    members: [{ id: 'u1', name: 'Ann' }],
  });
  assert.deepEqual(choices.people, [
    ['u1', 'Ann'],
    ['u3', 'Bea'],
    ['u2', 'Zed'],
  ]);
  assert.deepEqual(choices.groups, [
    ['g3', 'Alpha Owners (site: Alpha)'],
    ['g1', 'Staff (organisation group)'],
  ]);
  assert.deepEqual(filterChoices({}), { people: [], groups: [] });
});

const NOON = Date.parse('2026-11-13T12:00:00Z');
const EDITOR_ROLE = { id: 'editor', name: 'Editor' };

test('an expiry is shown as a day, with the year only when it is not this year', () => {
  const opts = { now: NOON, timeZone: 'UTC' };
  assert.equal(expiryText('2026-12-01T10:00:00Z', opts), 'expires 1 December');
  assert.equal(expiryText('2027-01-05T10:00:00Z', opts), 'expires 5 January 2027');
  assert.equal(expiryText(null, opts), 'never expires');
});

test('a link is labelled with its kind, role and expiry', () => {
  const opts = { now: NOON, timeZone: 'UTC' };
  const signedIn = { kind: 'link', id: 'l1', name: null, link_kind: 'signed_in', expires_at: '2026-12-01T10:00:00Z' };
  assert.equal(linkLabel(signedIn, EDITOR_ROLE, opts), 'Signed-in link (Editor, expires 1 December)');
  assert.equal(
    linkLabel({ ...signedIn, expires_at: null }, EDITOR_ROLE, opts),
    'Signed-in link (Editor, never expires)',
  );
  assert.equal(
    linkLabel({ kind: 'anyone', expires_at: '2026-12-01T10:00:00Z' }, { name: 'Viewer' }, opts),
    'Anyone link (Viewer, expires 1 December)',
  );
  assert.equal(linkLabel(signedIn, null, opts), 'Signed-in link (expires 1 December)');
  const parts = principalParts({ ...signedIn, expires_at: null }, EDITOR_ROLE);
  assert.equal(parts.name, 'Signed-in link (Editor, never expires)');
  assert.equal(parts.note, '');
  assert.equal(parts.hidden, false);
});

test('the explorer filter never offers a link as a person or group', () => {
  const link = { kind: 'link', id: 'l1', name: null, link_kind: 'signed_in', expires_at: null };
  const choices = filterChoices({ assignments: [{ principal: link }] });
  assert.deepEqual(choices, { people: [], groups: [] });
});

test('the person filter keeps the chosen person or group even when it is not listed', () => {
  const choices = { people: [['u1', 'Ann']], groups: [] };
  assert.deepEqual(withChosen(choices, 'user:u1', 'Ann'), choices);
  assert.deepEqual(withChosen(choices, '', ''), choices);
  assert.deepEqual(withChosen(choices, 'user:u9', 'Zed'), {
    people: [['u1', 'Ann'], ['u9', 'Zed']],
    groups: [],
  });
  assert.deepEqual(withChosen(choices, 'group:g9', 'Staff'), {
    people: [['u1', 'Ann']],
    groups: [['g9', 'Staff']],
  });
  assert.equal(choices.people.length, 1, 'the choices given are not changed');
});

test('a break says how many anyone links are live on it', () => {
  assert.equal(anyoneLinksText(2), 'Anyone links: 2 live');
  assert.equal(anyoneLinksText(1), 'Anyone links: 1 live');
  assert.equal(anyoneLinksText(0), '');
  assert.equal(anyoneLinksText(undefined), '');
});

test('links may be made only with roles within your access and without managing', () => {
  const roles = [
    { id: 'viewer', name: 'Viewer', perms: ['view'] },
    { id: 'editor', name: 'Editor', perms: ['view', 'add', 'edit', 'delete'] },
    { id: 'owner', name: 'Owner', perms: ['view', 'add', 'edit', 'delete', 'manage_access', 'manage_site'] },
    { id: 'mgr', name: 'Access manager', perms: ['view', 'manage_access'] },
  ];
  assert.deepEqual(linkRoleChoices(roles, OWNER).map((r) => r.id), ['viewer', 'editor']);
  assert.deepEqual(
    linkRoleChoices(roles, { ...VIEWER, perms: ['view', 'manage_access'] }).map((r) => r.id),
    ['viewer'],
  );
  assert.deepEqual(linkRoleChoices(roles, null), []);
});

test('only live links are listed', () => {
  const links = [
    { id: 'a', revoked: null, expires_at: null },
    { id: 'b', revoked: null, expires_at: '2026-12-01T10:00:00Z' },
    { id: 'c', revoked: null, expires_at: '2026-11-01T10:00:00Z' },
    { id: 'd', revoked: { reason: 'manager' }, expires_at: null },
  ];
  assert.deepEqual(liveLinks(links, NOON).map((l) => l.id), ['a', 'b']);
});

test('an expiry in days is checked against the limit', () => {
  const day = 86400000;
  assert.deepEqual(expiryFromDays('7', { maxDays: 30, required: true, now: NOON }), {
    at: new Date(NOON + 7 * day).toISOString(),
    error: null,
  });
  assert.equal(expiryFromDays('30', { maxDays: 30, required: true, now: NOON }).error, null);
  assert.equal(expiryFromDays('31', { maxDays: 30, required: true, now: NOON }).error, 'expiry_too_long');
  assert.equal(expiryFromDays('', { maxDays: 30, required: true, now: NOON }).error, 'expiry_required');
  assert.deepEqual(expiryFromDays(' ', { maxDays: 30, required: false, now: NOON }), { at: null, error: null });
  for (const bad of ['0', '-1', '1.5', 'abc', '1e3']) {
    assert.equal(expiryFromDays(bad, { maxDays: 30, required: false, now: NOON }).error, 'expiry_invalid', bad);
  }
});

test('the words of access requests', () => {
  assert.equal(REQUEST_SENT, 'If this item exists, the people who manage it will see your request.');
  assert.equal(requestProblem('x'.repeat(500)), null);
  assert.equal(requestProblem(''), null);
  assert.equal(requestProblem('x'.repeat(501)), 'message_too_long');
  assert.equal(requestProblem('\u{1F600}'.repeat(500)), null, 'counted in characters, not code units');
  assert.equal(requestDecisionText({ decision: 'approved', role: EDITOR_ROLE }), 'Approved as Editor');
  assert.equal(requestDecisionText({ decision: 'approved', role: null }), 'Approved');
  assert.equal(requestDecisionText({ decision: 'declined' }), 'Declined');
  assert.equal(ageText('2026-11-13T11:59:40Z', NOON), 'just now');
  assert.equal(ageText('2026-11-13T11:55:00Z', NOON), '5 minutes ago');
  assert.equal(ageText('2026-11-13T11:00:00Z', NOON), '1 hour ago');
  assert.equal(ageText('2026-11-10T12:00:00Z', NOON), '3 days ago');
  assert.equal(ageText('2026-11-13T13:00:00Z', NOON), 'just now', 'a clock a little ahead');
});

test('every new error code has its own sentence', () => {
  for (const code of [
    'link_not_found',
    'link_group',
    'link_role_too_high',
    'already_has',
    'anyone_links_off',
    'expiry_required',
    'expiry_too_long',
    'expiry_in_past',
    'expiry_invalid',
    'too_many_requests',
    'already_decided',
    'message_too_long',
  ]) {
    assert.notEqual(messageFor(code), messageFor('no_such_code'), code);
  }
  assert.equal(LINK_NOT_WORKING, 'This link does not work. It may have expired or been turned off.');
  assert.equal(ANYONE_NOTICE, 'This address is shown only now. If you lose it, make a new link.');
});

test('a request is answered the same whatever the server said, except for the two limits', () => {
  assert.equal(requestAnswer(null), REQUEST_SENT);
  for (const code of ['not_found', 'forbidden', 'network', 'unavailable', 'unknown', 'link_not_found']) {
    assert.equal(requestAnswer(code), REQUEST_SENT, code);
  }
  assert.equal(requestAnswer('too_many_requests'), messageFor('too_many_requests'));
  assert.equal(requestAnswer('message_too_long'), messageFor('message_too_long'));
  assert.notEqual(messageFor('too_many_requests'), REQUEST_SENT);
});

test('a link is revoked on its own item, linked from a break below holding a copy', () => {
  const item = { id: '11111111-1111-1111-1111-111111111111', name: 'Plans & notes' };
  const link = { kind: 'link', id: 'l', link_kind: 'signed_in', expires_at: null, item };
  assert.deepEqual(linkRevokeHint(link, item.id), { text: 'Revoke it under Share by link', on: null });
  assert.deepEqual(linkRevokeHint(link, '22222222-2222-2222-2222-222222222222'), {
    text: 'Revoke it under Share by link',
    on: { name: 'Plans & notes', href: '/access.html?id=11111111-1111-1111-1111-111111111111' },
  });
  // An item the person may not see: plain text, no name and no link.
  assert.deepEqual(linkRevokeHint({ ...link, item: { id: item.id, name: null } }, 'x'), {
    text: 'Revoke it under Share by link on the item it was made on.',
    on: null,
  });
  // Without its item (an older answer), the hint stays plain.
  assert.deepEqual(linkRevokeHint({ kind: 'link', id: 'l' }, 'x'), { text: 'Revoke it under Share by link', on: null });
});

test('a file never saved offers no download', () => {
  assert.deepEqual(fileOffer(undefined), { download: false, saved: 'Not saved yet' });
  assert.deepEqual(fileOffer(null), { download: false, saved: 'Not saved yet' });
  const saved = fileOffer('2026-11-13T10:00:00Z');
  assert.equal(saved.download, true);
  assert.match(saved.saved, /^Last saved /);
});

// ---- files: bin wording, bulk results, quota wording, selection ----

import {
  bulkSummary,
  bytesText,
  canPlaceHere,
  daysLeftText,
  deleteRoleText,
  emptyBinText,
  quotaRefusalText,
  quotaText,
  removeMemberText,
  selectionState,
  usagePercent,
} from './common.js';

test('days left in the recycle bin are worded plainly', () => {
  assert.equal(daysLeftText(93), '93 days left');
  assert.equal(daysLeftText(2), '2 days left');
  assert.equal(daysLeftText(1), '1 day left');
  assert.equal(daysLeftText(0), 'Erased within a day');
  assert.equal(daysLeftText(-4), 'Erased within a day');
  assert.equal(daysLeftText(null), '');
});

test('sizes are worded in the largest sensible unit', () => {
  assert.equal(bytesText(0), '0 bytes');
  assert.equal(bytesText(1023), '1023 bytes');
  assert.equal(bytesText(1536), '1.5 KB');
  assert.equal(bytesText(10 * 1024 ** 3), '10.0 GB');
});

test('a bulk summary names every item with its own outcome', () => {
  const names = { a: 'Budget.xlsx', b: 'Plans', c: 'Notes.docx', d: 'Old.pdf' };
  const results = [
    { id: 'a', ok: true },
    { id: 'b', ok: true, new_id: 'b2', renamed: { from: 'Plans', to: 'Plans (2)' } },
    { id: 'c', ok: false, error: 'forbidden' },
    { id: 'd', ok: false, error: 'quota_exceeded' },
  ];
  const summary = bulkSummary('copy', results, names);
  assert.equal(summary.ok, 2);
  assert.equal(summary.failed, 2);
  assert.equal(summary.headline, '2 of 4 items were copied. 2 could not be.');
  assert.equal(summary.lines.length, 4);
  assert.deepEqual(
    summary.lines.map((line) => [line.name, line.ok]),
    [['Budget.xlsx', true], ['Plans', true], ['Notes.docx', false], ['Old.pdf', false]],
  );
  assert.match(summary.lines[1].text, /Plans \(2\)/); // the rename is shown
  assert.equal(summary.lines[2].text, messageFor('forbidden'));
  assert.equal(summary.lines[3].text, messageFor('quota_exceeded'));
});

test('a bulk summary with everything done is one sentence per operation', () => {
  const names = { a: 'A.docx' };
  assert.equal(bulkSummary('delete', [{ id: 'a', ok: true }], names).headline, '1 item was moved to the recycle bin.');
  assert.equal(bulkSummary('move', [{ id: 'a', ok: true }], names).headline, '1 item was moved.');
  assert.equal(
    bulkSummary('restore', [{ id: 'a', ok: true, renamed: { from: 'A.docx', to: 'A (2).docx' } }], names).headline,
    '1 item was restored.',
  );
  assert.equal(
    bulkSummary('delete', [{ id: 'a', ok: false, error: 'not_found' }], names).headline,
    'No items were moved to the recycle bin. 1 could not be.',
  );
});

test('an item the summary has no name for is called an item', () => {
  const summary = bulkSummary('move', [{ id: 'zz', ok: false, error: 'not_found' }], {});
  assert.equal(summary.lines[0].name, 'An item');
});

test('select all reports none, some or all of the listed items', () => {
  assert.deepEqual(selectionState(new Set(), ['a', 'b']), { count: 0, all: false, some: false });
  assert.deepEqual(selectionState(new Set(['a']), ['a', 'b']), { count: 1, all: false, some: true });
  assert.deepEqual(selectionState(new Set(['a', 'b']), ['a', 'b']), { count: 2, all: true, some: false });
  // an id no longer listed is not counted
  assert.deepEqual(selectionState(new Set(['gone']), ['a']), { count: 0, all: false, some: false });
  assert.deepEqual(selectionState(new Set(), []), { count: 0, all: false, some: false });
});

test('copy is offered where move is not: the folder the items are in', () => {
  const here = { id: 'f', kind: 'folder', access: { perms: ['view', 'add'] } };
  assert.equal(canPlaceHere([here], ['x'], 'f', { copy: false }), false);
  assert.equal(canPlaceHere([here], ['x'], 'f', { copy: true }), true);
  // never into itself or below it, for either
  assert.equal(canPlaceHere([here], ['f'], 'p', { copy: true }), false);
  assert.equal(canPlaceHere([here], ['x', 'f'], 'p', { copy: false }), false);
  // needs add
  const viewOnly = { id: 'g', kind: 'folder', access: { perms: ['view'] } };
  assert.equal(canPlaceHere([viewOnly], ['x'], 'f', { copy: true }), false);
  assert.equal(canPlaceHere([], ['x'], 'f', { copy: true }), false);
});

test('the move dialog leaves out every item being moved', () => {
  const items = [
    { id: 'a', kind: 'folder', name: 'A', access: { perms: ['view', 'add'] } },
    { id: 'b', kind: 'folder', name: 'B', access: { perms: ['view', 'add'] } },
    { id: 'c', kind: 'folder', name: 'C', access: { perms: ['view', 'add'] } },
  ];
  assert.deepEqual(moveChoices(items, ['a', 'b']).map((choice) => choice.id), ['c']);
  assert.deepEqual(moveChoices(items, 'a').map((choice) => choice.id), ['b', 'c']);
});

test('quota wording: usage, warning from 90%, and refusals in plain words', () => {
  assert.equal(usagePercent(50, 100), 50);
  assert.equal(usagePercent(150, 100), 100);
  assert.equal(usagePercent(5, null), null);
  assert.equal(usagePercent(5, 0), 100);
  assert.equal(quotaText({ used: 2048, quota: null }), 'This site holds 2.0 KB. It has no storage quota.');
  assert.match(quotaText({ used: 512, quota: 1024 }), /512 bytes of its 1\.0 KB storage quota/);
  assert.doesNotMatch(quotaText({ used: 512, quota: 1024 }), /nearly full/);
  assert.match(quotaText({ used: 950, quota: 1000, quota_warning: true }), /nearly full/);
  // at or over the quota it is full, not "nearly" full
  for (const used of [1000, 1200]) {
    const text = quotaText({ used, quota: 1000, quota_warning: true });
    assert.match(text, /is full/);
    assert.doesNotMatch(text, /nearly/);
  }
  const refusal = quotaRefusalText({ used: 900, quota: 1000 }, { name: 'Big.pdf', size: 500 });
  assert.match(refusal, /Big\.pdf/);
  assert.match(refusal, /500 bytes/);
  assert.match(refusal, /900 bytes of its 1000 bytes/);
  // someone who cannot see the numbers still gets a sentence
  assert.match(quotaRefusalText(null, { name: 'Big.pdf', size: 5 }), /Big\.pdf/);
  assert.match(quotaRefusalText(null, { name: 'Big.pdf', size: 5 }), /no room/);
});

test('emptying a bin says what cannot be undone', () => {
  assert.match(emptyBinText('second', 3), /3 items/);
  assert.match(emptyBinText('second', 3), /cannot be undone/);
  assert.match(emptyBinText('second', 1), /1 item\b/);
  assert.match(emptyBinText('first', 2), /second stage/);
  assert.match(emptyBinText('first', 2), /people who manage the site/);
});

test('confirmations for removing a member and deleting a role', () => {
  assert.match(removeMemberText({ name: 'Ada' }, { name: 'Editors' }), /Remove Ada from Editors/);
  assert.match(deleteRoleText({ name: 'Reviewer' }), /Delete the role Reviewer/);
  assert.match(deleteRoleText({ name: 'Reviewer' }), /cannot be undone/);
});

test('new error codes have sentences', () => {
  for (const code of [
    'being_edited',
    'text_document',
    'checksum_mismatch',
    'too_many_uploads',
    'invalid_range',
    'wrong_offset',
  ]) {
    assert.notEqual(messageFor(code), messageFor('no-such-code'), code);
  }
});

import { parseQuotaGb } from './common.js';

test('the quota field: empty is no quota, a number is gigabytes, anything else is refused', () => {
  assert.equal(parseQuotaGb(''), null);
  assert.equal(parseQuotaGb('  '), null);
  assert.equal(parseQuotaGb('10'), 10 * 1024 ** 3);
  assert.equal(parseQuotaGb('0.5'), 512 * 1024 ** 2);
  assert.equal(parseQuotaGb('0'), 0);
  assert.equal(parseQuotaGb('-1'), undefined);
  assert.equal(parseQuotaGb('lots'), undefined);
  assert.equal(parseQuotaGb('1e3'), undefined);
});

import { quotaSubmit } from './common.js';

test('an empty quota field removes the quota only where the current quota is shown', () => {
  assert.deepEqual(quotaSubmit('', { blind: false }), { bytes: null });
  assert.deepEqual(quotaSubmit('2', { blind: false }), { bytes: 2 * 1024 ** 3 });
  assert.match(quotaSubmit('', { blind: true }).error, /number of gigabytes/);
  assert.match(quotaSubmit('  ', { blind: true }).error, /number of gigabytes/);
  assert.deepEqual(quotaSubmit('2', { blind: true }), { bytes: 2 * 1024 ** 3 });
  assert.match(quotaSubmit('lots', { blind: false }).error, /number of gigabytes/);
});

import { nearestLivingAncestor } from './common.js';

test('a deleted document links to the nearest folder above it that still exists', async () => {
  const mk = (id) => ({ id, kind: 'folder', name: id, access: { perms: ['view'], roles: [] } });
  const path = [mk('site'), mk('lib'), mk('a'), mk('b'), { id: 'doc', kind: 'document', name: 'doc', access: {} }];
  const alive = (...ids) => async (id) => ids.includes(id);
  assert.equal((await nearestLivingAncestor(path, alive('site', 'lib', 'a', 'b'))).id, 'b');
  assert.equal((await nearestLivingAncestor(path, alive('site', 'lib'))).id, 'lib');
  assert.equal(await nearestLivingAncestor(path, alive()), null);
  assert.equal(await nearestLivingAncestor(path, async () => null), null);
  assert.equal(await nearestLivingAncestor([path.at(-1)], alive('doc')), null);
});
