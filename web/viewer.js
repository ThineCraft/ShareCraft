// The viewer of files no editor opens (file.html): the maths of zooming and panning a picture,
// which files of the folder it steps through and in what order, and the lines of a text file.
// Everything here is pure and tested (viewer.test.mjs); file-viewer.js draws it.

import { previewKindOf } from './common.js';
import { parseSort, sortEntries } from './files.js';

/** The smallest and largest zoom a picture can have (5 % and 1600 %). */
export const MIN_ZOOM = 0.05;
export const MAX_ZOOM = 16;

/** The zoom levels + and − step through. */
export const ZOOM_STEPS = Object.freeze([0.05, 0.1, 0.25, 1 / 3, 0.5, 2 / 3, 0.75, 1, 1.25, 1.5, 2, 3, 4, 6, 8, 12, 16]);

/**
 * The scale at which a picture of `natural` size (`{ w, h }` in pixels) fits in `box` (`{ w, h }`):
 * the whole picture shows. A picture smaller than the box is shown at its actual size (`max` 1),
 * unless `max` allows more (a drawing, which stays sharp at any size). 1 when a size is unknown.
 */
export function fitScale(natural, box, max = 1) {
  const [w, h, bw, bh] = [natural?.w, natural?.h, box?.w, box?.h].map(Number);
  if (!(w > 0 && h > 0 && bw > 0 && bh > 0)) return 1;
  return Math.min(bw / w, bh / h, max);
}

/** `scale` kept between `min` and `max` (by default MIN_ZOOM and MAX_ZOOM); 1 when not a number. */
export function clampZoom(scale, min = MIN_ZOOM, max = MAX_ZOOM) {
  const value = Number(scale);
  if (!Number.isFinite(value) || value <= 0) return 1;
  return Math.min(Math.max(value, min), max);
}

/**
 * The next zoom step after `scale` going `direction` (1 larger, −1 smaller): the first level of
 * ZOOM_STEPS beyond it (a little tolerance, so 99.9 % goes on to 125 %), or the end of the range.
 */
export function nextZoom(scale, direction) {
  const value = clampZoom(scale);
  const near = 1.0001;
  if (direction > 0) return ZOOM_STEPS.find((step) => step > value * near) ?? MAX_ZOOM;
  return [...ZOOM_STEPS].reverse().find((step) => step * near < value) ?? MIN_ZOOM;
}

/**
 * The view after zooming to `scale` about `point`: the picture's spot under `point` stays there.
 * A view is `{ scale, x, y }`, `x`/`y` how far the picture's centre is moved from the stage's
 * centre; `point` is `{ x, y }` from the stage's centre (the centre itself when left out).
 */
export function zoomAbout(view, scale, point = { x: 0, y: 0 }) {
  const ratio = scale / view.scale;
  return {
    scale,
    x: point.x - (point.x - view.x) * ratio,
    y: point.y - (point.y - view.y) * ratio,
  };
}

/**
 * `view` with its move kept so that the picture (of `natural` size) never leaves the stage
 * (`box`) more than it must: centred on an axis where it is smaller than the stage, its edges no
 * further in than the stage's edges where it is larger.
 */
export function clampPan(view, natural, box) {
  const limit = (size, room, at) => {
    const spare = (size * view.scale - room) / 2;
    if (!(spare > 0)) return 0;
    return Math.min(Math.max(at, -spare), spare);
  };
  return { scale: view.scale, x: limit(natural.w, box.w, view.x), y: limit(natural.h, box.h, view.y) };
}

/** The zoom as shown: "Fit" when the picture is fitted, otherwise "250 %" (a no-break space). */
export function zoomLabel(scale, fitted = false) {
  if (fitted) return 'Fit';
  return `${Math.round(clampZoom(scale) * 100)} %`;
}

/** The scale a wheel turn (`deltaY`, in pixels) multiplies the zoom by: up is larger. */
export function wheelFactor(deltaY) {
  const value = Math.max(-200, Math.min(200, Number(deltaY) || 0));
  return Math.exp(-value * 0.002);
}

/**
 * Whether a listed child (`GET /api/nodes/{id}/children`) is a file the viewer shows: a file no
 * editor opens, with a version, that the browser can be shown (an image, sound, film or text),
 * not quarantined or held by the malware scanner.
 */
export function isViewable(item) {
  if (!item || item.kind !== 'document') return false;
  if ((item.editor ?? 'other') !== 'other') return false;
  if (item.scan || !item.revised_at) return false;
  return previewKindOf(item.name) !== null;
}

/**
 * The files the viewer steps through, from a folder's children: those it shows, plus the file
 * open now (`current`, whatever it is), in the order the library lists them (`sort`, as the
 * library remembers it: `{ key, direction }` or its JSON; by name when there is none).
 */
export function viewerList(children, current, sort) {
  const chosen = parseSort(sort ?? null);
  const kept = (Array.isArray(children) ? children : []).filter((item) => isViewable(item) || item?.id === current);
  return sortEntries(kept, chosen.key, chosen.direction);
}

/**
 * Where `id` is in `list` (items with `id`): `{ index, total, prev, next }`, `prev`/`next` the
 * items either side (null at the ends, as the viewer does not wrap round) and `index` from 0.
 * When `id` is not in the list: index −1 and no neighbours.
 */
export function neighbours(list, id) {
  const items = Array.isArray(list) ? list : [];
  const index = items.findIndex((item) => item?.id === id);
  if (index < 0) return { index: -1, total: items.length, prev: null, next: null };
  return {
    index,
    total: items.length,
    prev: index > 0 ? items[index - 1] : null,
    next: index < items.length - 1 ? items[index + 1] : null,
  };
}

/** "3 of 12", or '' when the file is not in the list or is the only one. */
export function positionText({ index, total }) {
  return index >= 0 && total > 1 ? `${index + 1} of ${total}` : '';
}

/**
 * The lines of `text` for the reading view: `{ lines, width }`, `lines` split at each line break
 * (\n, \r\n or \r) without one empty line after a final break, `width` the digits of the last
 * line number (for the gutter). Empty text is one empty line.
 */
export function lineNumbers(text) {
  const lines = String(text ?? '').split(/\r\n|\n|\r/);
  if (lines.length > 1 && lines[lines.length - 1] === '') lines.pop();
  return { lines, width: String(lines.length).length };
}

/** The total size from a `Content-Range` answer ("bytes 0-0/1234"), or null. */
export function rangeTotal(header) {
  const match = /\/(\d+)\s*$/.exec(String(header ?? ''));
  return match ? Number(match[1]) : null;
}
