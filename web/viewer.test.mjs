import test from 'node:test';
import assert from 'node:assert/strict';
import {
  MAX_ZOOM,
  MIN_ZOOM,
  clampPan,
  clampZoom,
  fitScale,
  isViewable,
  lineNumbers,
  neighbours,
  nextZoom,
  positionText,
  rangeTotal,
  viewerList,
  wheelFactor,
  zoomAbout,
  zoomLabel,
} from './viewer.js';

const near = (a, b) => assert.ok(Math.abs(a - b) < 1e-9, `${a} is not ${b}`);

test('fitScale fits the whole picture and does not enlarge a small one', () => {
  near(fitScale({ w: 2000, h: 1000 }, { w: 1000, h: 1000 }), 0.5);
  near(fitScale({ w: 1000, h: 2000 }, { w: 1000, h: 500 }), 0.25);
  assert.equal(fitScale({ w: 128, h: 128 }, { w: 1000, h: 800 }), 1);
  near(fitScale({ w: 100, h: 50 }, { w: 1000, h: 800 }, Infinity), 10);
  assert.equal(fitScale({ w: 0, h: 0 }, { w: 100, h: 100 }), 1);
  assert.equal(fitScale(null, { w: 100, h: 100 }), 1);
});

test('clampZoom keeps the zoom in range', () => {
  assert.equal(clampZoom(100), MAX_ZOOM);
  assert.equal(clampZoom(0.001), MIN_ZOOM);
  assert.equal(clampZoom(2.5), 2.5);
  assert.equal(clampZoom(NaN), 1);
  assert.equal(clampZoom(-1), 1);
  assert.equal(clampZoom(0.2, 0.5, 4), 0.5);
});

test('nextZoom steps through the levels both ways', () => {
  assert.equal(nextZoom(1, 1), 1.25);
  assert.equal(nextZoom(1, -1), 0.75);
  assert.equal(nextZoom(0.999999, 1), 1.25);
  assert.equal(nextZoom(1.1, -1), 1);
  assert.equal(nextZoom(MAX_ZOOM, 1), MAX_ZOOM);
  assert.equal(nextZoom(MIN_ZOOM, -1), MIN_ZOOM);
});

test('zoomAbout keeps the point under the pointer still', () => {
  const view = { scale: 1, x: 0, y: 0 };
  // Zooming about the centre does not move the picture.
  assert.deepEqual(zoomAbout(view, 2), { scale: 2, x: 0, y: 0 });
  // A spot 100 px right of the centre stays 100 px right of it.
  const after = zoomAbout(view, 2, { x: 100, y: -50 });
  assert.deepEqual(after, { scale: 2, x: -100, y: 50 });
  const spotBefore = (100 - view.x) / view.scale;
  const spotAfter = (100 - after.x) / after.scale;
  near(spotBefore, spotAfter);
  // And back again.
  const back = zoomAbout(after, 1, { x: 100, y: -50 });
  near(back.x, 0);
  near(back.y, 0);
});

test('clampPan centres a small picture and stops a large one at its edges', () => {
  const box = { w: 1000, h: 800 };
  assert.deepEqual(clampPan({ scale: 1, x: 300, y: -40 }, { w: 500, h: 400 }, box), { scale: 1, x: 0, y: 0 });
  // 2000 × 1600 at 1: 500 px spare each side across, 400 down.
  assert.deepEqual(clampPan({ scale: 1, x: 900, y: -100 }, { w: 2000, h: 1600 }, box), { scale: 1, x: 500, y: -100 });
  assert.deepEqual(clampPan({ scale: 1, x: -900, y: -900 }, { w: 2000, h: 1600 }, box), { scale: 1, x: -500, y: -400 });
});

test('zoomLabel says Fit or the percentage', () => {
  assert.equal(zoomLabel(0.31, true), 'Fit');
  assert.equal(zoomLabel(1), '100 %');
  assert.equal(zoomLabel(2.5), '250 %');
  assert.equal(zoomLabel(1 / 3), '33 %');
});

test('wheelFactor zooms in on a turn up and is bounded', () => {
  assert.ok(wheelFactor(-100) > 1);
  assert.ok(wheelFactor(100) < 1);
  assert.equal(wheelFactor(0), 1);
  near(wheelFactor(10_000), wheelFactor(200));
});

