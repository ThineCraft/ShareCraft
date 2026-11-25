import { test } from 'node:test';
import assert from 'node:assert/strict';
import { heroFrame, heroFrameStyle, heroHref, heroStep, heroWindow, moveHeroFrame, readHero, sameFrame } from './hero.js';

const URL_A = `/api/brand/images/${'a'.repeat(64)}`;
const URL_B = `/api/brand/images/${'b'.repeat(64)}`;
const MIDDLE = { x: 500, y: 500, zoom: 100 };

test('only brand image addresses are taken', () => {
  assert.equal(heroHref(URL_A), URL_A);
  assert.equal(heroHref('https://elsewhere.example/a.png'), null);
  assert.equal(heroHref(`${URL_A}?x`), null);
  assert.equal(heroHref(null), null);
});

test('a kept state is read back only when whole', () => {
  assert.deepEqual(readHero(JSON.stringify({ site: 's', library: 'l', url: URL_A })), { site: 's', library: 'l', url: URL_A, frame: MIDDLE });
  assert.deepEqual(readHero(JSON.stringify({ site: 's', library: 'l', url: URL_A, frame: { x: 1, y: 2, zoom: 150 } })).frame, { x: 1, y: 2, zoom: 150 });
  assert.equal(readHero(JSON.stringify({ site: 's', library: 'l', url: 'javascript:1' })), null);
  assert.equal(readHero(JSON.stringify({ site: 's', url: URL_A })), null);
  assert.equal(readHero('{not json'), null);
  assert.equal(readHero(null), null);
});

test('entering a library slides its picture in', () => {
  const step = heroStep(null, { site: 's', library: 'l', url: URL_A });
  assert.deepEqual(step, { url: URL_A, frame: MIDDLE, motion: 'in', keep: { site: 's', library: 'l', url: URL_A, frame: MIDDLE } });
});

test('another page of the same library shows it with no motion', () => {
  const previous = { site: 's', library: 'l', url: URL_A };
  assert.equal(heroStep(previous, { site: 's', library: 'l', url: URL_A }).motion, 'none');
  // Moved but the same picture: shown at once in its new place.
  const moved = heroStep(previous, { site: 's', library: 'l', url: URL_A, frame: { x: 0, y: 1000, zoom: 200 } });
  assert.equal(moved.motion, 'none');
  assert.deepEqual(moved.frame, { x: 0, y: 1000, zoom: 200 });
  assert.deepEqual(moved.keep.frame, { x: 0, y: 1000, zoom: 200 });
});

test('a changed picture, or another library, slides the new one in', () => {
  const previous = { site: 's', library: 'l', url: URL_A };
  assert.equal(heroStep(previous, { site: 's', library: 'l', url: URL_B }).motion, 'in');
  assert.equal(heroStep(previous, { site: 's', library: 'm', url: URL_B }).motion, 'in');
});

test('leaving the library for the same site slides it back out and forgets it', () => {
  const previous = { site: 's', library: 'l', url: URL_A, frame: { x: 10, y: 20, zoom: 300 } };
  // It leaves as it was framed.
  assert.deepEqual(heroStep(previous, { site: 's' }), { url: URL_A, frame: previous.frame, motion: 'out', keep: null });
  // A library with no picture of its own counts as leaving.
  assert.deepEqual(heroStep(previous, { site: 's', library: 'm', url: null }), { url: URL_A, frame: previous.frame, motion: 'out', keep: null });
});

test('another site shows nothing and forgets it', () => {
  const previous = { site: 's', library: 'l', url: URL_A };
  assert.deepEqual(heroStep(previous, { site: 't' }), { url: null, frame: null, motion: 'none', keep: null });
  assert.deepEqual(heroStep(null, { site: 's' }), { url: null, frame: null, motion: 'none', keep: null });
});

test('a picture that is not a brand image is never drawn', () => {
  assert.deepEqual(heroStep(null, { site: 's', library: 'l', url: '/elsewhere.png' }), { url: null, frame: null, motion: 'none', keep: null });
});

test('a frame is whole numbers in range, the middle at its smallest for anything else', () => {
  assert.deepEqual(heroFrame(null), MIDDLE);
  assert.deepEqual(heroFrame({ x: 'a', y: 1, zoom: 100 }), MIDDLE);
  assert.deepEqual(heroFrame({ x: -5, y: 1200.4, zoom: 50 }), { x: 0, y: 1000, zoom: 100 });
  assert.deepEqual(heroFrame({ x: 333.6, y: 0, zoom: 999 }), { x: 334, y: 0, zoom: 400 });
  assert.ok(sameFrame(null, MIDDLE));
  assert.ok(!sameFrame(MIDDLE, { ...MIDDLE, zoom: 105 }));
});

test('a frame is drawn at its point, enlarged around it', () => {
  assert.deepEqual(heroFrameStyle({ x: 250, y: 900, zoom: 150 }), { objectPosition: '25% 90%', transformOrigin: '25% 90%', scale: '1.5' });
  assert.deepEqual(heroFrameStyle(null), { objectPosition: '50% 50%', transformOrigin: '50% 50%', scale: '1' });
});

test('the part shown follows the header shape, the point and the size', () => {
  const box = { width: 1200, height: 160 };
  const picture = { width: 1920, height: 1080 };
  // Covered across: the whole width, a strip of the height.
  const middle = heroWindow(MIDDLE, box, picture);
  assert.equal(middle.width, 1);
  assert.ok(Math.abs(middle.height - (160 * 1920) / (1200 * 1080)) < 1e-9);
  assert.ok(Math.abs(middle.top - (1 - middle.height) / 2) < 1e-9);
  // At the top, twice the size: half as wide and high, at the left edge for x 0.
  const top = heroWindow({ x: 0, y: 0, zoom: 200 }, box, picture);
  assert.equal(top.width, 0.5);
  assert.equal(top.left, 0);
  assert.equal(top.top, 0);
  assert.ok(Math.abs(top.height - middle.height / 2) < 1e-9);
});

test('moving keeps the part shown within the picture', () => {
  const box = { width: 1200, height: 160 };
  const picture = { width: 1920, height: 1080 };
  // Across: nowhere to move at the smallest size.
  assert.equal(moveHeroFrame(MIDDLE, 0.3, 0, box, picture).x, 500);
  // Down by a tenth of the picture moves the part shown by a tenth.
  const shown = heroWindow(MIDDLE, box, picture);
  const down = moveHeroFrame(MIDDLE, 0, 0.1, box, picture);
  assert.ok(Math.abs(heroWindow(down, box, picture).top - (shown.top + 0.1)) < 0.002);
  // Never past the edge.
  assert.equal(moveHeroFrame(MIDDLE, 0, 5, box, picture).y, 1000);
  assert.equal(moveHeroFrame({ x: 500, y: 500, zoom: 200 }, -5, 0, box, picture).x, 0);
});
