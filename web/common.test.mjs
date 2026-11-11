import test from 'node:test';
import assert from 'node:assert/strict';
import {
  breadcrumbParts,
  breadcrumbs,
  canMoveHere,
  messageFor,
  moveChoices,
  pageFor,
  removedMessage,
  safeNext,
} from './common.js';

const origin = 'http://127.0.0.1:8787';

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

test('breadcrumbs link only the steps with a role', () => {
  const path = [
    { id: SITE, kind: 'site', name: 'Site', role: null },
    { id: LIB, kind: 'library', name: 'Documents', role: null },
    { id: DIR, kind: 'folder', name: 'Plans', role: 'viewer' },
    { id: FILE, kind: 'document', name: 'Plan.txt', role: 'viewer' },
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

test('rendered breadcrumbs hold links only where there is a role', () => {
  fakeDocument();
  try {
    const list = breadcrumbs([
      { id: SITE, kind: 'site', name: 'Site', role: null },
      { id: LIB, kind: 'library', name: 'Documents', role: 'owner' },
      { id: DIR, kind: 'folder', name: 'Plans', role: 'owner' },
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
    'not_empty',
    'into_itself',
    'wrong_place',
    'is_site',
    'last_owner',
    'invalid_name',
    'invalid_request',
    'no_such_user',
    'not_found',
  ]) {
    assert.notEqual(messageFor(code), general, code);
  }
  assert.match(messageFor('name_taken'), /already has that name/);
  assert.match(messageFor('not_empty'), /still has items/);
  assert.match(messageFor('into_itself'), /itself/);
});

test('a document that was deleted has its own removal message', () => {
  assert.equal(removedMessage('deleted'), 'This document was deleted.');
  assert.notEqual(removedMessage('access_revoked'), removedMessage('deleted'));
});

test('the move dialog never offers the item itself', () => {
  const items = [
    { id: DIR, kind: 'folder', name: 'Plans', role: 'editor' },
    { id: LIB, kind: 'folder', name: 'Other', role: 'owner' },
    { id: FILE, kind: 'document', name: 'Plan.txt', role: 'editor' },
    { id: SITE, kind: 'library', name: 'Limited', role: null },
  ];
  const offered = moveChoices(items, DIR);
  assert.deepEqual(offered.map((o) => o.id), [LIB, SITE]);
  assert.equal(offered[0].canChoose, true);
  assert.equal(offered[1].canChoose, false, 'limited access is a way through, not a place');
});

test('the move dialog never lets the item go into itself or below itself', () => {
  const site = { id: SITE, kind: 'site', name: 'S', role: 'owner' };
  const lib = { id: LIB, kind: 'library', name: 'L', role: 'owner' };
  const moving = { id: DIR, kind: 'folder', name: 'D', role: 'editor' };
  const below = { id: FILE, kind: 'folder', name: 'Below', role: 'editor' };
  assert.equal(canMoveHere([site, lib], DIR, FILE), true);
  assert.equal(canMoveHere([site, lib, moving], DIR, FILE), false, 'into itself');
  assert.equal(canMoveHere([site, lib, moving, below], DIR, FILE), false, 'below itself');
  assert.equal(canMoveHere([site, lib], DIR, LIB), false, 'where it already is');
  assert.equal(canMoveHere([site], DIR, LIB), false, 'a site is not a place for a folder');
  assert.equal(canMoveHere([site, { ...lib, role: 'viewer' }], DIR, FILE), false);
  assert.equal(canMoveHere([], DIR, FILE), false);
});