const child = (id, name, extra = {}) => ({ id, name, kind: 'document', editor: 'other', revised_at: '2026-01-01T00:00:00Z', ...extra });

test('isViewable takes previewable files only', () => {
  assert.equal(isViewable(child('a', 'a.png')), true);
  assert.equal(isViewable(child('a', 'a.txt')), true);
  assert.equal(isViewable(child('a', 'a.mp4')), true);
  assert.equal(isViewable(child('a', 'a.zip')), false);
  assert.equal(isViewable(child('a', 'a.docx', { editor: 'wordcraft' })), false);
  assert.equal(isViewable(child('a', 'a.png', { scan: 'held' })), false);
  assert.equal(isViewable(child('a', 'a.png', { revised_at: undefined })), false);
  assert.equal(isViewable({ id: 'f', name: 'Pictures.png', kind: 'folder' }), false);
  assert.equal(isViewable(null), false);
});

test('viewerList keeps the viewable files and the current one, in the library order', () => {
  const children = [
    { id: 'f', name: 'Folder', kind: 'folder' },
    child('b', 'b.png', { revised_at: '2026-01-03T00:00:00Z' }),
    child('z', 'z.zip'),
    child('a', 'a.png', { revised_at: '2026-01-01T00:00:00Z' }),
    child('c', 'c.txt', { revised_at: '2026-01-02T00:00:00Z' }),
  ];
  assert.deepEqual(viewerList(children, 'a').map((i) => i.id), ['a', 'b', 'c']);
  assert.deepEqual(viewerList(children, 'z').map((i) => i.id), ['a', 'b', 'c', 'z']);
  const newest = JSON.stringify({ key: 'modified', direction: 'descending' });
  assert.deepEqual(viewerList(children, 'a', newest).map((i) => i.id), ['b', 'c', 'a']);
  assert.deepEqual(viewerList(children, 'a', 'not json').map((i) => i.id), ['a', 'b', 'c']);
  assert.deepEqual(viewerList(undefined, 'a'), []);
});

test('neighbours finds the files either side, without wrapping', () => {
  const list = [{ id: 'a' }, { id: 'b' }, { id: 'c' }];
  assert.deepEqual(neighbours(list, 'b'), { index: 1, total: 3, prev: { id: 'a' }, next: { id: 'c' } });
  assert.deepEqual(neighbours(list, 'a'), { index: 0, total: 3, prev: null, next: { id: 'b' } });
  assert.deepEqual(neighbours(list, 'c'), { index: 2, total: 3, prev: { id: 'b' }, next: null });
  assert.deepEqual(neighbours(list, 'x'), { index: -1, total: 3, prev: null, next: null });
  assert.deepEqual(neighbours([], 'a'), { index: -1, total: 0, prev: null, next: null });
  assert.deepEqual(neighbours(null, 'a'), { index: -1, total: 0, prev: null, next: null });
});

test('positionText says where the file is', () => {
  assert.equal(positionText({ index: 2, total: 12 }), '3 of 12');
  assert.equal(positionText({ index: 0, total: 1 }), '');
  assert.equal(positionText({ index: -1, total: 5 }), '');
});

test('lineNumbers splits at every kind of line break', () => {
  assert.deepEqual(lineNumbers('a\nb\r\nc\rd'), { lines: ['a', 'b', 'c', 'd'], width: 1 });
  assert.deepEqual(lineNumbers('a\nb\n'), { lines: ['a', 'b'], width: 1 });
  assert.deepEqual(lineNumbers('a\n\n'), { lines: ['a', ''], width: 1 });
  assert.deepEqual(lineNumbers(''), { lines: [''], width: 1 });
  assert.deepEqual(lineNumbers(null), { lines: [''], width: 1 });
  assert.equal(lineNumbers('x\n'.repeat(1234)).width, 4);
  assert.equal(lineNumbers('x\n'.repeat(1234)).lines.length, 1234);
});

test('rangeTotal reads the size from Content-Range', () => {
  assert.equal(rangeTotal('bytes 0-0/1234'), 1234);
  assert.equal(rangeTotal('bytes */99'), 99);
  assert.equal(rangeTotal('bytes 0-0/*'), null);
  assert.equal(rangeTotal(null), null);
});
