import test from 'node:test';
import assert from 'node:assert/strict';
import {
  GAP,
  INTERVALS,
  MAX_RATIO,
  MIN_RATIO,
  dayLabel,
  dayOf,
  groupByDay,
  isPhoto,
  justify,
  moveIn,
  positionOf,
  ratioOf,
  rowHeightFor,
  stepIndex,
  swipeOf,
  tileLabel,
  viewerDate,
  whenOf,
} from './photos.js';

test('ratioOf uses the facts, then what loaded, then square, within the limits', () => {
  assert.equal(ratioOf({ id: 'a', photo: { width: 300, height: 200 } }), 1.5);
  assert.equal(ratioOf({ id: 'a' }, new Map([['a', 0.75]])), 0.75);
  assert.equal(ratioOf({ id: 'a' }), 1);
  assert.equal(ratioOf({ id: 'a', photo: { width: 0, height: 10 } }), 1);
  assert.equal(ratioOf({ id: 'p', photo: { width: 10000, height: 500 } }), MAX_RATIO);
  assert.equal(ratioOf({ id: 's', photo: { width: 10, height: 1000 } }), MIN_RATIO);
  assert.equal(ratioOf(null), 1);
});

test('justify fills each full row exactly, at one height per row, near the height aimed at', () => {
  const ratios = [1.5, 1.5, 0.75, 1, 1.5, 2, 0.66, 1.33, 1.5, 1];
  const width = 1000;
  const { tiles, rows, height } = justify(ratios, width, { height: 220, gap: 4 });
  assert.equal(tiles.length, ratios.length);
  for (const row of rows.slice(0, -1)) {
    const inRow = tiles.slice(row.first, row.first + row.count);
    assert.ok(inRow.every((t) => t.h === row.h && t.y === row.y));
    const last = inRow.at(-1);
    assert.equal(last.x + last.w, width, 'a full row reaches the right edge');
    for (let i = 1; i < inRow.length; i += 1) assert.equal(inRow[i].x, inRow[i - 1].x + inRow[i - 1].w + 4);
    assert.ok(row.h <= 220 && row.h > 110, `row height ${row.h}`);
  }
  // Shapes are kept to within the rounding of a pixel or two.
  for (const [i, t] of tiles.entries()) {
    if (t.row < rows.length - 1) assert.ok(Math.abs(t.w / t.h - ratios[i]) < 0.03, `tile ${i}`);
  }
  assert.equal(height, rows.at(-1).y + rows.at(-1).h);
  // Rows follow one another with the gap between.
  for (let i = 1; i < rows.length; i += 1) assert.equal(rows[i].y, rows[i - 1].y + rows[i - 1].h + 4);
});

test('a last row that would not fill keeps the height aimed at and is not stretched', () => {
  const { tiles, rows } = justify([1, 1], 1000, { height: 200, gap: 4 });
  assert.equal(rows.length, 1);
  assert.deepEqual(
    tiles.map((t) => [t.x, t.w, t.h]),
    [
      [0, 200, 200],
      [204, 200, 200],
    ],
  );
});

test('justify of nothing, of one panorama, and of a narrow width', () => {
  assert.deepEqual(justify([], 800), { tiles: [], rows: [], height: 0 });
  const wide = justify([3], 600, { height: 220 });
  assert.equal(wide.tiles[0].w, 600);
  assert.equal(wide.tiles[0].h, 200);
  const phone = justify([1.5, 1.5, 1.5], 360);
  assert.ok(phone.tiles.every((t) => t.x + t.w <= 360));
  assert.equal(rowHeightFor(360), 120);
  assert.equal(rowHeightFor(1200), 220);
  assert.equal(GAP, 4);
});

test('dayOf and whenOf prefer the day taken, read as local time', () => {
  const taken = { photo: { taken_at: '2024-07-14T18:05:09' }, revised_at: '2025-01-01T00:00:00Z' };
  assert.equal(dayOf(taken), '2024-07-14');
  const when = whenOf(taken);
  assert.deepEqual([when.getFullYear(), when.getMonth(), when.getDate(), when.getHours()], [2024, 6, 14, 18]);
  const saved = { revised_at: '2025-03-02T12:00:00Z' };
  const local = new Date('2025-03-02T12:00:00Z');
  assert.equal(dayOf(saved), `${local.getFullYear()}-${String(local.getMonth() + 1).padStart(2, '0')}-${String(local.getDate()).padStart(2, '0')}`);
  assert.equal(dayOf({}), '');
  assert.equal(whenOf({ revised_at: 'nonsense' }), null);
});

