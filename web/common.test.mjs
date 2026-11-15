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
  newMenu,
  rowMenu,
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
    'invalid_language',
    'invalid_time_zone',
    'invalid_navigation',
    'invalid_template',
    'archived',
    'site_deleted',
    'not_deleted',
    'not_a_hub',
    'is_hub',
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

test('a document whose site was archived has its own removal message', () => {
  assert.match(removedMessage('archived'), /archived/);
  assert.match(messageFor('archived'), /archived/);
  assert.notEqual(removedMessage('archived'), removedMessage('deleted'));
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

test('the Actions menu of a row lists what the person may do, in a fixed order', () => {
  assert.deepEqual(rowMenu({ kind: 'document', access: VIEWER }), ['copy']);
  assert.deepEqual(rowMenu({ kind: 'folder', access: null }), ['copy']);
  assert.deepEqual(rowMenu({ kind: 'folder', access: EDITOR }), ['rename', 'move', 'copy', 'delete']);
  assert.deepEqual(rowMenu({ kind: 'document', access: OWNER }), ['rename', 'move', 'copy', 'delete', 'access']);
  // A library is never copied, moved or deleted from here.
  assert.deepEqual(rowMenu({ kind: 'library', access: EDITOR }), []);
  assert.deepEqual(rowMenu({ kind: 'library', access: OWNER }), ['rename', 'access']);
});

test('the New menu offers folders, the allowed document kinds, templates and uploads', () => {
  const keys = (entries) => entries.map((entry) => entry.key);
  assert.deepEqual(keys(newMenu({ editors: ['text', 'gridcraft', 'wordcraft', 'solidcraft'] })), [
    'folder', 'doc:text', 'doc:gridcraft', 'doc:wordcraft', 'doc:solidcraft', 'files', 'folder-upload',
  ]);
  // A library that takes no folders and only images: uploads only.
  assert.deepEqual(keys(newMenu({ folders: false, editors: [] })), ['files', 'folder-upload']);
  assert.deepEqual(keys(newMenu({ editors: ['text', 'pdfcraft'], templates: 2 })), [
    'folder', 'doc:text', 'template', 'files', 'folder-upload',
  ]);
  const labels = newMenu({ editors: ['gridcraft'], templates: 1 }).map((entry) => entry.label);
  assert.deepEqual(labels, ['Folder', 'Spreadsheet (.xlsx)', 'From a template…', 'Upload files', 'Upload a folder']);
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
  binEntryText,
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

test('a bin entry says what it is and where it was', () => {
  assert.equal(binEntryText({ kind: 'file', path: [] }), 'File at the top of its library');
  assert.equal(binEntryText({ kind: 'file', path: ['Plans', '2026'] }), 'File in Plans / 2026');
  assert.equal(binEntryText({ kind: 'folder', path: ['Plans'] }), 'Folder in Plans');
  assert.equal(binEntryText({ kind: 'library', path: [] }), 'Library, with everything in it');
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

// ---- sites: regional formatting, navigation bars and the directory ----

import { directoryFilter, formatDate, formatSize, navigationView, safeHref, siteChrome } from './common.js';

/** Spaces as one plain space: ICU puts narrow no-break spaces before AM and PM. */
const plain = (text) => text.replace(/[\s  ]+/g, ' ');

test('dates_shown_in_site_time_zone_across_dst', () => {
  const london = { language: 'en-GB', timeZone: 'Europe/London' };
  assert.equal(plain(formatDate('2026-11-14T15:04:00Z', london)), '14 Nov 2026, 15:04 GMT');
  // Europe/London moves to summer time at 01:00 UTC on 29 March 2026.
  assert.equal(plain(formatDate('2026-03-29T00:30:00Z', london)), '29 Mar 2026, 00:30 GMT');
  assert.equal(plain(formatDate('2026-03-29T01:30:00Z', london)), '29 Mar 2026, 02:30 BST');
  // America/New_York moves to summer time at 07:00 UTC on 8 March 2026.
  const newYork = { language: 'en-US', timeZone: 'America/New_York' };
  assert.equal(plain(formatDate('2026-03-08T06:30:00Z', newYork)), 'Mar 8, 2026, 1:30 AM EST');
  assert.equal(plain(formatDate('2026-03-08T07:30:00Z', newYork)), 'Mar 8, 2026, 3:30 AM EDT');
  // The same moment in another zone and language.
  assert.equal(plain(formatDate('2026-07-01T12:00:00Z', { language: 'de', timeZone: 'Europe/Berlin' })), '1. Juli 2026, 14:00 MESZ');
  // Not a time: shown as it came; a bad zone or language falls back instead of throwing.
  assert.equal(formatDate('not a time', london), 'not a time');
  assert.equal(formatDate(null, london), '');
  assert.doesNotThrow(() => formatDate('2026-11-14T15:04:00Z', { language: 'xx-!!', timeZone: 'Mars/Olympus' }));
});

test('sizes_in_site_language', () => {
  assert.equal(formatSize(1536, 'en'), '1.5 KB');
  assert.equal(formatSize(1536, 'de'), '1,5 KB');
  assert.equal(formatSize(1536, 'fr'), '1,5 KB');
  assert.equal(formatSize(10 * 1024 ** 3, 'de'), '10,0 GB');
  assert.equal(formatSize(1023, 'en'), '1023 bytes');
  assert.equal(formatSize(0, 'de'), '0 bytes');
  // Without a language, sizes read as they always did.
  assert.equal(formatSize(1536), '1.5 KB');
  assert.equal(formatSize(1536, 'not a language!'), '1.5 KB');
});

// A document able to hold a drawn navigation: elements keep tag, attributes, children (strings
// stay strings, so text can never become markup) and listeners.
function chromeDocument() {
  const all = [];
  globalThis.document = {
    documentElement: { lang: 'en' },
    createElement(tag) {
      const element = {
        tag,
        attributes: {},
        children: [],
        listeners: {},
        hidden: false,
        setAttribute(name, value) {
          this.attributes[name] = value;
        },
        getAttribute(name) {
          return this.attributes[name] ?? null;
        },
        append(...nodes) {
          this.children.push(...nodes);
        },
        replaceChildren(...nodes) {
          this.children = nodes;
        },
        addEventListener(type, listener) {
          this.listeners[type] = listener;
        },
        set innerHTML(_) {
          throw new Error('innerHTML is never used');
        },
      };
      all.push(element);
      return element;
    },
    getElementById: () => null,
    querySelector: () => null,
    addEventListener() {},
  };
  return all;
}

/** Every string drawn below `element`, in order. */
const texts = (element) =>
  element.children.flatMap((child) => (typeof child === 'string' ? [child] : texts(child)));

test('navigation_drawn_as_text_only', () => {
  const all = chromeDocument();
  try {
    const hostile = '<img src=x onerror="alert(1)">';
    const nav = {
      global: [{ id: 'g1', label: hostile, target: { url: 'https://example.org/' }, targeted: false, children: [] }],
      hub: { id: SITE, name: `${hostile} hub`, items: [] },
      local: [
        {
          id: 'l1',
          label: hostile,
          target: null,
          targeted: true,
          children: [{ id: 'l2', label: 'Plans', target: { node: LIB, kind: 'library' }, targeted: false, children: [] }],
        },
      ],
      automatic: false,
    };
    const view = navigationView({ id: SITE, name: `${hostile} site` }, nav, { currentId: LIB });
    assert.ok(!all.some((element) => element.tag === 'img'), 'no element is made from a label');
    const drawn = texts(view);
    assert.ok(drawn.includes(hostile), 'the label is drawn as text');
    assert.ok(drawn.includes(`${hostile} hub`), 'the hub name is drawn as text');
    assert.ok(drawn.includes(`${hostile} site`));
    const links = all.filter((element) => element.tag === 'a');
    assert.ok(links.some((a) => a.attributes.href === 'https://example.org/'));
    assert.ok(links.some((a) => a.attributes.href === `/site.html?id=${SITE}`), 'the hub name links to the hub');
    const plans = links.find((a) => a.attributes.href === `/library.html?id=${LIB}`);
    assert.equal(plans.attributes['aria-current'], 'page');
    // The menu is a disclosure button at narrow widths, and so is each heading with children.
    const buttons = all.filter((element) => element.tag === 'button');
    assert.ok(buttons.length >= 2);
    for (const button of buttons) assert.equal(button.attributes['aria-expanded'], 'false');
  } finally {
    delete globalThis.document;
  }
});

test('a hub bar the viewer cannot name is drawn without the hub link', () => {
  const all = chromeDocument();
  try {
    const nav = {
      global: [],
      hub: { id: null, name: null, items: [{ id: 'h1', label: 'News', target: { url: '/news' }, targeted: false, children: [] }] },
      local: [],
      automatic: true,
    };
    navigationView(null, nav, {});
    const links = all.filter((element) => element.tag === 'a');
    assert.deepEqual(links.map((a) => a.attributes.href), ['/news']);
  } finally {
    delete globalThis.document;
  }
});

test('bad_url_never_becomes_a_link', async () => {
  for (const bad of [
    'javascript:alert(1)',
    'JavaScript:alert(1)',
    ' javascript:alert(1)',
    'data:text/html,<script>alert(1)</script>',
    '//evil.example/',
    '/\\evil.example',
    '/\t/evil.example',
    'vbscript:x',
    'relative/path',
    '',
    'https://example.org/' + 'a'.repeat(2000),
    null,
    42,
  ]) {
    assert.equal(safeHref(bad), null, String(bad));
  }
  for (const good of ['https://example.org/', 'HTTP://example.org', 'mailto:team@example.org', '/library.html?id=1']) {
    assert.equal(safeHref(good), good);
  }

  // A stubbed server sends a `javascript:` address anyway: the page draws the label as text.
  const all = chromeDocument();
  const realFetch = globalThis.fetch;
  globalThis.fetch = async (url) => {
    const body = url.endsWith('/navigation')
      ? {
          global: [{ id: 'g', label: 'Click me', target: { url: 'javascript:alert(1)' }, targeted: false, children: [] }],
          hub: null,
          local: [{ id: 'l', label: 'Sneaky', target: { url: '//evil.example/' }, targeted: false, children: [] }],
          automatic: false,
        }
      : { id: SITE, name: 'Sales', language: 'en-GB', time_zone: 'Europe/London', kind: 'team', description: '' };
    return { ok: true, status: 200, json: async () => body };
  };
  try {
    const place = document.createElement('div');
    const shown = await siteChrome(SITE, { place });
    assert.equal(shown.site.name, 'Sales');
    for (const a of all.filter((element) => element.tag === 'a')) {
      assert.notEqual(safeHref(a.attributes.href), null, `linked ${a.attributes.href}`);
    }
    const drawn = texts(place);
    assert.ok(drawn.includes('Click me'));
    assert.ok(drawn.includes('Sneaky'));
    // The page takes the site's language, and dates follow it.
    assert.equal(document.documentElement.lang, 'en-GB');
    assert.equal(plain(formatDate('2026-11-14T15:04:00Z')), '14 Nov 2026, 15:04 GMT');
  } finally {
    globalThis.fetch = realFetch;
    delete globalThis.document;
  }
});

test('directory_filter_matches_names_and_descriptions', () => {
  const sites = [
    { id: 'a', name: 'Sales', description: 'Quotes and contracts' },
    { id: 'b', name: 'Marketing', description: 'Campaigns for SALES teams' },
    { id: 'c', name: 'Legal', description: '' },
    { id: 'd', name: 'Ops' },
  ];
  const ids = (text) => directoryFilter(sites, text).map((site) => site.id);
  assert.deepEqual(ids('sales'), ['a', 'b']);
  assert.deepEqual(ids('  CONTRACT '), ['a']);
  assert.deepEqual(ids('leg'), ['c']);
  assert.deepEqual(ids(''), ['a', 'b', 'c', 'd']);
  assert.deepEqual(ids(undefined), ['a', 'b', 'c', 'd']);
  assert.deepEqual(ids('nothing like it'), []);
});

test('new_messages_for_new_error_codes', () => {
  const general = messageFor('no_such_code');
  const codes = [
    'archived',
    'invalid_navigation',
    'invalid_template',
    'invalid_language',
    'invalid_time_zone',
    'not_a_hub',
    'is_hub',
    'site_deleted',
    'not_deleted',
  ];
  const sentences = codes.map(messageFor);
  for (const [at, code] of codes.entries()) assert.notEqual(sentences[at], general, code);
  assert.equal(new Set(sentences).size, codes.length, 'each code has a sentence of its own');
});

import {
  adminSiteActions,
  adminSiteRow,
  deleteSiteText,
  languageChoices,
  purgeSiteText,
  templateReasonText,
  timeZoneChoices,
} from './common.js';

/** Rows as `GET /api/admin/sites` answers them. */
const ADMIN_ROWS = {
  active: { id: SITE, name: 'Sales', state: 'active', kind: 'team', is_hub: true, hub: null, used: 1536, quota: null, owners: 2, deleted: null, purge_at: null },
  archived: { id: LIB, name: 'Old projects', state: 'archived', kind: 'communication', is_hub: false, hub: { id: SITE, name: 'Sales' }, used: 0, quota: 1024 ** 3, owners: 0, deleted: null, purge_at: null },
  deleted: {
    id: DIR,
    name: '<b>Gone</b>',
    state: 'deleted',
    kind: 'team',
    is_hub: false,
    hub: null,
    used: 10,
    quota: null,
    owners: 1,
    deleted: { at: '2026-11-01T10:00:00Z', by: 'Root' },
    purge_at: '2027-02-02T10:00:00Z',
  },
};

test('admin_sites_rows_and_actions_by_state', () => {
  const actions = (row) => adminSiteActions(row).map((a) => a.action);
  assert.deepEqual(actions(ADMIN_ROWS.active), ['archive', 'delete']);
  assert.deepEqual(actions(ADMIN_ROWS.archived), ['reactivate', 'delete']);
  assert.deepEqual(actions(ADMIN_ROWS.deleted), ['restore', 'purge']);
  assert.deepEqual(adminSiteActions(ADMIN_ROWS.deleted).map((a) => a.label), ['Restore', 'Delete permanently']);

  const all = chromeDocument();
  try {
    const clicked = [];
    const rows = Object.values(ADMIN_ROWS).map((row) => adminSiteRow(row, (action, site) => clicked.push([action, site.id])));
    const [active, archived, deleted] = rows.map(texts);
    // State, storage, owners and hub, as text.
    assert.ok(active.includes('Sales'));
    assert.ok(active.some((t) => t.includes('Active')));
    assert.ok(active.some((t) => t.includes('Hub')));
    assert.ok(active.some((t) => t.includes('1.5 KB')));
    assert.ok(active.some((t) => t.includes('2 owners')));
    assert.ok(!active.some((t) => t.includes('No enabled owner')));
    assert.ok(archived.some((t) => t.includes('Archived')));
    assert.ok(archived.some((t) => t.includes('Sales')), 'the hub it joined');
    assert.ok(archived.some((t) => t.includes('No enabled owner')), 'a site with no enabled owner is flagged');
    assert.ok(deleted.includes('<b>Gone</b>'), 'the name is drawn as text');
    assert.ok(deleted.some((t) => t.includes('Deleted')));
    // A live site's name links to it; a deleted one does not.
    const links = all.filter((element) => element.tag === 'a').map((a) => a.attributes.href);
    assert.ok(links.includes(`/site.html?id=${SITE}`));
    assert.ok(!links.includes(`/site.html?id=${DIR}`));
    // Each action button is named for its site and calls back with the action.
    const buttons = all.filter((element) => element.tag === 'button');
    const purge = buttons.find((b) => b.attributes['aria-label'] === 'Delete <b>Gone</b> permanently');
    assert.ok(purge);
    purge.listeners.click();
    assert.deepEqual(clicked, [['purge', DIR]]);
    assert.ok(buttons.some((b) => b.attributes['aria-label'] === 'Archive Sales'));
    assert.ok(buttons.some((b) => b.attributes['aria-label'] === 'Reactivate Old projects'));
  } finally {
    delete globalThis.document;
  }
});

test('purge_confirmation_names_the_site', () => {
  const text = purgeSiteText(ADMIN_ROWS.deleted);
  assert.ok(text.includes('<b>Gone</b>'), 'names the site');
  assert.match(text, /cannot be undone/);
  assert.match(text, /permanently/);
  const deleting = deleteSiteText(ADMIN_ROWS.active);
  assert.ok(deleting.includes('Sales'));
  assert.ok(deleting.includes('Everyone loses access at once. It can be restored for 93 days.'));
});

test('regional choices and template refusals', () => {
  const languages = languageChoices('en');
  assert.ok(languages.length >= 35 && languages.length <= 50);
  for (const { value } of languages) assert.match(value, /^[a-zA-Z]{2,3}(-[a-zA-Z]{4})?(-([a-zA-Z]{2}|[0-9]{3}))?$/);
  // A site's language that is not in the list is kept as a choice.
  assert.equal(languageChoices('gsw-CH')[0].value, 'gsw-CH');
  const zones = timeZoneChoices('Europe/London');
  assert.equal(zones[0], 'UTC');
  assert.ok(zones.includes('Europe/London'));
  assert.equal(new Set(zones).size, zones.length);
  assert.ok(timeZoneChoices('Mars/Base').includes('Mars/Base'));
  for (const reason of ['too_large', 'too_deep', 'too_many_folders', 'too_many_libraries', 'unknown_field', 'identifiers', 'invalid_navigation', 'invalid_names']) {
    assert.notEqual(templateReasonText(reason), templateReasonText('no_such_reason'), reason);
  }
});

// ---- plan 3C: libraries, previews, scanning and quarantine ----

import * as libs from './common.js';

const DOC = '00000000-0000-4000-8000-0000000000d1';

test('file_page_chooses_editor_preview_or_download', () => {
  const view = (doc) => libs.fileView({ id: DOC, revision: 'r1', ...doc });
  // An editor's file opens in its editor, as before.
  assert.deepEqual(view({ name: 'Budget.xlsx', editor: 'gridcraft' }), { view: 'editor' });
  assert.deepEqual(view({ name: 'Scan.pdf', editor: 'pdfcraft' }), { view: 'editor' });
  // Any other file: a preview by its extension (the server's list), or a download.
  assert.deepEqual(view({ name: 'Photo.JPG', editor: 'other' }), { view: 'preview', kind: 'image' });
  assert.deepEqual(view({ name: 'logo.svg', editor: 'other' }), { view: 'preview', kind: 'image' });
  assert.deepEqual(view({ name: 'talk.mp3', editor: 'other' }), { view: 'preview', kind: 'audio' });
  assert.deepEqual(view({ name: 'clip.mov', editor: 'other' }), { view: 'preview', kind: 'video' });
  assert.deepEqual(view({ name: 'notes.md', editor: 'other' }), { view: 'preview', kind: 'text' });
  // A web page is only ever shown as text.
  assert.deepEqual(view({ name: 'page.html', editor: 'other' }), { view: 'preview', kind: 'text' });
  assert.deepEqual(view({ name: 'setup.exe', editor: 'other' }), { view: 'download' });
  assert.deepEqual(view({ name: 'archive.tar.gz', editor: 'other' }), { view: 'download' });
  assert.deepEqual(view({ name: '.png', editor: 'other' }), { view: 'download' });
  assert.deepEqual(view({ name: 'noextension', editor: 'other' }), { view: 'download' });
  // A file with no version has nothing to show.
  assert.deepEqual(view({ name: 'Photo.png', editor: 'other', revision: null }), { view: 'download' });
  // Held and quarantined come first, whatever the file is.
  assert.deepEqual(view({ name: 'Budget.xlsx', editor: 'gridcraft', scan: 'held' }), { view: 'held' });
  assert.deepEqual(view({ name: 'Photo.png', editor: 'other', scan: 'held' }), { view: 'held' });
  assert.deepEqual(view({ name: 'Photo.png', editor: 'other', scan: 'quarantined', threat: 'Eicar-Test' }), {
    view: 'quarantined',
    threat: 'Eicar-Test',
  });
  assert.equal(libs.previewKindOf('a.webm'), 'video');
  assert.equal(libs.previewKindOf('a.xlsx'), null);
});

test('held_file_polls_then_stops', async () => {
  // Checks every 10 seconds; stops as soon as the check has an answer.
  const waits = [];
  const wait = async (ms) => {
    waits.push(ms);
  };
  let checks = 0;
  const ready = await libs.pollHeld(async () => (++checks === 4 ? { scan: null } : null), { wait });
  assert.deepEqual(ready, { scan: null });
  assert.equal(checks, 4);
  assert.deepEqual(waits, [10_000, 10_000, 10_000, 10_000]);
  // At most 30 times, then it gives up.
  waits.length = 0;
  checks = 0;
  const never = await libs.pollHeld(async () => {
    checks += 1;
    return null;
  }, { wait });
  assert.equal(never, null);
  assert.equal(checks, 30);
  assert.equal(waits.length, 30);
  // A page that is left stops it at once.
  checks = 0;
  const stopped = { stopped: false };
  const early = await libs.pollHeld(async () => {
    checks += 1;
    if (checks === 2) stopped.stopped = true;
    return null;
  }, { wait, stop: () => stopped.stopped });
  assert.equal(early, null);
  assert.equal(checks, 2);
});

test('quarantine_text_names_the_threat_as_text', () => {
  const threat = '<img src=x onerror=alert(1)>';
  const text = libs.quarantineText(threat);
  assert.ok(text.includes(threat));
  assert.match(text, /An administrator can release or delete it/);
  assert.match(libs.quarantineText(null), /An administrator can release or delete it/);
  assert.equal(libs.HELD_TEXT, 'Being checked for malware');
  const all = chromeDocument();
  try {
    const notice = libs.quarantineNotice(threat);
    assert.ok(texts(notice).includes(threat), 'the threat is a string child, never markup');
    assert.ok(!all.some((element) => element.tag === 'img'));
    // A row of the administrators' quarantine list: threat, size, each document's path, as text.
    const entry = {
      sha256: 'ab'.repeat(32),
      scan: 'infected',
      threat,
      size: 1536,
      scanned_at: '2026-11-14T10:00:00Z',
      documents: [
        { id: DOC, name: '<b>x</b>.exe', path: 'Sales / Documents / <b>x</b>.exe', deleted: false },
        { id: FILE, name: 'y.exe', path: 'Sales / Documents / y.exe', deleted: true },
      ],
    };
    const actions = [];
    const row = libs.quarantineRow(entry, (action, which) => actions.push([action, which.sha256]));
    const words = texts(row);
    assert.ok(words.includes(threat));
    assert.ok(words.some((t) => t.includes('1.5 KB')));
    assert.ok(words.includes('Sales / Documents / <b>x</b>.exe'));
    assert.ok(words.some((t) => t.includes('recycle bin')));
    const buttons = all.filter((element) => element.tag === 'button');
    buttons.find((b) => b.children.includes('Release')).listeners.click();
    buttons.find((b) => b.children.includes('Delete')).listeners.click();
    assert.deepEqual(actions, [['release', entry.sha256], ['delete', entry.sha256]]);
    // Content held by a failed scan has no threat; it says why it is held.
    const failed = texts(libs.quarantineRow({ ...entry, scan: 'failed', threat: null }, () => {}));
    assert.ok(failed.some((t) => /scanner/i.test(t)));
    // An upload held while it waits for the scanner says so in words.
    const waiting = texts(libs.quarantineRow({ ...entry, scan: 'pending', threat: null, scanned_at: null }, () => {}));
    assert.ok(waiting.some((t) => t.includes('Waiting for the scanner')));
  } finally {
    delete globalThis.document;
  }
  // Content the brand still uses stays held; the page says who uses it, as text.
  assert.equal(libs.brandPinsText([]), null);
  assert.equal(libs.brandPinsText(undefined), null);
  assert.equal(
    libs.brandPinsText([{ owner: 'org' }]),
    'Still used by the brand centre; change the brand to remove it. Until then it stays held and is never shown.',
  );
  assert.equal(
    libs.brandPinsText([{ owner: 'site', id: SITE, name: '<b>Sales</b>' }]),
    'Still used by the look of site <b>Sales</b>; change the brand to remove it. Until then it stays held and is never shown.',
  );
  assert.equal(
    libs.brandPinsText([
      { owner: 'org' },
      { owner: 'site', id: SITE, name: 'Sales' },
      { owner: 'site', id: DOC, name: 'Legal' },
    ]),
    'Still used by the brand centre, by the look of site Sales and by the look of site Legal; change the brand to remove it. Until then it stays held and is never shown.',
  );
  assert.equal(
    libs.QUARANTINE_DELETE_TEXT,
    'Delete every file holding this content? They skip the recycle bin and cannot be restored.',
  );
});

test('type_list_field_normalises', () => {
  assert.deepEqual(libs.parseTypeList(' .PNG, jpg ,, .Tar.GZ , png'), { types: ['.png', '.jpg', '.tar.gz'] });
  assert.deepEqual(libs.parseTypeList(''), { types: [] });
  assert.deepEqual(libs.parseTypeList('  , '), { types: [] });
  assert.ok(libs.parseTypeList('.p ng').error);
  assert.ok(libs.parseTypeList('.').error, 'a dot alone is too short');
  assert.deepEqual(libs.parseTypeList('.a'), { types: ['.a'] });
  assert.ok(libs.parseTypeList(`.${'x'.repeat(20)}`).error, 'more than 20 characters');
  assert.ok(libs.parseTypeList('..png').error);
  assert.ok(libs.parseTypeList('.png.').error);
  assert.ok(libs.parseTypeList('.pn/g').error);
  const many = Array.from({ length: 101 }, (_, n) => `.t${n}`).join(',');
  assert.ok(libs.parseTypeList(many).error, 'at most 100');
  assert.equal(libs.typeListText(['.png', '.jpg']), '.png, .jpg');
  assert.equal(libs.typeListText(null), '');
  // The largest file, in MB: empty is no limit.
  assert.deepEqual(libs.parseMegabytes(''), { bytes: null });
  assert.deepEqual(libs.parseMegabytes('10'), { bytes: 10 * 1024 * 1024 });
  assert.deepEqual(libs.parseMegabytes('0.5'), { bytes: 512 * 1024 });
  assert.ok(libs.parseMegabytes('0').error);
  assert.ok(libs.parseMegabytes('ten').error);
  assert.equal(libs.megabytesText(10 * 1024 * 1024), '10');
  assert.equal(libs.megabytesText(null), '');
});

test('grid_switch_only_for_image_libraries', () => {
  assert.deepEqual(libs.libraryViews('image'), ['list', 'grid']);
  for (const kind of ['document', 'template', 'asset', undefined]) assert.deepEqual(libs.libraryViews(kind), ['list'], kind);
  assert.equal(libs.libraryKindLabel('image'), 'Image library');
  assert.equal(libs.libraryKindLabel('document'), 'Document library');
  assert.equal(libs.libraryKindLabel('template'), 'Template library');
  assert.equal(libs.libraryKindLabel('asset'), 'Asset library');
  // Asset is offered only to administrators.
  assert.deepEqual(libs.libraryKindChoices(false).map((c) => c.value), ['document', 'image', 'template']);
  assert.deepEqual(libs.libraryKindChoices(true).map((c) => c.value), ['document', 'image', 'template', 'asset']);
  // Marks on a row: held, quarantined (with its threat, as text) and blocked.
  assert.deepEqual(libs.itemMarks({ kind: 'document', scan: 'held' }), ['Being checked for malware']);
  assert.deepEqual(libs.itemMarks({ kind: 'document', scan: 'quarantined', threat: 'Eicar' }), ['Quarantined: Eicar']);
  assert.deepEqual(libs.itemMarks({ kind: 'document', policy: 'blocked' }), ['Type not allowed here']);
  assert.deepEqual(libs.itemMarks({ kind: 'document' }), []);
});

test('thumbnails_retry_when_busy_then_give_up', async () => {
  const waits = [];
  const wait = async (ms) => {
    waits.push(ms);
  };
  const answers = (list) => {
    let at = 0;
    return async () => list[Math.min(at++, list.length - 1)];
  };
  const png = { ok: true, status: 200, blob: async () => 'PNG' };
  const busy = { ok: false, status: 503, blob: async () => null };
  const none = { ok: false, status: 415, blob: async () => null };
  assert.equal(await libs.loadThumbnail('/t', { fetch: answers([busy, busy, png]), wait }), 'PNG');
  assert.deepEqual(waits, [1000, 2000]);
  waits.length = 0;
  assert.equal(await libs.loadThumbnail('/t', { fetch: answers([none]), wait }), null);
  assert.deepEqual(waits, []);
  assert.equal(await libs.loadThumbnail('/t', { fetch: answers([busy]), wait }), null);
  assert.deepEqual(waits, [1000, 2000, 4000, 8000]);
  const thrown = async () => {
    throw new TypeError('offline');
  };
  assert.equal(await libs.loadThumbnail('/t', { fetch: thrown, wait }), null);
});

test('new_messages_for_new_error_codes', () => {
  for (const code of [
    'blocked_file_type',
    'file_too_large',
    'folders_off',
    'held',
    'no_preview',
    'no_thumbnail',
    'invalid_library_settings',
    'invalid_scanner_url',
  ]) {
    assert.notEqual(messageFor(code), messageFor('no_such_code'), code);
  }
  // A refused type or size names what was refused, as the upload panel explains it.
  assert.match(libs.policyRefusalText('blocked_file_type', { extension: '.exe' }, 'setup.exe'), /“setup\.exe”.*\.exe/);
  assert.match(libs.policyRefusalText('blocked_file_type', { extension: '' }, 'README'), /“README”/);
  assert.match(libs.policyRefusalText('file_too_large', { limit: 10 * 1024 * 1024 }, 'big.iso'), /“big\.iso”.*10(\.0)? MB/);
  assert.equal(libs.policyRefusalText('name_taken', {}, 'x'), null);
  assert.match(libs.libraryReasonText('not_an_image_type'), /image/);
  assert.notEqual(libs.libraryReasonText('no_types'), libs.libraryReasonText('something_else'));
});

/** A document whose dialogs never fire `close` by themselves (as in a background tab). */
function dialogDocument() {
  const body = {
    children: [],
    append(node) {
      this.children.push(node);
    },
  };
  globalThis.document = {
    body,
    createElement(tag) {
      const element = {
        tag,
        attributes: {},
        children: [],
        listeners: {},
        open: false,
        setAttribute(name, value) {
          this.attributes[name] = value;
        },
        append(...nodes) {
          this.children.push(...nodes);
        },
        addEventListener(type, listener) {
          (this.listeners[type] ??= []).push(listener);
        },
        fire(type) {
          for (const listener of this.listeners[type] ?? []) listener({ type, preventDefault() {} });
        },
        showModal() {
          this.open = true;
        },
        close() {
          this.open = false; // no `close` event: a hidden tab may never deliver one
        },
        remove() {
          body.children = body.children.filter((child) => child !== this);
        },
        focus() {},
      };
      return element;
    },
  };
  return body;
}

const findIn = (element, wanted) => {
  if (typeof element === 'string') return null;
  if (wanted(element)) return element;
  for (const child of element.children) {
    const found = findIn(child, wanted);
    if (found) return found;
  }
  return null;
};
const buttonIn = (element, label) => findIn(element, (e) => e.tag === 'button' && e.children.includes(label));

test('confirm_dialog_resolves_on_click', { timeout: 2000 }, async () => {
  const body = dialogDocument();
  try {
    const yes = libs.confirmDialog('Delete it?', 'Delete');
    const dialog = body.children[0];
    buttonIn(dialog, 'Delete').fire('click');
    assert.equal(await yes, true);
    assert.equal(body.children.length, 0, 'the dialog is gone');
    assert.equal(dialog.open, false);

    const no = libs.confirmDialog('Delete it?', 'Delete');
    buttonIn(body.children[0], 'Cancel').fire('click');
    assert.equal(await no, false);

    // Escape (the dialog's own close) still answers no, and a late close after a click changes nothing.
    const escaped = libs.confirmDialog('Delete it?', 'Delete');
    body.children[0].fire('close');
    assert.equal(await escaped, false);
    const clicked = libs.confirmDialog('Delete it?', 'Delete');
    const again = body.children[0];
    buttonIn(again, 'Delete').fire('click');
    again.fire('close');
    assert.equal(await clicked, true);
  } finally {
    delete globalThis.document;
  }
});

// A stand-in document for showThumbnail: elements that keep their attributes and listeners.
function thumbnailDocument() {
  const element = (tag) => ({
    tag,
    attributes: {},
    listeners: {},
    textContent: '',
    setAttribute(name, value) {
      this.attributes[name] = String(value);
    },
    addEventListener(type, listener) {
      this.listeners[type] = listener;
    },
  });
  const picture = () => ({
    isConnected: true,
    children: [],
    replaceChildren(...children) {
      this.children = children;
    },
  });
  return { doc: { createElement: element }, picture };
}

test('thumbnails_load_their_route_and_survive_a_redraw', async () => {
  const { doc, picture } = thumbnailDocument();
  const url = '/api/docs/d1/thumbnail';
  // The image loads the route itself: the page's policy refuses blob: images.
  const first = picture();
  const img = libs.showThumbnail(first, url, { doc, load: async () => assert.fail('not asked') });
  assert.equal(first.children.length, 1);
  assert.equal(first.children[0], img);
  assert.equal(img.tag, 'img');
  assert.equal(img.attributes.src, url);
  assert.equal(img.attributes.alt, '');

  // The server is busy: while the retry is in flight the list is drawn again. The new tile's
  // image keeps its own live address, and the old, removed tile is not drawn into.
  let answer;
  const pending = new Promise((resolve) => {
    answer = resolve;
  });
  const busy = picture();
  const busyImg = libs.showThumbnail(busy, url, { doc, load: () => pending });
  const failed = busyImg.listeners.error();
  busy.isConnected = false;
  const redrawn = picture();
  const redrawnImg = libs.showThumbnail(redrawn, url, { doc, load: async () => null });
  answer('PNG');
  await failed;
  assert.deepEqual(busy.children, [busyImg]);
  assert.equal(redrawn.children[0], redrawnImg);
  assert.equal(redrawnImg.attributes.src, url);
  assert.ok(!JSON.stringify([busy, redrawn]).includes('blob:'));

  // Busy, then answered: the image again; failing again shows the placeholder, asked no more.
  let asked = 0;
  const later = picture();
  const laterImg = libs.showThumbnail(later, url, {
    doc,
    load: async () => {
      asked += 1;
      return 'PNG';
    },
  });
  await laterImg.listeners.error();
  assert.equal(asked, 1);
  const again = later.children[0];
  assert.notEqual(again, laterImg);
  assert.equal(again.tag, 'img');
  assert.equal(again.attributes.src, url);
  await again.listeners.error();
  assert.equal(asked, 1);
  assert.equal(later.children[0].tag, 'span');
  assert.equal(later.children[0].textContent, 'No thumbnail');

  // No thumbnail at all: the placeholder.
  const none = picture();
  const noneImg = libs.showThumbnail(none, url, { doc, load: async () => null });
  await noneImg.listeners.error();
  assert.equal(none.children[0].attributes.class, 'placeholder');
  assert.equal(none.children[0].textContent, 'No thumbnail');
});

// ---- the site's look: header, footer, vertical navigation, site cards and the frame bar ----

import { directoryRow, footerView, frameMark, headerView, siteInitials, siteMark } from './common.js';

// A document for the look's views: elements keep tag, attributes, children, listeners, `hidden`
// and the style properties set through the CSS object model. Markup and style attributes throw.
function lookDocument() {
  const all = [];
  globalThis.document = {
    createElement(tag) {
      const element = {
        tag,
        attributes: {},
        children: [],
        listeners: {},
        hidden: false,
        style: {
          props: {},
          setProperty(name, value) {
            this.props[name] = value;
          },
        },
        setAttribute(name, value) {
          if (name === 'style') throw new Error('no style attribute is ever set');
          this.attributes[name] = value;
        },
        getAttribute(name) {
          return this.attributes[name] ?? null;
        },
        append(...nodes) {
          this.children.push(...nodes);
        },
        replaceChildren(...nodes) {
          this.children = nodes;
        },
        addEventListener(type, listener) {
          this.listeners[type] = listener;
        },
        set innerHTML(_) {
          throw new Error('innerHTML is never used');
        },
      };
      all.push(element);
      return element;
    },
    getElementById: () => null,
    querySelector: () => null,
    addEventListener() {},
  };
  return all;
}

/** Every element below `element` (itself included). */
const elements = (element) =>
  typeof element === 'string' ? [] : [element, ...element.children.flatMap(elements)];
const byClass = (element, name) =>
  elements(element).filter((e) => (e.attributes.class ?? '').split(' ').includes(name));

const SHA = 'a'.repeat(64);
const IMAGE = `/api/brand/images/${SHA}`;
const OTHER_IMAGE = `/api/brand/images/${'b'.repeat(64)}`;
const SALES = { id: SITE, name: 'Sales' };
const HEADER = { layout: 'compact', background: 'none', align: 'left', hide_title: false };
const FOOTER = { enabled: false, layout: 'simple', name: '', background: 'none', groups: [] };
const look = (more = {}) => ({
  logo_alt: 'Sales',
  navigation: 'horizontal',
  theme_from: 'site',
  ...more,
  header: { ...HEADER, ...more.header },
  footer: { ...FOOTER, ...more.footer },
});

test('header_view_for_each_layout', () => {
  const all = lookDocument();
  try {
    for (const layout of ['minimal', 'compact', 'standard', 'extended']) {
      const view = headerView(SALES, look({ logo_url: IMAGE, header: { layout, background: 'strong' } }));
      assert.equal(view.tag, 'header');
      assert.equal(view.attributes['data-layout'], layout);
      assert.equal(view.attributes['data-background'], 'strong');
      assert.equal(view.attributes['data-align'], 'left');
      const brand = byClass(view, 'site-brand')[0];
      assert.equal(brand.tag, 'a');
      assert.equal(brand.attributes.href, `/site.html?id=${SITE}`);
      const logo = byClass(view, 'site-logo')[0];
      assert.equal(logo.tag, 'img');
      assert.equal(logo.attributes.src, IMAGE);
      assert.equal(logo.attributes.alt, '', 'the name beside it names the link');
      assert.deepEqual(texts(byClass(view, 'site-name')[0]), ['Sales']);
    }
    // An extended band shows its image through <img> under a fixed shade, aligned as set.
    const band = headerView(
      SALES,
      look({ header: { layout: 'extended', background: 'image', image_url: OTHER_IMAGE, align: 'centre' } }),
    );
    assert.equal(band.attributes['data-background'], 'image');
    assert.equal(band.attributes['data-align'], 'centre');
    const image = byClass(band, 'band-image')[0];
    assert.equal(image.tag, 'img');
    assert.equal(image.attributes.src, OTHER_IMAGE);
    assert.equal(image.attributes.alt, '');
    assert.equal(byClass(band, 'band-shade').length, 1);
    // Only an extended header has an image or an alignment.
    const compact = headerView(SALES, look({ header: { background: 'image', image_url: OTHER_IMAGE, align: 'right' } }));
    assert.equal(compact.attributes['data-background'], 'none');
    assert.equal(compact.attributes['data-align'], 'left');
    assert.equal(byClass(compact, 'band-image').length, 0);
    // No look (it could not be read): today's compact header with the name; unknown values too.
    const plain = headerView(SALES, null);
    assert.equal(plain.attributes['data-layout'], 'compact');
    assert.equal(plain.attributes['data-background'], 'none');
    assert.equal(byClass(plain, 'site-logo').length, 0);
    assert.equal(headerView(SALES, look({ header: { layout: 'huge', background: 'url(x)' } })).attributes['data-layout'], 'compact');
    // An image address that is not a brand image is never loaded.
    const odd = headerView(SALES, look({ logo_url: 'https://evil.example/x.png' }));
    assert.equal(byClass(odd, 'site-logo').length, 0);
    assert.ok(!all.some((e) => e.tag === 'img' && !e.attributes.src.startsWith('/api/brand/images/')));
  } finally {
    delete globalThis.document;
  }
});

test('hidden_title_still_named_for_screen_readers', () => {
  lookDocument();
  try {
    const view = headerView(SALES, look({ logo_url: IMAGE, logo_alt: 'Sales team logo', header: { hide_title: true } }));
    const name = byClass(view, 'site-name')[0];
    assert.deepEqual(texts(name), ['Sales'], 'the name stays in the header');
    assert.ok(name.attributes.class.split(' ').includes('sr'), 'shown only to screen readers');
    assert.equal(byClass(view, 'site-logo')[0].attributes.alt, 'Sales team logo');
    // Without a logo the title cannot be hidden.
    const bare = headerView(SALES, look({ header: { hide_title: true } }));
    assert.ok(!byClass(bare, 'site-name')[0].attributes.class.split(' ').includes('sr'));
  } finally {
    delete globalThis.document;
  }
});

test('logo_alt_matching_name_is_not_read_twice', () => {
  lookDocument();
  try {
    // The default description (empty, or the server's substitute, the site name) and one equal
    // to the name: the logo is decoration and the name names the link, once.
    for (const logoAlt of ['', 'Sales', ' Sales ']) {
      for (const hideTitle of [false, true]) {
        const view = headerView(SALES, look({ logo_url: IMAGE, logo_alt: logoAlt, header: { hide_title: hideTitle } }));
        assert.equal(byClass(view, 'site-logo')[0].attributes.alt, '', `${logoAlt} ${hideTitle}`);
        const names = byClass(view, 'site-name');
        assert.equal(names.length, 1);
        assert.deepEqual(texts(names[0]), ['Sales']);
        assert.equal(names[0].attributes.class.split(' ').includes('sr'), hideTitle);
      }
    }
    // A description of its own is kept beside the name.
    const own = headerView(SALES, look({ logo_url: IMAGE, logo_alt: 'Sales team logo' }));
    assert.equal(byClass(own, 'site-logo')[0].attributes.alt, 'Sales team logo');
  } finally {
    delete globalThis.document;
  }
});

test('broken_logo_falls_back_to_name', () => {
  lookDocument();
  try {
    // A held logo answers 423 and the browser fires `error`; the name shows instead.
    const view = headerView(SALES, look({ logo_url: IMAGE, header: { hide_title: true } }));
    const logo = byClass(view, 'site-logo')[0];
    logo.listeners.error();
    assert.equal(logo.hidden, true);
    const name = byClass(view, 'site-name')[0];
    assert.ok(!name.attributes.class.split(' ').includes('sr'), 'the name is shown');
    // A broken band image leaves the shaded band.
    const band = headerView(SALES, look({ header: { layout: 'extended', background: 'image', image_url: IMAGE } }));
    const image = byClass(band, 'band-image')[0];
    image.listeners.error();
    assert.equal(image.hidden, true);
  } finally {
    delete globalThis.document;
  }
});

test('footer_view_simple_and_extended', () => {
  lookDocument();
  try {
    assert.equal(footerView(look()), null, 'a footer that is off is not drawn');
    assert.equal(footerView(null), null);
    const links = (n, prefix) => Array.from({ length: n }, (_, i) => ({ label: `${prefix} ${i + 1}`, url: `/p${i}` }));
    const simple = footerView(
      look({
        footer: {
          enabled: true,
          layout: 'simple',
          name: 'Sales team',
          background: 'soft',
          logo_url: IMAGE,
          groups: [
            { heading: 'Ignored', links: links(10, 'Link') },
            { heading: 'Two', links: links(2, 'Other') },
          ],
        },
      }),
    );
    assert.equal(simple.tag, 'footer');
    assert.equal(simple.attributes['data-layout'], 'simple');
    assert.equal(simple.attributes['data-background'], 'soft');
    assert.ok(texts(simple).includes('Sales team'));
    assert.equal(byClass(simple, 'footer-logo')[0].attributes.src, IMAGE);
    assert.equal(byClass(simple, 'footer-heading').length, 0, 'a simple footer has no heading');
    const simpleLinks = elements(simple).filter((e) => e.tag === 'a');
    assert.equal(simpleLinks.length, 8, 'one group of at most 8 links');
    assert.ok(!texts(simple).includes('Other 1'));

    const groups = Array.from({ length: 5 }, (_, g) => ({ heading: `Group ${g + 1}`, links: links(9, `G${g + 1}`) }));
    const extended = footerView(look({ footer: { enabled: true, layout: 'extended', name: '', background: 'strong', groups } }));
    assert.equal(extended.attributes['data-layout'], 'extended');
    assert.equal(extended.attributes['data-background'], 'strong');
    const headings = byClass(extended, 'footer-heading');
    assert.deepEqual(headings.map((e) => texts(e)[0]), ['Group 1', 'Group 2', 'Group 3', 'Group 4']);
    assert.equal(elements(extended).filter((e) => e.tag === 'a').length, 32, 'four groups of 8 links');
    // An unknown background is drawn as none.
    assert.equal(footerView(look({ footer: { enabled: true, background: 'image' } })).attributes['data-background'], 'none');
  } finally {
    delete globalThis.document;
  }
});

test('footer_links_rendered_as_text_with_safe_href', () => {
  const all = lookDocument();
  try {
    const hostile = '<img src=x onerror="alert(1)">';
    const groups = [
      {
        heading: '',
        links: [
          { label: hostile, url: 'https://example.org/' },
          { label: 'Home', url: '/site.html' },
          { label: 'Plain', url: 'http://example.org/' },
          { label: 'Script', url: 'javascript:alert(1)' },
          { label: 'Data', url: 'data:text/html,<b>x</b>' },
          { label: 'Evil', url: '//evil.example' },
          { label: 'Slash', url: '/\\evil' },
          { label: 'Mail', url: 'mailto:a@example.org' },
        ],
      },
    ];
    const view = footerView(look({ footer: { enabled: true, layout: 'simple', name: hostile, groups } }));
    assert.ok(!all.some((e) => e.tag === 'img'), 'no element is made from a label');
    const drawn = texts(view);
    for (const label of [hostile, 'Script', 'Data', 'Evil', 'Slash', 'Mail']) assert.ok(drawn.includes(label), label);
    const anchors = all.filter((e) => e.tag === 'a');
    assert.deepEqual(
      anchors.map((a) => [a.attributes.href, a.attributes.rel ?? null, a.attributes.target ?? null]),
      [
        ['https://example.org/', 'noopener noreferrer', null],
        ['/site.html', null, null],
        ['http://example.org/', 'noopener noreferrer', null],
      ],
    );
  } finally {
    delete globalThis.document;
  }
});

test('vertical_navigation_class', () => {
  const all = lookDocument();
  try {
    const nav = {
      global: [],
      hub: { id: LIB, name: 'Hub', items: [] },
      local: [{ id: 'l1', label: 'Plans', target: { node: LIB, kind: 'library' }, targeted: false, children: [] }],
      automatic: false,
    };
    const across = navigationView(SALES, nav, {});
    assert.deepEqual(across.attributes.class.split(' '), ['chrome']);
    const down = navigationView(SALES, nav, { vertical: true, hubThumbnail: IMAGE });
    assert.deepEqual(down.attributes.class.split(' '), ['chrome', 'vertical']);
    // The hub bar shows the hub's thumbnail beside its name.
    const thumb = all.find((e) => e.tag === 'img');
    assert.equal(thumb.attributes.src, IMAGE);
    assert.equal(thumb.attributes.alt, '');
    thumb.listeners.error();
    assert.equal(thumb.hidden, true);
    navigationView(SALES, nav, { hubThumbnail: 'https://evil.example/x.png' });
    assert.equal(all.filter((e) => e.tag === 'img').length, 1, 'only brand images are shown');
  } finally {
    delete globalThis.document;
  }
});

test('site_card_initials_without_thumbnail', () => {
  const all = lookDocument();
  try {
    assert.equal(siteInitials('Sales'), 'S');
    assert.equal(siteInitials('  human   resources team '), 'HR');
    assert.equal(siteInitials('ÉQUIPE été'), 'ÉÉ');
    assert.equal(siteInitials('\u{1F600} smile'), '\u{1F600}S');
    assert.equal(siteInitials(''), '?');
    const card = siteMark({ id: SITE, name: 'Human Resources', accent: '#1f5f8b' });
    assert.equal(card.attributes['aria-hidden'], 'true');
    assert.ok(card.attributes.class.split(' ').includes('initials'));
    assert.deepEqual(texts(card), ['HR']);
    assert.equal(card.style.props['--mark-accent'], '#1f5f8b', 'set through the CSS object model');
    assert.ok(!all.some((e) => e.tag === 'img'));
    // A colour that is not #rrggbb is never set.
    assert.deepEqual(siteMark({ name: 'X', accent: 'red;background:url(x)' }).style.props, {});
    // With a thumbnail the picture shows; if it fails, the initials do.
    const pictured = siteMark({ id: SITE, name: 'Sales', accent: '#1f5f8b', thumbnail_url: IMAGE });
    const img = elements(pictured).find((e) => e.tag === 'img');
    assert.equal(img.attributes.src, IMAGE);
    assert.equal(img.attributes.alt, '');
    const initials = byClass(pictured, 'site-initials')[0];
    assert.equal(initials.hidden, true);
    img.listeners.error();
    assert.equal(img.hidden, true);
    assert.equal(initials.hidden, false);
    assert.ok(pictured.attributes.class.split(' ').includes('initials'));
    // The directory row starts with the card's mark.
    const row = directoryRow({ id: SITE, name: 'Sales', kind: 'team', accent: '#1f5f8b', access: null });
    assert.ok(row.children[0].attributes.class.split(' ').includes('site-mark'));
  } finally {
    delete globalThis.document;
  }
});

test('frame_bar_shows_the_thumbnail_or_logo', () => {
  lookDocument();
  try {
    const mono = document.createElement('span');
    mono.setAttribute('class', 'mono');
    mono.append('S');
    frameMark(mono, look({ thumbnail_url: IMAGE, logo_url: OTHER_IMAGE }));
    assert.equal(mono.children[0].tag, 'img');
    assert.equal(mono.children[0].attributes.src, IMAGE);
    assert.equal(mono.attributes.class, 'mono pic');
    mono.children[0].listeners.error();
    assert.deepEqual(mono.children, ['S'], 'the letter comes back');
    assert.equal(mono.attributes.class, 'mono');
    frameMark(mono, look({ logo_url: OTHER_IMAGE }));
    assert.equal(mono.children[0].attributes.src, OTHER_IMAGE);
    frameMark(mono, null);
    assert.deepEqual(mono.children, ['S']);
    assert.doesNotThrow(() => frameMark(null, look()));
  } finally {
    delete globalThis.document;
  }
});

test('messages_for_branding_errors', () => {
  const general = messageFor('no_such_code');
  const codes = [
    'low_contrast',
    'invalid_look',
    'invalid_theme',
    'not_an_image',
    'image_too_large',
    'image_gone',
    'too_many',
    'name_taken',
    'held',
  ];
  const sentences = codes.map(messageFor);
  for (const [at, code] of codes.entries()) assert.notEqual(sentences[at], general, code);
  assert.equal(new Set(sentences).size, codes.length);
});

// ---- what the look never reaches: the account bar, the editor, and the accent on a tint ----

import { readFileSync } from 'node:fs';

const STYLE = readFileSync(new URL('./style.css', import.meta.url), 'utf8').replace(/\/\*[\s\S]*?\*\//g, '');
/** The declarations of every rule whose selector list is exactly `selector`, joined. */
const declarations = (selector) =>
  [...STYLE.matchAll(/([^{}]+)\{([^{}]*)\}/g)]
    .filter(([, sel]) => sel.trim().split(/\s*,\s*/).includes(selector))
    .map(([, , body]) => body)
    .join(';');
const variable = (body, name) => new RegExp(`(?:^|;)\\s*${name}:([^;]+)`).exec(body)?.[1].trim() ?? null;

test('account_bar_keeps_default_colours', () => {
  const root = declarations(':root');
  const topbar = declarations('.topbar');
  for (const name of ['--accent', '--accent-ink', '--on-accent', '--soft']) {
    assert.ok(variable(topbar, name), `.topbar sets ${name}`);
    assert.equal(variable(topbar, name), variable(root, name), `${name} as in :root`);
  }
});

test('accent_text_never_on_soft_tint', () => {
  const footerLinks = declarations('.site-footer[data-background=soft] a');
  assert.equal(variable(footerLinks, 'color'), 'var(--text)');
  assert.equal(variable(footerLinks, 'text-decoration'), 'underline', 'links still read as links');
  assert.equal(variable(declarations('.site-header[data-background=soft] a'), 'color'), 'var(--text)');
  for (const selector of ['.navbar a[aria-current]', '.sitenav a[aria-current]', '.navbar li>a:hover']) {
    assert.equal(variable(declarations(selector), 'color'), 'var(--text)', selector);
  }
});

test('frame_look_on_bar_and_navigation_only', async () => {
  lookDocument();
  const styled = () => {
    const props = {};
    return { props, setProperty: (name, value) => (props[name] = value), removeProperty: (name) => delete props[name] };
  };
  const html = { lang: 'en', style: styled() };
  const bar = { style: styled() };
  const mono = document.createElement('span');
  document.documentElement = html;
  document.querySelector = (selector) => (selector === '.bar' ? bar : selector === '.bar .mono' ? mono : null);
  const theme = {
    light: { accent: '#1f5f8b', accent_ink: '#1f5f8b', on_accent: '#ffffff', header_bg: '#163447', header_ink: '#ffffff' },
    dark: { accent: '#6fa3c8', accent_ink: '#6fa3c8', on_accent: '#141c20', header_bg: '#163447', header_ink: '#ffffff' },
  };
  const realFetch = globalThis.fetch;
  globalThis.fetch = async (url) => {
    const body = url.endsWith('/navigation')
      ? { global: [], hub: null, local: [], automatic: false }
      : url.endsWith('/look')
        ? look({ theme, thumbnail_url: IMAGE })
        : { id: SITE, name: 'Sales', language: 'en-GB', time_zone: 'Europe/London', kind: 'team', description: '' };
    return { ok: true, status: 200, json: async () => body };
  };
  try {
    const place = document.createElement('div');
    await siteChrome(SITE, { place, frame: true });
    assert.deepEqual(html.style.props, {}, 'the editor page keeps its own colours');
    assert.equal(bar.style.props['--accent'], 'light-dark(#1f5f8b,#6fa3c8)');
    const nav = place.children[0];
    assert.equal(nav.style.props['--accent-ink'], 'light-dark(#1f5f8b,#6fa3c8)');
    assert.equal(mono.children[0].attributes.src, IMAGE);
    // Outside the frame the whole page takes the look.
    await siteChrome(SITE, { place });
    assert.equal(html.style.props['--accent'], 'light-dark(#1f5f8b,#6fa3c8)');
  } finally {
    globalThis.fetch = realFetch;
    delete globalThis.document;
  }
});

// ---- Change the look's preview: draft images chosen from a document ----

import { draftImageHref, swatch } from './common.js';

test('preview_loads_only_brand_or_document_images', () => {
  const all = lookDocument();
  try {
    const DOC_URL = `/api/docs/${FILE}/preview`;
    const THUMB_URL = `/api/docs/${FILE}/thumbnail`;
    assert.equal(draftImageHref(IMAGE), IMAGE);
    assert.equal(draftImageHref(DOC_URL), DOC_URL);
    assert.equal(draftImageHref(THUMB_URL), THUMB_URL);
    for (const bad of [
      `/api/docs/${FILE}/content`,
      `/api/docs/${FILE}/preview?x=1`,
      `/api/docs/${FILE}/preview/`,
      `/api/docs/not-an-id/preview`,
      `//evil.example/api/docs/${FILE}/preview`,
      `https://evil.example/api/docs/${FILE}/preview`,
      `/api/docs/${FILE}/../../x/preview`,
      'javascript:alert(1)',
      `data:image/svg+xml,<svg/>`,
      null,
    ]) {
      assert.equal(draftImageHref(bad), null, String(bad));
    }
    const draftLook = look({
      logo_url: DOC_URL,
      header: { layout: 'extended', background: 'image', image_url: THUMB_URL },
      footer: { enabled: true, name: '', logo_url: DOC_URL },
    });
    // On a page, a document address is never loaded: only brand images.
    const page = headerView(SALES, draftLook);
    assert.equal(byClass(page, 'site-logo').length, 0);
    assert.equal(byClass(page, 'band-image').length, 0);
    assert.equal(byClass(footerView(draftLook), 'footer-logo').length, 0);
    // In the preview element (draft) it is, and nothing else.
    const preview = headerView(SALES, draftLook, { draft: true });
    assert.equal(byClass(preview, 'site-logo')[0].attributes.src, DOC_URL);
    assert.equal(byClass(preview, 'band-image')[0].attributes.src, THUMB_URL);
    assert.equal(byClass(footerView(draftLook, { draft: true }), 'footer-logo')[0].attributes.src, DOC_URL);
    const odd = headerView(SALES, look({ logo_url: `/api/docs/${FILE}/content` }), { draft: true });
    assert.equal(byClass(odd, 'site-logo').length, 0);
    for (const img of all.filter((e) => e.tag === 'img')) {
      assert.ok(draftImageHref(img.attributes.src), img.attributes.src);
    }
  } finally {
    delete globalThis.document;
  }
});

test('swatches_set_colour_only_through_the_object_model', () => {
  lookDocument();
  try {
    const good = swatch('#1F5F8B', 'Primary');
    assert.equal(good.style.props['--swatch'], '#1f5f8b');
    assert.equal(good.attributes.title, 'Primary #1f5f8b');
    for (const bad of ['red;background:url(x)', 'url(x)', '#fff', null]) {
      const none = swatch(bad, 'Primary');
      assert.deepEqual(none.style.props, {}, String(bad));
      assert.ok(none.attributes.class.split(' ').includes('none'));
    }
  } finally {
    delete globalThis.document;
  }
});

import { contrastRows } from './common.js';

test('contrast_rows_name_each_failing_pair_with_its_suggestion', () => {
  lookDocument();
  try {
    const used = [];
    const rows = contrastRows(
      {
        ok: false,
        pairs: [
          { check: 'primary_on_light', ratio: 2.1, suggestion: '#4f6fd1' },
          { check: 'primary_dark_on_dark', ratio: 1.84, suggestion: null },
        ],
      },
      (pair) => used.push(pair.check),
    );
    assert.equal(rows.length, 2);
    assert.ok(texts(rows[0]).join(' ').includes('Links and accents on light pages'));
    assert.ok(texts(rows[0]).join(' ').includes('2.10:1, needs 4.5:1'));
    const use = elements(rows[0]).find((e) => e.tag === 'button');
    assert.deepEqual(use.children, ['Use suggested #4f6fd1']);
    assert.equal(byClass(rows[0], 'swatch')[0].style.props['--swatch'], '#4f6fd1');
    use.listeners.click();
    assert.deepEqual(used, ['primary_on_light']);
    // A derived dark colour that cannot pass has no button: it must be set by hand.
    assert.equal(elements(rows[1]).filter((e) => e.tag === 'button').length, 0);
    // Passing and unreadable themes get one row each.
    assert.equal(contrastRows({ ok: true }, () => {}).length, 1);
    assert.ok(texts(contrastRows({ ok: false, pairs: [], invalid: true }, () => {})[0]).join(' ').includes('#3156c9'));
  } finally {
    delete globalThis.document;
  }
});
