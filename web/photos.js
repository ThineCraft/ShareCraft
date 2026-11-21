// The Album view of an image library, the maths of it: justified rows of even height that
// keep each picture's shape, days to group them by, moving through them with the arrow keys,
// and the steps of the viewer and its slideshow. Everything here is pure and tested
// (photos.test.mjs); photo-view.js and photo-viewer.js draw it.

/** The height a row aims at, and the least, in CSS pixels, by the width it fills. */
export function rowHeightFor(width) {
  if (width < 480) return 120;
  if (width < 900) return 170;
  return 220;
}

/** The gap between pictures, in CSS pixels. */
export const GAP = 4;

/** The narrowest and widest shape a tile may take: panoramas and slivers are cropped to these. */
export const MIN_RATIO = 0.4;
export const MAX_RATIO = 3;

/** The slideshow's intervals, in seconds, and the one it starts with. */
export const INTERVALS = Object.freeze([3, 5, 10, 20]);
export const DEFAULT_INTERVAL = 5;

/**
 * The shape (width over height) of a listed picture: from its `photo` facts, else from what its
 * thumbnail showed once loaded (`known`, a Map of id to shape), else square until it loads.
 * Kept between {@link MIN_RATIO} and {@link MAX_RATIO}.
 */
export function ratioOf(item, known = null) {
  const facts = item?.photo;
  let ratio = facts?.width > 0 && facts?.height > 0 ? facts.width / facts.height : known?.get(item?.id);
  if (!(ratio > 0) || !Number.isFinite(ratio)) ratio = 1;
  return Math.min(MAX_RATIO, Math.max(MIN_RATIO, ratio));
}

/**
 * Lays `ratios` out in justified rows filling `width`: each row is as many pictures as fit at
 * about `height`, then stretched so they fill the width exactly, all of the row's height. The
 * last row, when not full, keeps `height` and is not stretched. Positions are whole pixels.
 * Returns `{ tiles: [{ x, y, w, h, row }], rows: [{ y, h, first, count }], height }`.
 */
export function justify(ratios, width, { height = rowHeightFor(width), gap = GAP } = {}) {
  const tiles = [];
  const rows = [];
  let y = 0;
  let start = 0;
  const usable = Math.max(1, width);
  const place = (first, count, rowHeight, stretch) => {
    const h = Math.max(1, Math.round(rowHeight));
    const room = usable - gap * (count - 1);
    let sum = 0;
    for (let i = first; i < first + count; i += 1) sum += ratios[i];
    // Each picture's edges are rounded from the running total, so the widths add up exactly.
    let edge = 0;
    for (let i = first; i < first + count; i += 1) {
      const left = Math.round(edge);
      edge += stretch ? (ratios[i] / sum) * room : ratios[i] * rowHeight;
      const right = stretch && i === first + count - 1 ? room : Math.round(edge);
      tiles.push({ x: left + (i - first) * gap, y, w: Math.max(1, right - left), h, row: rows.length });
    }
    rows.push({ y, h, first, count });
    y += h + gap;
  };
  let sum = 0;
  for (let i = 0; i < ratios.length; i += 1) {
    sum += ratios[i];
    const count = i - start + 1;
    const rowHeight = (usable - gap * (count - 1)) / sum;
    if (rowHeight <= height) {
      place(start, count, rowHeight, true);
      start = i + 1;
      sum = 0;
    }
  }
  if (start < ratios.length) {
    const count = ratios.length - start;
    // A last row that would not fill the width keeps the height aimed at.
    const fill = (usable - gap * (count - 1)) / sum;
    place(start, count, Math.min(height, fill), fill <= height);
  }
  return { tiles, rows, height: rows.length ? y - gap : 0 };
}

/**
 * The day a picture belongs to, `YYYY-MM-DD`: the day it was taken (as the camera's clock said,
 * no zone), else the local day of its last version (`revised_at`, RFC 3339). `''` for neither.
 */
export function dayOf(item) {
  const taken = item?.photo?.taken_at;
  if (typeof taken === 'string' && /^\d{4}-\d{2}-\d{2}/.test(taken)) return taken.slice(0, 10);
  const when = item?.revised_at ? new Date(item.revised_at) : null;
  if (!when || Number.isNaN(when.getTime())) return '';
  const pad = (n) => String(n).padStart(2, '0');
  return `${when.getFullYear()}-${pad(when.getMonth() + 1)}-${pad(when.getDate())}`;
}

/** When a picture was taken, else last changed, as a Date for showing; null for neither. */
export function whenOf(item) {
  const taken = item?.photo?.taken_at;
  if (typeof taken === 'string') {
    // No zone: read as local time, as the camera meant it.
    const m = /^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2}):(\d{2})/.exec(taken);
    if (m) return new Date(Number(m[1]), Number(m[2]) - 1, Number(m[3]), Number(m[4]), Number(m[5]), Number(m[6]));
  }
  const when = item?.revised_at ? new Date(item.revised_at) : null;
  return when && !Number.isNaN(when.getTime()) ? when : null;
}

/**
 * The pictures in days, newest day first and, within a day, newest first (then by name), each
 * `{ day, items }`; pictures with no day last. Ungrouped (`grouped` false), one group of the
 * pictures in the listing's order, with the day `''`.
 */