test('groupByDay puts the newest day first, newest first within it, undated last', () => {
  const items = [
    { name: 'b', photo: { taken_at: '2024-07-14T09:00:00' } },
    { name: 'none' },
    { name: 'a', photo: { taken_at: '2024-07-14T18:00:00' } },
    { name: 'old', photo: { taken_at: '2020-01-01T00:00:00' } },
    { name: 'c', photo: { taken_at: '2024-07-14T09:00:00' } },
  ];
  const groups = groupByDay(items);
  assert.deepEqual(
    groups.map((g) => [g.day, g.items.map((i) => i.name)]),
    [
      ['2024-07-14', ['a', 'b', 'c']],
      ['2020-01-01', ['old']],
      ['', ['none']],
    ],
  );
  assert.deepEqual(groupByDay(items, false), [{ day: '', items }]);
  assert.deepEqual(groupByDay([], false), []);
});

test('dayLabel says Today, Yesterday, a weekday this year, a full date before', () => {
  const now = new Date(2026, 10, 21, 10, 0, 0);
  assert.equal(dayLabel('2026-11-21', now, 'en-GB'), 'Today');
  assert.equal(dayLabel('2026-11-20', now, 'en-GB'), 'Yesterday');
  assert.match(dayLabel('2026-07-14', now, 'en-GB'), /^Tue,? 14 Jul$/);
  assert.match(dayLabel('2024-07-14', now, 'en-GB'), /^Sun,? 14 Jul 2024$/);
  assert.equal(dayLabel('', now), 'No date');
});

test('moveIn walks the order sideways and the rows up and down by the nearest middle', () => {
  // Row 0: three tiles; row 1: two wide ones.
  const tiles = [
    { x: 0, w: 100, row: 0 },
    { x: 104, w: 100, row: 0 },
    { x: 208, w: 100, row: 0 },
    { x: 0, w: 150, row: 1 },
    { x: 154, w: 154, row: 1 },
  ];
  assert.equal(moveIn(tiles, 0, 'ArrowLeft'), null);
  assert.equal(moveIn(tiles, 2, 'ArrowRight'), 3);
  assert.equal(moveIn(tiles, 4, 'ArrowRight'), null);
  assert.equal(moveIn(tiles, 2, 'ArrowDown'), 4);
  assert.equal(moveIn(tiles, 0, 'ArrowDown'), 3);
  assert.equal(moveIn(tiles, 4, 'ArrowUp'), 2);
  assert.equal(moveIn(tiles, 3, 'ArrowUp'), 0);
  assert.equal(moveIn(tiles, 3, 'ArrowDown'), null);
  assert.equal(moveIn(tiles, 3, 'Home'), 0);
  assert.equal(moveIn(tiles, 0, 'End'), 4);
  assert.equal(moveIn(tiles, 0, 'PageDown'), 3);
  assert.equal(moveIn(tiles, 0, 'Enter'), null);
  assert.equal(moveIn([], 0, 'ArrowRight'), null);
});

test('stepIndex stops at the ends unless it loops', () => {
  assert.equal(stepIndex(0, 3, 1), 1);
  assert.equal(stepIndex(2, 3, 1), null);
  assert.equal(stepIndex(2, 3, 1, true), 0);
  assert.equal(stepIndex(0, 3, -1, true), 2);
  assert.equal(stepIndex(0, 0, 1, true), null);
});

test('swipeOf tells quick, long, mostly straight swipes', () => {
  assert.equal(swipeOf(-120, 10, 200), 'next');
  assert.equal(swipeOf(120, -10, 200), 'previous');
  assert.equal(swipeOf(5, 140, 250), 'close');
  assert.equal(swipeOf(-30, 0, 200), null);
  assert.equal(swipeOf(-120, 100, 200), null);
  assert.equal(swipeOf(-120, 0, 900), null);
});

test('labels for the grid and the viewer', () => {
  const item = { name: 'Harbour.jpg', photo: { taken_at: '2024-07-14T18:05:00' } };
  assert.equal(tileLabel(item, 'en-GB'), 'Harbour.jpg, 14 July 2024');
  assert.equal(tileLabel({ name: 'Plain.png' }), 'Plain.png');
  assert.match(viewerDate(item, 'en-GB'), /^Taken 14 July 2024(,| at) 18:05$/);
  assert.match(viewerDate({ revised_at: '2024-07-14T12:00:00Z' }, 'en-GB'), /^Changed /);
  assert.equal(viewerDate({}), '');
  assert.equal(positionOf(2, 20), '3 of 20');
  assert.ok(isPhoto({ kind: 'document', thumbnail: true }));
  assert.ok(!isPhoto({ kind: 'document' }));
  assert.ok(!isPhoto({ kind: 'folder', thumbnail: true }));
  assert.ok(INTERVALS.includes(5));
});
