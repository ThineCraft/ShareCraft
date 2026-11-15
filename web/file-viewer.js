// The viewer that takes the editor's place on file.html for a file no editor opens: a picture on
// a dark stage that zooms and pans, sound and film with the browser's own controls, text as a
// reading view with line numbers, or a plain message (held, quarantined, no preview) with the
// type's icon. Previous and Next step through the viewable files of the folder, with large edge
// buttons, ← and →, and an optional filmstrip of thumbnails.
//
// Pictures only ever load from the file's own preview and thumbnail routes (same origin). All
// sizes and moves are set through CSSOM custom properties; the maths is in viewer.js (tested).

import { formatSize, h, seg, showThumbnail } from './common.js';
import { fileIcon, fileKind, typeLabel } from './files.js';
import { addShortcut, isTextField } from './shortcuts.js';
import {
  clampPan,
  clampZoom,
  fitScale,
  lineNumbers,
  MAX_ZOOM,
  MIN_ZOOM,
  neighbours,
  nextZoom,
  positionText,
  rangeTotal,
  wheelFactor,
  zoomAbout,
  zoomLabel,
} from './viewer.js';

export const STRIP_KEY = 'sharecraft-viewer-strip';
export const WRAP_KEY = 'sharecraft-viewer-wrap';

const remembered = (key, fallback) => {
  try {
    const value = globalThis.localStorage?.getItem(key);
    return value === null || value === undefined ? fallback : value === '1';
  } catch {
    return fallback;
  }
};
const remember = (key, on) => {
  try {
    globalThis.localStorage?.setItem(key, on ? '1' : '0');
  } catch {
    // Not remembered: the choice holds for this page load.
  }
};

/**
 * The size of the file at `url` (a preview route that answers byte ranges), or null: one byte is
 * asked for and the total read from `Content-Range`. A route that sends the whole file instead is
 * stopped once its headers are in.
 */
export async function fetchSize(url) {
  const stop = new AbortController();
  try {
    const response = await fetch(url, { credentials: 'same-origin', headers: { Range: 'bytes=0-0' }, signal: stop.signal });
    const total = response.status === 206 ? rangeTotal(response.headers.get('Content-Range')) : null;
    const length = response.ok && response.status !== 206 ? Number(response.headers.get('Content-Length')) : NaN;
    stop.abort();
    if (total !== null) return total;
    return Number.isFinite(length) && length > 0 ? length : null;
  } catch {
    return null;
  }
}

const SVG_NS = 'http://www.w3.org/2000/svg';
/** A chevron pointing left (−1) or right (1), drawn, so it has the same weight everywhere. */
function chevron(direction) {
  const svg = document.createElementNS(SVG_NS, 'svg');
  svg.setAttribute('viewBox', '0 0 24 24');
  svg.setAttribute('aria-hidden', 'true');
  svg.setAttribute('class', 'pv-chev');
  const path = document.createElementNS(SVG_NS, 'path');
  path.setAttribute('d', direction < 0 ? 'M15 5l-7 7 7 7' : 'M9 5l7 7-7 7');
  svg.append(path);
  return svg;
}

/**
 * Creates the viewer in `area` (the page's editor area). It shows nothing until one of its
 * `show…` calls. `onStep(item)` is called when the person asks for another file of the list.
 */
