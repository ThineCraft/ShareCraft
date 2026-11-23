import { test } from 'node:test';
import assert from 'node:assert/strict';
import { heroHref, heroStep, readHero } from './hero.js';

const URL_A = `/api/brand/images/${'a'.repeat(64)}`;
const URL_B = `/api/brand/images/${'b'.repeat(64)}`;

test('only brand image addresses are taken', () => {
  assert.equal(heroHref(URL_A), URL_A);
  assert.equal(heroHref('https://elsewhere.example/a.png'), null);
  assert.equal(heroHref(`${URL_A}?x`), null);
  assert.equal(heroHref(null), null);
});

test('a kept state is read back only when whole', () => {
  assert.deepEqual(readHero(JSON.stringify({ site: 's', library: 'l', url: URL_A })), { site: 's', library: 'l', url: URL_A });
  assert.equal(readHero(JSON.stringify({ site: 's', library: 'l', url: 'javascript:1' })), null);
  assert.equal(readHero(JSON.stringify({ site: 's', url: URL_A })), null);
  assert.equal(readHero('{not json'), null);
  assert.equal(readHero(null), null);
});

test('entering a library slides its picture in', () => {
  const step = heroStep(null, { site: 's', library: 'l', url: URL_A });
  assert.deepEqual(step, { url: URL_A, motion: 'in', keep: { site: 's', library: 'l', url: URL_A } });
});

test('another page of the same library shows it with no motion', () => {
  const previous = { site: 's', library: 'l', url: URL_A };
  assert.equal(heroStep(previous, { site: 's', library: 'l', url: URL_A }).motion, 'none');
});

test('a changed picture, or another library, slides the new one in', () => {
  const previous = { site: 's', library: 'l', url: URL_A };
  assert.equal(heroStep(previous, { site: 's', library: 'l', url: URL_B }).motion, 'in');
  assert.equal(heroStep(previous, { site: 's', library: 'm', url: URL_B }).motion, 'in');
});

test('leaving the library for the same site slides it back out and forgets it', () => {
  const previous = { site: 's', library: 'l', url: URL_A };
  assert.deepEqual(heroStep(previous, { site: 's' }), { url: URL_A, motion: 'out', keep: null });
  // A library with no picture of its own counts as leaving.
  assert.deepEqual(heroStep(previous, { site: 's', library: 'm', url: null }), { url: URL_A, motion: 'out', keep: null });
});

test('another site shows nothing and forgets it', () => {
  const previous = { site: 's', library: 'l', url: URL_A };
  assert.deepEqual(heroStep(previous, { site: 't' }), { url: null, motion: 'none', keep: null });
  assert.deepEqual(heroStep(null, { site: 's' }), { url: null, motion: 'none', keep: null });
});

test('a picture that is not a brand image is never drawn', () => {
  assert.deepEqual(heroStep(null, { site: 's', library: 'l', url: '/elsewhere.png' }), { url: null, motion: 'none', keep: null });
});
