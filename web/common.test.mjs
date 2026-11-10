import test from 'node:test';
import assert from 'node:assert/strict';
import { safeNext } from './common.js';

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