export function createViewer(area, { onStep = () => {} } = {}) {
  let list = [];
  let current = '';
  let kind = '';           // what is shown: image, audio, video, text, message
  let image = null;        // the picture's state while one is shown
  let stripOn = remembered(STRIP_KEY, false);
  let wrapOn = remembered(WRAP_KEY, true);
  let announceTimer = 0;
  let stripWatcher = null;

  const status = h('span', { class: 'visually-hidden', role: 'status', 'aria-live': 'polite' });
  const content = h('div', { class: 'pv-content' });
  const edgePrev = h('button', { type: 'button', class: 'pv-edge prev', tabindex: '-1', 'aria-hidden': 'true', title: 'Previous (←)' }, chevron(-1));
  const edgeNext = h('button', { type: 'button', class: 'pv-edge next', tabindex: '-1', 'aria-hidden': 'true', title: 'Next (→)' }, chevron(1));
  const main = h('div', { class: 'pv-main' }, content, edgePrev, edgeNext);

  const prevButton = h('button', { type: 'button', class: 'plain', 'aria-keyshortcuts': 'ArrowLeft' }, chevron(-1), h('span', { class: 'pv-word' }, 'Previous'));
  const nextButton = h('button', { type: 'button', class: 'plain', 'aria-keyshortcuts': 'ArrowRight' }, h('span', { class: 'pv-word' }, 'Next'), chevron(1));
  const position = h('span', { class: 'pv-pos' });
  const nav = h('span', { class: 'pv-group pv-nav' }, prevButton, position, nextButton);
  const info = h('span', { class: 'pv-info' });

  const zoomOut = h('button', { type: 'button', class: 'plain pv-icon', 'aria-label': 'Zoom out', title: 'Zoom out (−)', 'aria-keyshortcuts': '-' }, '−');
  const zoomShown = h('output', { class: 'pv-level', 'aria-label': 'Zoom' }, 'Fit');
  const zoomIn = h('button', { type: 'button', class: 'plain pv-icon', 'aria-label': 'Zoom in', title: 'Zoom in (+)', 'aria-keyshortcuts': '+' }, '+');
  const fitButton = h('button', { type: 'button', class: 'plain', 'aria-pressed': 'true', title: 'Fit to the window (0)', 'aria-keyshortcuts': '0' }, 'Fit');
  const actualButton = h('button', { type: 'button', class: 'plain', 'aria-label': 'Actual size', title: 'Actual size (1)', 'aria-keyshortcuts': '1' }, '1:1');
  const zoomGroup = h('span', { class: 'pv-group pv-zoom', role: 'group', 'aria-label': 'Zoom' }, zoomOut, zoomShown, zoomIn, fitButton, actualButton);
  const wrapButton = h('button', { type: 'button', class: 'plain', 'aria-pressed': String(wrapOn) }, 'Wrap lines');
  const stripButton = h('button', { type: 'button', class: 'plain', 'aria-pressed': String(stripOn), 'aria-controls': 'pv-strip' }, 'Filmstrip');
  const tools = h('span', { class: 'pv-group pv-tools' }, zoomGroup, wrapButton, stripButton);
  const bar = h('div', { class: 'pv-bar' }, nav, info, tools);
  const strip = h('ul', { class: 'pv-strip', id: 'pv-strip', role: 'listbox', 'aria-label': 'Files in this folder', 'aria-orientation': 'horizontal' });
  const root = h('div', { class: 'pv' }, main, bar, strip, status);

  const shown = () => root.isConnected;
  const place = () => neighbours(list, current);

  // ---- what is shown ----

  /** Puts `node` in the viewer as what it shows now, of `next` kind. */
  function show(next, node) {
    kind = next;
    image = null;
    root.dataset.kind = next;
    content.replaceChildren(node);
    if (!shown()) area.replaceChildren(root);
    drawBar();
  }

  function announce(text, wait = 0) {
    clearTimeout(announceTimer);
    announceTimer = setTimeout(() => {
      status.textContent = text;
    }, wait);
  }

  function setInfo(...parts) {
    info.textContent = parts.filter(Boolean).join(' · ');
  }

  function currentItem() {
    return list.find((item) => item.id === current) ?? null;
  }

  // ---- the picture ----

  /** Shows the picture at `url`: `name` is the file's name (its alternative text says so). */
  function showImage({ url, name, onError }) {
    const img = h('img', { src: url, alt: `Preview of ${name}`, draggable: 'false', decoding: 'async' });
    const stage = h('div', { class: 'pv-stage loading' }, img);
    const state = { name, img, stage, natural: { w: 0, h: 0 }, view: { scale: 1, x: 0, y: 0 }, fitted: true, fit: 1, vector: /\.svg$/i.test(name) };
    show('image', stage);
    image = state;
    setInfo(typeLabel({ name, kind: 'document', editor: 'other' }));
    img.addEventListener('load', () => {
      if (image !== state) return;
      // A drawing with no size of its own is drawn at the stage's size.
      state.natural = { w: img.naturalWidth || 300, h: img.naturalHeight || 150 };
      img.style.setProperty('--w', `${state.natural.w}px`);
      img.style.setProperty('--h', `${state.natural.h}px`);
      stage.classList.remove('loading');
      setFit();
      sizeInfo();
    });
    img.addEventListener('error', () => {
      if (image === state) onError?.();
    });
    wireStage(state);
  }

  function boxOf(state) {
    return { w: state.stage.clientWidth, h: state.stage.clientHeight };
  }

  function apply(state = image, { say = true } = {}) {
    if (!state) return;
    state.view = clampPan(state.view, state.natural, boxOf(state));
    const { img, view } = state;
    img.style.setProperty('--s', String(view.scale));
    img.style.setProperty('--x', `${view.x}px`);
    img.style.setProperty('--y', `${view.y}px`);
    const box = boxOf(state);
    const larger = state.natural.w * view.scale > box.w + 1 || state.natural.h * view.scale > box.h + 1;
    state.stage.classList.toggle('can-pan', larger);
    state.stage.classList.toggle('pixels', view.scale >= 3 && !state.vector);
    const label = zoomLabel(view.scale, state.fitted);
    zoomShown.textContent = label;
    fitButton.setAttribute('aria-pressed', String(state.fitted));
    zoomIn.disabled = view.scale >= MAX_ZOOM - 1e-9;
    zoomOut.disabled = view.scale <= minZoom(state) + 1e-9;
    if (say) announce(state.fitted ? `Zoom: fitted, ${zoomLabel(view.scale)}` : `Zoom ${label}`, 400);
  }

  const minZoom = (state) => Math.min(MIN_ZOOM, state.fit);

  function setFit(state = image) {
    if (!state) return;
    state.fit = fitScale(state.natural, boxOf(state), state.vector ? MAX_ZOOM : 1);
    state.fitted = true;
    state.view = { scale: state.fit, x: 0, y: 0 };
    apply(state, { say: false });
  }

  /** Zooms to `scale` about `point` (from the stage's centre; the centre when left out). */
  function zoomTo(scale, point) {
    const state = image;
    if (!state || !state.natural.w) return;
    const next = clampZoom(scale, minZoom(state), MAX_ZOOM);
    state.view = zoomAbout(state.view, next, point);
    state.fitted = Math.abs(next - state.fit) < 1e-6 && state.view.x === 0 && state.view.y === 0;
    apply(state);
  }

  /** A point of a pointer event from the stage's centre. */
  function fromCentre(state, event) {
    const rect = state.stage.getBoundingClientRect();
    return { x: event.clientX - rect.left - rect.width / 2, y: event.clientY - rect.top - rect.height / 2 };
  }

  function pan(dx, dy) {
    const state = image;
    if (!state) return;
    state.view = { ...state.view, x: state.view.x + dx, y: state.view.y + dy };
    state.fitted = false;
    apply(state, { say: false });
  }

  function wireStage(state) {
    const { stage } = state;
    // The stage stays where it is: the wheel never scrolls; with Ctrl (and a touchpad's pinch,
    // which browsers send as a wheel with Ctrl) it zooms about the pointer.
    stage.addEventListener(
      'wheel',
      (event) => {
        event.preventDefault();
        if (!event.ctrlKey && !event.metaKey) return;
        const delta = event.deltaMode === 1 ? event.deltaY * 16 : event.deltaY;
        zoomTo(state.view.scale * wheelFactor(delta), fromCentre(state, event));
      },
      { passive: false },
    );
    stage.addEventListener('dblclick', (event) => {
      if (state.fitted) zoomTo(state.fit >= 1 ? Math.min(state.fit * 2, MAX_ZOOM) : 1, fromCentre(state, event));
      else setFit(state);
      announce(`Zoom ${zoomLabel(state.view.scale, state.fitted)}`);
    });
    // Drag to pan, two fingers to pinch, a swipe on an unzoomed picture to step.
    const pointers = new Map();
    let gesture = null;
    stage.addEventListener('pointerdown', (event) => {
      if (event.button !== 0 && event.pointerType === 'mouse') return;
      if (event.target.closest?.('button')) return;
      stage.setPointerCapture?.(event.pointerId);
      pointers.set(event.pointerId, { x: event.clientX, y: event.clientY });
      begin();
    });
    const begin = () => {
      const points = [...pointers.values()];
      if (points.length === 1) {
        gesture = { type: 'drag', start: points[0], view: { ...state.view }, fitted: state.fitted };
      } else if (points.length === 2) {
        const [a, b] = points;
        const rect = stage.getBoundingClientRect();
        const mid = { x: (a.x + b.x) / 2 - rect.left - rect.width / 2, y: (a.y + b.y) / 2 - rect.top - rect.height / 2 };
        gesture = { type: 'pinch', distance: Math.hypot(a.x - b.x, a.y - b.y) || 1, mid, view: { ...state.view } };
      }
    };
    stage.addEventListener('pointermove', (event) => {
      if (!pointers.has(event.pointerId) || !gesture) return;
      pointers.set(event.pointerId, { x: event.clientX, y: event.clientY });
      const points = [...pointers.values()];
      if (gesture.type === 'drag' && points.length === 1) {
        if (!stage.classList.contains('can-pan')) return;
        stage.classList.add('dragging');
        state.view = { ...gesture.view, x: gesture.view.x + points[0].x - gesture.start.x, y: gesture.view.y + points[0].y - gesture.start.y };
        state.fitted = false;
        apply(state, { say: false });
      } else if (gesture.type === 'pinch' && points.length === 2) {
        const [a, b] = points;
        const rect = stage.getBoundingClientRect();
        const mid = { x: (a.x + b.x) / 2 - rect.left - rect.width / 2, y: (a.y + b.y) / 2 - rect.top - rect.height / 2 };
        const scale = clampZoom(gesture.view.scale * (Math.hypot(a.x - b.x, a.y - b.y) / gesture.distance), minZoom(state), MAX_ZOOM);
        const zoomed = zoomAbout(gesture.view, scale, gesture.mid);
        state.view = { scale, x: zoomed.x + mid.x - gesture.mid.x, y: zoomed.y + mid.y - gesture.mid.y };
        state.fitted = false;
        apply(state);
      }
    });
    const end = (event) => {
      if (!pointers.has(event.pointerId)) return;
      const last = pointers.get(event.pointerId);
      pointers.delete(event.pointerId);
      stage.classList.remove('dragging');
      // A quick sideways swipe of a touch on a fitted picture steps to the next or previous file.
      if (gesture?.type === 'drag' && gesture.fitted && event.pointerType !== 'mouse' && pointers.size === 0 && event.type === 'pointerup') {
        const dx = last.x - gesture.start.x;
        if (Math.abs(dx) > 60 && Math.abs(dx) > Math.abs(last.y - gesture.start.y) * 1.5) step(dx < 0 ? 1 : -1);
      }
      gesture = null;
      if (pointers.size) begin();
    };
    stage.addEventListener('pointerup', end);
    stage.addEventListener('pointercancel', end);
  }

  // The stage follows the window: a fitted picture fits again, a zoomed one stays inside.
  const resize = new ResizeObserver(() => {
    if (!image?.natural.w) return;
    if (image.fitted) setFit();
    else apply(image, { say: false });
  });
  resize.observe(main);

  function sizeInfo() {
    const state = image;
    if (!state) return;
    setInfo(typeLabel({ name: state.name, kind: 'document', editor: 'other' }), state.natural.w ? `${state.natural.w} × ${state.natural.h} pixels` : '');
  }

  zoomIn.addEventListener('click', () => zoomTo(nextZoom(image?.view.scale ?? 1, 1)));
  zoomOut.addEventListener('click', () => zoomTo(nextZoom(image?.view.scale ?? 1, -1)));
  fitButton.addEventListener('click', () => {
    setFit();
    announce('Zoom: fitted');
  });
  actualButton.addEventListener('click', () => zoomTo(1));

  // ---- sound and film ----

  /** Shows sound or film (`which` audio or video) at `url` with the browser's controls; never plays by itself. */
  function showMedia(which, { url, name, onError }) {
    const media = h(which, { src: url, controls: true, preload: 'metadata', 'aria-label': `Preview of ${name}`, ...(which === 'video' ? { playsinline: true } : {}) });
    media.autoplay = false;
    const size = h('span', { class: 'meta' });
    const caption = h('p', { class: 'pv-caption' }, h('strong', {}, name), size);
    const box = h('div', { class: `pv-media ${which}` }, which === 'audio' ? h('span', { class: 'pv-big-icon' }, fileIcon('audio')) : null, media, caption);
    media.addEventListener('error', () => {
      if (content.contains(media)) onError?.();
    });
    show(which, box);
    setInfo(typeLabel({ name, kind: 'document', editor: 'other' }));
    fetchSize(url).then((bytes) => {
      if (!content.contains(media) || bytes === null) return;
      size.textContent = formatSize(bytes);
      setInfo(typeLabel({ name, kind: 'document', editor: 'other' }), formatSize(bytes));
    });
  }

  // ---- text ----

  /** Shows `text` as a reading view with line numbers; `cut` adds the note that only 1 MiB shows. */
  function showText({ text, cut, name }) {
    const { lines, width } = lineNumbers(text);
    const body = h('div', { class: 'pv-lines' });
    body.style.setProperty('--digits', String(width));
    const fragment = document.createDocumentFragment();
    // One block per line: the number is drawn beside it (never selected or copied), and an empty
    // line holds a line break so that copying keeps it.
    for (const line of lines) {
      const row = document.createElement('div');
      row.className = 'ln';
      if (line) row.textContent = line;
      else row.append(document.createElement('br'));
      fragment.append(row);
    }
    body.append(fragment);
    const reader = h('div', { class: 'pv-text', tabindex: '0', role: 'region', 'aria-label': `Text of ${name}` },
      cut ? h('p', { class: 'note' }, 'Only the first 1 MiB is shown. Download the file to see all of it.') : null,
      body);
    reader.classList.toggle('nowrap', !wrapOn);
    show('text', reader);
    setInfo(typeLabel({ name, kind: 'document', editor: 'other' }), `${lines.length.toLocaleString()} ${lines.length === 1 ? 'line' : 'lines'}`);
  }

  wrapButton.addEventListener('click', () => {
    wrapOn = !wrapOn;
    remember(WRAP_KEY, wrapOn);
    wrapButton.setAttribute('aria-pressed', String(wrapOn));
    content.querySelector('.pv-text')?.classList.toggle('nowrap', !wrapOn);
  });

  // ---- messages ----

  /**
   * Shows a message in place of a preview: `text` (and `detail`), the type's icon for `name`, and
   * Download when `download` is a link to the file (`noVersion` says there is nothing to download).
   * `tone` is '' or `stop` (quarantined).
   */
  function showMessage({ name = '', text, detail = '', download = null, noVersion = false, tone = '', node = null }) {
    const iconKind = fileKind(name, 'other');
    const box = h(
      'div',
      { class: tone ? `pv-message ${tone}` : 'pv-message' },
      h('span', { class: 'pv-big-icon' }, fileIcon(iconKind)),
      name ? h('p', { class: 'pv-name' }, name) : null,
      node ?? h('p', {}, text),
      detail ? h('p', { class: 'meta' }, detail) : null,
      download ? h('a', { class: 'button', href: download, download: '' }, 'Download') : noVersion ? h('p', { class: 'meta' }, 'Not saved yet') : null,
    );
    show('message', box);
    setInfo(name ? typeLabel({ name, kind: 'document', editor: 'other' }) : '');
    return box;
  }

  // ---- stepping ----

  function step(direction) {
    const { prev, next } = place();
    const target = direction < 0 ? prev : next;
    if (target) onStep(target);
    return Boolean(target);
  }

  edgePrev.addEventListener('click', () => step(-1));
  edgeNext.addEventListener('click', () => step(1));
  prevButton.addEventListener('click', () => step(-1));
  nextButton.addEventListener('click', () => step(1));

  function drawBar() {
    const where = place();
    const many = where.index >= 0 && where.total > 1;
    nav.hidden = !many;
    position.textContent = positionText(where);
    prevButton.disabled = !where.prev;
    nextButton.disabled = !where.next;
    prevButton.setAttribute('aria-label', where.prev ? `Previous: ${where.prev.name}` : 'Previous');
    nextButton.setAttribute('aria-label', where.next ? `Next: ${where.next.name}` : 'Next');
    edgePrev.hidden = !where.prev;
    edgeNext.hidden = !where.next;
    zoomGroup.hidden = kind !== 'image';
    wrapButton.hidden = kind !== 'text';
    stripButton.hidden = !many;
    strip.hidden = !many || !stripOn;
    root.classList.toggle('with-strip', many && stripOn);
    markStrip();
  }

  stripButton.addEventListener('click', () => {
    stripOn = !stripOn;
    remember(STRIP_KEY, stripOn);
    stripButton.setAttribute('aria-pressed', String(stripOn));
    drawStrip();
    drawBar();
  });

  // ---- the filmstrip ----

  function drawStrip() {
    stripWatcher?.disconnect();
    stripWatcher = null;
    if (!stripOn || list.length < 2) {
      strip.replaceChildren();
      return;
    }
    stripWatcher = new IntersectionObserver(
      (entries) => {
        for (const entry of entries) {
          if (!entry.isIntersecting) continue;
          stripWatcher?.unobserve(entry.target);
          loadThumb(entry.target);
        }
      },
      { root: strip, rootMargin: '0px 300px' },
    );
    strip.replaceChildren(
      ...list.map((item) => {
        const picture = h('span', { class: 'pv-thumb', 'aria-hidden': 'true' }, fileIcon(fileKind(item.name, 'other')));
        picture.dataset.id = item.id;
        const option = h('li', { role: 'option', tabindex: '-1', 'aria-selected': 'false', title: item.name }, picture, h('span', { class: 'visually-hidden' }, item.name));
        option.dataset.id = item.id;
        option.addEventListener('click', () => {
          if (item.id !== current) onStep(item);
        });
        stripWatcher.observe(picture);
        return option;
      }),
    );
    markStrip();
  }

  function loadThumb(picture) {
    const item = list.find((entry) => entry.id === picture.dataset.id);
    if (!item) return;
    const icon = () => fileIcon(fileKind(item.name, 'other'));
    if (item.thumbnail) showThumbnail(picture, `/api/docs/${seg(item.id)}/thumbnail`, { fallback: icon });
    else if (/\.svg$/i.test(item.name)) showThumbnail(picture, `/api/docs/${seg(item.id)}/preview`, { fallback: icon, load: async () => null });
  }

  function markStrip() {
    let chosen = null;
    for (const option of strip.children) {
      const on = option.dataset.id === current;
      option.setAttribute('aria-selected', String(on));
      option.tabIndex = on ? 0 : -1;
      if (on) chosen = option;
    }
    if (chosen && !strip.hidden) {
      const reduce = globalThis.matchMedia?.('(prefers-reduced-motion: reduce)').matches;
      chosen.scrollIntoView({ block: 'nearest', inline: 'center', behavior: reduce ? 'auto' : 'smooth' });
    }
  }

  strip.addEventListener('keydown', (event) => {
    const options = [...strip.children];
    const at = options.findIndex((option) => option === document.activeElement);
    let to = -1;
    if (event.key === 'ArrowRight' || event.key === 'ArrowDown') to = Math.min(at + 1, options.length - 1);
    else if (event.key === 'ArrowLeft' || event.key === 'ArrowUp') to = Math.max(at - 1, 0);
    else if (event.key === 'Home') to = 0;
    else if (event.key === 'End') to = options.length - 1;
    if (to < 0) return;
    event.preventDefault();
    options[to].focus();
    const item = list.find((entry) => entry.id === options[to].dataset.id);
    if (item && item.id !== current) onStep(item);
  });

  // ---- keys ----

  // Arrows that belong to something else: a player's seek bar, a text field, a scrolled text
  // without wrapping, the filmstrip (a list box, which the registry leaves alone).
  const arrowsFree = (event) => {
    const target = event.target;
    if (isTextField(target)) return false;
    if (target?.closest?.('audio,video,input[type=range]')) return false;
    if (target?.closest?.('.pv-text.nowrap')) return false;
    return true;
  };
  addShortcut({
    keys: ['ArrowLeft'],
    label: 'Previous file in the folder',
    scope: 'viewer',
    shown: () => shown() && place().total > 1,
    when: (event) => shown() && arrowsFree(event) && Boolean(place().prev),
    run: () => step(-1),
  });
  addShortcut({
    keys: ['ArrowRight'],
    label: 'Next file in the folder',
    scope: 'viewer',
    shown: () => shown() && place().total > 1,
    when: (event) => shown() && arrowsFree(event) && Boolean(place().next),
    run: () => step(1),
  });
  const picture = () => shown() && kind === 'image' && Boolean(image?.natural.w);
  addShortcut({ keys: ['+', '='], display: ['+'], label: 'Zoom in', scope: 'viewer', shown: picture, when: picture, run: () => zoomTo(nextZoom(image.view.scale, 1)) });
  addShortcut({ keys: ['-'], label: 'Zoom out', scope: 'viewer', shown: picture, when: picture, run: () => zoomTo(nextZoom(image.view.scale, -1)) });
  addShortcut({ keys: ['0'], label: 'Fit the picture to the window', scope: 'viewer', shown: picture, when: picture, run: () => { setFit(); announce('Zoom: fitted'); } });
  addShortcut({ keys: ['1'], label: 'Actual size (100 %)', scope: 'viewer', shown: picture, when: picture, run: () => zoomTo(1) });
  const panStep = () => Math.max(40, Math.round(Math.min(main.clientWidth, main.clientHeight) * 0.1));
  addShortcut({
    keys: ['Shift+ArrowLeft', 'Shift+ArrowRight', 'Shift+ArrowUp', 'Shift+ArrowDown'],
    display: ['Shift+ArrowLeft', 'Shift+ArrowRight', 'Shift+ArrowUp', 'Shift+ArrowDown'],
    label: 'Move around a zoomed picture',
    scope: 'viewer',
    shown: picture,
    when: (event) => picture() && arrowsFree(event) && image.stage.classList.contains('can-pan'),
    run: (event) => {
      const d = panStep();
      const moves = { ArrowLeft: [d, 0], ArrowRight: [-d, 0], ArrowUp: [0, d], ArrowDown: [0, -d] };
      const [dx, dy] = moves[event.key] ?? [0, 0];
      pan(dx, dy);
    },
  });

  return {
    root,
    shown,
    showImage,
    showMedia,
    showText,
    showMessage,
    /** The files to step through and the one open now (`list` from viewerList). */
    setList(next, id) {
      list = Array.isArray(next) ? next : [];
      current = id;
      drawStrip();
      drawBar();
    },
    /** The file open now changed (a step): the bar, the strip and the announcement follow. */
    setCurrent(id) {
      current = id;
      drawBar();
      const where = place();
      const item = currentItem();
      if (item) announce([item.name, positionText(where)].filter(Boolean).join(', '));
    },
    /** A file of the list was renamed. */
    rename(id, name) {
      const item = list.find((entry) => entry.id === id);
      if (!item) return;
      item.name = name;
      const option = [...strip.children].find((entry) => entry.dataset.id === id);
      if (option) {
        option.title = name;
        option.querySelector('.visually-hidden').textContent = name;
      }
      drawBar();
    },
  };
}