export function groupByDay(items, grouped = true) {
  if (!grouped) return items.length ? [{ day: '', items: [...items] }] : [];
  const time = (item) => whenOf(item)?.getTime() ?? -Infinity;
  const sorted = [...items].sort((a, b) => {
    const da = dayOf(a);
    const db = dayOf(b);
    if (da !== db) return da === '' ? 1 : db === '' ? -1 : da < db ? 1 : -1;
    return time(b) - time(a) || String(a.name).localeCompare(String(b.name));
  });
  const groups = [];
  for (const item of sorted) {
    const day = dayOf(item);
    if (groups.at(-1)?.day !== day) groups.push({ day, items: [] });
    groups.at(-1).items.push(item);
  }
  return groups;
}

/**
 * The heading of a day: Today, Yesterday, the weekday and date within this year, or the full
 * date; `No date` for `''`. `now` is the present (a Date), `language` the page's.
 */
export function dayLabel(day, now = new Date(), language = undefined) {
  if (!day) return 'No date';
  const [y, m, d] = day.split('-').map(Number);
  const date = new Date(y, m - 1, d);
  if (Number.isNaN(date.getTime())) return day;
  const today = new Date(now.getFullYear(), now.getMonth(), now.getDate());
  const days = Math.round((today - date) / 86_400_000);
  if (days === 0) return 'Today';
  if (days === 1) return 'Yesterday';
  const options = { weekday: 'short', day: 'numeric', month: 'short' };
  if (y !== now.getFullYear()) options.year = 'numeric';
  return new Intl.DateTimeFormat(language, options).format(date);
}

/**
 * Where an arrow key (or Home, End, PageUp, PageDown) moves focus from picture `index` in a
 * layout's `tiles` (in order, with their row), or null when it does not move. Up and Down go
 * to the picture of the row above or below whose middle is nearest; `page` is how many rows
 * PageUp and PageDown skip.
 */
export function moveIn(tiles, index, key, page = 3) {
  const n = tiles.length;
  if (n === 0) return null;
  const at = Math.min(Math.max(index, 0), n - 1);
  switch (key) {
    case 'ArrowLeft':
      return at > 0 ? at - 1 : null;
    case 'ArrowRight':
      return at < n - 1 ? at + 1 : null;
    case 'Home':
      return at === 0 ? null : 0;
    case 'End':
      return at === n - 1 ? null : n - 1;
    case 'ArrowUp':
    case 'ArrowDown':
    case 'PageUp':
    case 'PageDown': {
      const step = key === 'ArrowUp' ? -1 : key === 'ArrowDown' ? 1 : key === 'PageUp' ? -page : page;
      const rows = tiles.at(-1).row + 1;
      const target = Math.min(Math.max(tiles[at].row + step, 0), rows - 1);
      if (target === tiles[at].row) return null;
      const middle = tiles[at].x + tiles[at].w / 2;
      let best = null;
      for (let i = 0; i < n; i += 1) {
        if (tiles[i].row !== target) continue;
        const distance = Math.abs(tiles[i].x + tiles[i].w / 2 - middle);
        if (best === null || distance < best.distance) best = { i, distance };
      }
      return best?.i ?? null;
    }
    default:
      return null;
  }
}

/** The picture after (`step` 1) or before (−1) `index` of `count`, wrapping when `loop`; null at an end. */
export function stepIndex(index, count, step, loop = false) {
  if (count <= 0) return null;
  const next = index + step;
  if (next >= 0 && next < count) return next;
  if (!loop) return null;
  return ((next % count) + count) % count;
}

/**
 * What a finished touch on the viewer means: `next` for a swipe left, `previous` for one right,
 * `close` for a swipe down, null otherwise. A swipe is quick (under 600 ms), long enough
 * (50 CSS pixels) and mostly in one direction.
 */
export function swipeOf(dx, dy, ms) {
  if (!(ms < 600)) return null;
  const ax = Math.abs(dx);
  const ay = Math.abs(dy);
  if (ax >= 50 && ax > ay * 1.5) return dx < 0 ? 'next' : 'previous';
  if (dy >= 80 && ay > ax * 1.5) return 'close';
  return null;
}

/** What the viewer announces and shows of where it is: `3 of 20`. */
export const positionOf = (index, count) => `${index + 1} of ${count}`;

/** A picture's accessible name in the grid: its name and, when known, its day. */
export function tileLabel(item, language = undefined) {
  const when = whenOf(item);
  if (!when) return String(item?.name ?? '');
  const date = new Intl.DateTimeFormat(language, { day: 'numeric', month: 'long', year: 'numeric' }).format(when);
  return `${item.name}, ${date}`;
}

/** The time shown under a picture in the viewer, e.g. `14 July 2024, 18:05`. */
export function viewerDate(item, language = undefined) {
  const when = whenOf(item);
  if (!when) return '';
  const date = new Intl.DateTimeFormat(language, { day: 'numeric', month: 'long', year: 'numeric', hour: 'numeric', minute: '2-digit' }).format(when);
  return item?.photo?.taken_at ? `Taken ${date}` : `Changed ${date}`;
}

/** Whether a listed item is a picture the Album view shows: a file with a thumbnail. */
export const isPhoto = (item) => item?.kind === 'document' && item?.thumbnail === true;
